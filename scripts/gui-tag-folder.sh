#!/usr/bin/env bash
# Bulk-apply one tag over a folder subtree, in the running metafolder GUI.
# Given a TAG (a "/"-separated tag path) and a FOLDER, asks whether the folder
# carries the tag; three answers:
#
#   y (oui)   -> `mf tag add` on the folder AND its whole subtree
#                (mfr_path ->* folder).
#   n (non)   -> `mf tag deny` on the same scope.
#   m (mixed) -> `mf tag mixed` on the folder only, then descend: ask again for
#                each direct child (files: y/n; sub-dirs: y/n/m). Mixed sub-dirs
#                are processed in turn until none remains unprocessed.
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
# Usage: gui-tag-folder.sh <tag> <folder>

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

[ $# -eq 2 ] || mf_die "usage: $0 <tag> <folder>"
TAG=$1
FOLDER=$2
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

mf_gui_bind_repo

FOLDER_ABS=$(readlink -f -- "$FOLDER") || mf_die "no such folder: $FOLDER"
[ -d "$FOLDER_ABS" ] || mf_die "not a directory: $FOLDER_ABS"
FOLDER_UUID=$(mf track "$FOLDER_ABS") || mf_die "cannot track $FOLDER_ABS (inside the repo root?)"
FOLDER_TP=$(mf path --relative "$FOLDER_UUID")

mf_gui_session_open metarecord-detail

# Apply T over a node and its whole subtree (self + descendants). One `mf tag`
# call per scope; the subsumption is handled server-side across the whole set.
apply_tree() { # <uuid> <treepath> <verb: add|deny>
    mf tag -i "$1" "$3" "$TAG" >/dev/null
    mf tag -q "mfr_path ->* \"$2\"" "$3" "$TAG" >/dev/null
}

# handle_dir / handle_file return 1 to signal "stop the whole run".
handle_dir() { # <uuid> <treepath> <abs>
    mf_gui_show_file "$3"
    case "$(mf_gui_ask "'$2' has tag '$TAG'?   [y] oui   [n] non   [m] mixed   [Esc] stop" y n m escape)" in
        y) apply_tree "$1" "$2" add ;;
        n) apply_tree "$1" "$2" deny ;;
        m) mf tag -i "$1" mixed "$TAG" >/dev/null; QUEUE+=("$1") ;;
        *) return 1 ;;
    esac
}
handle_file() { # <uuid> <treepath> <abs>
    mf_gui_show_file "$3"
    case "$(mf_gui_ask "'$2' has tag '$TAG'?   [y] oui   [n] non   [Esc] stop" y n escape)" in
        y) mf tag -i "$1" add "$TAG" >/dev/null ;;
        n) mf tag -i "$1" deny "$TAG" >/dev/null ;;
        *) return 1 ;;
    esac
}

# Ask about the top folder; recurse into mixed folders breadth-first.
QUEUE=()
stop=""
handle_dir "$FOLDER_UUID" "$FOLDER_TP" "$FOLDER_ABS" || stop=1

while [ -z "$stop" ] && [ ${#QUEUE[@]} -gt 0 ]; do
    parent=${QUEUE[0]}; QUEUE=("${QUEUE[@]:1}")
    parent_tp=$(mf path --relative "$parent")
    while IFS= read -r child || [ -n "$child" ]; do
        [ -n "$child" ] || continue
        ctype=$(mf metarecord -i "$child" field get mfr_type | head -n1)
        ctp=$(mf path --relative "$child")
        cabs=$(mf path "$child" 2>/dev/null || true)
        if [ "$ctype" = dir ]; then
            handle_dir "$child" "$ctp" "$cabs" || { stop=1; break; }
        else
            handle_file "$child" "$ctp" "$cabs" || { stop=1; break; }
        fi
    done < <(mf metarecord -q "mfr_path -> \"$parent_tp\"" get)
done

[ -n "$stop" ] && echo "stopped." || echo "done tagging '$TAG' under $FOLDER_TP."
