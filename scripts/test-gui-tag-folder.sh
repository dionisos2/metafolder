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
mock_input escape
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

assert_summary
