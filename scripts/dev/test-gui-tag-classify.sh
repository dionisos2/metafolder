#!/usr/bin/env bash
# Tests for scripts/shipped/gui-tag-classify.sh — interactive hierarchical-tag
# classification of ONE metarecord. The scripted `mf` shim
# (scripts/dev/lib/mf-mock.sh) stands in for the daemon + GUI, and its built-in tag
# store makes `mf tag -i U add/deny` observable to the next `field get … tag`,
# so the descend-until-exhausted loop runs for real. We assert the exact
# question ORDER the selector drives, the add/deny calls, the summary, and the
# empty-vocabulary / file-prompt paths. No daemon/GUI.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/../shipped/gui-tag-classify.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

# A four-tag vocabulary: two top-level, two children of `music`.
# Name-sorted, because that is the only order `mf tag list` ever prints
# (`commands.rs`, `vocab.names.sort()`). Fed unsorted, the suite was exercising
# the depth tie-break on an input the CLI cannot produce.
UNIVERSE=$'admin\nmusic\nmusic/jazz\nmusic/rock'

# The optional argument is what `mf gui query` answers (the table is
# first-match-wins, so it cannot be overridden by a later row).
setup_common() { # [<gui query response>]
    mock_respond 'gui repo'         'repo-1'
    mock_respond 'gui query'        "${1-@exit:1}"
    mock_respond 'repo root'        '/abs'
    # Every relative path of the scope in one read (rows: uuid<TAB>path).
    mock_respond 'metarecord -q * get --resolve-tree mfr_path --tsv' \
        $'rec-1\t/file\nrec-2\t/file2'
    mock_respond 'tag list'         "$UNIVERSE"
    mock_respond 'gui layout left'  'saved-left'
    mock_respond 'gui layout right' 'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
    # The scope resolves to the one record the old single-uuid form named: a
    # bare uuid is a valid query (doc "Query DSL grammar", the uuid-atom bullet), so the old
    # invocation keeps working.
    mock_respond 'metarecord -q rec-1 get --sort mfr_path*' 'rec-1'
}

# The ordered list of tags actually asked about, comma-joined.
asked_order() {
    # The question rides on the wait's own --prompt (doc "Status bar and task bar"), so
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
mock_respond 'repo root'        '/abs'
mock_respond 'metarecord -q * get --resolve-tree mfr_path --tsv' $'rec-1\t/file'
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
mock_input q q
out=$(bash "$SCRIPT"); code=$?
assert "gui scope: exits 0" [ "$code" -eq 0 ]
assert "gui scope: asks the GUI what it shows" [ "$(mock_count 'gui query')" -ge 1 ]
assert "gui scope: no folder prompt when one is published" \
    [ "$(mock_count 'gui prompt*')" -eq 0 ]
assert_contains "gui scope: classifies the matching record" "$out" "rec-1"

# ── Case 5b: answering s targets the checked selection ──────────────────────
mock_reset
setup_common
mock_respond 'gui selected' 'rec-1'
mock_input s q            # the scope: the selection; then stop the walk
out=$(bash "$SCRIPT"); code=$?
assert "selected: exits 0" [ "$code" -eq 0 ]
assert "selected: the scope is the checked record's UUID" \
    [ "$(mock_count 'metarecord -q rec-1 get --sort mfr_path*')" -ge 1 ]
assert "selected: the list's query is never read" [ "$(mock_count 'gui query')" -eq 0 ]
assert_contains "selected: classifies the checked record" "$out" "rec-1"

# ── Case 5c: s on an empty selection is a refusal, never the query ──────────
mock_reset
setup_common
mock_respond 'gui selected' ''
mock_input s              # the scope: the selection — which is empty
err=$(bash "$SCRIPT" 2>&1 >/dev/null); code=$?
assert "no selection: non-zero exit" [ "$code" -ne 0 ]
assert_contains "no selection: says nothing is checked" "$err" "nothing is checked"
assert "no selection: the list's query is never read" [ "$(mock_count 'gui query')" -eq 0 ]

# ── Case 6: nothing published falls back to the folder completion ───────────
mock_reset
setup_common
mock_respond 'metarecord -q mfr_type = "dir" get*' '/some/dir'      # completion
mock_respond 'metarecord -q mfr_path =>* "/some/dir" get --sort mfr_path*' 'rec-1'
mock_prompt '/some/dir'
mock_input q q
out=$(bash "$SCRIPT"); code=$?
assert "fallback: exits 0" [ "$code" -eq 0 ]
assert "fallback: the folder becomes an inclusive-subtree query" \
    [ "$(mock_count 'metarecord -q mfr_path =>* "/some/dir" get --sort mfr_path*')" -eq 1 ]

# ── Case 7: cancelling the folder prompt aborts ─────────────────────────────
mock_reset
setup_common
mock_respond 'metarecord -q mfr_type = "dir" get*' '/some/dir'
mock_input q              # the scope: the query, then the folder
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
mock_input n y y n   q               # rec-1 fully classified, then rec-2 stopped
out=$(bash "$SCRIPT" "$Q"); code=$?
assert "set: exits 0" [ "$code" -eq 0 ]
assert "set: the first record is classified" [ "$(mock_count 'tag -i rec-1 add music')" -eq 1 ]
assert "set: the second record is reached" [ "$(mock_count 'gui progress*--phase /file2')" -ge 1 ]
assert "set: each preview is the root + the relative path" \
    [ "$(mock_count 'gui view left viewer --path /abs/file')" -eq 1 ] \
    && [ "$(mock_count 'gui view left viewer --path /abs/file2')" -eq 1 ]
assert "set: the root is read once, no \`mf path\` per record" \
    [ "$(mock_count 'repo root')" -eq 1 ] && [ "$(mock_count 'path *')" -eq 0 ]
assert "set: the progress bar knows the total" \
    [ "$(mock_count 'gui progress --done 1 --total 2*')" -ge 1 ]

# ── Case 9: quitting one record stops the whole run ────────────────────────
mock_reset
Q='rating > 3'
setup_common
mock_respond "metarecord -q $Q get --sort mfr_path*" $'rec-1\nrec-2'
mock_input q
bash "$SCRIPT" "$Q" >/dev/null; code=$?
assert "quit: exits 0" [ "$code" -eq 0 ]
assert "quit: no tag op at all" [ "$(mock_count 'tag -i *')" -eq 0 ]
assert "quit: the second record is never shown" \
    [ "$(mock_count 'gui progress*--phase /file2')" -eq 0 ]

# ── Flags (doc "Tag flags"): columns always, partition, sparse after the uuid ──
FLAGGED_BASE=$UNIVERSE

# Case 10: a partition is asked yes/no, then one child is picked by completion.
mock_reset
UNIVERSE=$'admin\tA\nmusic\tM\t0\t1\t0\nmusic/jazz\tJ\nmusic/rock\tR'
setup_common
mock_input n y
mock_prompt jazz
out=$(bash "$SCRIPT" rec-1); code=$?
assert "partition: exits 0" [ "$code" -eq 0 ]
assert_eq "partition: only the parent is asked yes/no" "admin,music" "$(asked_order)"
assert "partition: the children are offered in one prompt" \
    [ "$(mock_count "gui prompt*music*--completions-stdin")" -eq 1 ]
assert "partition: the picked child is added" [ "$(mock_count 'tag -i rec-1 add music/jazz')" -eq 1 ]
assert "partition: the others are left alone" [ "$(mock_count 'tag -i rec-1 * music/rock')" -eq 0 ]
assert_contains "partition: a pick counts as a yes" "$out" "2 oui, 1 non"

# Case 11: nothing more, on a partition, denies the children still offered.
mock_reset
setup_common
mock_input n y
mock_prompt '(rien de plus)'
bash "$SCRIPT" rec-1 >/dev/null; code=$?
assert "nothing more: exits 0" [ "$code" -eq 0 ]
assert "nothing more: jazz denied" [ "$(mock_count 'tag -i rec-1 deny music/jazz')" -eq 1 ]
assert "nothing more: rock denied" [ "$(mock_count 'tag -i rec-1 deny music/rock')" -eq 1 ]
assert "nothing more: one prompt" [ "$(mock_count 'gui prompt*')" -eq 1 ]

# Case 12: a sparse tag is picked from until nothing more is chosen.
mock_reset
UNIVERSE=$'music\tM\t0\t0\t1\nmusic/jazz\tJ\nmusic/pop\tP\nmusic/rock\tR'
setup_common
mock_input y
mock_prompt jazz rock '(rien de plus)'
bash "$SCRIPT" rec-1 >/dev/null; code=$?
assert "sparse: exits 0" [ "$code" -eq 0 ]
assert "sparse: three prompts" [ "$(mock_count 'gui prompt*')" -eq 3 ]
assert "sparse: jazz added" [ "$(mock_count 'tag -i rec-1 add music/jazz')" -eq 1 ]
assert "sparse: rock added" [ "$(mock_count 'tag -i rec-1 add music/rock')" -eq 1 ]
assert "sparse: pop denied by nothing more" [ "$(mock_count 'tag -i rec-1 deny music/pop')" -eq 1 ]

# Case 13: a value that is no candidate is asked again; Escape stops the run.
mock_reset
setup_common
mock_input y
mock_prompt nope @cancel
out=$(bash "$SCRIPT" rec-1); code=$?
assert "pick cancel: exits 0" [ "$code" -eq 0 ]
assert "pick cancel: asked again after an unknown value" [ "$(mock_count 'gui prompt*')" -eq 2 ]
assert "pick cancel: nothing written under music" [ "$(mock_count 'tag -i rec-1 * music/*')" -eq 0 ]

# Case 14: an always tag is added without a question, unless already answered.
mock_reset
UNIVERSE=$'admin\tA\t1\nmusic\tM\nmusic/jazz\tJ'
setup_common
mock_input n
bash "$SCRIPT" rec-1 >/dev/null; code=$?
assert "always: exits 0" [ "$code" -eq 0 ]
assert "always: added unasked" [ "$(mock_count 'tag -i rec-1 add admin')" -eq 1 ]
assert_eq "always: never asked" "music" "$(asked_order)"

mock_reset
setup_common
mock_respond 'metarecord -i rec-1 field get negative_tag*' 'admin'
mock_input n
bash "$SCRIPT" rec-1 >/dev/null
assert "always: a denied tag stays denied" [ "$(mock_count 'tag -i rec-1 add admin')" -eq 0 ]
UNIVERSE=$FLAGGED_BASE

assert_summary
