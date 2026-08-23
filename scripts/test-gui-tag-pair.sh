#!/usr/bin/env bash
# Tests for scripts/shipped/gui-tag-pair.sh — interactive y/n tagging of ONE
# tag across a repository's files. A scripted `mf` shim (scripts/lib/mf-mock.sh)
# stands in for the daemon + GUI: we drive the yes/no/skip/Escape answers and
# assert the exact `mf tag …` commands the script issues, its stop-on-Escape
# behaviour, its summary counts, and its argument validation. No daemon/GUI.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/shipped/gui-tag-pair.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

# Common GUI-plumbing responses shared by every case.
setup_gui() {
    mock_respond 'gui repo'          'repo-1'
    mock_respond 'tag list'          $'music\t0\t0\nmusic/jazz\t0\t0'
    mock_respond 'gui layout left'   'saved-left'
    mock_respond 'gui layout right'  'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
    # `mf path --relative U` must be tried before the bare `mf path U`.
    mock_respond 'path --relative *' 'rel/path'
    mock_respond 'path *'            '/abs/path'
}

# ── Case 1: a y / n / s / Escape walk over five files ────────────────────────
mock_reset
setup_gui
mock_prompt 'music/jazz'                     # the tag being applied
mock_respond 'metarecord -q * get' $'u1\nu2\nu3\nu4\nu5'
mock_input y n s escape                       # f1=yes f2=no f3=skip f4=STOP
out=$(bash "$SCRIPT"); code=$?

assert "walk: exits 0" [ "$code" -eq 0 ]
assert "walk: u1 tagged (add)"  [ "$(mock_count 'tag -i u1 add music/jazz')" -eq 1 ]
assert "walk: u2 denied"        [ "$(mock_count 'tag -i u2 deny music/jazz')" -eq 1 ]
assert "walk: u3 skipped (no tag op)"          [ "$(mock_count 'tag -i u3 *')" -eq 0 ]
assert "walk: u4 not tagged (Escape)"          [ "$(mock_count 'tag -i u4 *')" -eq 0 ]
assert "walk: u5 untouched after Escape stops" [ "$(mock_count 'tag -i u5 *')" -eq 0 ]
assert_contains "walk: summary counts correct" "$out" "1 yes, 1 no, 1 skipped"

# The predicate must exclude files already carrying an opinion and target files.
pred=$(mock_calls_matching 'metarecord -q * get')
assert_contains "walk: predicate filters to files" "$pred" 'mfr_type = "file"'
assert_contains "walk: predicate excludes files with an opinion" "$pred" 'NOT ('
assert_contains "walk: predicate uses exact tag-path node" "$pred" 'path = "music/jazz"'

# ── Case 2: the tag prompt is cancelled (Escape) ─────────────────────────────
mock_reset
setup_gui
mock_prompt @cancel
err=$(bash "$SCRIPT" 2>&1 >/dev/null); code=$?
assert "cancel: non-zero exit" [ "$code" -ne 0 ]
assert_contains "cancel: reports 'cancelled'" "$err" cancelled
assert "cancel: no tag op issued" [ "$(mock_count 'tag -i *')" -eq 0 ]
assert "cancel: no workspace opened before the tag is known" \
    [ "$(mock_count 'gui workspace new*')" -eq 0 ]

# ── Case 3: a tag name containing a double quote is rejected ─────────────────
mock_reset
setup_gui
mock_prompt 'bad"tag'
err=$(bash "$SCRIPT" 2>&1 >/dev/null); code=$?
assert "quote: non-zero exit" [ "$code" -ne 0 ]
assert_contains "quote: explains the quote rule" "$err" 'double quote'

# ── Case 4: no files match — clean run, zeroed summary ───────────────────────
mock_reset
setup_gui
mock_prompt 'music'
mock_respond 'metarecord -q * get' ''         # empty universe
out=$(bash "$SCRIPT"); code=$?
assert "empty: exits 0" [ "$code" -eq 0 ]
assert_contains "empty: zeroed summary" "$out" "0 yes, 0 no, 0 skipped"
assert "empty: no tag op" [ "$(mock_count 'tag -i *')" -eq 0 ]

assert_summary
