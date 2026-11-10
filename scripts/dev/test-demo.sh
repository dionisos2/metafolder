#!/usr/bin/env bash
# shellcheck source-path=SCRIPTDIR
# Tests of the demonstration repository (doc "The demonstration repository"):
# scripts/dev/demo/lib.sh builds it over the demonstration home against a
# throwaway daemon, and every metadata step must have found what it names — a
# home reorganised under the script fails here, not as an empty screenshot.
#
#   scripts/dev/test-demo.sh [SOURCE]     (default: /home/metafolder_user)
#
# Not hermetic: it needs the built daemon and mf, and the demonstration home,
# without which it is a loud SKIP. Not part of check.sh for that reason.
set -uo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)" || exit 2

SOURCE=${1:-/home/metafolder_user}

# shellcheck source=lib/daemon-fixture.sh
. scripts/dev/lib/daemon-fixture.sh
# shellcheck source=lib/assert.sh
. scripts/dev/lib/assert.sh
# shellcheck source=demo/lib.sh
. scripts/dev/demo/lib.sh

if ! df_available; then
    echo "SKIP demo: build first (cargo build -p metafolder-daemon -p metafolder-cli)"
    exit 0
fi
if [ ! -d "$SOURCE" ]; then
    echo "SKIP demo: no demonstration home at $SOURCE"
    exit 0
fi

df_start || { echo "error: the daemon did not start" >&2; exit 2; }

assert "demo: the repository is built" demo_build_repo "$SOURCE" "$_DF_TMP/demo.metafolder"
export METAFOLDER_REPO_NAME=$DEMO_NAME

count() { df_mf metarecord -q "$1" -s get --count 2>/dev/null; }

assert_not "demo: nothing is written into the source" test -e "$SOURCE/.metafolder"
assert_eq "demo: the repository is named after the persona" camille "$DEMO_NAME"

tags=$(df_mf tag list | cut -f1)
for tag in place/europe/iceland place/asia/japan/kyoto art/painting/impressionism \
    nature/animal/bird music/romantic film/animation document/invoice; do
    assert_contains "demo: the tag $tag exists" "$tags" "$tag"
done

assert_eq "demo: every photo of Iceland is tagged" 8 "$(count '#=place/europe/iceland')"
assert_eq "demo: Kyoto is under Japan" 6 "$(count '##=place/asia/japan/kyoto')"
assert "demo: some files are rated" test "$(count 'rating>=4')" -gt 3
assert "demo: some files carry a comment" test "$(count 'comment?')" -gt 0
assert "demo: a wallpaper is denied an animal" test "$(count 'negative_tag #=nature/animal')" -gt 0

# `mf log` lists the innermost labels, the commands; the step is the label they
# are nested in, which `mf log show` names.
assert_contains "demo: the steps are labelled for the log" \
    "$(df_mf log show HEAD 2>/dev/null)" "Comment on a few files"
assert_not "demo: no label shows the throwaway daemon's port" \
    grep -q -- "-p $DF_PORT" <<<"$(df_mf log list --all 2>/dev/null)"

assert_summary
