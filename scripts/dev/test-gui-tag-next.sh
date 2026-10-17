#!/usr/bin/env bash
# Tests for scripts/gui-tag-next.sh — the pure "next question" selector of the
# hierarchical-tag classification flow. No daemon/GUI involved: fixtures are
# plain files, we compare the selector's stdout/exit code. Mirrors the harness
# style of scripts/test-prune-target.sh.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
NEXT="$HERE/../shipped/gui-tag-next.sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

pass=0
fail=0

# run_case <name> <universe> <pos> <neg> <expected-stdout> <expected-exit> [<fn> <arg>...]
# universe/pos/neg are passed as literal multi-line strings (may be empty).
# Without <fn>, the selector's own question (`ask<TAB>P` / `pick<TAB>P`); with
# it, that function of the sourced script, given the three files then <arg>s.
run_case() {
    local name=$1 universe=$2 pos=$3 neg=$4 want_out=$5 want_code=$6
    shift 6
    printf '%s' "$universe" >"$tmp/u"
    printf '%s' "$pos"      >"$tmp/p"
    printf '%s' "$neg"      >"$tmp/n"

    local got_out got_code
    set +e
    if [ $# -eq 0 ]; then
        got_out=$(bash "$NEXT" "$tmp/u" "$tmp/p" "$tmp/n")
    else
        got_out=$(bash -c 'source "$1"; shift; fn=$1; shift; "$fn" "$@"' _ \
            "$NEXT" "$1" "$tmp/u" "$tmp/p" "$tmp/n" "${@:2}")
    fi
    got_code=$?
    set -e

    if [ "$got_out" = "$want_out" ] && [ "$got_code" -eq "$want_code" ]; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "$name"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n     want out=%q code=%d\n     got  out=%q code=%d\n' \
            "$name" "$want_out" "$want_code" "$got_out" "$got_code"
    fi
}

# Universe: one tag path per line, optionally <TAB> its uuid (what `mf tag
# list` prints); without a uuid each path is a tag of its own.
U_BASE=$(printf 'musique\nadministratif\nmusique/jazz\nmusique/rock\n')

# 1. Top level first: nothing answered yet -> shallowest, in universe order.
run_case "top-level-first" "$U_BASE" "" "" $'ask\tmusique' 0

# 2. Already-positive top tag is not re-asked; but its child becomes askable,
#    while the other unanswered top-level tag is shallower -> administratif first.
run_case "skip-positive-top" "$U_BASE" $'musique\n' "" $'ask\tadministratif' 0

# 3. A generic negative blocks the whole subtree: musique in NEG -> never
#    musique/jazz; only administratif remains.
run_case "negative-blocks-subtree" "$U_BASE" "" $'musique\n' $'ask\tadministratif' 0

# 4. Descent: once every top-level is answered and musique is positive, its
#    child is proposed.
run_case "descend-to-child" "$U_BASE" $'musique\n' $'administratif\n' $'ask\tmusique/jazz' 0

# 5. Multi-genre: musique specialised to jazz (parent removed), the sibling
#    rock is still offered — siblings never exclude each other.
run_case "multi-genre-sibling" "$U_BASE" $'musique/jazz\n' $'administratif\n' \
    $'ask\tmusique/rock' 0

# 6. Columns after the uuid are ignored: the sibling is still offered.
U_EXTRA=$(printf 'musique\tA\tx\nmusique/jazz\tB\ty\nmusique/rock\tC\tz\n')
run_case "extra-columns-ignored" "$U_EXTRA" $'musique/jazz\n' "" $'ask\tmusique/rock' 0

# 7. A tag of several parents (doc "Hierarchy references") is one line per
#    path, with its uuid. A negative above one of its paths rules out the tag
#    under every path: not an event, so not a marathon, even as a race.
U_HIER=$(printf 'sport\tU1\nsport/courir\tU2\nsport/courir/maraton\tU3\névènement\tU4\névènement/maraton\tU3\n')
run_case "negative-blocks-every-path" "$U_HIER" $'sport/courir\n' $'évènement\n' "" 1

# 8. Without that negative, the marathon is asked about, once, by the path
#    under the accepted branch.
run_case "alias-asked-once" "$U_HIER" $'sport/courir\n' "" $'ask\tévènement' 0

# ── Flags (doc "Tag flags"): columns always, partition, sparse after the uuid ──

# A partition's children are not asked one by one: once the parent is had, its
# children are picked by completion.
U_PART=$(printf 'musique\tM\t0\t1\t0\nmusique/jazz\tJ\nmusique/rock\tR\nmusique/rock/punk\tP\nadmin\tA\n')
run_case "partition-parent-asked-yes-no" "$U_PART" "" $'admin\n' $'ask\tmusique' 0
run_case "partition-picks-its-children" "$U_PART" $'musique\n' $'admin\n' $'pick\tmusique' 0
run_case "partition-candidates" "$U_PART" $'musique\n' $'admin\n' \
    $'musique/jazz\nmusique/rock' 0 gui_tag_children musique

# A child already had settles a partition: no pick, its own children next.
run_case "partition-settled-by-a-child" "$U_PART" $'musique/rock\n' $'admin\n' \
    $'ask\tmusique/rock/punk' 0
# ... even through a descendant of that child.
run_case "partition-settled-by-a-grandchild" "$U_PART" $'musique/rock/punk\n' $'admin\n' "" 1

# Negative children are no candidates; all of them negative, no pick.
run_case "candidates-skip-negatives" "$U_PART" $'musique\n' $'admin\nmusique/jazz\n' \
    'musique/rock' 0 gui_tag_children musique
run_case "no-candidate-no-pick" "$U_PART" $'musique\n' $'admin\nmusique/jazz\nmusique/rock\n' "" 1

# A sparse parent is picked from again and again, while candidates remain.
U_SPARSE=$(printf 'musique\tM\t0\t0\t1\nmusique/jazz\tJ\nmusique/jazz/bebop\tB\nmusique/rock\tR\nmusique/pop\tO\n')
run_case "sparse-picks-again" "$U_SPARSE" $'musique/jazz\n' "" $'pick\tmusique' 0
run_case "sparse-candidates-left" "$U_SPARSE" $'musique/jazz\n' "" \
    $'musique/rock\nmusique/pop' 0 gui_tag_children musique
run_case "sparse-before-the-deeper-questions" "$U_SPARSE" $'musique/jazz\n' $'musique/pop\nmusique/rock\n' \
    $'ask\tmusique/jazz/bebop' 0

# A tag of several parents is one candidate, named under the parent asked.
U_MULTI=$(printf 'sport\tS\t0\t1\t0\nsport/maraton\tX\névènement\tE\névènement/maraton\tX\nsport/sprint\tY\n')
run_case "multi-parent-candidate-once" "$U_MULTI" $'sport\n' "" \
    $'sport/maraton\nsport/sprint' 0 gui_tag_children sport

# always: added unless it, or something above it, is denied, or it is had —
# itself or through a descendant; nor when an always tag below it will be, which
# says it already (photo/nb is photo).
U_ALWAYS=$(printf 'photo\tF\t1\nphoto/nb\tN\t1\nperso\tP\t0\nperso/famille\tA\t1\nvideo\tV\t1\n')
run_case "always-tags" "$U_ALWAYS" "" "" $'photo/nb\nperso/famille\nvideo' 0 gui_tag_always
run_case "always-respects-answers" "$U_ALWAYS" $'photo/nb\n' $'perso\nvideo\n' "" 1 gui_tag_always

# 9. Nothing left to ask -> exit 1, empty output.
run_case "exhausted" "$U_BASE" $'musique/jazz\nmusique/rock\n' $'administratif\n' "" 1

echo "----"
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
