#!/usr/bin/env bash
# Summary: Example/template — classify then sort a folder's files by metadata.
# EXAMPLE — a *template* to copy and adapt, not a finished tool. It walks a
# folder, and for each entry: (1) fully classifies its tags with
# gui-tag-classify.sh, (2) asks a few extra field questions, then (3) moves the
# file to a destination computed from the resulting metadata.
#
# The end goal is your own one-shot sorter: point it at e.g. ~/a_trier, answer
# the questions per file, and each file lands where it belongs.
#
# Assumptions: the metafolder GUI is running with a repository in the focused
# workspace, and SORT_DIR is inside that repository's root (so files can be
# tracked). Moving a file OUT of the repo makes the watcher blank its mfr_path;
# the metarecord (and the tags you set) is kept — only the file becomes
# untracked at its new home. Adapt the paths/rules below to your own tree.
#
# Usage: example-gui-sort-folder.sh [SORT_DIR]      (default: ~/a_trier)
#   DEST_ROOT (env) — where sorted files go         (default: ~/divertissement)

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"
SORT_DIR=${1:-$HOME/a_trier}
DEST_ROOT=${DEST_ROOT:-$HOME/divertissement}

mf_gui_bind_repo
[ -d "$SORT_DIR" ] || mf_die "not a directory: $SORT_DIR"

# The metarecord uuid of a path. `mf track` is idempotent: it creates the
# metarecord if needed and always prints the uuid (existing or new).
ensure_metarecord() { mf track "$1"; }

# ask "<question>" -> prints y | n | q  ("escape" too, when the question could
# not be answered at all: a timeout, a closed GUI). Anything but y/n stops.
ask() { mf_gui_ask_answer "$1" y n q; }

# The tag paths a metarecord references through its `tag` field — resolved in
# one round-trip (each ref → the tag entry's `path` TreeRef → its path string).
record_tag_names() { mf metarecord -i "$1" field get tag --resolve path 2>/dev/null; }

# --- Routing rules: the heart you customise. Return a destination dir, or ""
# to leave the file in place. Reads $TAGS (see below) and any scalar field. ---
has_tag() { printf '%s\n' "$TAGS" | grep -qxF -- "$1"; }

destination() {
    local uuid=$1 rate
    rate=$(mf metarecord -i "$uuid" field get rate 2>/dev/null | head -n1)
    # Most specific rules first; combine tags and other fields freely.
    if has_tag "musique/jazz"; then
        if [ -n "$rate" ] && [ "$rate" -lt 3 ]; then
            printf '%s' "$DEST_ROOT/musique/mauvais/jazz"   # jazz + rate < 3
        else
            printf '%s' "$DEST_ROOT/musique/jazz"           # jazz otherwise
        fi
    elif has_tag "musique"; then
        printf '%s' "$DEST_ROOT/musique/a_classer"          # music, genre unknown
    elif has_tag "administratif"; then
        printf '%s' "$HOME/administratif"                   # a wholly different tree
    fi
    # no rule matched -> "" -> left in place
}

# --- Per-entry loop ---------------------------------------------------------
stop=""
for abs in "$SORT_DIR"/*; do
    [ -e "$abs" ] || continue                 # empty dir -> the glob is literal
    [ "$(basename "$abs")" = .metafolder ] && continue
    abs=$(readlink -f -- "$abs")

    uuid=$(ensure_metarecord "$abs") || { echo "skip (untrackable): $abs" >&2; continue; }

    # 1) Fully specify the tags (the previous script drives its own display).
    "$HERE/gui-tag-classify.sh" "$uuid"

    # 2) Show the file again for the extra questions.
    mf gui view right metarecord-detail
    mf gui view left file --path "$abs"

    # 3) Extra field questions — three patterns to copy:
    #    (a) a boolean flag: the yes/no answer *is* the value.
    case "$(ask "Mark as favorite?   [y/n]   q = stop")" in
        y) mf metarecord -i "$uuid" field set favorite:bool=true >/dev/null ;;
        n) : ;;
        *) stop=1 ;;
    esac
    [ -n "$stop" ] && break

    #    (b) "add? -> value" for a number.
    case "$(ask "Add a rating?   [y/n]   q = stop")" in
        y) v=$(mf gui prompt "Value for rate (integer): ") || v=""
           [ -n "$v" ] && mf metarecord -i "$uuid" field set "rate:int=$v" >/dev/null ;;
        n) : ;;
        *) stop=1 ;;
    esac
    [ -n "$stop" ] && break

    #    (c) same pattern for free text.
    case "$(ask "Add a comment?   [y/n]   q = stop")" in
        y) v=$(mf gui prompt "Comment: ") || v=""
           [ -n "$v" ] && mf metarecord -i "$uuid" field set "comment:string=$v" >/dev/null ;;
        n) : ;;
        *) stop=1 ;;
    esac
    [ -n "$stop" ] && break

    # 4) Route by metadata and move.
    TAGS=$(record_tag_names "$uuid")
    dest=$(destination "$uuid")
    if [ -n "$dest" ]; then
        mkdir -p "$dest"
        # `mv -n` never overwrites, so an existing destination would be a silent
        # no-op — detect it and report honestly instead of claiming "moved".
        if [ -e "$dest/$(basename "$abs")" ]; then
            mf gui message "skipped $(basename "$abs") — $dest/$(basename "$abs") exists" --timeout-ms 3000
        else
            mv -n -- "$abs" "$dest/"
            mf gui message "moved $(basename "$abs") -> $dest" --timeout-ms 3000
        fi
    else
        mf gui message "no rule matched for $(basename "$abs") — left in place" --timeout-ms 3000
    fi
done

[ -n "$stop" ] && mf_gui_finish "stopped." || mf_gui_finish "done."
