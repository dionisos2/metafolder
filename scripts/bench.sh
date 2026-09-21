#!/usr/bin/env bash
# The timed performance-regression suite (spec-perf).
#
#   scripts/bench.sh                  # the standard suite: generated data, release
#   scripts/bench.sh --quick          # the small size only, for a fast answer
#   scripts/bench.sh --big            # + the large size (minutes to generate)
#   scripts/bench.sh --real           # + the persistent benchmarks/bench_data* repos
#   scripts/bench.sh --filter log.    # only the scenarios whose id starts with log.
#   scripts/bench.sh --no-history     # measure and compare, record nothing
#   scripts/bench.sh --report         # print the recorded history, measure nothing
#   scripts/bench.sh --debug          # measure the debug build (its own history)
#
# What it does: builds the daemon in release, generates (or reuses) the
# synthetic repositories under target/bench-data/, runs each scenario five
# times, compares the median against the median of the last five runs recorded
# for this machine in benchmarks/history/<machine>.jsonl, and appends the new
# measurements there.
#
# Exit status is non-zero when a scenario is more than 30% slower than its
# baseline (--tolerance changes that), so a release check can gate on it. It is
# deliberately NOT part of scripts/check.sh: a machine-dependent measurement
# that fails a build teaches people to ignore failing builds. The check that
# *is* in check.sh is the other half — the cost assertions in
# crates/daemon/tests/perf_cost.rs, which count SQL statements instead of
# milliseconds and so cannot flake.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo" || exit 1

profile=release
args=()
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) args+=("$arg") ;;
    esac
done

# The suite drives a daemon it spawns itself: that binary has to exist, and in
# the same profile as the harness, or the numbers describe the wrong build.
if [ "$profile" = release ]; then
    cargo build --release -p metafolder-daemon -p metafolder-bench || exit 1
    exec ./target/release/metafolder-bench regression "${args[@]}"
else
    cargo build -p metafolder-daemon -p metafolder-bench || exit 1
    exec ./target/debug/metafolder-bench regression "${args[@]}"
fi
