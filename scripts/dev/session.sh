#!/usr/bin/env bash
# The session scenario (doc "The session scenario"): a real GUI on a large
# repository, driven through a day's worth of use, every step timed against
# its own budget, with the slow log at its floor saying where the time and the
# keys went. A report, not a gate.
#
#   scripts/dev/session.sh --unsandboxed            # in a container forbidding WebKit's sandbox
#   scripts/dev/session.sh --data benchmarks/bench_data   # the small folder, for a fast run
#   scripts/dev/session.sh --keep                   # keep target/bench-data/session (logs, report)
#   scripts/dev/session.sh --calibrate              # measure this machine's neutral load (30 s)
#   scripts/dev/session.sh --record                 # also append the run to benchmarks/session/
#   scripts/dev/session.sh --record --force         # record although the machine is busier
#
# Needs a display and the frontend built (npm --prefix crates/gui/frontend run
# build). A recorded run is taken on a machine left neutral: ask its user
# first, then --calibrate once, and --record compares each run with it.
set -uo pipefail
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo" || exit 1
case "${1:-}" in
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \?//'; exit 0 ;;
esac
cargo build --release -p metafolder-daemon -p metafolder-gui -p metafolder-bench || exit 1
exec ./target/release/metafolder-bench session "$@"
