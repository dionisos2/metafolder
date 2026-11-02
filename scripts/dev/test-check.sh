#!/usr/bin/env bash
# Tests for scripts/dev/check.sh — the disk-space warning, the verdict of a suite
# that ran nothing, and the help.
#
# check.sh never prunes target/ (pruning after clippy and the tests, without
# the plain build and the sync-config core, would evict those universes every
# run: see make dev), so a target/ that grows until a test dies on
# "No space left on device" has to be announced instead. The checks
# themselves run against stubs: what is asserted is the warning.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/dev/lib/assert.sh"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/check.XXXXXX" 2>/dev/null) \
    || { mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
         tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/check.XXXXXX"); }
trap 'rm -rf "$tmp"' EXIT

fake="$tmp/repo"
bin="$tmp/bin"
mkdir -p "$fake/scripts/dev" "$fake/target" "$bin"
cp "$repo/scripts/dev/check.sh" "$fake/scripts/dev/"

# The repo-local suites check.sh runs: succeed, silently.
for s in run-tests.sh test-shipped-scripts.sh test-tooling.sh doc; do
    printf '#!/usr/bin/env bash\nexit 0\n' >"$fake/scripts/dev/$s"
done

# Every external: succeeds. `df` reports $FREE_KIB free, in its -P form.
for tool in cargo semgrep; do
    printf '#!/usr/bin/env bash\nexit 0\n' >"$bin/$tool"
done
cat >"$bin/git" <<STUB
#!/usr/bin/env bash
printf '%s\n' "$fake"
STUB
cat >"$bin/df" <<'STUB'
#!/usr/bin/env bash
echo "Filesystem 1024-blocks Used Available Capacity Mounted on"
echo "/dev/x 100000000 1 $FREE_KIB 99% /"
STUB
chmod +x "$bin"/*

run() { # <free KiB>
    (cd "$fake" && FREE_KIB=$1 PATH="$bin:$PATH" bash scripts/dev/check.sh 2>&1)
}

# ── 1. plenty of room: no warning ────────────────────────────────────────────
out=$(run $((200 * 1024 * 1024))); rc=$?
assert_eq "exits 0 with room to spare" 0 "$rc"
assert_not "no warning with room to spare" grep -q 'free on' <<<"$out"

# ── 2. nearly full: a warning naming the cure, and still a pass ──────────────
out=$(run $((3 * 1024 * 1024))); rc=$?
assert_eq "a nearly full disk is a warning, not a failure" 0 "$rc"
assert_contains "the warning says how much is left" "$out" "3 GiB free on"
assert_contains "the warning names the cure" "$out" "make dev"

# ── 2b. the workspace-hack is checked against the manifests ─────────────────
assert_contains "the suite runs the hakari check" "$out" "hakari"

# ── 3. the threshold can be moved ────────────────────────────────────────────
out=$(cd "$fake" && FREE_KIB=$((3 * 1024 * 1024)) METAFOLDER_CHECK_MIN_FREE_GIB=2 \
    PATH="$bin:$PATH" bash scripts/dev/check.sh 2>&1)
assert_not "under a lower threshold, no warning" grep -q 'free on' <<<"$out"

# ── 4. a suite that verified nothing is not an "ok" ──────────────────────────
printf '#!/usr/bin/env bash\necho "SKIP: not built"\nexit 77\n' \
    >"$fake/scripts/dev/test-shipped-scripts.sh"
out=$(run $((200 * 1024 * 1024))); rc=$?
assert_eq "a skipped suite does not fail the pass" 0 "$rc"
assert_contains "it is reported as skipped, with its reason" "$out" "skipped"
assert_contains "the reason comes from the suite" "$out" "SKIP: not built"
assert_contains "the summary counts it" "$out" "skipped (scripts"

# Only that suite may say so: 77 from any other check is a plain failure.
printf '#!/usr/bin/env bash\nexit 77\n' >"$fake/scripts/dev/test-tooling.sh"
out=$(run $((200 * 1024 * 1024))); rc=$?
assert_eq "77 from another check fails" 1 "$rc"
printf '#!/usr/bin/env bash\nexit 0\n' >"$fake/scripts/dev/test-tooling.sh"

# A real failure of that suite stays one.
printf '#!/usr/bin/env bash\nexit 1\n' >"$fake/scripts/dev/test-shipped-scripts.sh"
out=$(run $((200 * 1024 * 1024))); rc=$?
assert_eq "a failing suite fails the pass" 1 "$rc"

# ── 5. --help is the whole header, and the header names every check ──────────
help=$(bash "$repo/scripts/dev/check.sh" --help)
assert_contains "--help reaches the end of the header" "$help" "verified nothing"
assert_not "--help stops at the header" grep -q 'set -uo pipefail' <<<"$help"
assert_not "--help prints no comment marker" grep -q '^#' <<<"$help"
for name in $(sed -n 's/^\s*\(run\|skip\) \([a-z]*\) .*/\2/p' "$repo/scripts/dev/check.sh" | sort -u); do
    assert_contains "the header describes $name" "$help" "$name"
done

assert_summary
