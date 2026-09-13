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
# Name-sorted, because that is the only order `mf tag list` ever prints
# (`commands.rs`, `vocab.names.sort()`). Fed unsorted, the suite was exercising
# the depth tie-break on an input the CLI cannot produce.
UNIVERSE=$'admin\t0\t0\nmusic\t0\t0\nmusic/jazz\t0\t0\nmusic/rock\t0\t0'

# The optional argument is what `mf gui query` answers (the table is
# first-match-wins, so it cannot be overridden by a later row).
setup_common() { # [<gui query response>]
    mock_respond 'gui repo'         'repo-1'
    mock_respond 'gui query'        "${1-@exit:1}"
    mock_respond 'path rec-1'       '/abs/file'
    mock_respond 'path --relative rec-1' '/file'
    mock_respond 'tag list'         "$UNIVERSE"
    mock_respond 'gui layout left'  'saved-left'
    mock_respond 'gui layout right' 'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
    # The scope resolves to the one record the old single-uuid form named: a
    # bare uuid is a valid query (spec-query, the UUID-atom bullet), so the old
    # invocation keeps working.
    mock_respond 'metarecord -q rec-1 get --sort mfr_path*' 'rec-1'
}

# The ordered list of tags actually asked about, comma-joined.
asked_order() {
    # The question rides on the wait's own --prompt (spec-gui "Status bar"), so
    # it is the `gui input` call that carries it.
    mock_calls_matching "gui input --prompt add tag '*' ?*" \
        | sed -n "s/.*add tag '\\([^']*\\)'.*/\\1/p" | paste -sd, -
}

# ── Case 1: a full descent — admin(n) music(y) jazz(y) rock(n) ───────────────
# The two top-level tags come in the vocabulary's own (name-sorted) order, so
# `admin` is asked first; accepting `music` then opens its two children.
mock_reset
setup_common
mock_input n y y n
out=$(bash "$SCRIPT" rec-1); code=$?
assert "descent: exits 0" [ "$code" -eq 0 ]
assert_eq "descent: question order shallow-first then into accepted branch" \
    "admin,music,music/jazz,music/rock" "$(asked_order)"
assert "descent: music added"   [ "$(mock_count 'tag -i rec-1 add music')" -eq 1 ]
assert "descent: admin denied"  [ "$(mock_count 'tag -i rec-1 deny admin')" -eq 1 ]
assert "descent: jazz added"    [ "$(mock_count 'tag -i rec-1 add music/jazz')" -eq 1 ]
assert "descent: rock denied"   [ "$(mock_count 'tag -i rec-1 deny music/rock')" -eq 1 ]
assert_contains "descent: summary counts" "$out" "2 oui, 2 non"

# ── Case 2: denying `music` prunes its whole subtree (no jazz/rock asked) ─────
mock_reset
setup_common
mock_input y n            # admin=yes; music=no -> its subtree gone; exhausted
bash "$SCRIPT" rec-1 >/dev/null; code=$?
assert "prune: exits 0" [ "$code" -eq 0 ]
assert_eq "prune: only the two top-level tags are asked" "admin,music" "$(asked_order)"
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
mock_respond 'gui query'        '@exit:1'
mock_respond 'path rec-1'       '/abs/file'
mock_respond 'metarecord -q rec-1 get --sort mfr_path*' 'rec-1'
mock_respond 'gui layout left'  'saved-left'
mock_respond 'gui layout right' 'saved-right'
mock_respond 'gui workspace new*' 'ws-1'
err=$(bash "$SCRIPT" rec-1 2>&1 >/dev/null); code=$?
assert "no-vocab: non-zero exit" [ "$code" -ne 0 ]
assert_contains "no-vocab: explains the empty vocabulary" "$err" 'no tag entries'

# ── Case 5: no argument — the scope is what the GUI shows ───────────────────
mock_reset
G='mfr_type = "file"'
setup_common "$G"
mock_respond "metarecord -q $G get --sort mfr_path*" 'rec-1'
mock_input q
out=$(bash "$SCRIPT"); code=$?
assert "gui scope: exits 0" [ "$code" -eq 0 ]
assert "gui scope: asks the GUI what it shows" [ "$(mock_count 'gui query')" -ge 1 ]
assert "gui scope: no folder prompt when one is published" \
    [ "$(mock_count 'gui prompt*')" -eq 0 ]
assert_contains "gui scope: classifies the matching record" "$out" "rec-1"

# ── Case 6: nothing published falls back to the folder completion ───────────
mock_reset
setup_common
mock_respond 'metarecord -q mfr_type = "dir" get*' '/some/dir'      # completion
mock_respond 'metarecord -q mfr_path =>* "/some/dir" get --sort mfr_path*' 'rec-1'
mock_prompt '/some/dir'
mock_input q
out=$(bash "$SCRIPT"); code=$?
assert "fallback: exits 0" [ "$code" -eq 0 ]
assert "fallback: the folder becomes an inclusive-subtree query" \
    [ "$(mock_count 'metarecord -q mfr_path =>* "/some/dir" get --sort mfr_path*')" -eq 1 ]

# ── Case 7: cancelling the folder prompt aborts ─────────────────────────────
mock_reset
setup_common
mock_respond 'metarecord -q mfr_type = "dir" get*' '/some/dir'
mock_prompt @cancel
err=$(bash "$SCRIPT" 2>&1 >/dev/null); code=$?
assert "prompt-cancel: non-zero exit" [ "$code" -ne 0 ]
assert_contains "prompt-cancel: reports cancelled" "$err" cancelled

# ── Case 8: a query matching several records classifies each in turn ────────
# The old script took ONE metarecord; the scope is now a set, so a run walks it.
mock_reset
Q='rating > 3'
setup_common
mock_respond "metarecord -q $Q get --sort mfr_path*" $'rec-1\nrec-2'
mock_respond 'path rec-2'            '/abs/file2'
mock_respond 'path --relative rec-2' '/file2'
mock_input n y y n   q               # rec-1 fully classified, then rec-2 stopped
out=$(bash "$SCRIPT" "$Q"); code=$?
assert "set: exits 0" [ "$code" -eq 0 ]
assert "set: the first record is classified" [ "$(mock_count 'tag -i rec-1 add music')" -eq 1 ]
assert "set: the second record is reached" [ "$(mock_count 'gui progress*--phase /file2')" -ge 1 ]
assert "set: the progress bar knows the total" \
    [ "$(mock_count 'gui progress --done 1 --total 2*')" -ge 1 ]

# ── Case 9: quitting one record stops the whole run ────────────────────────
mock_reset
Q='rating > 3'
setup_common
mock_respond "metarecord -q $Q get --sort mfr_path*" $'rec-1\nrec-2'
mock_respond 'path rec-2'            '/abs/file2'
mock_respond 'path --relative rec-2' '/file2'
mock_input q
bash "$SCRIPT" "$Q" >/dev/null; code=$?
assert "quit: exits 0" [ "$code" -eq 0 ]
assert "quit: no tag op at all" [ "$(mock_count 'tag -i *')" -eq 0 ]
assert "quit: the second record is never shown" \
    [ "$(mock_count 'gui progress*--phase /file2')" -eq 0 ]

assert_summary
