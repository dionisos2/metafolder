#!/usr/bin/env bash
# Runs every test for the launchable shipped scripts (the ones reachable from
# the GUI's `script:run`, i.e. carrying a `# Summary:` header). Each suite is a
# standalone bash script that mocks the `mf` CLI + GUI via scripts/lib/mf-mock.sh
# (except test-gui-tag-next.sh, which needs no mock — the selector is pure).
#
#   scripts/test-shipped-scripts.sh
#
# Exit status is non-zero if any suite fails.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

suites=(
    test-gui-tag-next.sh          # pure selector (already existed)
    test-gui-tag-pair.sh
    test-gui-tag-folder.sh
    test-gui-tag-classify.sh
    test-example-gui-sort-folder.sh
)

if [ -t 1 ]; then
    red=$'\e[31m'; green=$'\e[32m'; bold=$'\e[1m'; off=$'\e[0m'
else
    red=''; green=''; bold=''; off=''
fi

failed=()
for s in "${suites[@]}"; do
    printf '%s══ %s%s\n' "$bold" "$s" "$off"
    if bash "$HERE/$s"; then
        :
    else
        failed+=("$s")
    fi
    echo
done

if [ ${#failed[@]} -gt 0 ]; then
    printf '%s%d suite(s) failed: %s%s\n' "$red$bold" "${#failed[@]}" "${failed[*]}" "$off"
    exit 1
fi
printf '%sall shipped-script suites passed%s\n' "$green$bold" "$off"
