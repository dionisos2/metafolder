#!/usr/bin/env bash
# Tests for scripts/shipped/gui-tag-folder.sh — bulk-apply one tag over a set of
# metarecords with a yes/no/mixed walk. The scripted `mf` shim
# (scripts/lib/mf-mock.sh) stands in for the daemon + GUI.
#
# THE QUERY IS THE WALK STATE (docs/gui-tag-folder-rework.md). An answer is a
# write, so "what is left to ask" is a query: each step asks ONE question with
# `--limit 1` over `<scope> AND <undecided> AND <not skipped>`, and every answer
# takes its subject — a whole subtree, for a folder answered y/n — out of that
# set. Nothing is read up front and nothing is held in bash.
#
# So these tests declare the walk as the SUCCESSIVE ANSWERS to the step queries
# (`walk_children` / `walk_descendants`, queues that empty as the real set
# would) rather than as a scope listing, and they assert the shape of the
# queries: the mocked `mf` cannot resolve a query, so what it can pin is that
# the right question was asked of the daemon.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/shipped/gui-tag-folder.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

# The GUI session boilerplate every run needs.
setup_gui() {
    mock_respond 'gui repo'          'repo-1'
    mock_respond 'gui layout left'   'saved-left'
    mock_respond 'gui layout right'  'saved-right'
    mock_respond 'gui workspace new*' 'ws-1'
}

# The scope the folder completion builds for /top.
SC='mfr_path =>* "/top"'
setup_top() {
    setup_gui
    mock_respond 'gui query'                             '@exit:1'   # nothing published
    mock_respond 'metarecord -q mfr_type = "dir" get*'   '/top'      # folder completion
}

# ── Declaring the walk ───────────────────────────────────────────────────────
# Each step query is answered from a QUEUE: the first call gets the first uuid,
# the next call the next one, and an exhausted queue answers nothing — which is
# what the real set does as answers remove their subject from it.

_qn=0
_qnext() { _qn=$((_qn + 1)); printf 'q%d' "$_qn"; }

# The step query for the direct children of <parent>, one kind at a time. The
# repository root is nobody's child, so at the walk root the query asks for the
# forest root as well.
_child_glob() { # <parent-path> <dir|file>
    local parent type
    if [ -z "$1" ]; then
        parent='(mfr_path:parent = "" OR mfr_path:parent IS ABSENT)'
    else
        parent="mfr_path:parent = \"$1\""
    fi
    # A leaf is "not a folder", so a tracked symlink is asked about like a file
    # instead of being reachable by the descent and by nothing else.
    if [ "$2" = dir ]; then type='mfr_type = "dir"'; else type='NOT mfr_type = "dir"'; fi
    printf 'metarecord -q *%s AND %s get --sort*' "$type" "$parent"
}

walk_children() { # <parent-path> <dir|file> <uuid>...
    local parent=$1 kind=$2 q
    shift 2
    q=$(_qnext)
    mock_respond "$(_child_glob "$parent" "$kind")" "@queue:$q"
    mock_queue "$q" "$@"
}

# The rule-2 query: the first open DESCENDANT of <parent>, by path. It is what
# reaches a scattered scope — a folder out of scope holding a file in it.
walk_descendants() { # <parent-path> <uuid>...
    local parent=$1 q
    shift
    q=$(_qnext)
    mock_respond "metarecord -q *AND mfr_path ->* \"$parent\" get --sort mfr_path --limit 1" "@queue:$q"
    mock_queue "$q" "$@"
}

# What `--resolve-tree` answers for one uuid (the root's path is the empty one).
walk_path() { mock_respond "metarecord -i $1 get --resolve-tree mfr_path" "${2-}"; }

# The successive answers to the counter query (`--count`): the first is the
# total the progress bar is built on, the rest are the "N left" of each question.
walk_counts() {
    mock_respond 'metarecord -q *mfr_path IS PRESENT get --count' '@queue:counts'
    mock_queue counts "$@"
}

# The tag's vocabulary entry, which the skip marker refs.
tag_entry() { mock_respond "metarecord -q mf_schema = \"tag\" AND path = \"$1\" get --limit 1" "$2"; }

# How many skip markers this tag has left behind (the resume offer reads it).
markers_count() { mock_respond 'metarecord -q *gui_tag_skipped -> (mf_schema = "tag" AND path = "*") get --count' "$1"; }

# How many times the walk asked about one entry. The question travels as the
# wait's own --prompt (spec-gui "Status bar": that is what puts it in the
# dedicated question bar), so it is the `gui input` call that carries it.
asked() { mock_count "gui input --prompt '$1' has tag*"; }

# ── Case 1: a folder answered "yes" — the node and its subtree, scoped ───────
mock_reset
setup_top
walk_children "" dir dir-top
walk_path dir-top /top
walk_counts 1 1
mock_prompt '/top'
mock_input y
out=$(bash "$SCRIPT" music); code=$?
assert "yes: exits 0" [ "$code" -eq 0 ]
assert "yes: tags the node" [ "$(mock_count 'tag -i dir-top add music')" -eq 1 ]
assert "yes: tags the subtree INTERSECTED with the scope" \
    [ "$(mock_count "tag -q ($SC) AND mfr_path ->* \"/top\" add music")" -eq 1 ]
assert "yes: never tags outside the scope" \
    [ "$(mock_count 'tag -q mfr_path ->* "/top" add music')" -eq 0 ]
assert_contains "yes: reports done" "$out" "done tagging 'music'"

# ── Case 1b: nothing is read up front ───────────────────────────────────────
# The old walk read the whole scope — ordered uuids and paths, per kind —
# before the first question. Every read the new one makes is bounded: a
# `--limit`, a `--count`, or one record by uuid.
unbounded=$(mock_calls_matching "metarecord -q ($SC) AND * get*" \
    | grep -v -- '--limit' | grep -v -- '--count' | grep -c . || true)
assert "no up-front read: every read of the scope is bounded" [ "$unbounded" -eq 0 ]
assert "no up-front read: the scope is never listed with --resolve-tree" \
    [ "$(mock_count "metarecord -q ($SC) AND * get --resolve-tree*")" -eq 0 ]

# ── Case 2: "no" denies the same two scopes ─────────────────────────────────
mock_reset
setup_top
walk_children "" dir dir-top
walk_path dir-top /top
walk_counts 1 1
mock_prompt '/top'
mock_input n
bash "$SCRIPT" music >/dev/null; code=$?
assert "no: exits 0" [ "$code" -eq 0 ]
assert "no: denies the node" [ "$(mock_count 'tag -i dir-top deny music')" -eq 1 ]
assert "no: denies the scoped subtree" \
    [ "$(mock_count "tag -q ($SC) AND mfr_path ->* \"/top\" deny music")" -eq 1 ]

# ── Case 3: "mixed" descends into the folder; its children are asked ─────────
mock_reset
setup_top
walk_children "" dir dir-top
walk_children /top dir dir-sub
walk_children /top file file-a file-b
walk_path dir-top /top
walk_path dir-sub /top/sub
walk_path file-a /top/a.txt
walk_path file-b /top/b.txt
walk_counts 5 5 4 2 1
mock_prompt '/top'
#   top=m  sub=y (whole subtree)  a.txt=y  b.txt=n
mock_input m y y n
out=$(bash "$SCRIPT" music); code=$?
assert "mixed: exits 0" [ "$code" -eq 0 ]
assert "mixed: marks the parent mixed" [ "$(mock_count 'tag -i dir-top mixed music')" -eq 1 ]
assert "mixed: the child folder is asked" [ "$(asked /top/sub)" -eq 1 ]
assert "mixed: the child folder answered y takes its subtree with it" \
    [ "$(mock_count "tag -q ($SC) AND mfr_path ->* \"/top/sub\" add music")" -eq 1 ]
assert "mixed: child file y -> add" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]
assert "mixed: child file n -> deny" [ "$(mock_count 'tag -i file-b deny music')" -eq 1 ]
assert "mixed: a mixed folder is never applied to its subtree" \
    [ "$(mock_count 'tag -q * mixed music')" -eq 0 ]

# ── Case 4: at one level, the folders are asked before the files ─────────────
mock_reset
setup_top
walk_children "" dir dir-top
walk_children /top dir dir-sub
walk_children /top file file-z
walk_path dir-top /top
walk_path dir-sub /top/sub
walk_path file-z /top/z.txt
walk_counts 3 3 2 1
mock_prompt '/top'
mock_input m s s
out=$(bash "$SCRIPT" music); code=$?
assert "order: exits 0" [ "$code" -eq 0 ]
order=$(mock_calls_matching "gui input --prompt '*' has tag*" \
    | sed "s/.*--prompt '\([^']*\)'.*/\1/")
assert_contains "order: the folder comes before the file" \
    "$(printf '%s' "$order" | tr '\n' ' ')" "/top /top/sub /top/z.txt"

# ── Case 5: an explicit query argument is the scope, no folder prompt ────────
mock_reset
setup_gui
Q='rating > 3'
walk_children "" file file-a
walk_path file-a /x/a.txt
walk_counts 1 1
mock_input y
out=$(bash "$SCRIPT" music "$Q"); code=$?
assert "query arg: exits 0" [ "$code" -eq 0 ]
assert "query arg: no folder completion is offered" \
    [ "$(mock_count 'gui prompt*')" -eq 0 ]
assert "query arg: the walk is narrowed to the query" \
    [ "$(mock_count "metarecord -q ($Q) AND *")" -ge 1 ]
assert "query arg: the matching file is asked" [ "$(asked /x/a.txt)" -eq 1 ]
assert "query arg: and tagged" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 6: with no argument the scope is what the GUI shows ────────────────
mock_reset
setup_gui
G='mfr_type = "file" AND rating > 3'
mock_respond 'gui query' "$G"
walk_children "" file file-a
walk_path file-a /x/a.txt
walk_counts 1 1
mock_input y
out=$(bash "$SCRIPT" music); code=$?
assert "gui query: exits 0" [ "$code" -eq 0 ]
assert "gui query: asks the GUI what it shows" [ "$(mock_count 'gui query')" -ge 1 ]
assert "gui query: no folder prompt when one is published" [ "$(mock_count 'gui prompt*')" -eq 0 ]
assert "gui query: the walk is narrowed to it" [ "$(mock_count "metarecord -q ($G) AND *")" -ge 1 ]
assert "gui query: the record is tagged" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 7: an empty scope (the GUI shows everything) is not narrowed ────────
mock_reset
setup_gui
mock_respond 'gui query' ''
walk_children "" file file-a
walk_path file-a /x/a.txt
walk_counts 1 1
mock_input y
bash "$SCRIPT" music >/dev/null; code=$?
assert "all: exits 0" [ "$code" -eq 0 ]
assert "all: no empty parentheses in the query" [ "$(mock_count 'metarecord -q () AND*')" -eq 0 ]
assert "all: the record is tagged" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 8: the ROOT folder uses the empty-string tree form ──────────────────
# The repository root is nobody's child, so the walk's first step asks for the
# forest root (`mfr_path:parent IS ABSENT`) beside the root's own children.
mock_reset
setup_gui
mock_respond 'gui query'                            '@exit:1'
mock_respond 'metarecord -q mfr_type = "dir" get*'  '/'
RSC='mfr_path =>* ""'
walk_children "" dir dir-root
walk_path dir-root ''
walk_counts 1 1
mock_prompt '/'
mock_input y
bash "$SCRIPT" roottag >/dev/null; code=$?
assert "root: exits 0" [ "$code" -eq 0 ]
assert "root: the scope uses the empty-string form" \
    [ "$(mock_count "metarecord -q ($RSC) AND *")" -ge 1 ]
# The daemon folds a redundant slash away now, so `"/"` would resolve too —
# this pins the CANONICAL spelling (the empty string, what `path_of` prints),
# so every script keeps writing the one form.
assert "root: spells the root the one canonical way" \
    [ "$(mock_count 'metarecord -q (mfr_path =>* "/") AND*')" -eq 0 ]
assert "root: the forest root is asked for, since it is nobody's child" \
    [ "$(mock_count '*mfr_path:parent IS ABSENT*')" -ge 1 ]
assert "root: answering it applies to the whole subtree" \
    [ "$(mock_count "tag -q ($RSC) AND mfr_path ->* \"\" add roottag")" -eq 1 ]

# ── Case 9: a skip is RECORDED, so it survives the run ───────────────────────
# The marker is a ref to the tag's own vocabulary entry, so a skip left by a run
# on one tag never silences the same entry for another.
mock_reset
setup_top
tag_entry music tag-music
markers_count 0
walk_children "" dir dir-top
walk_children /top file file-b
walk_path dir-top /top
walk_path file-b /top/b.txt
walk_counts 3 3 1
mock_prompt '/top'
#   top=m  b.txt=s (skipped), then the cleanup offer at the end: keep them
mock_input m s n
out=$(bash "$SCRIPT" music); code=$?
assert "skip: exits 0" [ "$code" -eq 0 ]
assert "skip: no tag op on the skipped entry" [ "$(mock_count 'tag -i file-b *')" -eq 0 ]
assert "skip: the marker refs the tag entry" \
    [ "$(mock_count 'metarecord -i file-b field add gui_tag_skipped:ref=tag-music')" -eq 1 ]
assert_contains "skip: the summary counts it" "$out" "1 skipped"

# ── Case 10: a skipped FOLDER takes its subtree out of the questions ─────────
# The marker sits on the folder alone, so the walk and the counter both carry a
# `NOT mfr_path ->* "<folder>"` for it — otherwise the files under a folder the
# user deliberately left alone would be asked one by one.
mock_reset
setup_top
tag_entry music tag-music
markers_count 0
walk_children "" dir dir-top
walk_children /top dir dir-sub
walk_children /top file file-b
walk_path dir-top /top
walk_path dir-sub /top/sub
walk_path file-b /top/b.txt
walk_counts 4 4 3 1
mock_prompt '/top'
#   top=m  sub=s (skipped folder)  b.txt=y, then keep the markers
mock_input m s y n
out=$(bash "$SCRIPT" music); code=$?
assert "skip dir: exits 0" [ "$code" -eq 0 ]
assert "skip dir: the folder is marked, not tagged" \
    [ "$(mock_count 'metarecord -i dir-sub field add gui_tag_skipped:ref=tag-music')" -eq 1 ]
assert "skip dir: nothing under it is asked for any more" \
    [ "$(mock_count '*AND NOT mfr_path ->* "/top/sub"*')" -ge 1 ]
assert "skip dir: the counter excludes the subtree too" \
    [ "$(mock_count '*AND NOT mfr_path ->* "/top/sub"* get --count')" -ge 1 ]
assert "skip dir: the walk carries on with its siblings" \
    [ "$(mock_count 'tag -i file-b add music')" -eq 1 ]

# ── Case 11: the skip markers are offered for cleanup at the end ────────────
mock_reset
setup_top
tag_entry music tag-music
markers_count 0
walk_children "" file file-a
walk_path file-a /top/a.txt
walk_counts 1 1
mock_prompt '/top'
mock_input s y          # skip the only entry, then ACCEPT the cleanup
out=$(bash "$SCRIPT" music); code=$?
assert "cleanup: exits 0" [ "$code" -eq 0 ]
assert "cleanup: the offer names what it will forget" \
    [ "$(mock_count "gui input --prompt *skipped*")" -ge 1 ]
assert "cleanup: accepted, the markers are removed by value" \
    [ "$(mock_count 'metarecord -q * field remove gui_tag_skipped:ref=tag-music')" -eq 1 ]
assert "cleanup: and only this tag's marker, never the whole field" \
    [ "$(mock_count 'metarecord -q * field unset gui_tag_skipped*')" -eq 0 ]

# ── Case 11b: declined, the markers stay for the next run ───────────────────
mock_reset
setup_top
tag_entry music tag-music
markers_count 0
walk_children "" file file-a
walk_path file-a /top/a.txt
walk_counts 1 1
mock_prompt '/top'
mock_input s n          # skip, then DECLINE the cleanup
bash "$SCRIPT" music >/dev/null
assert "cleanup declined: nothing is removed" \
    [ "$(mock_count 'metarecord -q * field remove gui_tag_skipped*')" -eq 0 ]

# ── Case 11c: leftover markers are offered again at the next run's start ────
# That is where the choice is actually informed: resume where you were, or ask
# those entries again.
mock_reset
setup_top
tag_entry music tag-music
markers_count 2
mock_respond 'metarecord -q *mfr_type = "dir" AND gui_tag_skipped -> * get --resolve-tree mfr_path' '/top/sub'
walk_children "" file file-a
walk_path file-a /top/a.txt
walk_counts 1 1
mock_prompt '/top'
mock_input n y n        # "ask them again" = no, keep; then y on the file; then keep at the end
out=$(bash "$SCRIPT" music); code=$?
assert "resume offer: exits 0" [ "$code" -eq 0 ]
assert "resume offer: the run starts by saying how many are skipped" \
    [ "$(mock_count "gui input --prompt *2 *skipped*")" -ge 1 ]
assert "resume offer: kept, a skipped FOLDER is read back so its subtree stays out" \
    [ "$(mock_count 'metarecord -q *mfr_type = "dir" AND gui_tag_skipped -> * get --resolve-tree mfr_path')" -eq 1 ]
assert "resume offer: and its subtree is excluded from the walk" \
    [ "$(mock_count '*AND NOT mfr_path ->* "/top/sub"*')" -ge 1 ]

# ── Case 11d: asked again, the markers are cleared before the walk ──────────
mock_reset
setup_top
tag_entry music tag-music
markers_count 2
walk_children "" file file-a
walk_path file-a /top/a.txt
walk_counts 1 1
mock_prompt '/top'
mock_input y y          # "ask them again" = yes, then answer the file
bash "$SCRIPT" music >/dev/null
assert "ask again: the markers are cleared" \
    [ "$(mock_count 'metarecord -q * field remove gui_tag_skipped:ref=tag-music')" -eq 1 ]
assert "ask again: and their folders are not read back to close their subtrees" \
    [ "$(mock_count 'metarecord -q *mfr_type = "dir" AND gui_tag_skipped -> * get --resolve-tree*')" -eq 0 ]

# ── Case 12: quit stops the walk ────────────────────────────────────────────
mock_reset
setup_top
walk_children "" dir dir-top
walk_path dir-top /top
walk_counts 2 2
mock_prompt '/top'
mock_input q
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert "stop: exits 0" [ "$code" -eq 0 ]
assert_contains "stop: reports stopped" "$out" stopped
assert "stop: no tag op at all" [ "$(mock_count 'tag *')" -eq 0 ]
assert "stop: reports no error of its own" \
    [ "$(printf '%s' "$err" | grep -c '^error:')" -eq 0 ]

# ── Case 13: a tag with a double quote is rejected ──────────────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" 'bad"tag' 2>&1 >/dev/null); code=$?
assert "quote: non-zero exit" [ "$code" -ne 0 ]
assert_contains "quote: explains the rule" "$err" 'double quote'

# ── Case 14: too many positional arguments is a usage error ─────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" a b c 2>&1 >/dev/null); code=$?
assert "usage: non-zero exit on 3 args" [ "$code" -ne 0 ]
assert_contains "usage: prints a usage line" "$err" usage

# ── Case 15: what is already decided is spelled in the query, not re-read ───
# A positive on the tag OR BELOW it answers the question; a negative on the tag
# or ABOVE it denies it; an exact mixed marker is one. The daemon owns the
# subsumption — the walk only has to ask for what is still open. (That the
# daemon really resolves those forms is pinned against a real one in
# test-scripts-integration.sh.)
mock_reset
setup_top
walk_children "" file file-a
walk_path file-a /top/a.txt
walk_counts 1 1
mock_prompt '/top'
mock_input y
bash "$SCRIPT" music/jazz >/dev/null; code=$?
assert "open: exits 0" [ "$code" -eq 0 ]
assert "open: a positive on the tag or below is not asked again" \
    [ "$(mock_count '*NOT (tag -> (mf_schema = "tag" AND path =>* "music/jazz")*')" -ge 1 ]
assert "open: the negative side names the tag and its ancestors" \
    [ "$(mock_count '*negative_tag -> (mf_schema = "tag" AND (path = "music/jazz" OR path = "music"))*')" -ge 1 ]
assert "open: a mixed folder is decided too (the walk enters it by descending)" \
    [ "$(mock_count '*mixed_tag -> (mf_schema = "tag" AND path = "music/jazz")*')" -ge 1 ]
assert "open: nothing is read one entry at a time" \
    [ "$(mock_count 'metarecord -i * field get *')" -eq 0 ]

# ── Case 16: --redo ignores every marker, and asks what it has not asked ────
# The decided/skipped clauses go away — so the walk needs its own memory of
# what it has just asked, which rides in a `uuid_in` list bounded by keypresses.
mock_reset
setup_top
tag_entry music tag-music
markers_count 3
walk_children "" file file-a file-b
walk_path file-a /top/a.txt
walk_path file-b /top/b.txt
walk_counts 2 2 1
mock_prompt '/top'
mock_input y y
bash "$SCRIPT" --redo music >/dev/null; code=$?
assert "redo: exits 0" [ "$code" -eq 0 ]
assert "redo: the decided clause is gone" [ "$(mock_count '*NOT (tag -> *')" -eq 0 ]
assert "redo: the skip markers are neither read nor honoured" \
    [ "$(mock_count '*NOT gui_tag_skipped*')" -eq 0 ]
assert "redo: no resume offer" [ "$(mock_count 'gui input --prompt *skipped*')" -eq 0 ]
assert "redo: an entry already asked is excluded by uuid" \
    [ "$(mock_count '*NOT uuid_in(file-a)*')" -ge 1 ]
assert "redo: both entries are asked" [ "$(($(asked /top/a.txt) + $(asked /top/b.txt)))" -eq 2 ]

# ── Case 17: the counter is a count, exact and dropping by whole subtrees ───
# `--count` is O(1) on the index, so "N left" is asked once per question and is
# exact — it drops by everything a folder answered whole took with it.
mock_reset
setup_top
walk_children "" dir dir-top
walk_children /top dir dir-sub
walk_children /top file file-y
walk_path dir-top /top
walk_path dir-sub /top/sub
walk_path file-y /top/y.txt
#   4 entries: /top, /top/sub, /top/sub/x.txt, /top/y.txt. Answering /top/sub
#   whole removes x.txt, so the question after it says nothing is left.
walk_counts 4 4 3 1
mock_prompt '/top'
mock_input m y y
bash "$SCRIPT" music >/dev/null; code=$?
assert "count: exits 0" [ "$code" -eq 0 ]
assert "count: the total comes from one --count call, up front" \
    [ "$(mock_count 'metarecord -q * get --count')" -ge 1 ]
assert "count: the mixed root says 3 left" \
    [ "$(mock_count "gui input --prompt '/top' has tag*3 left*")" -eq 1 ]
assert "count: the folder about to be settled says 2 left" \
    [ "$(mock_count "gui input --prompt '/top/sub' has tag*2 left*")" -eq 1 ]
assert "count: the next question has the whole subtree discounted" \
    [ "$(mock_count "gui input --prompt '/top/y.txt' has tag*0 left*")" -eq 1 ]
assert "count: the bar starts at 1 of 4" \
    [ "$(mock_count 'gui progress --done 1 --total 4*')" -eq 1 ]
assert "count: and reaches its total" [ "$(mock_count 'gui progress --done 4 --total 4*')" -ge 1 ]

# ── Case 18: a scattered scope is reached by descending ─────────────────────
# The query is the scope, so a folder can be out of scope while a file under it
# is in it. With no open child at the walk root, the walk takes the first open
# DESCENDANT, moves to its parent, and asks there — skipping every empty level
# in one step.
mock_reset
setup_gui
mock_respond 'gui query' 'rating > 3'
walk_descendants "" file-deep
walk_children /a/b file file-deep
walk_path file-deep /a/b/deep.txt
walk_counts 1 1
mock_input y
out=$(bash "$SCRIPT" music); code=$?
assert "scattered: exits 0" [ "$code" -eq 0 ]
assert "scattered: the walk asks for the first open descendant" \
    [ "$(mock_count 'metarecord -q * AND mfr_path ->* "" get --sort mfr_path --limit 1')" -ge 1 ]
assert "scattered: it descends straight to its parent, asking nothing on the way" \
    [ "$(asked /a)" -eq 0 ]
assert "scattered: and asks the file there" [ "$(asked /a/b/deep.txt)" -eq 1 ]
assert "scattered: which is tagged" [ "$(mock_count 'tag -i file-deep add music')" -eq 1 ]

# ── Case 19: coming back up finds the siblings left behind ──────────────────
# /a/b/c/d.txt sorts before /a/b/z.txt, so the walk descends to /a/b/c first,
# empties it, comes back up to /a/b and finds z.txt there.
mock_reset
setup_gui
mock_respond 'gui query' 'rating > 3'
walk_descendants "" file-d
walk_children /a/b/c file file-d
walk_children /a/b file file-z
walk_path file-d /a/b/c/d.txt
walk_path file-z /a/b/z.txt
walk_counts 2 2 1
mock_input y y
out=$(bash "$SCRIPT" music); code=$?
assert "up: exits 0" [ "$code" -eq 0 ]
assert "up: the deep file is asked" [ "$(asked /a/b/c/d.txt)" -eq 1 ]
assert "up: the sibling left behind is found on the way up" [ "$(asked /a/b/z.txt)" -eq 1 ]

# ── Case 20: going back undoes the previous answer and asks it again ────────
# `backspace` during a question resolves the wait with `back` (spec-gui
# "Reserved keys"); the letter `u` (undo) is its typed twin. The answer is undone
# through the event log — which restores the exact rows — and the entry, open
# once more, is what the next step query returns.
mock_reset
setup_top
walk_children "" file file-a file-b file-a file-b
walk_path file-a /top/a.txt
walk_path file-b /top/b.txt
walk_counts 2 2 1 2 1
mock_respond 'log head' '@queue:heads'
mock_queue heads 10 11 12 13 14 15
mock_prompt '/top'
#   a.txt yes, then BACK at b.txt's question — which undoes it and re-asks —
#   then a.txt no, b.txt yes.
mock_input y u n y
bash "$SCRIPT" music >/dev/null; code=$?
assert "back: exits 0" [ "$code" -eq 0 ]
assert "back: the undone answer was rolled back through the log" \
    [ "$(mock_count 'log rollback --id * --silent')" -eq 1 ]
assert "back: the previous entry is asked again" [ "$(asked /top/a.txt)" -eq 2 ]
assert "back: and the second answer is the one that stands" \
    [ "$(mock_count 'tag -i file-a deny music')" -eq 1 ]
assert "back: the walk carries on and finishes" \
    [ "$(mock_count 'tag -i file-b add music')" -eq 1 ]

# ── Case 20b: going back over a skip un-skips ───────────────────────────────
# A skip is a write like any other, so it is undone like any other — and the
# subtree it closed is open again.
mock_reset
setup_top
tag_entry music tag-music
markers_count 0
walk_children "" dir dir-sub '' dir-sub
walk_children "" file file-b file-b
walk_path dir-sub /sub
walk_path file-b /b.txt
walk_counts 3 3 2 3 2
mock_respond 'log head' '@queue:heads2'
mock_queue heads2 20 21 22 23 24 25
mock_prompt '/top'
#   sub skipped, BACK at the next question, sub answered m this time, then done
mock_input s u m n
out=$(bash "$SCRIPT" music); code=$?
assert "back skip: exits 0" [ "$code" -eq 0 ]
assert "back skip: the marker write was rolled back" \
    [ "$(mock_count 'log rollback --id * --silent')" -eq 1 ]
assert "back skip: the folder is asked again" [ "$(asked /sub)" -eq 2 ]
assert "back skip: its subtree is no longer excluded once the skip is undone" \
    [ "$(mock_calls_matching 'metarecord -q * get --count' | tail -1 | grep -c 'NOT mfr_path' || true)" -eq 0 ]
assert "back skip: and the cleanup is not offered for a skip that was taken back" \
    [ "$(mock_count 'gui input --prompt *forget the skips*')" -eq 0 ]

# ── Case 20c: nothing to go back to on the first question ───────────────────
mock_reset
setup_top
walk_children "" dir dir-top dir-top
walk_path dir-top /top
walk_counts 1 1 1
mock_respond 'log head' '7'
mock_prompt '/top'
mock_input u y
out=$(bash "$SCRIPT" music 2>&1); code=$?
assert "back at the start: exits 0" [ "$code" -eq 0 ]
assert_contains "back at the start: says there is nothing to go back to" "$out" "nothing to go back to"
assert "back at the start: nothing was rolled back" [ "$(mock_count 'log rollback*')" -eq 0 ]
assert "back at the start: the first entry is asked twice" [ "$(asked /top)" -eq 2 ]

# ── Case 21: a failing `mf tag` is reported, not a silent "stopped" ─────────
mock_reset
setup_top
walk_children "" dir dir-top
walk_path dir-top /top
walk_counts 1 1
mock_respond 'tag -i dir-top add music' '@exit:1'
mock_prompt '/top'
mock_input y
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert "tag failure: non-zero exit" [ "$code" -ne 0 ]
assert_contains "tag failure: names the failing step" "$err$out" "/top"
assert "tag failure: does not claim a clean stop" \
    [ "$(printf '%s' "$out" | grep -c '^stopped\.$')" -eq 0 ]

# ── Case 22: an unanswerable question says why instead of vanishing ────────
mock_reset
setup_top
walk_children "" dir dir-top
walk_path dir-top /top
walk_counts 1 1
mock_prompt '/top'
mock_input @fail
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert_contains "unanswerable: explains itself" "$err" "could not be answered"
assert "unanswerable: no tag op" [ "$(mock_count 'tag -i *')" -eq 0 ]

# ── Case 23: a metarecord with no resolvable path is not in the walk ────────
# A deleted file keeps its metarecord with `mfr_path = Nothing` (spec-file-
# tracking), so it still answers `mfr_type = "file"` while resolving to no path
# at all. Every query of the walk — the steps and the counter alike — carries
# `mfr_path IS PRESENT`, so it is in none of them.
mock_reset
setup_top
walk_children "" file file-a
walk_path file-a /top/a.txt
walk_counts 1 1
mock_prompt '/top'
mock_input y
bash "$SCRIPT" music >/dev/null; code=$?
assert "orphan: exits 0" [ "$code" -eq 0 ]
unfiltered=$(mock_calls_matching 'metarecord -q * get --sort*' | grep -vc 'mfr_path IS PRESENT' || true)
assert "orphan: every step query requires a resolvable path" [ "$unfiltered" -eq 0 ]
assert "orphan: so does the counter" \
    [ "$(mock_calls_matching 'metarecord -q *NOT (tag -> * get --count' | grep -vc 'mfr_path IS PRESENT' || true)" -eq 0 ]

# ── Case 24: a tracked symlink is a leaf, not a hole in the walk ────────────
# `mfr_type` is file / dir / **symlink** (fs_meta.rs, which never dereferences
# one). A leaf query spelled `mfr_type = "file"` would leave a symlink reachable
# by the descent and by nothing else: the walk would move to its parent, find no
# child to ask, and descend again — for ever, with no key pressed.
mock_reset
setup_top
walk_children "" leaf link-a
walk_path link-a /top/a.lnk
walk_counts 1 1
mock_prompt '/top'
mock_input y
out=$(bash "$SCRIPT" music); code=$?
assert "symlink: exits 0" [ "$code" -eq 0 ]
assert "symlink: the leaf query asks for everything that is not a folder" \
    [ "$(mock_count '*AND NOT mfr_type = "dir" AND*')" -ge 1 ]
assert "symlink: it is asked like a file" [ "$(asked /top/a.lnk)" -eq 1 ]
assert "symlink: a leaf is never offered the mixed answer" \
    [ "$(mock_count "gui input --prompt '/top/a.lnk'*mixed*")" -eq 0 ]
assert "symlink: and it is tagged" [ "$(mock_count 'tag -i link-a add music')" -eq 1 ]

# ── Case 25: the descent must move, or the run says so instead of spinning ───
# Nothing in the descent waits for a key, so a descent that does not move is a
# silent infinite loop. It takes the two queries disagreeing — an open
# descendant of P whose parent is P, which the step query asks for — so this is
# a guard, not a behaviour; what matters is that it ends the run.
mock_reset
setup_top
walk_descendants "" ghost
walk_path ghost /ghost.txt
walk_counts 1
mock_prompt '/top'
out=$(bash "$SCRIPT" music 2>&1); code=$?
assert "stuck descent: non-zero exit" [ "$code" -ne 0 ]
assert_contains "stuck descent: names the entry it cannot reach" "$out" "/ghost.txt"
assert "stuck descent: nothing was asked" [ "$(mock_count 'gui input*')" -eq 0 ]

# ── Case 26: an empty scope is refused before the first question ────────────
mock_reset
setup_top
walk_counts 0 0          # nothing open, and nothing in the scope either
mock_prompt '/top'
out=$(bash "$SCRIPT" music 2>&1); code=$?
assert "empty: non-zero exit" [ "$code" -ne 0 ]
assert_contains "empty: says the query matches nothing" "$out" "no tracked metarecord"
assert "empty: nothing is asked" [ "$(mock_count 'gui input*')" -eq 0 ]

# ── Case 27: a fully decided scope is a finished run, not an empty query ────
# What a resumed walk looks like once there is nothing left: the scope holds
# records, none of them is open. Reporting it as "the query matches no tracked
# metarecord" would send the user looking for a mistake in their query.
mock_reset
setup_top
walk_counts 0 4          # nothing open, but four records in the scope
mock_prompt '/top'
out=$(bash "$SCRIPT" music); code=$?
assert "decided: exits 0" [ "$code" -eq 0 ]
assert_contains "decided: says the query is fully decided" "$out" "nothing left to ask"
assert "decided: nothing is asked" [ "$(mock_count 'gui input*')" -eq 0 ]
assert "decided: the scope is counted only when the open set is empty" \
    [ "$(mock_count 'metarecord -q *mfr_path IS PRESENT get --count')" -eq 2 ]

# ── Case 28: an entry whose file cannot be resolved CLEARS the preview ──────
# The preview is set from `mf path <uuid>`, which can come back empty — a
# record with no resolvable `mfr_path`, a repository unloaded mid-run. Showing
# nothing was a no-op, so the PREVIOUS entry's file stayed on screen while the
# question asked about this one: the wrong file, presented as the subject.
mock_reset
setup_top
walk_children "" file file-a file-b
walk_path file-a /a.txt
walk_path file-b /b.txt
mock_respond 'path file-a' '/abs/a.txt'   # the first entry previews fine
# file-b has no `path` row: `mf path file-b` answers nothing, as the real one
# does for a record it cannot resolve.
walk_counts 2 2 1
mock_prompt '/top'
mock_input y y
bash "$SCRIPT" music >/dev/null; code=$?
assert "unresolved preview: exits 0" [ "$code" -eq 0 ]
assert "unresolved preview: the resolvable entry is shown" \
    [ "$(mock_count 'gui view left file --path /abs/a.txt')" -eq 1 ]
assert "unresolved preview: the unresolvable one clears the panel" \
    [ "$(mock_count 'gui view left file --path ')" -eq 1 ]
assert "unresolved preview: both questions were asked" \
    [ "$(mock_count 'gui input*')" -eq 2 ]

assert_summary
