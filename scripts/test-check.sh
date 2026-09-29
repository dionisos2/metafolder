#!/usr/bin/env bash
# Tests for scripts/check.sh — the disk-space warning only.
#
# check.sh never prunes target/ (pruning after clippy and the tests, without
# the plain build and the sync-config core, would evict those universes every
# run: see complete-build.sh), so a target/ that grows until a test dies on
# "No space left on device" has to be announced instead. The checks
# themselves run against stubs: what is asserted is the warning.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/lib/assert.sh"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/check.XXXXXX" 2>/dev/null) \
    || { mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
         tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/check.XXXXXX"); }
trap 'rm -rf "$tmp"' EXIT

fake="$tmp/repo"
bin="$tmp/bin"
mkdir -p "$fake/scripts" "$fake/target" "$bin"
cp "$repo/scripts/check.sh" "$fake/scripts/"

# The repo-local suites check.sh runs: succeed, silently.
for s in run-tests.sh test-shipped-scripts.sh test-tooling.sh; do
    printf '#!/usr/bin/env bash\nexit 0\n' >"$fake/scripts/$s"
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
    (cd "$fake" && FREE_KIB=$1 PATH="$bin:$PATH" bash scripts/check.sh 2>&1)
}

# ── 1. plenty of room: no warning ────────────────────────────────────────────
out=$(run $((200 * 1024 * 1024))); rc=$?
assert_eq "exits 0 with room to spare" 0 "$rc"
assert_not "no warning with room to spare" grep -q 'free on' <<<"$out"

# ── 2. nearly full: a warning naming the cure, and still a pass ──────────────
out=$(run $((3 * 1024 * 1024))); rc=$?
assert_eq "a nearly full disk is a warning, not a failure" 0 "$rc"
assert_contains "the warning says how much is left" "$out" "3 GiB free on"
assert_contains "the warning names the cure" "$out" "scripts/prune-target.sh"

# ── 3. the threshold can be moved ────────────────────────────────────────────
out=$(cd "$fake" && FREE_KIB=$((3 * 1024 * 1024)) METAFOLDER_CHECK_MIN_FREE_GIB=2 \
    PATH="$bin:$PATH" bash scripts/check.sh 2>&1)
assert_not "under a lower threshold, no warning" grep -q 'free on' <<<"$out"

assert_summary
