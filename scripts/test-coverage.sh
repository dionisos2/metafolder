#!/usr/bin/env bash
# Tests for scripts/coverage.sh — the order of its steps and what each is given.
#
# The point of the script is that the GUI end-to-end run counts: its binaries
# must be the instrumented ones (METAFOLDER_BIN_DIR), built and run under the
# same environment as the tests, *before* the one report. Every tool is a stub
# that logs its call; what is asserted is the log.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/lib/assert.sh"

mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/coverage.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

fake="$tmp/repo"
bin="$tmp/bin"
mkdir -p "$fake/scripts" "$bin"
cp "$repo/scripts/coverage.sh" "$fake/scripts/"

# gui-e2e.sh: logs the binary directory it was pointed at and its arguments;
# exits $E2E_RC.
cat >"$fake/scripts/gui-e2e.sh" <<'STUB'
#!/usr/bin/env bash
echo "gui-e2e bin=${METAFOLDER_BIN_DIR:-} profile=${LLVM_PROFILE_FILE:-}${*:+ $*}" >>"$LOG"
exit "${E2E_RC:-0}"
STUB
# cargo: `llvm-cov show-env` prints an environment naming the target dir it
# was given; everything else is logged. `test` exits $TEST_RC.
cat >"$bin/cargo" <<'STUB'
#!/usr/bin/env bash
if [ "$1 $2" = "llvm-cov show-env" ]; then
    echo "export LLVM_PROFILE_FILE='$CARGO_TARGET_DIR/p-%p.profraw'"
    echo "export CARGO_LLVM_COV_TARGET_DIR='$CARGO_TARGET_DIR'"
    exit 0
fi
echo "cargo $* target=${CARGO_TARGET_DIR:-} profile=${LLVM_PROFILE_FILE:-}" >>"$LOG"
[ "$1" = test ] && exit "${TEST_RC:-0}"
exit 0
STUB
cat >"$bin/git" <<STUB
#!/usr/bin/env bash
printf '%s\n' "$fake"
STUB
chmod +x "$bin"/* "$fake/scripts/gui-e2e.sh"

run() { # [env assignments…] -- [coverage.sh args…]
    : >"$tmp/log"
    local envs=()
    while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
    shift
    (cd "$fake" && env LOG="$tmp/log" PATH="$bin:$PATH" "${envs[@]}" \
        bash scripts/coverage.sh "$@" 2>&1)
}

cov="$fake/target/llvm-cov-target"

# ── 1. with a display: tests, instrumented build, e2e, then one report ───────
out=$(run DISPLAY=:9 --); rc=$?
assert_eq "exits 0 when everything passes" 0 "$rc"
assert_eq "the steps, in order" \
"cargo llvm-cov clean --workspace target=$cov profile=$cov/p-%p.profraw
cargo test --workspace --no-fail-fast target=$cov profile=$cov/p-%p.profraw
cargo build -p metafolder-daemon -p metafolder-cli -p metafolder-gui target=$cov profile=$cov/p-%p.profraw
gui-e2e bin=$cov/debug profile=$cov/p-%p.profraw
cargo llvm-cov report --summary-only target=$cov profile=$cov/p-%p.profraw" \
    "$(cat "$tmp/log")"

# ── 2. options: --unsandboxed reaches the e2e, --html the report ─────────────
out=$(run DISPLAY=:9 -- --unsandboxed --html); rc=$?
assert_eq "exits 0 with options" 0 "$rc"
assert "--unsandboxed is passed to gui-e2e.sh" grep -q '^gui-e2e .* --unsandboxed$' "$tmp/log"
assert "--html asks for the html report" grep -q '^cargo llvm-cov report --html ' "$tmp/log"

# ── 3. no display: a loud SKIP, the report still written ─────────────────────
out=$(run DISPLAY= WAYLAND_DISPLAY= --); rc=$?
assert_eq "no display is not a failure" 0 "$rc"
assert_contains "no display is announced" "$out" "SKIP gui-e2e"
assert_not "no e2e without a display" grep -q '^gui-e2e' "$tmp/log"
assert "the report is still written" grep -q '^cargo llvm-cov report' "$tmp/log"

# ── 4. --no-gui: the same skip, asked for ────────────────────────────────────
out=$(run DISPLAY=:9 -- --no-gui); rc=$?
assert_not "--no-gui runs no e2e" grep -q '^gui-e2e' "$tmp/log"
assert_not "--no-gui builds nothing extra" grep -q '^cargo build' "$tmp/log"

# ── 5. failures: reported in the exit status, the report written anyway ──────
out=$(run DISPLAY=:9 TEST_RC=101 --); rc=$?
assert_eq "a failing test fails the run" 1 "$rc"
assert "the e2e still runs after a failing test" grep -q '^gui-e2e' "$tmp/log"
assert "the report is written after a failing test" grep -q '^cargo llvm-cov report' "$tmp/log"

out=$(run DISPLAY=:9 E2E_RC=1 --); rc=$?
assert_eq "a failing e2e fails the run" 1 "$rc"
assert "the report is written after a failing e2e" grep -q '^cargo llvm-cov report' "$tmp/log"

# ── 6. an unknown option is a usage error ────────────────────────────────────
out=$(run --  --bogus); rc=$?
assert_eq "an unknown option exits 2" 2 "$rc"

assert_summary
