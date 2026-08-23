#!/usr/bin/env bash
# Tests for scripts/shipped/example-gui-sort-folder.sh — the copy-and-adapt
# TEMPLATE that classifies each file in a folder then moves it to a destination
# computed from its metadata. It is not a finished tool, but its routing +
# move logic and its integration with gui-tag-classify.sh are worth pinning.
#
# The scripted `mf` shim (scripts/lib/mf-mock.sh) stands in for the daemon +
# GUI (the nested classify run drives itself from the SAME answer queue). The
# folder and destination are REAL temp directories, so we can assert the file
# actually moved. No daemon/GUI.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/shipped/example-gui-sort-folder.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

work=$(mktemp -d "${TMPDIR:-/tmp}/mf-sort.XXXXXX")
trap 'rm -rf "$work"' EXIT

# Vocabulary uses the French tag paths the template's routing rules expect.
UNIVERSE=$'musique\t0\t0\nmusique/jazz\t0\t0'

setup_common() {
    mock_respond 'gui repo'          'repo-1'
    mock_respond 'tag list'          "$UNIVERSE"
    mock_respond 'path rec-song'     '/abs/song'
    mock_respond 'gui layout left'   'saved-left'
    mock_respond 'gui layout right'  'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
    mock_respond 'track *'           'rec-song'
    # `mf … field get rate` (scalar, not a tag field) returns nothing.
    mock_respond 'metarecord -i rec-song field get rate' ''
}

# ── Case 1: a jazz file gets classified and moved to musique/jazz ────────────
mock_reset
setup_common
sort_dir="$work/trier1"; dest_root="$work/dest1"
mkdir -p "$sort_dir"
printf 'x' >"$sort_dir/song.mp3"
# Answer queue, consumed in order across the nested classify then the extras:
#   classify: musique=y, musique/jazz=y  (then exhausted)
#   extras:   favorite=n, rating=n, comment=n
mock_input y y n n n
out=$(DEST_ROOT="$dest_root" bash "$SCRIPT" "$sort_dir"); code=$?
assert "jazz: exits 0" [ "$code" -eq 0 ]
assert "jazz: tracks the file" [ "$(mock_count 'track *')" -ge 1 ]
assert "jazz: nested classify tagged the record" \
    [ "$(mock_count 'tag -i rec-song add musique/jazz')" -eq 1 ]
assert "jazz: file left the sort dir" [ ! -e "$sort_dir/song.mp3" ]
assert "jazz: file landed under musique/jazz" [ -f "$dest_root/musique/jazz/song.mp3" ]
assert_contains "jazz: reports done" "$out" "done"

# ── Case 2: an unclassified file matches no rule and stays in place ───────────
mock_reset
setup_common
sort_dir="$work/trier2"; dest_root="$work/dest2"
mkdir -p "$sort_dir"
printf 'x' >"$sort_dir/mystery.bin"
#   classify: Escape immediately (no tag); extras: favorite/rating/comment = n
mock_input escape n n n
DEST_ROOT="$dest_root" bash "$SCRIPT" "$sort_dir" >/dev/null; code=$?
assert "norule: exits 0" [ "$code" -eq 0 ]
assert "norule: file stays in the sort dir" [ -f "$sort_dir/mystery.bin" ]
assert "norule: no destination tree created" [ ! -d "$dest_root" ]
assert "norule: nothing tagged" [ "$(mock_count 'tag -i rec-song *')" -eq 0 ]

# ── Case 3: a non-existent sort directory is a hard error ─────────────────────
mock_reset
setup_common
err=$(DEST_ROOT="$work/x" bash "$SCRIPT" "$work/does-not-exist" 2>&1 >/dev/null); code=$?
assert "missing-dir: non-zero exit" [ "$code" -ne 0 ]
assert_contains "missing-dir: explains the error" "$err" 'not a directory'

# ── Case 4: the .metafolder entry is skipped, not tracked/moved ──────────────
mock_reset
setup_common
sort_dir="$work/trier4"; dest_root="$work/dest4"
mkdir -p "$sort_dir/.metafolder"
mock_input escape n n n
DEST_ROOT="$dest_root" bash "$SCRIPT" "$sort_dir" >/dev/null; code=$?
assert "skip-mf: exits 0" [ "$code" -eq 0 ]
assert "skip-mf: .metafolder untouched" [ -d "$sort_dir/.metafolder" ]
assert "skip-mf: .metafolder never tracked" [ "$(mock_count 'track *')" -eq 0 ]

assert_summary
