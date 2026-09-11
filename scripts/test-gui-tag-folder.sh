#!/usr/bin/env bash
# Tests for scripts/shipped/gui-tag-folder.sh — bulk-apply one tag over a set of
# metarecords with a yes/no/mixed walk. The scripted `mf` shim
# (scripts/lib/mf-mock.sh) stands in for the daemon + GUI.
#
# The scope is a QUERY (spec-gui "Finder"): given on the command line, taken
# from what the GUI shows (`mf gui query`), or built from the folder completion
# as `mfr_path =>* "<folder>"`. The walk reads the whole scope in a fixed number
# of round-trips instead of one listing per folder, so the tests assert those
# calls and the order the entries are asked in.
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

# A scope of one folder /top holding: a.txt, sub/, b.txt (plus /top itself).
# `SC` is the scope query the script builds from the folder completion.
SC='mfr_path =>* "/top"'
setup_top() {
    setup_gui
    mock_respond 'gui query'                             '@exit:1'   # nothing published
    mock_respond 'metarecord -q mfr_type = "dir" get*'   '/top'      # folder completion
}

# Declare the scope's contents: the two ordered-uuid calls and the two
# uuid<TAB>path calls the walk makes (dirs and files, each once).
scope_holds() { # <dir rows...> -- <file rows...>   rows are "uuid<TAB>path"
    local dirs=() files=() seen=0 row
    for row in "$@"; do
        if [ "$row" = "--" ]; then seen=1; continue; fi
        if [ "$seen" = 0 ]; then dirs+=("$row"); else files+=("$row"); fi
    done
    local dir_uuids="" file_uuids="" dir_rows="" file_rows=""
    for row in ${dirs+"${dirs[@]}"}; do
        dir_uuids+="${row%%	*}"$'\n'; dir_rows+="$row"$'\n'
    done
    for row in ${files+"${files[@]}"}; do
        file_uuids+="${row%%	*}"$'\n'; file_rows+="$row"$'\n'
    done
    mock_respond "metarecord -q ($SC) AND mfr_type = \"dir\" get --sort order_dir --sort mfr_path" "${dir_uuids%$'\n'}"
    mock_respond "metarecord -q ($SC) AND mfr_type = \"file\" get --sort order_file --sort mfr_path" "${file_uuids%$'\n'}"
    mock_respond "metarecord -q ($SC) AND mfr_type = \"dir\" get --resolve-tree mfr_path --tsv" "${dir_rows%$'\n'}"
    mock_respond "metarecord -q ($SC) AND mfr_type = \"file\" get --resolve-tree mfr_path --tsv" "${file_rows%$'\n'}"
}

# How many times the walk asked about one entry (its question message).
asked() { mock_count "gui message '$1' has tag*"; }

# ── Case 1: a folder answered "yes" — the node and its subtree, scoped ───────
mock_reset
setup_top
scope_holds "dir-top	/top" -- 
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

# ── Case 2: "no" denies the same two scopes ─────────────────────────────────
mock_reset
setup_top
scope_holds "dir-top	/top" --
mock_prompt '/top'
mock_input n
bash "$SCRIPT" music >/dev/null; code=$?
assert "no: exits 0" [ "$code" -eq 0 ]
assert "no: denies the node" [ "$(mock_count 'tag -i dir-top deny music')" -eq 1 ]
assert "no: denies the scoped subtree" \
    [ "$(mock_count "tag -q ($SC) AND mfr_path ->* \"/top\" deny music")" -eq 1 ]

# ── Case 3: "mixed" descends; a "yes" child folder prunes its own subtree ────
mock_reset
setup_top
scope_holds "dir-top	/top" "dir-sub	/top/sub" -- "file-a	/top/a.txt" "file-b	/top/b.txt" "file-d	/top/sub/deep.txt"
mock_prompt '/top'
#   top=m  sub=y (whole subtree)  a.txt=y  b.txt=n
mock_input m y y n
out=$(bash "$SCRIPT" music); code=$?
assert "mixed: exits 0" [ "$code" -eq 0 ]
assert "mixed: marks the parent mixed" [ "$(mock_count 'tag -i dir-top mixed music')" -eq 1 ]
assert "mixed: the child folder is asked before the files" [ "$(asked /top/sub)" -eq 1 ]
assert "mixed: child file y -> add" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]
assert "mixed: child file n -> deny" [ "$(mock_count 'tag -i file-b deny music')" -eq 1 ]
assert "mixed: a file under the answered folder is never asked" [ "$(asked /top/sub/deep.txt)" -eq 0 ]
assert "mixed: nor tagged on its own" [ "$(mock_count 'tag -i file-d *')" -eq 0 ]

# ── Case 4: folders are asked before files, then by order_*, then by name ────
# The two `--sort order_* --sort mfr_path` calls are what orders each kind; the
# walk must keep that order and put the folders of a level first.
mock_reset
setup_top
scope_holds "dir-top	/top" "dir-sub	/top/sub" -- "file-z	/top/z.txt" "file-a	/top/a.txt"
mock_prompt '/top'
mock_input m s s s        # top=mixed, then skip each of the three
out=$(bash "$SCRIPT" music); code=$?
assert "order: exits 0" [ "$code" -eq 0 ]
order=$(mock_calls_matching "gui message '*' has tag*" | sed "s/.*message '\([^']*\)'.*/\1/")
assert_contains "order: the folder comes before the files" \
    "$(printf '%s' "$order" | tr '\n' ' ')" "/top /top/sub /top/z.txt /top/a.txt"

# ── Case 5: an explicit query argument is the scope, no folder prompt ────────
mock_reset
setup_gui
Q='rating > 3'
mock_respond "metarecord -q ($Q) AND mfr_type = \"dir\" get --sort order_dir --sort mfr_path"  ''
mock_respond "metarecord -q ($Q) AND mfr_type = \"file\" get --sort order_file --sort mfr_path" 'file-a'
mock_respond "metarecord -q ($Q) AND mfr_type = \"dir\" get --resolve-tree mfr_path --tsv"  ''
mock_respond "metarecord -q ($Q) AND mfr_type = \"file\" get --resolve-tree mfr_path --tsv" "file-a	/x/a.txt"
mock_input y
out=$(bash "$SCRIPT" music "$Q"); code=$?
assert "query arg: exits 0" [ "$code" -eq 0 ]
assert "query arg: no folder completion is offered" \
    [ "$(mock_count 'gui prompt*')" -eq 0 ]
assert "query arg: the matching file is asked" [ "$(asked /x/a.txt)" -eq 1 ]
assert "query arg: and tagged" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 6: with no argument the scope is what the GUI shows ────────────────
mock_reset
setup_gui
G='mfr_type = "file" AND rating > 3'
mock_respond 'gui query' "$G"
mock_respond "metarecord -q ($G) AND mfr_type = \"dir\" get --sort order_dir --sort mfr_path"  ''
mock_respond "metarecord -q ($G) AND mfr_type = \"file\" get --sort order_file --sort mfr_path" 'file-a'
mock_respond "metarecord -q ($G) AND mfr_type = \"dir\" get --resolve-tree mfr_path --tsv"  ''
mock_respond "metarecord -q ($G) AND mfr_type = \"file\" get --resolve-tree mfr_path --tsv" "file-a	/x/a.txt"
mock_input y
out=$(bash "$SCRIPT" music); code=$?
assert "gui query: exits 0" [ "$code" -eq 0 ]
assert "gui query: asks the GUI what it shows" [ "$(mock_count 'gui query')" -ge 1 ]
assert "gui query: no folder prompt when one is published" [ "$(mock_count 'gui prompt*')" -eq 0 ]
assert "gui query: the record is tagged" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 7: an empty scope (the GUI shows everything) is not narrowed ────────
mock_reset
setup_gui
mock_respond 'gui query' ''
mock_respond 'metarecord -q mfr_type = "dir" get --sort order_dir --sort mfr_path'   ''
mock_respond 'metarecord -q mfr_type = "file" get --sort order_file --sort mfr_path' 'file-a'
mock_respond 'metarecord -q mfr_type = "dir" get --resolve-tree mfr_path --tsv'   ''
mock_respond 'metarecord -q mfr_type = "file" get --resolve-tree mfr_path --tsv' "file-a	/x/a.txt"
mock_input y
bash "$SCRIPT" music >/dev/null; code=$?
assert "all: exits 0" [ "$code" -eq 0 ]
assert "all: no empty parentheses in the query" [ "$(mock_count 'metarecord -q () AND*')" -eq 0 ]
assert "all: the record is tagged" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 8: the ROOT folder uses the empty-string tree form ──────────────────
mock_reset
setup_gui
mock_respond 'gui query'                            '@exit:1'
mock_respond 'metarecord -q mfr_type = "dir" get*'  '/'
RSC='mfr_path =>* ""'
mock_respond "metarecord -q ($RSC) AND mfr_type = \"dir\" get --sort order_dir --sort mfr_path"  'dir-root'
mock_respond "metarecord -q ($RSC) AND mfr_type = \"file\" get --sort order_file --sort mfr_path" ''
mock_respond "metarecord -q ($RSC) AND mfr_type = \"dir\" get --resolve-tree mfr_path --tsv"  "dir-root	"
mock_respond "metarecord -q ($RSC) AND mfr_type = \"file\" get --resolve-tree mfr_path --tsv" ''
mock_prompt '/'
mock_input y
bash "$SCRIPT" roottag >/dev/null; code=$?
assert "root: exits 0" [ "$code" -eq 0 ]
assert "root: the scope uses the empty-string form" \
    [ "$(mock_count "metarecord -q ($RSC) AND mfr_type = \"dir\"*")" -ge 1 ]
assert "root: never uses the broken \"/\" tree-query form" \
    [ "$(mock_count 'metarecord -q (mfr_path =>* "/") AND*')" -eq 0 ]

# ── Case 9: skip leaves the whole subtree alone ─────────────────────────────
mock_reset
setup_top
scope_holds "dir-top	/top" "dir-sub	/top/sub" -- "file-d	/top/sub/deep.txt" "file-b	/top/b.txt"
mock_prompt '/top'
#   top=m  sub=s (whole subtree skipped)  b.txt=y
mock_input m s y
out=$(bash "$SCRIPT" music); code=$?
assert "skip: exits 0" [ "$code" -eq 0 ]
assert "skip: no tag op on the skipped folder" [ "$(mock_count 'tag -i dir-sub *')" -eq 0 ]
assert "skip: nothing under it is asked" [ "$(asked /top/sub/deep.txt)" -eq 0 ]
assert "skip: the walk continues with the next entry" [ "$(mock_count 'tag -i file-b add music')" -eq 1 ]
assert_contains "skip: the summary counts it" "$out" "1 skipped"

# ── Case 10: quit stops the walk ────────────────────────────────────────────
mock_reset
setup_top
scope_holds "dir-top	/top" -- "file-a	/top/a.txt"
mock_prompt '/top'
mock_input q
out=$(bash "$SCRIPT" music); code=$?
assert "stop: exits 0" [ "$code" -eq 0 ]
assert_contains "stop: reports stopped" "$out" stopped
assert "stop: no tag op at all" [ "$(mock_count 'tag *')" -eq 0 ]

# ── Case 11: a tag with a double quote is rejected ──────────────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" 'bad"tag' 2>&1 >/dev/null); code=$?
assert "quote: non-zero exit" [ "$code" -ne 0 ]
assert_contains "quote: explains the rule" "$err" 'double quote'

# ── Case 12: too many positional arguments is a usage error ─────────────────
mock_reset
setup_top
err=$(bash "$SCRIPT" a b c 2>&1 >/dev/null); code=$?
assert "usage: non-zero exit on 3 args" [ "$code" -ne 0 ]
assert_contains "usage: prints a usage line" "$err" usage

# ── Case 13: an already-decided entry is not asked again ────────────────────
mock_reset
setup_top
scope_holds "dir-top	/top" -- "file-a	/top/a.txt" "file-b	/top/b.txt"
mock_respond 'metarecord -i dir-top field get mixed_tag*' 'music'
mock_respond 'metarecord -i file-a field get tag*'        'music'
mock_prompt '/top'
mock_input y                     # answers file-b, the only question left
out=$(bash "$SCRIPT" music); code=$?
assert "resume: exits 0" [ "$code" -eq 0 ]
assert "resume: the already-mixed folder is not asked" [ "$(asked /top)" -eq 0 ]
assert "resume: no redundant mixed op on it" [ "$(mock_count 'tag -i dir-top mixed music')" -eq 0 ]
assert "resume: the tagged child is not asked" [ "$(asked /top/a.txt)" -eq 0 ]
assert "resume: nor re-tagged" [ "$(mock_count 'tag -i file-a *')" -eq 0 ]
assert "resume: the undecided child is asked" [ "$(asked /top/b.txt)" -eq 1 ]
assert "resume: and answered" [ "$(mock_count 'tag -i file-b add music')" -eq 1 ]
assert_contains "resume: the summary counts the decided entries" "$out" "2 already"

# ── Case 14: a decision that subsumes the asked tag counts as decided ───────
mock_reset
setup_top
scope_holds "dir-top	/top" -- "file-a	/top/a.txt" "file-b	/top/b.txt" "file-c	/top/c.txt"
mock_respond 'metarecord -i dir-top field get mixed_tag*'   'music'
mock_respond 'metarecord -i file-a field get tag*'          'music/jazz'
mock_respond 'metarecord -i file-b field get negative_tag*' 'music'
mock_prompt '/top'
mock_input y
bash "$SCRIPT" music >/dev/null; code=$?
assert "subsume: a more specific positive answers the question" [ "$(asked /top/a.txt)" -eq 0 ]
assert "subsume: an exact negative answers it too" [ "$(asked /top/b.txt)" -eq 0 ]
assert "subsume: the undecided child is still asked" [ "$(asked /top/c.txt)" -eq 1 ]

# ── Case 15: --redo asks everything again, decided or not ───────────────────
mock_reset
setup_top
scope_holds "dir-top	/top" -- "file-a	/top/a.txt"
mock_respond 'metarecord -i dir-top field get mixed_tag*' 'music'
mock_respond 'metarecord -i file-a field get tag*'        'music'
mock_prompt '/top'
mock_input m y
bash "$SCRIPT" --redo music >/dev/null; code=$?
assert "redo: exits 0" [ "$code" -eq 0 ]
assert "redo: the decided folder is asked again" [ "$(asked /top)" -eq 1 ]
assert "redo: the decided child is asked again" [ "$(asked /top/a.txt)" -eq 1 ]
assert "redo: and answered" [ "$(mock_count 'tag -i file-a add music')" -eq 1 ]

# ── Case 16: the total is known up front, so the bar is exact ───────────────
# The scope is read in one pass, so — unlike the old per-folder walk — the
# number of entries is known before the first question.
mock_reset
setup_top
scope_holds "dir-top	/top" -- "file-a	/top/a.txt" "file-b	/top/b.txt"
mock_prompt '/top'
mock_input m y y
bash "$SCRIPT" music >/dev/null; code=$?
assert "count: the first entry is 1 of 3" \
    [ "$(mock_count 'gui progress --done 1 --total 3 --phase /top')" -eq 1 ]
assert "count: the last is 3 of 3" \
    [ "$(mock_count 'gui progress --done 3 --total 3 --phase /top/b.txt')" -eq 1 ]
assert "count: the question says how many are left" [ "$(mock_count "gui message*2 left*")" -eq 1 ]

# ── Case 17: a failing `mf tag` is reported, not a silent "stopped" ─────────
mock_reset
setup_top
scope_holds "dir-top	/top" --
mock_respond 'tag -i dir-top add music' '@exit:1'
mock_prompt '/top'
mock_input y
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert "tag failure: non-zero exit" [ "$code" -ne 0 ]
assert_contains "tag failure: names the failing step" "$err$out" "/top"
assert "tag failure: does not claim a clean stop" \
    [ "$(printf '%s' "$out" | grep -c '^stopped\.$')" -eq 0 ]

# ── Case 18: an unanswerable question says why instead of vanishing ────────
mock_reset
setup_top
scope_holds "dir-top	/top" --
mock_prompt '/top'
mock_input @fail
out=$(bash "$SCRIPT" music 2>"$MF_MOCK_DIR/err"); code=$?
err=$(cat "$MF_MOCK_DIR/err")
assert_contains "unanswerable: explains itself" "$err" "could not be answered"
assert "unanswerable: no tag op" [ "$(mock_count 'tag -i *')" -eq 0 ]

assert_summary
