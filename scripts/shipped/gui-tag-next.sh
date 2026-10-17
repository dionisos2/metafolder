#!/usr/bin/env bash
# Pure selector ("specify") for the hierarchical-tag classification flow: given
# the tag universe and a metarecord's current positive/negative tags, print the
# single next question, or nothing (exit 1) when no question remains:
#
#   ask<TAB><path>   "add tag <path> ?", answered yes/no;
#   pick<TAB><path>  pick one of the tag's children by completion — the tag is
#                    a `partition` or `sparse` one, and is had (doc "Tag flags").
#
# Sourced, it also offers `gui_tag_children` (the candidates of a pick) and
# `gui_tag_always` (the `always` tags to add without asking). No daemon/GUI
# dependency, so it is unit-testable (scripts/dev/test-gui-tag-next.sh) and is
# *sourced* by scripts/gui-tag-classify.sh to share the hierarchy helpers.
#
# Usage: gui-tag-next.sh <universe_file> <pos_file> <neg_file>
#   universe_file : one line per tag path (`mf tag list`):
#                   "path <TAB> uuid <TAB> always <TAB> partition <TAB> sparse",
#                   flags 1 for true, anything else (or missing) false.
#                   A tag of several parents has several paths, one uuid; a
#                   line without the uuid is a tag of its own. Further columns
#                   are ignored. Line order breaks depth ties.
#   pos_file      : positive tag paths (one per line).
#   neg_file      : negative tag paths (one per line).
#
# A tag path is "/"-separated (e.g. musique/jazz). Semantics: adding a specific
# tag drops its more general ancestors; a generic negative blocks its whole
# subtree — under every path of the tags in it; a specific positive implies
# (and hides) its ancestors. The children of a partition or sparse tag are
# never asked one by one: they are picked, once the tag is had; a partition is
# settled once one child is had (by itself or a descendant).

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

# _gui_tag_load <universe_file> <pos_file> <neg_file>
# Reads the three files into the CALLER's variables (declared there, so bash's
# dynamic scope fills them): order, positives, tag_of, is_pos, is_neg, f_always,
# f_partition, f_sparse, blocked, under_neg, held.
#
# Everything is read into memory once, and every test below is parameter
# expansion. This used to run `grep` twice and `awk` once or more per
# candidate tag, plus a command substitution per parent lookup: with a
# hundred-tag vocabulary and a handful of positives that is thousands of
# processes for a single question, and the classify loop asks several per
# metarecord. gui-tag-folder.sh already records the lesson — "one process
# per entry is seconds of pure forking" — and this was the worst offender.
_gui_tag_load() {
    local universe=$1 pos=$2 neg=$3
    local path line uuid always partition sparse tag ancestor

    while IFS=$'\t' read -r path uuid always partition sparse _ || [ -n "$path" ]; do
        [ -n "$path" ] || continue
        order+=("$path")
        tag=${uuid:-$path}
        tag_of[$path]=$tag
        [ "${always:-}" != 1 ] || f_always[$tag]=1
        [ "${partition:-}" != 1 ] || f_partition[$tag]=1
        [ "${sparse:-}" != 1 ] || f_sparse[$tag]=1
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

    # The answered tags, by whichever path the answer named them.
    local -A pos_tag=() neg_tag=()
    for path in ${order+"${order[@]}"}; do
        [ -z "${is_pos[$path]:-}" ] || pos_tag[${tag_of[$path]}]=1
        [ -z "${is_neg[$path]:-}" ] || neg_tag[${tag_of[$path]}]=1
    done

    # A generic negative blocks its whole subtree, and a tag of several
    # parents is in it whichever path the question would name it by: the
    # tags under a negative, by any of their paths, are set aside first.
    # Walking a path's own ancestors costs its depth rather than the
    # negatives' length. A tag had is held, and so is every tag above it,
    # by each of its paths.
    for path in ${order+"${order[@]}"}; do
        tag=${tag_of[$path]}
        [ -z "${neg_tag[$tag]:-}" ] || under_neg[$tag]=1
        if [ -n "${pos_tag[$tag]:-}" ] || [ -n "${neg_tag[$tag]:-}" ]; then
            blocked[$tag]=1 # answered, under one of its paths
        fi
        [ -z "${pos_tag[$tag]:-}" ] || held[$tag]=1
        ancestor=$path
        while [ "${ancestor%/*}" != "$ancestor" ]; do
            ancestor=${ancestor%/*}
            if [ -n "${pos_tag[$tag]:-}" ] && [ -n "${tag_of[$ancestor]+set}" ]; then
                held[${tag_of[$ancestor]}]=1
            fi
            if [ -n "${tag_of[$ancestor]+set}" ] && [ -n "${neg_tag[${tag_of[$ancestor]}]:-}" ]; then
                blocked[$tag]=1
                under_neg[$tag]=1
            fi
        done
    done
}

# _gui_tag_candidates <path> — on loaded state: the children of the tag at
# <path>, by every path of it, that are neither answered, under a negative, nor
# had; one line each, named under <path> when they hang there.
_gui_tag_candidates() {
    local at=$1 tag=${tag_of[$1]:-$1} c parent pass
    local -A seen=()
    for pass in here elsewhere; do
        for c in ${order+"${order[@]}"}; do
            parent=${c%/*}
            [ "$parent" != "$c" ] || continue
            [ "${tag_of[$parent]:-}" = "$tag" ] || continue
            if [ "$pass" = here ]; then
                [ "$parent" = "$at" ] || continue
            fi
            local u=${tag_of[$c]}
            [ -z "${seen[$u]:-}" ] || continue
            seen[$u]=1
            [ -z "${blocked[$u]:-}" ] || continue
            [ -z "${held[$u]:-}" ] || continue
            printf '%s\n' "$c"
        done
    done
}

# _gui_tag_picked <tag> — on loaded state: success when the tag's children are
# picked rather than asked (a partition or sparse tag).
_gui_tag_picked() { [ -n "${f_partition[$1]:-}" ] || [ -n "${f_sparse[$1]:-}" ]; }

# _gui_tag_settled <tag> — on loaded state: success when a child of the
# partition <tag> is had, by itself or through a descendant.
_gui_tag_settled() {
    local c parent
    for c in ${order+"${order[@]}"}; do
        parent=${c%/*}
        [ "$parent" != "$c" ] || continue
        [ "${tag_of[$parent]:-}" = "$1" ] || continue
        [ -z "${held[${tag_of[$c]}]:-}" ] || return 0
    done
    return 1
}

_gui_tag_state() {
    order=() positives=()
    tag_of=() is_pos=() is_neg=() f_always=() f_partition=() f_sparse=()
    blocked=() under_neg=() held=()
}

# gui_tag_next <universe_file> <pos_file> <neg_file>
# Prints the next question (shallowest, universe order) or nothing. A pick is
# as deep as the children it picks from. Returns 0 if a question was printed,
# 1 otherwise.
gui_tag_next() {
    local -a order positives
    local -A tag_of is_pos is_neg f_always f_partition f_sparse blocked under_neg held
    _gui_tag_state
    _gui_tag_load "$1" "$2" "$3"

    local best="" best_kind="" best_depth=-1
    local path tag child child_parent parent depth slashes hidden engaged
    for path in ${order+"${order[@]}"}; do
        tag=${tag_of[$path]}
        slashes=${path//[!\/]/}
        depth=${#slashes}

        # A pick: the tag is had and its children are picked, some remain,
        # and — for a partition — none of them is had yet.
        if _gui_tag_picked "$tag" && [ -n "${held[$tag]:-}" ] \
            && { [ "$best_depth" -lt 0 ] || [ "$((depth + 1))" -lt "$best_depth" ]; }; then
            if { [ -z "${f_partition[$tag]:-}" ] || ! _gui_tag_settled "$tag"; } \
                && [ -n "$(_gui_tag_candidates "$path")" ]; then
                best=$path best_kind=pick best_depth=$((depth + 1))
            fi
        fi

        # Answered, or under a negative — by any of the tag's paths.
        [ -z "${blocked[$tag]:-}" ] || continue

        # A specific positive implies — and so hides — its ancestors.
        hidden=0
        for child in ${positives+"${positives[@]}"}; do
            if [[ $child == "$path"/* ]]; then
                hidden=1
                break
            fi
        done
        [ "$hidden" = 0 ] || continue

        # Reachable: the branch is engaged (the parent, or one of its direct
        # children, is positive). Top-level tags are always reachable. The
        # children of a picked tag are not asked by this path.
        parent=${path%/*}
        [ "$parent" = "$path" ] && parent=""
        if [ -n "$parent" ]; then
            if [ -n "${tag_of[$parent]+set}" ] && _gui_tag_picked "${tag_of[$parent]}"; then
                continue
            fi
            engaged=0
            [ -n "${is_pos[$parent]:-}" ] && engaged=1
            for child in ${positives+"${positives[@]}"}; do
                [ "$engaged" = 0 ] || break
                child_parent=${child%/*}
                [ "$child_parent" = "$child" ] && child_parent=""
                [ "$child_parent" = "$parent" ] && engaged=1
            done
            [ "$engaged" = 1 ] || continue
        fi

        if [ "$best_depth" -lt 0 ] || [ "$depth" -lt "$best_depth" ]; then
            best=$path best_kind=ask best_depth=$depth
        fi
    done

    [ -n "$best" ] || return 1
    printf '%s\t%s\n' "$best_kind" "$best"
}

# gui_tag_children <universe_file> <pos_file> <neg_file> <path>
# The candidates of a pick on the tag at <path>: its children neither answered,
# under a negative, nor had. Returns 1 when there is none.
gui_tag_children() {
    local -a order positives
    local -A tag_of is_pos is_neg f_always f_partition f_sparse blocked under_neg held
    _gui_tag_state
    _gui_tag_load "$1" "$2" "$3"
    local out
    out=$(_gui_tag_candidates "$4")
    [ -n "$out" ] || return 1
    printf '%s\n' "$out"
}

# gui_tag_always <universe_file> <pos_file> <neg_file>
# The `always` tags to add without asking, one path each: those neither had
# (themselves or through a descendant), denied, nor under a negative — and not
# those above another one listed, which says them already. Returns 1 when there
# is none.
gui_tag_always() {
    local -a order positives
    local -A tag_of is_pos is_neg f_always f_partition f_sparse blocked under_neg held
    _gui_tag_state
    _gui_tag_load "$1" "$2" "$3"
    local -a picked=()
    local -A seen=() seen_listed=()
    local path tag own other found=1 below
    for path in ${order+"${order[@]}"}; do
        tag=${tag_of[$path]}
        [ -n "${f_always[$tag]:-}" ] || continue
        [ -z "${seen[$tag]:-}" ] || continue
        seen[$tag]=1
        [ -z "${held[$tag]:-}" ] || continue
        [ -z "${under_neg[$tag]:-}" ] || continue
        picked+=("$path")
        seen_listed[$tag]=1
    done
    # Every path of a listed tag, to tell which listed tag lies below another.
    local -a listed_paths=()
    for path in ${order+"${order[@]}"}; do
        [ -z "${seen_listed[${tag_of[$path]}]:-}" ] || listed_paths+=("$path")
    done
    for path in ${picked+"${picked[@]}"}; do
        tag=${tag_of[$path]}
        below=0
        for own in ${order+"${order[@]}"}; do
            [ "${tag_of[$own]}" = "$tag" ] || continue
            for other in ${listed_paths+"${listed_paths[@]}"}; do
                [[ $other == "$own"/* ]] && below=1
            done
        done
        [ "$below" = 0 ] || continue
        printf '%s\n' "$path"
        found=0
    done
    return "$found"
}

# Run as a script (not when sourced).
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    [ $# -eq 3 ] || { echo "usage: $0 <universe_file> <pos_file> <neg_file>" >&2; exit 2; }
    gui_tag_next "$1" "$2" "$3"
fi
