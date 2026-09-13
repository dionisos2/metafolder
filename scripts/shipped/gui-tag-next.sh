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

    # Everything is read into memory once, and every test below is parameter
    # expansion. This used to run `grep` twice and `awk` once or more per
    # candidate tag, plus a command substitution per parent lookup: with a
    # hundred-tag vocabulary and a handful of positives that is thousands of
    # processes for a single question, and the classify loop asks several per
    # metarecord. gui-tag-folder.sh already records the lesson — "one process
    # per entry is seconds of pure forking" — and this was the worst offender.
    local -a order=() positives=()
    local -A part=() excl=() is_pos=() is_neg=()
    local path col_part col_excl line

    while IFS=$'\t' read -r path col_part col_excl || [ -n "$path" ]; do
        [ -n "$path" ] || continue
        order+=("$path")
        part[$path]=${col_part:-0}
        excl[$path]=${col_excl:-0}
    done <"$universe"

    while IFS= read -r line || [ -n "$line" ]; do
        [ -n "$line" ] || continue
        is_pos[$line]=1
        positives+=("$line")
    done <"$pos"

    while IFS= read -r line || [ -n "$line" ]; do
        [ -n "$line" ] || continue
        is_neg[$line]=1
    done <"$neg"

    local best="" best_depth=-1
    local ancestor child child_parent parent depth slashes blocked engaged closed
    for path in ${order+"${order[@]}"}; do
        [ -z "${is_pos[$path]:-}" ] || continue           # already positive
        [ -z "${is_neg[$path]:-}" ] || continue           # already negative

        # A generic negative blocks its whole subtree. Walking the candidate's
        # own ancestors is the same set as scanning the negatives for one, and
        # costs the path's depth rather than the negatives' length.
        blocked=0
        ancestor=$path
        while [ "${ancestor%/*}" != "$ancestor" ]; do
            ancestor=${ancestor%/*}
            if [ -n "${is_neg[$ancestor]:-}" ]; then
                blocked=1
                break
            fi
        done
        [ "$blocked" = 0 ] || continue

        # A specific positive implies — and so hides — its ancestors.
        blocked=0
        for child in ${positives+"${positives[@]}"}; do
            if [[ $child == "$path"/* ]]; then
                blocked=1
                break
            fi
        done
        [ "$blocked" = 0 ] || continue

        # Reachable: the branch is engaged (the parent, or one of its direct
        # children, is positive) and not closed by an exclusive sibling already
        # chosen. Top-level tags are always reachable.
        parent=${path%/*}
        [ "$parent" = "$path" ] && parent=""
        if [ -n "$parent" ]; then
            engaged=0
            closed=0
            for child in ${positives+"${positives[@]}"}; do
                child_parent=${child%/*}
                [ "$child_parent" = "$child" ] && child_parent=""
                [ "$child_parent" = "$parent" ] || continue
                engaged=1
                # Exclusive by its own flag, or because its parent partitions
                # its children.
                if [ "${excl[$child]:-0}" = 1 ] || [ "${part[$parent]:-0}" = 1 ]; then
                    closed=1
                fi
            done
            [ -n "${is_pos[$parent]:-}" ] && engaged=1
            { [ "$engaged" = 1 ] && [ "$closed" = 0 ]; } || continue
        fi

        slashes=${path//[!\/]/}
        depth=${#slashes}
        if [ "$best_depth" -lt 0 ] || [ "$depth" -lt "$best_depth" ]; then
            best=$path
            best_depth=$depth
        fi
    done

    [ -n "$best" ] || return 1
    printf '%s\n' "$best"
}

# Run as a script (not when sourced).
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    [ $# -eq 3 ] || { echo "usage: $0 <universe_file> <pos_file> <neg_file>" >&2; exit 2; }
    gui_tag_next "$1" "$2" "$3"
fi
