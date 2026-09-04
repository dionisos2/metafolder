#!/usr/bin/env bash
# Tests for scripts/shipped/gui-tag-classify.sh — interactive hierarchical-tag
# classification of ONE metarecord. The scripted `mf` shim
# (scripts/lib/mf-mock.sh) stands in for the daemon + GUI, and its built-in tag
# store makes `mf tag -i U add/deny` observable to the next `field get … tag`,
# so the descend-until-exhausted loop runs for real. We assert the exact
# question ORDER the selector drives, the add/deny calls, the summary, and the
# empty-vocabulary / file-prompt paths. No daemon/GUI.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/shipped/gui-tag-classify.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

# A four-tag vocabulary: two top-level, two children of `music` (non-exclusive).
UNIVERSE=$'music\t0\t0\nadmin\t0\t0\nmusic/jazz\t0\t0\nmusic/rock\t0\t0'

setup_common() {
    mock_respond 'gui repo'         'repo-1'
    mock_respond 'path rec-1'       '/abs/file'
    mock_respond 'tag list'         "$UNIVERSE"
    mock_respond 'gui layout left'  'saved-left'
    mock_respond 'gui layout right' 'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
}

# The ordered list of tags actually asked about, comma-joined.
asked_order() {
    mock_calls_matching "gui message add tag '*' ?*" \
        | sed -n "s/.*add tag '\\([^']*\\)'.*/\\1/p" | paste -sd, -
}

# ── Case 1: a full descent — music(y) admin(n) jazz(y) rock(n) ───────────────
mock_reset
setup_common
mock_input y n y n
out=$(bash "$SCRIPT" rec-1); code=$?
assert "descent: exits 0" [ "$code" -eq 0 ]
assert_eq "descent: question order shallow-first then into accepted branch" \
    "music,admin,music/jazz,music/rock" "$(asked_order)"
assert "descent: music added"   [ "$(mock_count 'tag -i rec-1 add music')" -eq 1 ]
assert "descent: admin denied"  [ "$(mock_count 'tag -i rec-1 deny admin')" -eq 1 ]
assert "descent: jazz added"    [ "$(mock_count 'tag -i rec-1 add music/jazz')" -eq 1 ]
assert "descent: rock denied"   [ "$(mock_count 'tag -i rec-1 deny music/rock')" -eq 1 ]
assert_contains "descent: summary counts" "$out" "2 oui, 2 non"

# ── Case 2: denying `music` prunes its whole subtree (no jazz/rock asked) ─────
mock_reset
setup_common
mock_input n y            # music=no -> subtree gone; admin=yes; then exhausted
bash "$SCRIPT" rec-1 >/dev/null; code=$?
assert "prune: exits 0" [ "$code" -eq 0 ]
assert_eq "prune: only the two top-level tags are asked" "music,admin" "$(asked_order)"
assert "prune: no child tag touched" [ "$(mock_count 'tag -i rec-1 * music/*')" -eq 0 ]

# ── Case 3: Escape stops immediately with a zeroed summary ───────────────────
mock_reset
setup_common
mock_input q
out=$(bash "$SCRIPT" rec-1); code=$?
assert "q: exits 0" [ "$code" -eq 0 ]
assert "q: no tag op" [ "$(mock_count 'tag -i rec-1 *')" -eq 0 ]
assert_contains "q: zeroed summary" "$out" "0 oui, 0 non"

# ── Case 4: an empty tag vocabulary is a hard error ──────────────────────────
mock_reset
mock_respond 'tag list'         ''            # empty vocabulary
mock_respond 'gui repo'         'repo-1'
mock_respond 'path rec-1'       '/abs/file'
mock_respond 'gui layout left'  'saved-left'
mock_respond 'gui layout right' 'saved-right'
mock_respond 'gui workspace new*' 'ws-1'
err=$(bash "$SCRIPT" rec-1 2>&1 >/dev/null); code=$?
assert "no-vocab: non-zero exit" [ "$code" -ne 0 ]
assert_contains "no-vocab: explains the empty vocabulary" "$err" 'no tag entries'

# ── Case 5: no uuid arg — the file is chosen through the GUI completion ───────
mock_reset
setup_common
mock_respond 'metarecord -q mfr_type = "file" get*'  '/some/file'   # completion (drained)
mock_respond 'metarecord -q mfr_path = "/some/file" get' 'rec-1'    # path -> uuid
mock_prompt '/some/file'
mock_input q
out=$(bash "$SCRIPT"); code=$?
assert "prompt: exits 0" [ "$code" -eq 0 ]
assert "prompt: resolves the chosen path to a uuid" \
    [ "$(mock_count 'metarecord -q mfr_path = "/some/file" get')" -eq 1 ]
assert_contains "prompt: classifies the resolved record" "$out" "Classification de rec-1"

# ── Case 6: cancelling the file prompt aborts ────────────────────────────────
mock_reset
setup_common
mock_respond 'metarecord -q mfr_type = "file" get*' '/some/file'
mock_prompt @cancel
err=$(bash "$SCRIPT" 2>&1 >/dev/null); code=$?
assert "prompt-cancel: non-zero exit" [ "$code" -ne 0 ]
assert_contains "prompt-cancel: reports cancelled" "$err" cancelled

assert_summary
