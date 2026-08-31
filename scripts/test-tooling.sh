#!/usr/bin/env bash
# Runs the tests for the project's own tooling scripts — the ones that build,
# check and prune the tree, as opposed to the shipped GUI scripts covered by
# scripts/test-shipped-scripts.sh. Each suite is standalone and hermetic (they
# stub their probes rather than touching the real host).
#
#   scripts/test-tooling.sh
#
# Exit status is non-zero if any suite fails.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

suites=(
    test-check-deps.sh      # the dependency table + its exit codes
    test-prune-target.sh    # target/ pruning (never deletes a live artifact)
    test-run-tests.sh       # the test-runner wrapper's totals/reporting
)

if [ -t 1 ]; then
    red=$'\e[31m'; green=$'\e[32m'; bold=$'\e[1m'; off=$'\e[0m'
else
    red=''; green=''; bold=''; off=''
fi

failed=()
for s in "${suites[@]}"; do
    printf '%s══ %s%s\n' "$bold" "$s" "$off"
    bash "$HERE/$s" || failed+=("$s")
    echo
done

if [ ${#failed[@]} -gt 0 ]; then
    printf '%s%s%d suite(s) failed: %s%s\n' "$bold" "$red" "${#failed[@]}" "${failed[*]}" "$off"
    exit 1
fi
printf '%s%sall tooling suites passed%s\n' "$bold" "$green" "$off"
