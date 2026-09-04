#!/usr/bin/env bash
# Tests for scripts/shipped/gui-unwatch-folder.sh — stop watching a folder and
# delete every metarecord inside it. A scripted `mf` shim (scripts/lib/mf-mock.sh)
# stands in for the daemon + GUI: we drive the folder argument/prompt and the
# confirmation key, then assert the exact `mf` commands issued (the mf_watch
# write, the subtree delete and their ORDER), the abort paths, and the tree-path
# form used for the queries. No daemon/GUI.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/shipped/gui-unwatch-folder.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

export MF_UNWATCH_SETTLE=0 # no watcher-quiesce sleep in the tests

# A real directory to pass as the filesystem-path argument.
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/mf-unwatch.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/top"

# Common plumbing: the GUI repo binding plus the folder resolution for
# "$WORKDIR/top" → dir-top → tree path /top.
setup_top() {
    mock_respond 'gui repo'                 'repo-1'
    mock_respond "track $WORKDIR/top"       'dir-top'
    mock_respond 'path --relative dir-top'  '/top'
}

# ── Case 1: the happy path — confirm, unwatch, delete the subtree ────────────
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path ->* "/top" get'          $'u1\nu2\nu3'
mock_respond 'metarecord -q mfr_path ->* "/top" delete*'      '3'
mock_input y
out=$(bash "$SCRIPT" "$WORKDIR/top"); code=$?

assert "happy: exits 0" [ "$code" -eq 0 ]
assert "happy: mf_watch set to false on the folder itself" \
    [ "$(mock_count 'metarecord -i dir-top field set mf_watch:bool=false')" -eq 1 ]
assert "happy: the subtree is deleted without a second prompt" \
    [ "$(mock_count 'metarecord -q mfr_path ->* "/top" delete --force')" -eq 1 ]
assert "happy: the folder metarecord itself is not deleted" \
    [ "$(mock_count 'metarecord -i dir-top delete*')" -eq 0 ]
assert_contains "happy: reports the count" "$out" '3'

# mf_watch must land BEFORE the delete: unwatching first is what keeps the
# watcher from re-creating the metarecords we are about to remove.
order=$(mock_calls_matching 'metarecord *' | grep -n 'mf_watch:bool=false\|delete --force')
assert_contains "happy: mf_watch is written before the delete" \
    "$(printf '%s' "$order" | head -1)" 'mf_watch:bool=false'

# ── Case 2: the confirmation is declined — nothing is touched ────────────────
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path ->* "/top" get' $'u1\nu2'
mock_input n
out=$(bash "$SCRIPT" "$WORKDIR/top"); code=$?
assert "decline: exits 0" [ "$code" -eq 0 ]
assert "decline: no mf_watch write" [ "$(mock_count '*mf_watch*')" -eq 0 ]
assert "decline: no delete"         [ "$(mock_count '*delete*')" -eq 0 ]
assert_contains "decline: says so" "$out" 'cancelled'

# ── Case 3: Escape declines just like "n" ────────────────────────────────────
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path ->* "/top" get' 'u1'
mock_input q
bash "$SCRIPT" "$WORKDIR/top" >/dev/null; code=$?
assert "q: exits 0"     [ "$code" -eq 0 ]
assert "q: no delete"   [ "$(mock_count '*delete*')" -eq 0 ]
assert "q: no mf_watch" [ "$(mock_count '*mf_watch*')" -eq 0 ]

# ── Case 4: an empty folder still gets unwatched, with no confirmation ───────
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path ->* "/top" get' ''
out=$(bash "$SCRIPT" "$WORKDIR/top"); code=$?
assert "empty: exits 0" [ "$code" -eq 0 ]
assert "empty: mf_watch still set to false" \
    [ "$(mock_count 'metarecord -i dir-top field set mf_watch:bool=false')" -eq 1 ]
assert "empty: no delete issued" [ "$(mock_count '*delete*')" -eq 0 ]
assert "empty: no key was asked for" [ "$(mock_count 'gui input*')" -eq 0 ]

# ── Case 5: the folder is prompted when no argument is given ─────────────────
mock_reset
mock_respond 'gui repo'                              'repo-1'
mock_respond 'metarecord -q mfr_type = "dir" get*'   $'/\n/top'   # completion source
mock_respond 'metarecord -q mfr_path = "/top" get'   'dir-top'
mock_respond 'path --relative dir-top'               '/top'
mock_respond 'metarecord -q mfr_path ->* "/top" get' 'u1'
mock_respond 'metarecord -q mfr_path ->* "/top" delete*' '1'
mock_prompt '/top'
mock_input y
bash "$SCRIPT" >/dev/null; code=$?
assert "prompt: exits 0" [ "$code" -eq 0 ]
assert "prompt: resolves the prompted path to its uuid" \
    [ "$(mock_count 'metarecord -q mfr_path = "/top" get')" -eq 1 ]
assert "prompt: never tracks a path it did not get from the filesystem" \
    [ "$(mock_count 'track *')" -eq 0 ]
assert "prompt: deletes the prompted subtree" \
    [ "$(mock_count 'metarecord -q mfr_path ->* "/top" delete --force')" -eq 1 ]

# ── Case 6: a cancelled prompt aborts before anything is written ─────────────
mock_reset
mock_respond 'gui repo'                            'repo-1'
mock_respond 'metarecord -q mfr_type = "dir" get*' '/top'
mock_prompt @cancel
err=$(bash "$SCRIPT" 2>&1 >/dev/null); code=$?
assert "cancel: non-zero exit" [ "$code" -ne 0 ]
assert_contains "cancel: reports 'cancelled'" "$err" cancelled
assert "cancel: nothing written" [ "$(mock_count '*mf_watch*')" -eq 0 ]

# ── Case 7: a path that is not a directory is rejected ───────────────────────
mock_reset
setup_top
printf 'x' >"$WORKDIR/file.txt"
err=$(bash "$SCRIPT" "$WORKDIR/file.txt" 2>&1 >/dev/null); code=$?
assert "notdir: non-zero exit" [ "$code" -ne 0 ]
assert_contains "notdir: explains why" "$err" 'not a directory'
assert "notdir: nothing written" [ "$(mock_count '*mf_watch*')" -eq 0 ]

# ── Case 8: too many positional arguments is a usage error ───────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" a b 2>&1 >/dev/null); code=$?
assert "usage: non-zero exit on 2 args" [ "$code" -ne 0 ]
assert_contains "usage: prints a usage line" "$err" usage

# ── Case 9: the ROOT folder uses the empty-string tree-query form ────────────
# `mf path --relative` prints the repository root as "/", but the mfr_path tree
# queries want it as "" — `->* "/"` matches nothing on the real daemon.
mock_reset
mock_respond 'gui repo'                            'repo-1'
mock_respond 'metarecord -q mfr_type = "dir" get*' '/'
mock_respond 'metarecord -q mfr_path = "" get'     'dir-root'
mock_respond 'path --relative dir-root'            '/'
mock_respond 'metarecord -q mfr_path ->* "" get'   $'u1\nu2'
mock_respond 'metarecord -q mfr_path ->* "" delete*' '2'
mock_prompt '/'
mock_input y
bash "$SCRIPT" >/dev/null; code=$?
assert "root: exits 0" [ "$code" -eq 0 ]
assert "root: resolves the root via the empty-string form" \
    [ "$(mock_count 'metarecord -q mfr_path = "" get')" -eq 1 ]
assert "root: deletes with the empty-string form" \
    [ "$(mock_count 'metarecord -q mfr_path ->* "" delete --force')" -eq 1 ]
assert "root: never uses the broken \"/\" tree-query form" \
    [ "$(mock_count 'metarecord -q mfr_path ->* "/"*')" -eq 0 ]

assert_summary
