#!/usr/bin/env bash
# Summary: Stop watching a folder and delete every metarecord inside it.
# Stop tracking a folder, in the running metafolder GUI: writes
# `mf_watch = false` on the folder's own metarecord, then deletes every
# metarecord *inside* it (the strict subtree, `mfr_path ->* <folder>`).
#
# The folder's own metarecord is KEPT — it is what carries the `mf_watch =
# false` that stops the watcher (and the reconcile walk) from descending there
# again. Only its contents go. Nothing on the filesystem is touched: this
# removes metadata, not files.
#
# Order matters and is not configurable: `mf_watch` is written first, so the
# watcher has already dropped its watches on the subtree when the deletion
# lands and cannot re-create what we remove. The script then waits
# $MF_UNWATCH_SETTLE seconds (default 1) for the watcher's pending-event buffer
# to drain before deleting.
#
# The metarecords of files that were already gone (`mfr_path = Nothing`) are not
# in the subtree any more, so they are not covered — clear those with
# `mf orphan`.
#
# The argument is optional: a missing one is asked in the GUI with completion
# over the repository's tracked directories. A given one is a FILESYSTEM path
# (tracked on the fly if the folder has no metarecord yet, so `mf_watch` has
# somewhere to live).
#
# Usage: gui-unwatch-folder.sh [<folder>]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

[ $# -le 1 ] || mf_die "usage: $0 [<folder>]"
FOLDER=${1:-}

mf_gui_bind_repo

# Folder: a command-line argument is a filesystem path (tracked on the fly); a
# prompted value is an in-repo tree-path chosen from the folder completion.
if [ -n "$FOLDER" ]; then
    FOLDER_ABS=$(readlink -f -- "$FOLDER") || mf_die "no such folder: $FOLDER"
    [ -d "$FOLDER_ABS" ] || mf_die "not a directory: $FOLDER_ABS"
    FOLDER_UUID=$(mf track "$FOLDER_ABS") || mf_die "cannot track $FOLDER_ABS (inside the repo root?)"
else
    FOLDER_TP=$(mf_gui_prompt_folder "Folder to stop watching: ") || mf_die "cancelled"
    [ -n "$FOLDER_TP" ] || mf_die "empty folder"
    FOLDER_UUID=$(mf_gui_path_uuid "$FOLDER_TP")
    [ -n "$FOLDER_UUID" ] || mf_die "no tracked folder at $FOLDER_TP"
fi
FOLDER_TP=$(mf path --relative "$FOLDER_UUID")
QUERY_PATH=$(mf_gui_query_path "$FOLDER_TP")
SUBTREE="mfr_path ->* \"$QUERY_PATH\""

# What the deletion would take: the strict descendants (the folder itself is
# excluded by `->*`, which is why it survives with its mf_watch = false).
INSIDE=$(mf metarecord -q "$SUBTREE" get | grep -c . || true)

# Deleting metarecords is irreversible (no trash for metadata), so it is
# confirmed — unless there is nothing to delete, in which case unwatching alone
# is harmless and needs no key press.
if [ "$INSIDE" -gt 0 ]; then
    case "$(mf_gui_ask_answer \
        "Stop watching '$FOLDER_TP' and DELETE the $INSIDE metarecords inside it?   [y →] yes   [n ← / Esc] no" \
        y n escape)" in
        y) ;;
        *) echo "cancelled."; exit 0 ;;
    esac
fi

mf metarecord -i "$FOLDER_UUID" field set mf_watch:bool=false >/dev/null

if [ "$INSIDE" -eq 0 ]; then
    echo "not watching '$FOLDER_TP' any more; no metarecord inside it."
    exit 0
fi

# Let the watcher's 500 ms quiet period elapse (and its buffered events flush)
# before the delete, so a late event cannot re-create what we remove.
SETTLE=${MF_UNWATCH_SETTLE:-1}
[ "$SETTLE" = 0 ] || sleep "$SETTLE"

DELETED=$(mf metarecord -q "$SUBTREE" delete --force)
echo "not watching '$FOLDER_TP' any more; deleted $DELETED metarecords inside it."
