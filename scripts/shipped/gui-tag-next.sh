#!/usr/bin/env bash
# Pure selector ("specify") for the hierarchical-tag classification flow: given
# the tag universe and a metarecord's current positive/negative tags, print the
# single next tag to ask "add tag <path> ?" about, or nothing (exit 1) when no
# question remains. No daemon/GUI dependency, so it is unit-testable
# (scripts/test-gui-tag-next.sh) and is *sourced* by scripts/gui-tag-classify.sh
# to share the hierarchy helpers.
#
# Usage: gui-tag-next.sh <universe_file> <pos_file> <neg_file>
#   universe_file : one tag per line, TAB columns "path <TAB> partition <TAB> exclusive"
#                   (flags 0/1, missing = 0). Line order breaks depth ties.
#   pos_file      : positive tag paths (one per line).
#   neg_file      : negative tag paths (one per line).
#
# A tag path is "/"-separated (e.g. musique/jazz). Semantics: adding a specific
# tag drops its more general ancestors; a generic negative blocks its whole
# subtree; a specific positive implies (and hides) its ancestors. Exclusivity is
# data-driven — `exclusive` on a child, or `partition` on its parent, makes the
# parent's direct children mutually exclusive.

set -euo pipefail

# --- hierarchy helpers (shared with the driver via `source`) -----------------

# tag_parent <path> -> the parent path ("" for a top-level tag).
tag_parent() {
    case $1 in
        */*) printf '%s' "${1%/*}" ;;
        *)   printf '%s' "" ;;
    esac
}

# is_ancestor <a> <b> -> success if <a> is a strict ancestor path of <b>.
is_ancestor() { [ "$1" != "$2" ] && case "$2/" in "$1/"*) return 0 ;; esac; return 1; }

# in_set <path> <set-file> -> success if the exact path is a line of the file.
in_set() { grep -qxF -- "$1" "$2"; }

# has_ancestor_in <path> <set-file> -> success if any strict ancestor is present.
has_ancestor_in() {
    local p=$1 f=$2 line
    while IFS= read -r line || [ -n "$line" ]; do
        [ -n "$line" ] || continue
        is_ancestor "$line" "$p" && return 0
    done <"$f"
    return 1
}

# has_descendant_in <path> <set-file> -> success if any strict descendant present.
has_descendant_in() {
    local p=$1 f=$2 line
    while IFS= read -r line || [ -n "$line" ]; do
        [ -n "$line" ] || continue
        is_ancestor "$p" "$line" && return 0
    done <"$f"
    return 1
}

# --- selection ---------------------------------------------------------------

# gui_tag_next <universe_file> <pos_file> <neg_file>
# Prints the next askable tag (shallowest, universe order) or nothing.
# Returns 0 if a tag was printed, 1 otherwise.
gui_tag_next() {
    local universe=$1 pos=$2 neg=$3

    # is_exclusive <path> : the tag excludes its siblings (own `exclusive`, or
    # its parent is a `partition`). Reads the universe flag columns.
    is_exclusive() {
        local t=$1 par excl part
        excl=$(awk -F '\t' -v t="$t" '$1==t{print ($3==1)?1:0; exit}' "$universe")
        [ "${excl:-0}" = 1 ] && return 0
        par=$(tag_parent "$t")
        [ -n "$par" ] || return 1
        part=$(awk -F '\t' -v t="$par" '$1==t{print ($2==1)?1:0; exit}' "$universe")
        [ "${part:-0}" = 1 ]
    }

    # reachable <path> : the branch is engaged and not closed by an exclusive
    # sibling already chosen. Top-level tags are always reachable.
    reachable() {
        local t=$1 par c engaged=1 closed=1
        par=$(tag_parent "$t")
        [ -n "$par" ] || return 0            # top level
        engaged=1; closed=1                  # 1 = false, 0 = true (shell truth)
        # Scan the parent's direct children that are currently positive.
        while IFS= read -r c || [ -n "$c" ]; do
            [ -n "$c" ] || continue
            [ "$(tag_parent "$c")" = "$par" ] || continue
            engaged=0
            is_exclusive "$c" && closed=0
        done <"$pos"
        # Also engaged if the parent itself is still positive.
        in_set "$par" "$pos" && engaged=0
        [ "$engaged" -eq 0 ] && [ "$closed" -ne 0 ]
    }

    local depth best="" best_depth=-1 path rest
    while IFS=$'\t' read -r path rest || [ -n "$path" ]; do
        [ -n "$path" ] || continue
        in_set "$path" "$pos" && continue                 # already positive
        in_set "$path" "$neg" && continue                 # already negative
        has_ancestor_in "$path" "$neg" && continue        # generic negative blocks
        has_descendant_in "$path" "$pos" && continue       # specific positive implies it
        reachable "$path" || continue

        depth=$(awk -F/ '{print NF-1}' <<<"$path")
        if [ "$best_depth" -lt 0 ] || [ "$depth" -lt "$best_depth" ]; then
            best=$path
            best_depth=$depth
        fi
    done <"$universe"

    unset -f is_exclusive reachable
    [ -n "$best" ] || return 1
    printf '%s\n' "$best"
}

# Run as a script (not when sourced).
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    [ $# -eq 3 ] || { echo "usage: $0 <universe_file> <pos_file> <neg_file>" >&2; exit 2; }
    gui_tag_next "$1" "$2" "$3"
fi
