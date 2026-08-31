#!/usr/bin/env bash
# Tests for scripts/check-deps.sh.
#
# check-deps.sh probes the host for build/runtime dependencies, which is not
# reproducible in a test. The seam MF_CHECK_STUB makes it deterministic: a file
# of "name=0" lines forces those dependencies absent; every unlisted dependency
# is treated as present. So a test forces exactly the absences it wants and
# asserts the report + exit code, independent of what the host actually has.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
script="$repo/scripts/check-deps.sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

pass=0
fail=0

# check <description> <expected_exit> <grep_regex_or_-> — runs the script with
# the current stub, asserts the exit code and (optionally) that stdout matches.
run_case() {
    local desc=$1 want_exit=$2 want_grep=$3 stub=${4:-}
    local out rc
    if [ -n "$stub" ]; then
        out=$(MF_CHECK_STUB="$stub" "$script" 2>&1); rc=$?
    else
        out=$("$script" 2>&1); rc=$?
    fi
    local ok=true
    [ "$rc" -eq "$want_exit" ] || { ok=false; echo "  exit: got $rc want $want_exit"; }
    if [ "$want_grep" != "-" ] && ! grep -qE "$want_grep" <<<"$out"; then
        ok=false; echo "  stdout missing /$want_grep/"; echo "$out" | sed 's/^/    | /'
    fi
    if $ok; then echo "ok   — $desc"; pass=$((pass+1))
    else echo "FAIL — $desc"; fail=$((fail+1)); fi
}

# 1. Everything present (empty stub) → success, exit 0.
: >"$tmp/all-present"
run_case "all deps present ⇒ exit 0" 0 "all (required )?dependencies" "$tmp/all-present"

# 2. A build dependency missing ⇒ failure + Arch package hint.
printf 'cargo=0\n' >"$tmp/no-cargo"
run_case "missing build dep ⇒ exit 1"          1 "rust"        "$tmp/no-cargo"

# 3. The hard runtime dependency (bwrap) missing ⇒ failure.
printf 'bwrap=0\n' >"$tmp/no-bwrap"
run_case "missing bubblewrap ⇒ exit 1"         1 "bubblewrap"  "$tmp/no-bwrap"

# 4. Only an OPTIONAL runtime dependency missing ⇒ still success (exit 0),
#    but the dependency is reported as missing.
printf 'ffmpeg=0\n' >"$tmp/no-ffmpeg"
run_case "missing optional dep ⇒ exit 0"       0 "ffmpeg"      "$tmp/no-ffmpeg"

# 5. The fast-AV1 decoder is optional too, and names its own package: absent,
#    AV1 still plays (libaom), just far too slowly to watch.
printf 'gst-av1=0\n' >"$tmp/no-dav1d"
run_case "missing AV1 fast decoder ⇒ exit 0" 0 "gst-plugin-dav1d" "$tmp/no-dav1d"

# 6. --help works and exits 0 (its own invocation — an explicit flag, no stub).
out=$("$script" --help 2>&1); rc=$?
if [ "$rc" -eq 0 ] && grep -qiE "usage|check-deps" <<<"$out"; then
    echo "ok   — --help exits 0"; pass=$((pass+1))
else
    echo "FAIL — --help exits 0 (rc=$rc)"; fail=$((fail+1))
fi

echo
echo "check-deps tests: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
