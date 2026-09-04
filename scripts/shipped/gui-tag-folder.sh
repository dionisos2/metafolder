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
# Usage: gui-tag-folder.sh [<tag> [<folder>]]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

[ $# -le 2 ] || mf_die "usage: $0 [<tag> [<folder>]]"
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
QUEUE=()

# Ask about one entry and apply the answer. For a folder, yes/no cover the whole
# subtree, mixed descends, skip leaves the subtree untouched.
handle() { # <uuid> <treepath> <abs> <dir|file>
    local uuid=$1 tp=$2 abs=$3 kind=$4 answer
    mf_gui_show_file "$abs"
    mf_gui_progress --phase "$tp"
    if [ "$kind" = dir ]; then
        answer=$(mf_gui_ask_answer \
            "'$tp' has tag '$TAG'?   [y →] oui   [n ←] non   [m ↑] mixed   [s ↓] skip   [Esc] stop" \
            y n m s escape)
    else
        answer=$(mf_gui_ask_answer \
            "'$tp' has tag '$TAG'?   [y →] oui   [n ←] non   [s ↓] skip   [Esc] stop" \
            y n s escape)
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
handle "$FOLDER_UUID" "$FOLDER_TP" "$FOLDER_ABS" dir

while [ -z "$STOP" ] && [ ${#QUEUE[@]} -gt 0 ]; do
    parent=${QUEUE[0]}; QUEUE=("${QUEUE[@]:1}")
    parent_tp=$(mf path --relative "$parent")
    while IFS= read -r child || [ -n "$child" ]; do
        [ -n "$child" ] || continue
        [ -z "$STOP" ] || break
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
    done < <(mf metarecord -q "mfr_path -> \"$(mf_gui_query_path "$parent_tp")\"" get)
done

case $STOP in
    "")   mf_gui_finish "done tagging '$TAG' under $FOLDER_TP ($SKIPPED skipped)." ;;
    user) mf_gui_finish "stopped tagging '$TAG' under $FOLDER_TP ($SKIPPED skipped)." ;;
    *)    mf_gui_finish "tagging '$TAG' aborted: $STOP"; exit 1 ;;
esac
