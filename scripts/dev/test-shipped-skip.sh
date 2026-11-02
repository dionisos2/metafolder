#!/usr/bin/env bash
# Tests for how the shipped-script suites report having verified nothing. Exit
# status 77 means "skipped": binaries that are not built are a reason not to run,
# a daemon that does not start is a failure, and check.sh must tell them apart.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/dev/lib/assert.sh"

mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/shipped-skip.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# ── the integration suite ────────────────────────────────────────────────────
out=$(METAFOLDER_BIN_DIR="$tmp/nowhere" bash "$repo/scripts/dev/test-scripts-integration.sh" 2>&1); rc=$?
assert_eq "binaries not built: skipped" 77 "$rc"
assert_contains "and it says why" "$out" "SKIP"

# Binaries that exist but cannot serve: the daemon dies on start.
dead="$tmp/dead"
mkdir -p "$dead"
for b in metafolder-daemon mf; do
    printf '#!/usr/bin/env bash\necho "daemon says no" >&2\nexit 1\n' >"$dead/$b"
    chmod +x "$dead/$b"
done
out=$(METAFOLDER_BIN_DIR="$dead" bash "$repo/scripts/dev/test-scripts-integration.sh" 2>&1); rc=$?
assert_eq "a daemon that does not start: failed" 1 "$rc"
assert_contains "and its log is shown" "$out" "daemon says no"

# ── the runner: a skip survives, a failure wins ──────────────────────────────
fake="$tmp/fake"
mkdir -p "$fake"
cp "$repo/scripts/dev/test-shipped-scripts.sh" "$fake/"
sed -n 's/^    \(test-[a-z-]*\.sh\).*/\1/p' "$fake/test-shipped-scripts.sh" >"$tmp/suites"
set_suites() { # <exit status of every suite> [<exit status of the integration suite>]
    local s status
    while read -r s; do
        status=$1
        [ "$s" = test-scripts-integration.sh ] && status=${2:-$1}
        printf '#!/usr/bin/env bash\nexit %s\n' "$status" >"$fake/$s"
    done <"$tmp/suites"
}

set_suites 0
bash "$fake/test-shipped-scripts.sh" >/dev/null 2>&1; rc=$?
assert_eq "all pass: 0" 0 "$rc"

set_suites 0 77
out=$(bash "$fake/test-shipped-scripts.sh" 2>&1); rc=$?
assert_eq "one skipped: 77" 77 "$rc"
assert_contains "and the runner names it" "$out" "SKIP: test-scripts-integration.sh"

set_suites 1 77
bash "$fake/test-shipped-scripts.sh" >/dev/null 2>&1; rc=$?
assert_eq "a failure wins over a skip" 1 "$rc"

assert_summary
