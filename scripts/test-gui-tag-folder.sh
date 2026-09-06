#!/usr/bin/env bash
# Tests for scripts/shipped/gui-tag-folder.sh — bulk-apply one tag over a folder
# subtree with a yes/no/mixed walk. The scripted `mf` shim (scripts/lib/mf-mock.sh)
# stands in for the daemon + GUI. The tag is passed on the command line (so no
# tag prompt); the folder is chosen through the GUI folder completion. We assert
# the exact `mf tag …` commands for each answer and the breadth-first descent
# into a "mixed" folder. No daemon/GUI.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/shipped/gui-tag-folder.sh"
# shellcheck source=lib/mf-mock.sh
source "$HERE/lib/mf-mock.sh"
mock_init
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

# Resolve the prompted "/top" folder to a uuid + its tree-path/abs.
setup_top() {
    mock_respond 'gui repo'                                       'repo-1'
    mock_respond 'metarecord -q mfr_type = "dir" get*'            '/top'   # completion (drained)
    mock_respond 'metarecord -q mfr_path = "/top" get'           'dir-top'
    mock_respond 'gui layout left'                               'saved-left'
    mock_respond 'gui layout right'                              'saved-right'
    mock_respond 'gui workspace new*'                            'ws-1'
    mock_respond 'path --relative dir-top'                       '/top'
    # NB: no catch-all `path *` / `path --relative *` rows — first-match-wins,
    # so a catch-all added here would shadow the per-record rows the cases add
    # below. Every needed path is spelled out; a bare `mf path U` that has no
    # row returns empty (the scripts only use it for the best-effort preview).
}

# ── Case 1: top folder = "yes" — one tag on the node + one on the subtree ────
mock_reset
setup_top
mock_prompt '/top'          # the folder completion answer
mock_input y                # the top folder HAS the tag
out=$(bash "$SCRIPT" music); code=$?
assert "yes: exits 0" [ "$code" -eq 0 ]
assert "yes: tags the node" [ "$(mock_count 'tag -i dir-top add music')" -eq 1 ]
assert "yes: tags the whole subtree" [ "$(mock_count 'tag -q mfr_path ->* "/top" add music')" -eq 1 ]
assert "yes: no descent" [ "$(mock_count 'tag -i * mixed *')" -eq 0 ]
assert_contains "yes: reports done" "$out" "done tagging 'music' under /top"

# ── Case 2: top folder = "no" — deny on the node + subtree ───────────────────
mock_reset
setup_top
mock_prompt '/top'
mock_input n
bash "$SCRIPT" music >/dev/null; code=$?
assert "no: exits 0" [ "$code" -eq 0 ]
assert "no: denies the node" [ "$(mock_count 'tag -i dir-top deny music')" -eq 1 ]
assert "no: denies the subtree" [ "$(mock_count 'tag -q mfr_path ->* "/top" deny music')" -eq 1 ]

# ── Case 3: top folder = "mixed" — descend into its direct children ──────────
mock_reset
setup_top
# The three direct children of /top and their per-record reads.
mock_respond 'metarecord -q mfr_path -> "/top" get'   $'file-a\ndir-sub\nfile-b'
mock_respond 'metarecord -i file-a field get mfr_type' 'file'
mock_respond 'metarecord -i dir-sub field get mfr_type' 'dir'
mock_respond 'metarecord -i file-b field get mfr_type' 'file'
mock_respond 'path --relative file-a'                  '/top/a.txt'
mock_respond 'path --relative dir-sub'                 '/top/sub'
mock_respond 'path --relative file-b'                  '/top/b.txt'
mock_prompt '/top'
#   top=m  a.txt=y  sub=y  b.txt=n
mock_input m y y n
bash "$SCRIPT" music >/dev/null; code=$?
assert "mixed: exits 0" [ "$code" -eq 0 ]
assert "mixed: marks the parent mixed" [ "$(mock_count 'tag -i dir-top mixed music')" -eq 1 ]
assert "mixed: child file y -> add" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]
assert "mixed: child dir y -> add node" [ "$(mock_count 'tag -i dir-sub add music')" -eq 1 ]
assert "mixed: child dir y -> add subtree" [ "$(mock_count 'tag -q mfr_path ->* "/top/sub" add music')" -eq 1 ]
assert "mixed: child file n -> deny" [ "$(mock_count 'tag -i file-b deny music')" -eq 1 ]
# A "yes" child dir is applied whole, NOT enqueued for a further descent.
assert "mixed: 'yes' child dir is not descended into" \
    [ "$(mock_count 'metarecord -q mfr_path -> "/top/sub" get')" -eq 0 ]

# ── Case 4: Escape on the top folder stops with no tag op ────────────────────
mock_reset
setup_top
mock_prompt '/top'
mock_input q
out=$(bash "$SCRIPT" music); code=$?
assert "stop: exits 0" [ "$code" -eq 0 ]
assert_contains "stop: reports stopped" "$out" stopped
assert "stop: no tag op at all" [ "$(mock_count 'tag *')" -eq 0 ]

# ── Case 5: a tag with a double quote is rejected ────────────────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" 'bad"tag' 2>&1 >/dev/null); code=$?
assert "quote: non-zero exit" [ "$code" -ne 0 ]
assert_contains "quote: explains the rule" "$err" 'double quote'

# ── Case 6: too many positional arguments is a usage error ───────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" a b c 2>&1 >/dev/null); code=$?
assert "usage: non-zero exit on 3 args" [ "$code" -ne 0 ]
assert_contains "usage: prints a usage line" "$err" usage

# ── Case 7: the ROOT folder ("/") uses the empty-string query form ───────────
# The repository root's relative path is "/", but the mfr_path tree queries want
# the root as "" — `mfr_path = "/"` / `->* "/"` match nothing on the real daemon
# (verified by test-scripts-integration.sh). The script must map "/" → "".
mock_reset
mock_respond 'gui repo'                              'repo-1'
mock_respond 'metarecord -q mfr_type = "dir" get*'   '/'          # completion (drained)
mock_respond 'metarecord -q mfr_path = "" get'       'dir-root'   # root resolves via ""
mock_respond 'gui layout left'                       'saved-left'
mock_respond 'gui layout right'                      'saved-right'
mock_respond 'gui workspace new*'                    'ws-1'
mock_respond 'path --relative dir-root'              '/'
mock_prompt '/'
mock_input y
bash "$SCRIPT" roottag >/dev/null; code=$?
assert "root: exits 0" [ "$code" -eq 0 ]
assert "root: resolves the folder via the empty-string form" \
    [ "$(mock_count 'metarecord -q mfr_path = "" get')" -eq 1 ]
assert "root: subtree tagged with the empty-string form" \
    [ "$(mock_count 'tag -q mfr_path ->* "" add roottag')" -eq 1 ]
assert "root: never uses the broken \"/\" tree-query form" \
    [ "$(mock_count 'tag -q mfr_path ->* "/"*')" -eq 0 ]

# ── Case 8: the arrow keys answer too (→ yes, ← no, ↑ mixed, ↓ skip) ────────
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path -> "/top" get'    $'file-a\nfile-b\ndir-sub'
mock_respond 'metarecord -i file-a field get mfr_type' 'file'
mock_respond 'metarecord -i file-b field get mfr_type' 'file'
mock_respond 'metarecord -i dir-sub field get mfr_type' 'dir'
mock_respond 'path --relative file-a'                  '/top/a.txt'
mock_respond 'path --relative file-b'                  '/top/b.txt'
mock_respond 'path --relative dir-sub'                 '/top/sub'
mock_prompt '/top'
#   top=↑(mixed)  a.txt=→(yes)  b.txt=←(no)  sub=↓(skip)
mock_input up right left down
out=$(bash "$SCRIPT" music); code=$?
assert "arrows: exits 0" [ "$code" -eq 0 ]
assert "arrows: up marks the parent mixed" [ "$(mock_count 'tag -i dir-top mixed music')" -eq 1 ]
assert "arrows: right adds" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]
assert "arrows: left denies" [ "$(mock_count 'tag -i file-b deny music')" -eq 1 ]
assert "arrows: down skips (no tag op on the dir)" [ "$(mock_count 'tag -i dir-sub *')" -eq 0 ]
assert "arrows: the awaited key list offers the arrows" \
    [ "$(mock_count 'gui input*right*')" -ge 1 ]

# ── Case 9: skip on a folder leaves its whole subtree alone ──────────────────
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path -> "/top" get'     $'dir-sub\nfile-b'
mock_respond 'metarecord -i dir-sub field get mfr_type' 'dir'
mock_respond 'metarecord -i file-b field get mfr_type'  'file'
mock_respond 'path --relative dir-sub'                  '/top/sub'
mock_respond 'path --relative file-b'                   '/top/b.txt'
mock_prompt '/top'
#   top=m  sub=s (skipped whole)  b.txt=y
mock_input m s y
out=$(bash "$SCRIPT" music); code=$?
assert "skip: exits 0" [ "$code" -eq 0 ]
assert "skip: no tag op on the skipped folder" [ "$(mock_count 'tag -i dir-sub *')" -eq 0 ]
assert "skip: the skipped subtree is never descended into" \
    [ "$(mock_count 'metarecord -q mfr_path -> "/top/sub" get')" -eq 0 ]
assert "skip: the walk continues with the next sibling" \
    [ "$(mock_count 'tag -i file-b add music')" -eq 1 ]
assert_contains "skip: the summary counts it" "$out" "1 skipped"

# ── Case 10: a failing `mf tag` is reported, not a silent "stopped" ──────────
# `set -e` is disabled inside a tested command, so a failing tag call used to
# surface as the handler returning non-zero — indistinguishable from "the user
# pressed Escape". The run must abort loudly instead (spec-gui "Script session").
mock_reset
setup_top
mock_respond 'tag -i dir-top add music' '@exit:1'
mock_prompt '/top'
mock_input y
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert "tag failure: non-zero exit" [ "$code" -ne 0 ]
assert_contains "tag failure: names the failing step" "$err$out" "/top"
assert "tag failure: does not claim a clean stop" \
    [ "$(printf '%s' "$out" | grep -c '^stopped\.$')" -eq 0 ]

# ── Case 11: an unanswerable question says why instead of vanishing ─────────
# A closed GUI (or a 409 from a leaked wait) makes `mf gui input` fail. Treating
# that as a silent Escape is what made a run look like it "just stopped".
mock_reset
setup_top
mock_prompt '/top'
mock_input @fail
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert_contains "unanswerable: explains itself" "$err" "could not be answered"
assert "unanswerable: no tag op" [ "$(mock_count 'tag -i *')" -eq 0 ]

# How many times the walk asked about one entry (its question message).
asked() { mock_count "gui message '$1' has tag*"; }

# ── Case 12: an entry that already carries the tag is not asked again ────────
# Re-running over a partly classified folder must resume, not re-ask: the top
# folder is already `mixed_tag = music`, so the walk descends into it without a
# question; the child that already has `tag = music` is left alone; only the
# undecided one is asked.
mock_reset
setup_top
mock_respond 'metarecord -i dir-top field get mixed_tag*'  'music'
mock_respond 'metarecord -q mfr_path -> "/top" get'        $'file-a\nfile-b'
mock_respond 'metarecord -i file-a field get mfr_type'     'file'
mock_respond 'metarecord -i file-b field get mfr_type'     'file'
mock_respond 'metarecord -i file-a field get tag*'         'music'
mock_respond 'path --relative file-a'                      '/top/a.txt'
mock_respond 'path --relative file-b'                      '/top/b.txt'
mock_prompt '/top'
mock_input y                     # answers file-b, the only question left
out=$(bash "$SCRIPT" music); code=$?
assert "resume: exits 0" [ "$code" -eq 0 ]
assert "resume: the already-mixed folder is not asked" [ "$(asked /top)" -eq 0 ]
assert "resume: no redundant mixed op on it" [ "$(mock_count 'tag -i dir-top mixed music')" -eq 0 ]
assert "resume: it is descended into all the same" \
    [ "$(mock_count 'metarecord -q mfr_path -> "/top" get')" -eq 1 ]
assert "resume: the tagged child is not asked" [ "$(asked /top/a.txt)" -eq 0 ]
assert "resume: but the progress bar still walks past it" \
    [ "$(mock_count 'gui progress*--phase /top/a.txt')" -eq 1 ]
assert "resume: nor re-tagged" [ "$(mock_count 'tag -i file-a *')" -eq 0 ]
assert "resume: the undecided child is asked" [ "$(asked /top/b.txt)" -eq 1 ]
assert "resume: and answered" [ "$(mock_count 'tag -i file-b add music')" -eq 1 ]
assert_contains "resume: the summary counts the decided entries" "$out" "2 already"

# ── Case 13: a decision that subsumes the asked tag counts as decided ────────
# `tag = music/jazz` implies music (a specific positive implies its ancestors),
# and `negative_tag = music` blocks music/jazz and its whole subtree.
mock_reset
setup_top
mock_respond 'metarecord -i dir-top field get mixed_tag*'  'music'
mock_respond 'metarecord -q mfr_path -> "/top" get'        $'file-a\nfile-b\nfile-c'
mock_respond 'metarecord -i file-* field get mfr_type'     'file'
mock_respond 'metarecord -i file-a field get tag*'         'music/jazz'
mock_respond 'metarecord -i file-b field get negative_tag*' 'music'
mock_respond 'path --relative file-a'                      '/top/a.txt'
mock_respond 'path --relative file-b'                      '/top/b.txt'
mock_respond 'path --relative file-c'                      '/top/c.txt'
mock_prompt '/top'
mock_input y
out=$(bash "$SCRIPT" music); code=$?
assert "subsume: exits 0" [ "$code" -eq 0 ]
assert "subsume: a more specific positive answers the question" [ "$(asked /top/a.txt)" -eq 0 ]
assert "subsume: an exact negative answers it too" [ "$(asked /top/b.txt)" -eq 0 ]
assert "subsume: the undecided child is still asked" [ "$(asked /top/c.txt)" -eq 1 ]

# ── Case 14: --redo asks everything again, decided or not ───────────────────
mock_reset
setup_top
mock_respond 'metarecord -i dir-top field get mixed_tag*'  'music'
mock_respond 'metarecord -q mfr_path -> "/top" get'        $'file-a'
mock_respond 'metarecord -i file-a field get mfr_type'     'file'
mock_respond 'metarecord -i file-a field get tag*'         'music'
mock_respond 'path --relative file-a'                      '/top/a.txt'
mock_prompt '/top'
mock_input m y                   # the top folder again, then the tagged child
out=$(bash "$SCRIPT" --redo music); code=$?
assert "redo: exits 0" [ "$code" -eq 0 ]
assert "redo: the decided folder is asked again" [ "$(asked /top)" -eq 1 ]
assert "redo: the decided child is asked again" [ "$(asked /top/a.txt)" -eq 1 ]
assert "redo: and answered" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 15: the remaining count is reported, on the bar and in the question ──
# The walk cannot know its total up front (a "mixed" answer adds children), so
# the count is what is *known* to be left: the entries still to visit in the
# folder being walked, plus one per mixed folder queued for a later descent.
mock_reset
setup_top
mock_respond 'metarecord -q mfr_path -> "/top" get'     $'file-a\ndir-sub\nfile-b'
mock_respond 'metarecord -i file-a field get mfr_type'  'file'
mock_respond 'metarecord -i dir-sub field get mfr_type' 'dir'
mock_respond 'metarecord -i file-b field get mfr_type'  'file'
mock_respond 'path --relative file-a'                   '/top/a.txt'
mock_respond 'path --relative dir-sub'                  '/top/sub'
mock_respond 'path --relative file-b'                   '/top/b.txt'
mock_prompt '/top'
#   top=m  a.txt=y  sub=m (queued)  b.txt=y ; then sub is opened and is empty
mock_input m y m y
out=$(bash "$SCRIPT" music); code=$?
assert "count: exits 0" [ "$code" -eq 0 ]
assert "count: the top folder is 1 of 1 known" \
    [ "$(mock_count 'gui progress --done 1 --total 1 --phase /top')" -eq 1 ]
assert "count: its three children raise the total" \
    [ "$(mock_count 'gui progress --done 2 --total 4 --phase /top/a.txt')" -eq 1 ]
assert "count: the question says how many are left here" \
    [ "$(mock_count "gui message*3 left*")" -eq 1 ]
assert "count: a queued mixed folder adds to the total" \
    [ "$(mock_count 'gui progress --done 4 --total 5 --phase /top/b.txt')" -eq 1 ]
assert "count: and is named in the question" \
    [ "$(mock_count "gui message*1 left, 1 folder to open*")" -eq 1 ]

assert_summary
