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

# Common GUI-plumbing responses shared by every case. The optional argument is
# what `mf gui query` answers — the table is first-match-wins, so it has to be
# set here and not overridden later by a second row.
setup_gui() { # [<gui query response>]
    mock_respond 'gui repo'          'repo-1'
    mock_respond 'tag list'          $'music\t0\t0\nmusic/jazz\t0\t0'
    mock_respond 'gui layout left'   'saved-left'
    mock_respond 'gui layout right'  'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
    # Default: the GUI is showing everything — the empty query, a real answer
    # (every metarecord) and not "nothing published".
    mock_respond 'gui query'         "${1-}"
    # `mf path --relative U` must be tried before the bare `mf path U`.
    mock_respond 'path --relative *' 'rel/path'
    mock_respond 'path *'            '/abs/path'
    # The relative paths come from one batched read, not from `mf path` per
    # entry; the rows are uuid<TAB>path.
    mock_respond 'metarecord -q * get --resolve-tree mfr_path --tsv' \
        $'u1\t/f1\nu2\t/f2\nu3\t/f3\nu4\t/f4\nu5\t/f5'
}

# ── Case 1: a y / n / s / Escape walk over five files ────────────────────────
mock_reset
setup_gui
mock_prompt 'music/jazz'                     # the tag being applied
mock_respond 'metarecord -q * get --limit*' $'u1\nu2\nu3\nu4\nu5'
mock_input y n s q                       # f1=yes f2=no f3=skip f4=STOP
out=$(bash "$SCRIPT"); code=$?

assert "walk: exits 0" [ "$code" -eq 0 ]
assert "walk: u1 tagged (add)"  [ "$(mock_count 'tag -i u1 add music/jazz')" -eq 1 ]
assert "walk: u2 denied"        [ "$(mock_count 'tag -i u2 deny music/jazz')" -eq 1 ]
assert "walk: u3 skipped (no tag op)"          [ "$(mock_count 'tag -i u3 *')" -eq 0 ]
assert "walk: u4 not tagged (Escape)"          [ "$(mock_count 'tag -i u4 *')" -eq 0 ]
assert "walk: u5 untouched after Escape stops" [ "$(mock_count 'tag -i u5 *')" -eq 0 ]
assert_contains "walk: summary counts correct" "$out" "1 yes, 1 no, 1 skipped"

# The predicate must exclude files already carrying an opinion and target files.
pred=$(mock_calls_matching 'metarecord -q * get --limit*')
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
mock_respond 'metarecord -q * get --limit*' ''         # empty universe
out=$(bash "$SCRIPT"); code=$?
assert "empty: exits 0" [ "$code" -eq 0 ]
assert_contains "empty: zeroed summary" "$out" "0 yes, 0 no, 0 skipped"
assert "empty: no tag op" [ "$(mock_count 'tag -i *')" -eq 0 ]

# ── Case: the arrow keys answer, and the summary survives the teardown ───────
# The summary used to be posted while the scratch workspace was still on screen,
# so the session teardown removed it along with the workspace. It must land on
# the workspace the script was launched from, AFTER the scratch one is gone.
mock_reset
setup_gui
mock_prompt 'music/jazz'
mock_respond 'metarecord -q * get --limit*' $'u1\nu2\nu3'
mock_input right left down                    # → yes, ← no, ↓ skip
out=$(bash "$SCRIPT"); code=$?
assert "arrows: exits 0" [ "$code" -eq 0 ]
assert "arrows: right adds" [ "$(mock_count 'tag -i u1 add music/jazz')" -eq 1 ]
assert "arrows: left denies" [ "$(mock_count 'tag -i u2 deny music/jazz')" -eq 1 ]
assert "arrows: down skips" [ "$(mock_count 'tag -i u3 *')" -eq 0 ]
assert_contains "arrows: summary counts them" "$out" "1 yes, 1 no, 1 skipped"
assert "summary: posted to the launching workspace" \
    [ "$(mock_count 'gui message *--workspace saved-left*')" -ge 1 ]
# Order matters: the scratch workspace is removed first, so the message cannot
# land on a workspace that is about to disappear.
rm_line=$(mf_log | grep -n '^gui workspace rm' | head -n1 | cut -d: -f1)
msg_line=$(mf_log | grep -n '^gui message .*--workspace saved-left' | head -n1 | cut -d: -f1)
assert "summary: posted after the scratch workspace is removed" \
    [ "$msg_line" -gt "$rm_line" ]

# ── The query is the scope (spec-gui "A query is the scope") ────────────────

# ── Case 5: what the GUI shows narrows the walk ─────────────────────────────
mock_reset
G='mfr_path ->* "/music"'
setup_gui "$G"
mock_prompt 'music/jazz'
mock_respond 'metarecord -q * get --limit*' 'u1'
mock_input y
out=$(bash "$SCRIPT"); code=$?
assert "scope: exits 0" [ "$code" -eq 0 ]
pred=$(mock_calls_matching 'metarecord -q * get --limit*')
assert_contains "scope: the predicate is narrowed to what the GUI shows" "$pred" "($G) AND"
assert "scope: no folder prompt when a query is published" \
    [ "$(mock_count 'gui prompt Folder*')" -eq 0 ]

# ── Case 6: an explicit query argument wins, and skips the GUI ──────────────
mock_reset
setup_gui
Q='rating > 3'
mock_respond 'metarecord -q * get --limit*' 'u1'
mock_input y
bash "$SCRIPT" music/jazz "$Q" >/dev/null; code=$?
assert "arg: exits 0" [ "$code" -eq 0 ]
pred=$(mock_calls_matching 'metarecord -q * get --limit*')
assert_contains "arg: the predicate uses the argument" "$pred" "($Q) AND"
assert "arg: the tag is not prompted either" [ "$(mock_count 'gui prompt Tag*')" -eq 0 ]

# ── Case 7: nothing published falls back to the folder completion ───────────
mock_reset
setup_gui @exit:1
mock_respond 'metarecord -q mfr_type = "dir" get*' '/music'
mock_prompt 'music/jazz' '/music'      # the tag, then the folder
mock_respond 'metarecord -q * get --limit*' 'u1'
mock_input y
bash "$SCRIPT" >/dev/null; code=$?
assert "fallback: exits 0" [ "$code" -eq 0 ]
pred=$(mock_calls_matching 'metarecord -q * get --limit*')
assert_contains "fallback: the folder becomes an inclusive-subtree query" "$pred" \
    '(mfr_path =>* "/music") AND'

# ── Case 8: an empty scope is not wrapped ──────────────────────────────────
mock_reset
setup_gui
mock_prompt 'music'
mock_respond 'metarecord -q * get --limit*' ''
bash "$SCRIPT" >/dev/null; code=$?
assert "all: exits 0" [ "$code" -eq 0 ]
assert "all: no empty parentheses in the predicate" [ "$(mock_count 'metarecord -q () AND*')" -eq 0 ]

assert_summary
