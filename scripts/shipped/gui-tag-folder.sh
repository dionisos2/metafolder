#!/usr/bin/env bash
# Summary: Bulk-apply one tag over a folder subtree (yes/no/mixed walk).
# Bulk-apply one tag over a folder subtree, in the running metafolder GUI.
# Given a TAG (a "/"-separated tag path) and a FOLDER, asks whether the folder
# carries the tag; three answers:
#
#   y (oui)   -> `mf tag add` on the folder AND its whole subtree
#                (mfr_path ->* folder).
#   n (non)   -> `mf tag deny` on the same scope.
#   m (mixed) -> `mf tag mixed` on the folder only, then descend: ask again for
#                each direct child (files: y/n/s; sub-dirs: y/n/m/s). Mixed
#                sub-dirs are processed in turn until none remains unprocessed.
#   s (skip)  -> leave this entry alone: no tag op, and for a folder no descent
#                either — the whole subtree is left for another run.
#
# The arrow keys answer as well: → yes, ← no, ↑ mixed, ↓ skip.
#
# Every question carries what is left to answer ("— 7 left, 2 folders to open")
# and the GUI task bar shows the same as a bar. Neither is a total known in
# advance: the walk discovers a folder's contents only when a "mixed" answer
# opens it, so the count is the entries left in the folder being walked plus one
# per mixed folder still to open — a lower bound that rises as the walk goes
# deeper and lands on the real figure at the end.
#
# Resumable: an entry whose answer is already recorded is not asked again. The
# record carries the tag (`tag`, exactly or through a more specific tag), carries
# its negation (`negative_tag`, exactly or through a more general one), or is a
# `mixed_tag` folder — which is descended into straight away, no question. So a
# run interrupted halfway (skip, stop, Escape) is continued by re-running the
# same command, and only the open questions come back. `--redo` asks everything
# again, decided or not — the way to revise a wrong answer over a subtree.
#
# `mf tag` owns the tag model: it creates the entry if the vocabulary lacks it,
# adds the ref idempotently, and applies the subsumption/exclusivity rewrites
# (add drops the more general ancestor tags, deny drops the more specific
# descendant negatives). So this script is only the folder walk + the y/n/m
# questions — no tag bookkeeping of its own.
#
# Operates on TRACKED metarecords only — reconcile the folder first if you want
# everything under it covered.
#
# Both arguments are optional: a missing one is asked in the GUI with
# completion (the tag over the vocabulary, the folder over the repository's
# tracked directories).
#
# Usage: gui-tag-folder.sh [--redo] [<tag> [<folder>]]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"
# Reuse the pure hierarchy helpers (in_set / has_ancestor_in / has_descendant_in)
# that decide whether a recorded tag already answers our question; its `main`
# guard keeps the selector itself inert when sourced.
# shellcheck source=gui-tag-next.sh
source "$HERE/gui-tag-next.sh"

# `--redo`: ask every entry again, ignoring the answers already recorded.
# (`&&` here would end the script under `set -e` whenever the flag is absent.)
REDO=0
if [ "${1:-}" = "--redo" ]; then
    REDO=1
    shift
fi
[ $# -le 2 ] || mf_die "usage: $0 [--redo] [<tag> [<folder>]]"
TAG=${1:-}
FOLDER=${2:-}

mf_gui_bind_repo

# Tag: from the command line, or prompted with completion over the vocabulary.
[ -n "$TAG" ] || TAG=$(mf_gui_prompt_tag "Tag: ") || mf_die "cancelled"
[ -n "$TAG" ] || mf_die "empty tag name"
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

# Folder: a command-line argument is a filesystem path (tracked on the fly); a
# prompted value is an in-repo tree-path chosen from the folder completion.
if [ -n "$FOLDER" ]; then
    FOLDER_ABS=$(readlink -f -- "$FOLDER") || mf_die "no such folder: $FOLDER"
    [ -d "$FOLDER_ABS" ] || mf_die "not a directory: $FOLDER_ABS"
    FOLDER_UUID=$(mf track "$FOLDER_ABS") || mf_die "cannot track $FOLDER_ABS (inside the repo root?)"
else
    FOLDER_TP=$(mf_gui_prompt_folder "Folder: ") || mf_die "cancelled"
    [ -n "$FOLDER_TP" ] || mf_die "empty folder"
    FOLDER_UUID=$(mf_gui_path_uuid "$FOLDER_TP")
    [ -n "$FOLDER_UUID" ] || mf_die "no tracked folder at $FOLDER_TP"
fi
FOLDER_TP=$(mf path --relative "$FOLDER_UUID")
# Absolute filesystem path for the preview (unset in the prompted branch, which
# never touched the filesystem): derive it from the uuid, like the child walk.
FOLDER_ABS=$(mf path "$FOLDER_UUID" 2>/dev/null || true)

mf_gui_session_open metarecord-detail

TMP=$(mf_gui_tmpdir)
POS="$TMP/pos"
NEG="$TMP/neg"
MIX="$TMP/mix"

# The answer already recorded for TAG on a metarecord, or nothing when the
# question is still open. Subsumption is the one `mf tag` applies when writing:
# a more specific positive implies TAG, a more general negative denies it.
# One round-trip per field, stopping at the first hit — so an entry that is
# already tagged costs a single call.
decided() { # <uuid> -> y | n | m | ""
    [ "$REDO" = 0 ] || return 0
    mf metarecord -i "$1" field get tag --resolve path >"$POS"
    if in_set "$TAG" "$POS" || has_descendant_in "$TAG" "$POS"; then printf y; return 0; fi
    mf metarecord -i "$1" field get negative_tag --resolve path >"$NEG"
    if in_set "$TAG" "$NEG" || has_ancestor_in "$TAG" "$NEG"; then printf n; return 0; fi
    mf metarecord -i "$1" field get mixed_tag --resolve path >"$MIX"
    if in_set "$TAG" "$MIX"; then printf m; fi
    return 0
}

# Apply T over a node and its whole subtree (self + descendants). One `mf tag`
# call per scope; the subsumption is handled server-side across the whole set.
apply_tree() { # <uuid> <treepath> <verb: add|deny>
    mf tag -i "$1" "$3" "$TAG" >/dev/null \
        && mf tag -q "mfr_path ->* \"$(mf_gui_query_path "$2")\"" "$3" "$TAG" >/dev/null
}

# How the walk ended: "" = still going, "user" = Escape, anything else is an
# ERROR MESSAGE. The two must stay apart. Bash disables `set -e` wherever a
# failure is tested, so a `mf tag` that failed inside a handler used to surface
# only as "the handler returned non-zero" — indistinguishable from Escape, and
# the run ended with a cheerful "stopped." and exit 0 (spec-gui "Script
# session"). Now a failed tag op aborts loudly with its own message.
STOP=""
SKIPPED=0
ALREADY=0
QUEUE=()

# How far the walk has got. A folder's whole subtree is discovered one listing at
# a time (a "mixed" answer is what reveals the next one), so there is no total to
# know up front: DONE counts the entries visited, LEFT_HERE the ones still to
# visit in the listing being walked (the current one included), and each mixed
# folder waiting in QUEUE stands for at least one more entry. DONE + LEFT_HERE -
# 1 + |QUEUE| is therefore a *lower bound* on the work — the bar's total grows
# whenever a mixed answer opens a new folder, and lands exactly on DONE at the
# end.
DONE=0
LEFT_HERE=1

# Ask about one entry and apply the answer. For a folder, yes/no cover the whole
# subtree, mixed descends, skip leaves the subtree untouched.
handle() { # <uuid> <treepath> <abs> <dir|file>
    local uuid=$1 tp=$2 abs=$3 kind=$4 answer prior left counter
    left=$((LEFT_HERE + ${#QUEUE[@]}))
    DONE=$((DONE + 1))
    # Report progress before the decision, so a resume scanning past entries it
    # has already answered still moves the bar instead of looking frozen.
    mf_gui_progress --done "$DONE" --total "$((DONE + left - 1))" --phase "$tp"
    # Already answered in an earlier run: no question, no tag op. A mixed folder
    # is still descended into — that is where its remaining questions live.
    prior=$(decided "$uuid")
    case $prior in
        y | n) ALREADY=$((ALREADY + 1)); return 0 ;;
        m)
            ALREADY=$((ALREADY + 1))
            [ "$kind" = dir ] && QUEUE+=("$uuid")
            return 0 ;;
    esac
    mf_gui_show_file "$abs"
    # What is left to answer, spelled out next to the question: the entries of
    # this folder, then the mixed folders still to open (each one a listing of
    # its own, so its children cannot be counted yet).
    counter="$LEFT_HERE left"
    case ${#QUEUE[@]} in
        0) ;;
        1) counter="$counter, 1 folder to open" ;;
        *) counter="$counter, ${#QUEUE[@]} folders to open" ;;
    esac
    if [ "$kind" = dir ]; then
        answer=$(mf_gui_ask_answer \
            "'$tp' has tag '$TAG'?   [y →] oui   [n ←] non   [m ↑] mixed   [s ↓] skip   [q] stop   — $counter" \
            y n m s q)
    else
        answer=$(mf_gui_ask_answer \
            "'$tp' has tag '$TAG'?   [y →] oui   [n ←] non   [s ↓] skip   [q] stop   — $counter" \
            y n s q)
    fi
    case $answer in
        y)
            if [ "$kind" = dir ]; then
                apply_tree "$uuid" "$tp" add || STOP="cannot tag '$tp'"
            else
                mf tag -i "$uuid" add "$TAG" >/dev/null || STOP="cannot tag '$tp'"
            fi ;;
        n)
            if [ "$kind" = dir ]; then
                apply_tree "$uuid" "$tp" deny || STOP="cannot untag '$tp'"
            else
                mf tag -i "$uuid" deny "$TAG" >/dev/null || STOP="cannot untag '$tp'"
            fi ;;
        m)
            if mf tag -i "$uuid" mixed "$TAG" >/dev/null; then
                QUEUE+=("$uuid")
            else
                STOP="cannot mark '$tp' mixed"
            fi ;;
        s) SKIPPED=$((SKIPPED + 1)) ;;   # a folder's whole subtree, untouched
        *) STOP=user ;;
    esac
}

# Ask about the top folder; recurse into mixed folders breadth-first.
LEFT_HERE=1
handle "$FOLDER_UUID" "$FOLDER_TP" "$FOLDER_ABS" dir

while [ -z "$STOP" ] && [ ${#QUEUE[@]} -gt 0 ]; do
    parent=${QUEUE[0]}; QUEUE=("${QUEUE[@]:1}")
    parent_tp=$(mf path --relative "$parent")
    # The whole listing up front, so the walk knows how many entries this folder
    # still owes an answer (`mapfile`, like gui-tag-pair.sh's worklist).
    mapfile -t children < <(mf metarecord -q "mfr_path -> \"$(mf_gui_query_path "$parent_tp")\"" get)
    seen=0
    for child in ${children+"${children[@]}"}; do
        [ -n "$child" ] || continue
        [ -z "$STOP" ] || break
        seen=$((seen + 1))
        LEFT_HERE=$((${#children[@]} - seen + 1))
        # No `| head -n1` here: with `pipefail`, head closing the pipe early can
        # fail the whole read and kill the run.
        ctype=$(mf metarecord -i "$child" field get mfr_type)
        ctype=${ctype%%$'\n'*}
        ctp=$(mf path --relative "$child")
        cabs=$(mf path "$child" 2>/dev/null || true)
        if [ "$ctype" = dir ]; then
            handle "$child" "$ctp" "$cabs" dir
        else
            handle "$child" "$ctp" "$cabs" file
        fi
    done
done

case $STOP in
    "")   mf_gui_finish "done tagging '$TAG' under $FOLDER_TP ($SKIPPED skipped, $ALREADY already decided)." ;;
    user) mf_gui_finish "stopped tagging '$TAG' under $FOLDER_TP ($SKIPPED skipped, $ALREADY already decided)." ;;
    *)    mf_gui_finish "tagging '$TAG' aborted: $STOP"; exit 1 ;;
esac
