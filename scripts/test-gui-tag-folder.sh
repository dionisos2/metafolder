#!/usr/bin/env bash
# Tests for the pure path helpers of scripts/gui-tag-folder.sh
# (tag_ancestors / tag_descendants). The script's GUI/daemon logic lives behind
# a `main` guard, so sourcing it here loads only the helpers.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=gui-tag-folder.sh
source "$HERE/gui-tag-folder.sh"

pass=0; fail=0
chk() { # chk <name> <got> <want>
    if [ "$2" = "$3" ]; then pass=$((pass+1)); printf 'ok   %s\n' "$1"
    else fail=$((fail+1)); printf 'FAIL %s\n     got  [%s]\n     want [%s]\n' "$1" "$2" "$3"; fi
}

# tag_ancestors: proper ancestor paths, deepest-first.
chk "anc-two-levels" "$(tag_ancestors musique/jazz/bebop | paste -sd,)" "musique/jazz,musique"
chk "anc-one-level"  "$(tag_ancestors musique/jazz | paste -sd,)"       "musique"
chk "anc-top-level"  "$(tag_ancestors musique | paste -sd,)"            ""

# tag_descendants: from candidate names on stdin, those strictly under <tag>.
NAMES=$'musique\nmusique/jazz\nmusique/jazz/bebop\nmusique/rock\nmusiquerie\nadministratif'
chk "desc-strict-under" "$(printf '%s\n' "$NAMES" | tag_descendants musique | paste -sd,)" \
    "musique/jazz,musique/jazz/bebop,musique/rock"
chk "desc-no-prefix-bleed" "$(printf '%s\n' "$NAMES" | tag_descendants musiquerie | paste -sd,)" ""
chk "desc-leaf" "$(printf '%s\n' "$NAMES" | tag_descendants musique/rock | paste -sd,)" ""

echo "----"; echo "passed=$pass failed=$fail"; [ "$fail" -eq 0 ]
