#!/usr/bin/env bash
# Summary: Bulk-apply one tag over a query's metarecords (yes/no/mixed walk).
# Bulk-apply one tag over a set of metarecords, in the running metafolder GUI.
# Given a TAG (a "/"-separated tag path) and a QUERY, walks what the query
# matches and asks, per entry, whether it carries the tag:
#
#   y (oui)   -> `mf tag add` on the entry; for a folder, on its whole subtree
#                too — *intersected with the query*, which is the scope.
#   n (non)   -> `mf tag deny` on the same scope.
#   m (mixed) -> `mf tag mixed` on the folder only; its children stay in the
#                walk and are asked in turn.
#   s (skip)  -> leave this entry alone: no tag op, and for a folder nothing
#                under it is asked either — the subtree is left for another run.
#
# The arrow keys answer as well: → yes, ← no, ↑ mixed, ↓ skip.
#
# THE QUERY IS THE SCOPE. A "yes" on a folder never reaches a metarecord the
# query excludes: the subtree op is `(<query>) AND mfr_path ->* "<path>"`. So
# narrowing the list in the GUI narrows what this script can touch.
#
# Where the query comes from, in order: the QUERY argument; else what the GUI
# is showing (`mf gui query` — the checkbox selection, else the list's query
# with its finder narrowing); else a folder chosen from the completion, turned
# into `mfr_path =>* "<folder>"` (the folder and its whole subtree). An empty
# query means every metarecord, and is left as such rather than wrapped.
#
# ORDER. The whole scope is read in four round-trips — ordered uuids and
# uuid→path for each of the two kinds — instead of one listing per folder. The
# walk then goes level by level (a folder before what it contains), folders
# before files, and within one folder in the order the daemon returned:
# `--sort order_dir` / `--sort order_file` first, then `--sort mfr_path`. So a
# folder that `mf order` has numbered is walked in its own order (an album by
# track number), and everything else alphabetically by path. The script never
# runs `mf order` itself: numbering is a deliberate act, and its date-based
# fallback would be a worse walking order than the alphabetical one.
#
# Because the scope is read up front, the total is known before the first
# question and the progress bar is exact.
#
# Resumable: an entry whose answer is already recorded is not asked again. The
# record carries the tag (`tag`, exactly or through a more specific tag), carries
# its negation (`negative_tag`, exactly or through a more general one), or is a
# `mixed_tag` folder — which is walked into straight away, no question. Those
# three sets are read over the whole scope in one round-trip each, with the
# subsumption spelled in the query, so a resume costs three calls and not three
# per entry. So a
# run interrupted halfway (skip, stop, Escape) is continued by re-running the
# same command, and only the open questions come back. `--redo` asks everything
# again, decided or not — the way to revise a wrong answer over a subtree.
#
# `mf tag` owns the tag model: it creates the entry if the vocabulary lacks it,
# adds the ref idempotently, and applies the subsumption/exclusivity rewrites
# (add drops the more general ancestor tags, deny drops the more specific
# descendant negatives). So this script is only the walk + the y/n/m questions.
#
# Operates on TRACKED metarecords only — reconcile first if you want everything
# covered.
#
# Usage: gui-tag-folder.sh [--redo] [<tag> [<query>]]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

# `--redo`: ask every entry again, ignoring the answers already recorded.
# (`&&` here would end the script under `set -e` whenever the flag is absent.)
REDO=0
if [ "${1:-}" = "--redo" ]; then
    REDO=1
    shift
fi
[ $# -le 2 ] || mf_die "usage: $0 [--redo] [<tag> [<query>]]"
TAG=${1:-}
QUERY_GIVEN=0
[ $# -ge 2 ] && QUERY_GIVEN=1
QUERY_ARG=${2-}

mf_gui_bind_repo

# Tag: from the command line, or prompted with completion over the vocabulary.
[ -n "$TAG" ] || TAG=$(mf_gui_prompt_tag "Tag: ") || mf_die "cancelled"
[ -n "$TAG" ] || mf_die "empty tag name"
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

# The scope. Resolved BEFORE the session takeover: `mf gui query` answers for
# the focused workspace, and the scratch workspace the session opens publishes
# nothing.
# SCOPE is read by mf_gui_scoped / mf_gui_scope_get, in lib/mf-gui.sh (a
# sourced file this check does not follow from here).
# shellcheck disable=SC2034
if [ "$QUERY_GIVEN" = 1 ]; then
    SCOPE=$QUERY_ARG
else
    SCOPE=$(mf_gui_default_scope "Folder: ") || mf_die "cancelled"
fi

mf_gui_session_open metarecord-detail

TMP=$(mf_gui_tmpdir)

declare -A PATH_OF RANK KIND

# Read one kind of the scope: the uuids in walking order, and their paths.
# Two round-trips per kind, whatever the size — `--sort` and `--resolve-tree`
# are exclusive, so the order comes from one call and the paths from the other.
#
# `mfr_path IS PRESENT` keeps out what has no place in a tree walk: a deleted
# file keeps its metarecord with `mfr_path = Nothing` (spec-file-tracking), so
# it still answers `mfr_type = "file"` while resolving to no path at all.
# Without the filter such a record entered the walk with an empty path — which
# sorts as the repository root, so it was asked about FIRST, in a question
# naming no file and with a preview that could not move off whatever the panels
# already held.
collect() { # <dir|file> <order field>
    local kind=$1 field=$2 i=0 uuid path
    local scope
    scope=$(mf_gui_scoped "mfr_type = \"$kind\" AND mfr_path IS PRESENT")
    # Through files rather than `< <(mf …)`: a process substitution throws the
    # exit status away, so a refused query or a stopped daemon came back as an
    # empty walk and the script announced "the query matches no tracked
    # metarecord" — reporting an error as an answer.
    local -a ordered=()
    mf_into "$TMP/order.$kind" metarecord -q "$scope" get --sort "$field" --sort mfr_path
    mapfile -t ordered <"$TMP/order.$kind"
    for uuid in ${ordered+"${ordered[@]}"}; do
        [ -n "$uuid" ] || continue
        i=$((i + 1))
        RANK[$uuid]=$i
        KIND[$uuid]=$kind
    done
    mf_into "$TMP/paths.$kind" metarecord -q "$scope" get --resolve-tree mfr_path --tsv
    while IFS=$'\t' read -r uuid path; do
        [ -n "$uuid" ] || continue
        PATH_OF[$uuid]=$path
    done <"$TMP/paths.$kind"
}

# TAG and its ancestors, as an alternation over a tag entry's path: a more
# general "no" denies the tag we are asking about.
neg_paths() { # <tag> -> path = "a/b" OR path = "a"
    local t=$1 out=""
    while [ -n "$t" ]; do
        out+="path = \"$t\" OR "
        case $t in */*) t=${t%/*} ;; *) t="" ;; esac
    done
    printf '%s' "${out% OR }"
}

# What TAG already answers for, read as three SETS — one round-trip each for
# the whole scope. Reading it per entry instead cost three daemon calls before
# every question, which on a resume is the whole walk spent re-reading answers
# it already has.
#
# Subsumption moves into the query, spelled exactly as the per-record reads
# spelled it: a positive on TAG *or any tag below it* implies TAG; a negative
# on TAG *or any tag above it* denies it; only an exact mixed marker is one.
# `mf tag` applies the same rules when writing, so the two agree.
declare -A DECIDED
load_decided() {
    [ "$REDO" = 0 ] || return 0
    # Weakest first: a record that is both mixed and denied reads as denied,
    # and one that is also tagged reads as tagged — the precedence the per-entry
    # reads had when they stopped at the first hit.
    local pair answer pred uuid
    for pair in \
        "m:mixed_tag -> (mf_schema = \"tag\" AND path = \"$TAG\")" \
        "n:negative_tag -> (mf_schema = \"tag\" AND ($(neg_paths "$TAG")))" \
        "y:tag -> (mf_schema = \"tag\" AND path =>* \"$TAG\")"; do
        answer=${pair%%:*}
        pred=${pair#*:}
        # Through a file for the same reason as `collect`, and here the silent
        # failure was worse: an empty answer set is indistinguishable from "you
        # have answered nothing yet", so a --resume run would quietly ask every
        # question again.
        mf_into "$TMP/decided.$answer" metarecord -q "$(mf_gui_scoped "$pred")" get
        while read -r uuid; do
            [ -n "$uuid" ] || continue
            DECIDED[$uuid]=$answer
        done <"$TMP/decided.$answer"
    done
}

collect dir order_dir
collect file order_file

load_decided

# One sortable line per entry: depth, parent path, kind (folders first), then
# the daemon's own rank. Sorting on that gives the walk its order — level by
# level, a folder before its contents, folders before files.
ENTRIES=()
for uuid in "${!RANK[@]}"; do
    # No path, no place in the walk. The two round-trips above are separate
    # queries, so one can hold a record the other does not; and the repository
    # root's path is the EMPTY string, which is why this tests for the key
    # rather than for a non-empty value.
    [ -n "${PATH_OF[$uuid]+set}" ] || continue
    path=${PATH_OF[$uuid]}
    # The depth is the number of "/" in the path (the root's "" is 0). Spelled
    # with parameter expansion rather than `awk`: this loop runs once per
    # metarecord in the scope, and one process per entry is seconds of pure
    # forking on a scope of any size — the whole cost of getting to the first
    # question.
    slashes=${path//[!\/]/}
    depth=${#slashes}
    parent=${path%/*}
    krank=1
    [ "${KIND[$uuid]}" = dir ] && krank=0
    # The parent is prefixed so the field is never empty: a tab is IFS
    # *whitespace*, so `read` collapses two consecutive ones and an empty middle
    # field would shift every field after it (the root's parent is "").
    ENTRIES+=("$depth	.$parent	$krank	${RANK[$uuid]}	$uuid")
done

TOTAL=${#ENTRIES[@]}
[ "$TOTAL" -gt 0 ] || mf_die "the query matches no tracked metarecord"

# The walk order, materialised in a file rather than read from a pipe. Leaving
# the walk early (stop, Escape, a failed tag op) closes its input, and a `sort`
# still writing then dies of SIGPIPE — which the ERR trap reported as an error,
# so a deliberate stop looked like a crash. A file has no writer to kill.
WALK="$TMP/walk"
printf '%s\n' "${ENTRIES[@]}" | sort -t$'\t' -k1,1n -k2,2 -k3,3n -k4,4n >"$WALK"

# Apply T over a node and its subtree, the subtree narrowed to the scope.
apply_tree() { # <uuid> <path> <verb: add|deny>
    mf tag -i "$1" "$3" "$TAG" >/dev/null \
        && mf tag -q "$(mf_gui_scoped "mfr_path ->* \"$(mf_dsl_str "$2")\"")" "$3" "$TAG" \
            >/dev/null
}

# Subtrees that are settled: a folder answered yes/no (its whole subtree took
# the answer) or skipped (deliberately left alone). Nothing under them is asked.
PRUNED=()
is_pruned() { # <path>
    local path=$1 root
    for root in ${PRUNED+"${PRUNED[@]}"}; do
        case "$path/" in "$root/"*) return 0 ;; esac
    done
    return 1
}

# How the walk ended: "" = still going, "user" = stopped, anything else is an
# ERROR MESSAGE. The two must stay apart: bash disables `set -e` wherever a
# failure is tested, so a failed `mf tag` inside a handler would otherwise be
# indistinguishable from Escape and end the run with a cheerful "stopped."
STOP=""
SKIPPED=0
ALREADY=0
DONE=0

# The progress bar is STEPPED, not smooth. Every report is a process and a
# round-trip, and a resume walks past entries it has already answered without
# stopping at any of them — one report each would cost more than the walk. An
# entry that is actually asked always reports (that is where the user is
# looking); a skipped one reports only once every percent or so of the scope,
# and the last entry always does, so the bar still reaches the end.
STEP=$((TOTAL / 100))
[ "$STEP" -ge 1 ] || STEP=1
LAST_REPORT=0
report_progress() { # <done> <phase>
    LAST_REPORT=$1
    mf_gui_progress --done "$1" --total "$TOTAL" --phase "$2"
}
report_step() { # <done> <phase>   — only when a step has gone by
    if [ $(($1 - LAST_REPORT)) -ge "$STEP" ] || [ "$1" -eq "$TOTAL" ]; then
        report_progress "$1" "$2"
    fi
}

# `depth`, `parent`, `krank` and `rank` are the sort key and are not read again
# here; only the uuid is.
# shellcheck disable=SC2034
while IFS=$'\t' read -r depth parent krank rank uuid; do
    [ -z "$STOP" ] || break
    [ -n "$uuid" ] || continue
    path=${PATH_OF[$uuid]-}
    kind=${KIND[$uuid]}
    # Counted before the prune test, not after: TOTAL counts every entry, so a
    # DONE that skipped the pruned ones never reached it — the "the last entry
    # always reports" rule never fired, and the "N left" counter claimed a
    # folder answered whole was still ahead.
    DONE=$((DONE + 1))
    is_pruned "$path" && continue
    # Already answered in an earlier run: no question, no tag op. A folder that
    # took the answer whole settles its subtree; a mixed one does not — that is
    # where its remaining questions live. The answer is a lookup in the sets
    # read up front, so it is free: read it before reporting, and the report
    # can then say whether this entry is one the user will be asked about.
    prior=${DECIDED[$uuid]-}
    case $prior in
        y | n)
            ALREADY=$((ALREADY + 1))
            report_step "$DONE" "$path"
            [ "$kind" = dir ] && PRUNED+=("$path")
            continue
            ;;
        m)
            ALREADY=$((ALREADY + 1))
            report_step "$DONE" "$path"
            continue
            ;;
    esac
    report_progress "$DONE" "$path"
    mf_gui_show_file "$(mf path "$uuid" 2>/dev/null || true)"
    counter="$((TOTAL - DONE)) left"
    if [ "$kind" = dir ]; then
        answer=$(mf_gui_ask_answer \
            "'$path' has tag '$TAG'?   [y →] oui   [n ←] non   [m ↑] mixed   [s ↓] skip   [q] stop   — $counter" \
            y n m s q)
    else
        answer=$(mf_gui_ask_answer \
            "'$path' has tag '$TAG'?   [y →] oui   [n ←] non   [s ↓] skip   [q] stop   — $counter" \
            y n s q)
    fi
    case $answer in
        y)
            if [ "$kind" = dir ]; then
                if apply_tree "$uuid" "$path" add; then PRUNED+=("$path"); else STOP="cannot tag '$path'"; fi
            else
                mf tag -i "$uuid" add "$TAG" >/dev/null || STOP="cannot tag '$path'"
            fi
            ;;
        n)
            if [ "$kind" = dir ]; then
                if apply_tree "$uuid" "$path" deny; then PRUNED+=("$path"); else STOP="cannot untag '$path'"; fi
            else
                mf tag -i "$uuid" deny "$TAG" >/dev/null || STOP="cannot untag '$path'"
            fi
            ;;
        m)
            # The children stay in the walk; only the marker is written.
            mf tag -i "$uuid" mixed "$TAG" >/dev/null || STOP="cannot mark '$path' mixed"
            ;;
        s)
            SKIPPED=$((SKIPPED + 1))
            [ "$kind" = dir ] && PRUNED+=("$path")
            ;;
        *) STOP=user ;;
    esac
done <"$WALK"

case $STOP in
    "")   mf_gui_finish "done tagging '$TAG' ($SKIPPED skipped, $ALREADY already decided)." ;;
    user) mf_gui_finish "stopped tagging '$TAG' ($SKIPPED skipped, $ALREADY already decided)." ;;
    *)    mf_gui_finish "tagging '$TAG' aborted: $STOP"; exit 1 ;;
esac
