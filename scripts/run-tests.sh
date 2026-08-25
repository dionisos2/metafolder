#!/usr/bin/env bash
# Runs the workspace test suite and prints the one thing cargo never does:
# a GLOBAL report over every test binary.
#
#   scripts/run-tests.sh                 # cargo test --workspace, then a total
#   scripts/run-tests.sh -p metafolder-cli   # extra args go to cargo test
#   cargo test --workspace 2>&1 | scripts/run-tests.sh --stdin
#
# cargo prints one "test result:" line per test binary (per crate's unit tests,
# per integration test file, per doc-test target) and no total, so a run of
# forty suites ends with the count of the last one — which says nothing about
# the run. This wrapper streams cargo's output through unchanged and adds, at
# the end, the totals across all of them plus the names of the tests that
# failed and the suite each belongs to.
#
# --stdin aggregates an existing run's output instead of starting one (the seam
# the tests drive; exit status is then 1 iff a test failed or nothing ran).
# Tests: scripts/test-run-tests.sh

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo" || exit 1

stdin=false
case "${1:-}" in
    --stdin) stdin=true; shift ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \?//'; exit 0 ;;
esac

# Terminal colours, but only when writing to one.
if [ -t 1 ]; then
    red=$'\e[31m'; green=$'\e[32m'; yellow=$'\e[33m'; bold=$'\e[1m'; off=$'\e[0m'
else
    red=''; green=''; yellow=''; bold=''; off=''
fi

# The aggregator: passes every line through, remembers the suite it is inside,
# and sums the "test result:" lines. Exits 1 if anything failed or if no suite
# reported at all (a compile error: cargo's own status carries that in the
# non---stdin path, but a total of "0 passed" must never read as success).
aggregate() {
    awk -v red="$red" -v green="$green" -v yellow="$yellow" -v bold="$bold" -v off="$off" '
    # num("… 295 passed; 2 failed …", "passed") -> 295
    function num(line, kw,   s) {
        if (match(line, "[0-9]+ " kw)) {
            s = substr(line, RSTART, RLENGTH); sub(/ .*/, "", s); return s + 0
        }
        return 0
    }
    { print; fflush() }

    # Suite banners: "Running unittests src/lib.rs (target/debug/deps/x-<hash>)",
    # "Running tests/storage.rs (…)", "Doc-tests <crate>". The binary name in
    # the parens is what tells two crates unit tests apart; the hash is noise.
    /^[[:space:]]+Running / {
        if (match($0, /\([^)]*\)/)) {
            suite = substr($0, RSTART + 1, RLENGTH - 2)
            sub(/.*\//, "", suite)
            sub(/-[0-9a-f]+$/, "", suite)
        } else suite = "?"
        next
    }
    /^[[:space:]]+Doc-tests / { suite = "doc-tests " $2; next }

    # Each failing test announces itself as it finishes; collecting them here
    # (rather than from the trailing "failures:" block) keeps the suite it
    # belongs to attached, which is the part cargo drops.
    /^test .* \.\.\. FAILED/ {
        name = $0
        sub(/^test /, "", name); sub(/ \.\.\. FAILED.*$/, "", name)
        if (!(suite in failed_names)) order[++nsuites_failed] = suite
        failed_names[suite] = (suite in failed_names) ? failed_names[suite] ", " name : name
        next
    }

    /^test result:/ {
        suites++
        passed   += num($0, "passed")
        failed   += num($0, "failed")
        ignored  += num($0, "ignored")
        filtered += num($0, "filtered out")
        if (match($0, /finished in [0-9.]+s/)) {
            s = substr($0, RSTART + 12, RLENGTH - 13); secs += s + 0
        }
    }

    END {
        printf "\n%s── test summary ──%s\n", bold, off
        if (suites == 0) {
            printf "  %sno test ran%s — no suite reported a result (compile error?)\n", yellow, off
            exit 1
        }
        verdict = failed > 0 ? red bold "FAILED" off : green bold "ok" off
        printf "  %s — %d passed, %d failed, %d ignored, %d filtered out" \
               "  (%d suites, %.1fs)\n", verdict, passed, failed, ignored, filtered, suites, secs
        if (failed > 0) {
            printf "\n  %sfailing tests:%s\n", bold, off
            for (i = 1; i <= nsuites_failed; i++)
                printf "    %-24s %s\n", order[i], failed_names[order[i]]
        }
        exit failed > 0 ? 1 : 0
    }'
}

if $stdin; then
    aggregate
    exit $?
fi

# cargo writes its progress to stderr and the harness to stdout: both are the
# report, so merge them before aggregating. cargo's own status (a compile
# error) wins over the aggregate's.
"${CARGO:-cargo}" test --workspace "$@" 2>&1 | aggregate
cargo_rc=${PIPESTATUS[0]}
agg_rc=$?
[ "$cargo_rc" -ne 0 ] && exit "$cargo_rc"
exit "$agg_rc"
