#!/usr/bin/env bash
# Tests for scripts/gui-tag-next.sh — the pure "next question" selector of the
# hierarchical-tag classification flow. No daemon/GUI involved: fixtures are
# plain files, we compare the selector's stdout/exit code. Mirrors the harness
# style of scripts/test-prune-target.sh.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
NEXT="$HERE/shipped/gui-tag-next.sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

pass=0
fail=0

# run_case <name> <universe> <pos> <neg> <expected-stdout> <expected-exit>
# universe/pos/neg are passed as literal multi-line strings (may be empty).
run_case() {
    local name=$1 universe=$2 pos=$3 neg=$4 want_out=$5 want_code=$6
    printf '%s' "$universe" >"$tmp/u"
    printf '%s' "$pos"      >"$tmp/p"
    printf '%s' "$neg"      >"$tmp/n"

    local got_out got_code
    set +e
    got_out=$(bash "$NEXT" "$tmp/u" "$tmp/p" "$tmp/n")
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
run_case "top-level-first" "$U_BASE" "" "" "musique" 0

# 2. Already-positive top tag is not re-asked; but its child becomes askable,
#    while the other unanswered top-level tag is shallower -> administratif first.
run_case "skip-positive-top" "$U_BASE" $'musique\n' "" "administratif" 0

# 3. A generic negative blocks the whole subtree: musique in NEG -> never
#    musique/jazz; only administratif remains.
run_case "negative-blocks-subtree" "$U_BASE" "" $'musique\n' "administratif" 0

# 4. Descent: once every top-level is answered and musique is positive, its
#    child is proposed.
run_case "descend-to-child" "$U_BASE" $'musique\n' $'administratif\n' "musique/jazz" 0

# 5. Multi-genre: musique specialised to jazz (parent removed), the sibling
#    rock is still offered — siblings never exclude each other.
run_case "multi-genre-sibling" "$U_BASE" $'musique/jazz\n' $'administratif\n' \
    "musique/rock" 0

# 6. Columns after the uuid are ignored: the sibling is still offered.
U_EXTRA=$(printf 'musique\tA\tx\nmusique/jazz\tB\ty\nmusique/rock\tC\tz\n')
run_case "extra-columns-ignored" "$U_EXTRA" $'musique/jazz\n' "" "musique/rock" 0

# 7. A tag of several parents (doc "Hierarchy references") is one line per
#    path, with its uuid. A negative above one of its paths rules out the tag
#    under every path: not an event, so not a marathon, even as a race.
U_HIER=$(printf 'sport\tU1\nsport/courir\tU2\nsport/courir/maraton\tU3\névènement\tU4\névènement/maraton\tU3\n')
run_case "negative-blocks-every-path" "$U_HIER" $'sport/courir\n' $'évènement\n' "" 1

# 8. Without that negative, the marathon is asked about, once, by the path
#    under the accepted branch.
run_case "alias-asked-once" "$U_HIER" $'sport/courir\n' "" "évènement" 0

# 9. Nothing left to ask -> exit 1, empty output.
run_case "exhausted" "$U_BASE" $'musique/jazz\nmusique/rock\n' $'administratif\n' "" 1

echo "----"
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
