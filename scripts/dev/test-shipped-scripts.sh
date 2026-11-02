#!/usr/bin/env bash
# Runs every test for the launchable shipped scripts (the ones reachable from
# the GUI's `script:run`, i.e. carrying a `# Summary:` header). Each suite is a
# standalone bash script that mocks the `mf` CLI + GUI via scripts/dev/lib/mf-mock.sh
# (except test-gui-tag-next.sh, which needs no mock — the selector is pure).
#
#   scripts/dev/test-shipped-scripts.sh
#
# Exit status: 1 if any suite fails, otherwise 77 if a suite was skipped (it
# verified nothing, which check.sh reports), otherwise 0.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

suites=(
    test-gui-tag-next.sh          # pure selector (already existed)
    test-gui-tag-pair.sh
    test-gui-tag-folder.sh
    test-gui-tag-classify.sh
    test-template-sort-folder.sh
    test-gui-unwatch-folder.sh
    test-scripts-integration.sh  # real daemon; SKIPs if binaries not built
)

if [ -t 1 ]; then
    red=$'\e[31m'; green=$'\e[32m'; yellow=$'\e[33m'; bold=$'\e[1m'; off=$'\e[0m'
else
    red=''; green=''; yellow=''; bold=''; off=''
fi

failed=()
skipped=()
for s in "${suites[@]}"; do
    printf '%s══ %s%s\n' "$bold" "$s" "$off"
    rc=0
    bash "$HERE/$s" || rc=$?
    case "$rc" in
        0) ;;
        77) skipped+=("$s") ;;
        *) failed+=("$s") ;;
    esac
    echo
done

if [ ${#failed[@]} -gt 0 ]; then
    printf '%s%d suite(s) failed: %s%s\n' "$red$bold" "${#failed[@]}" "${failed[*]}" "$off"
    exit 1
fi
if [ ${#skipped[@]} -gt 0 ]; then
    printf '%sSKIP: %s%s\n' "$yellow$bold" "${skipped[*]}" "$off"
    exit 77
fi
printf '%sall shipped-script suites passed%s\n' "$green$bold" "$off"
