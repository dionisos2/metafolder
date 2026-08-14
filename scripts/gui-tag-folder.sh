#!/usr/bin/env bash
# Bulk-apply one tag over a folder subtree, in the running metafolder GUI.
# Given a TAG (a "/"-separated tag path) and a FOLDER, asks whether the folder
# carries the tag; three answers:
#
#   y (oui)   -> add the tag (via a `tags` ref) to the folder's metarecord AND
#                to every tracked file/dir under it (mfr_path ->* folder).
#   n (non)   -> the same, but on `negative_tags`.
#   m (mixed) -> add the tag to the folder's `mixed_tags` field ONLY, then
#                descend: ask again for each direct child (files: y/n only;
#                sub-dirs: y/n/m). Mixed sub-dirs are processed in turn until no
#                unprocessed mixed folder remains.
#
# During a positive apply, more general tags are dropped (adding musique/jazz
# removes musique). During a negative apply, more specific negatives are dropped
# instead (adding negative musique removes negative musique/jazz) — a generic
# negative subsumes the specific ones.
#
# Data model (same tag entries as the other gui-tag-* scripts): the universe is
# the repository's `type = "tag"` metarecords, whose `name` is the path; files
# reference them through the multi-map Ref fields `tags` / `negative_tags` /
# `mixed_tags`. Operates on TRACKED metarecords only — reconcile the folder
# first if you want everything under it to be covered.
#
# Usage: gui-tag-folder.sh <tag> <folder>

# --- pure path helpers (also used by scripts/test-gui-tag-folder.sh) --------

# tag_ancestors <tag> : proper ancestor paths, deepest-first (one per line).
tag_ancestors() {
    local p=$1
    while [[ "$p" == */* ]]; do p=${p%/*}; printf '%s\n' "$p"; done
}

# tag_descendants <tag> : of the candidate names read on stdin (one per line),
# those strictly under <tag> (path prefix "<tag>/").
tag_descendants() {
    local t=$1 n
    while IFS= read -r n || [ -n "$n" ]; do
        case "$n" in "$t"/*) printf '%s\n' "$n" ;; esac
    done
}

# --- everything below runs only when executed, not when sourced -------------

main() {
    set -euo pipefail

    die() { echo "error: $*" >&2; exit 1; }
    [ $# -eq 2 ] || die "usage: $0 <tag> <folder>"
    TAG=$1
    FOLDER=$2
    case $TAG in *\"*) die "tag names must not contain double quotes" ;; esac

    REPO=$(mf gui repo) || die "no GUI running or no repository in the focused workspace"
    export METAFOLDER_REPO="$REPO"

    FOLDER_ABS=$(readlink -f -- "$FOLDER") || die "no such folder: $FOLDER"
    [ -d "$FOLDER_ABS" ] || die "not a directory: $FOLDER_ABS"
    FOLDER_UUID=$(mf track "$FOLDER_ABS") || die "cannot track $FOLDER_ABS (inside the repo root?)"
    FOLDER_TP=$(mf path --relative "$FOLDER_UUID")

    # Take over the layout: one workspace, file (left) + metarecord-detail (right).
    SAVED_LEFT=$(mf gui layout left)
    SAVED_RIGHT=$(mf gui layout right)
    WS=$(mf gui workspace new --repo "$REPO")
    cleanup() {
        mf gui workspace rm "$WS" >/dev/null 2>&1 || true
        mf gui layout left "$SAVED_LEFT" >/dev/null 2>&1 || true
        mf gui layout right "$SAVED_RIGHT" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    mf gui layout left "$WS" >/dev/null
    mf gui layout right "$WS" >/dev/null
    mf gui view right metarecord-detail >/dev/null

    # Build the tag universe once: name<->uuid + the full name list.
    declare -A NAME2UUID
    ALL_NAMES=()
    local tu nm
    while IFS= read -r tu || [ -n "$tu" ]; do
        [ -n "$tu" ] || continue
        nm=$(mf metarecord -i "$tu" field get name | head -n1)
        [ -n "$nm" ] || continue
        NAME2UUID["$nm"]=$tu
        ALL_NAMES+=("$nm")
    done < <(mf metarecord -q 'type = "tag"' get)

    # Resolve the tag entry, creating it if the vocabulary does not have it yet.
    T_UUID=${NAME2UUID[$TAG]:-}
    if [ -z "$T_UUID" ]; then
        T_UUID=$(mf metarecord add type:string=tag "name:string=$TAG")
        NAME2UUID["$TAG"]=$T_UUID
        ALL_NAMES+=("$TAG")
        mf gui message "created tag entry '$TAG'" --timeout-ms 3000 >/dev/null
    fi

    # Fixed removal sets for T (only entries that actually exist):
    #  positive apply -> drop ancestor tags; negative apply -> drop descendant tags.
    ANCESTORS=()
    local a
    while IFS= read -r a; do [ -n "${NAME2UUID[$a]:-}" ] && ANCESTORS+=("$a"); done \
        < <(tag_ancestors "$TAG")
    DESCENDANTS=()
    local d
    while IFS= read -r d; do DESCENDANTS+=("$d"); done \
        < <(printf '%s\n' "${ALL_NAMES[@]}" | tag_descendants "$TAG")

    # apply <uuid> <treepath> <field> <removal-path...>
    # Idempotently add T (delete-then-add avoids duplicate rows) to the record
    # and its whole subtree, then drop the subsumed tags across the same scope.
    apply() {
        local uuid=$1 tp=$2 field=$3; shift 3
        local r ru
        for scope in self sub; do
            local sel; [ "$scope" = self ] && sel=(-i "$uuid") || sel=(-q "mfr_path ->* \"$tp\"")
            mf metarecord "${sel[@]}" field delete "$field:ref=$T_UUID" >/dev/null 2>&1 || true
            mf metarecord "${sel[@]}" field add "$field:ref=$T_UUID" >/dev/null
            for r in "$@"; do
                ru=${NAME2UUID[$r]:-}; [ -n "$ru" ] || continue
                mf metarecord "${sel[@]}" field delete "$field:ref=$ru" >/dev/null 2>&1 || true
            done
        done
    }

    set_mixed() { # add T to the folder's mixed_tags only (idempotent)
        mf metarecord -i "$1" field delete "mixed_tags:ref=$T_UUID" >/dev/null 2>&1 || true
        mf metarecord -i "$1" field add "mixed_tags:ref=$T_UUID" >/dev/null
    }

    show() { # show <abs> in the panels (also publishes selected_metarecord)
        [ -n "$1" ] && mf gui view left file --path "$1" >/dev/null 2>&1 || true
    }

    # handle_dir / handle_file return 1 to signal "stop the whole run".
    handle_dir() { # <uuid> <treepath> <abs>
        show "$3"
        mf gui message "'$2' has tag '$TAG'?   [y] oui   [n] non   [m] mixed   [Esc] stop" \
            --workspace "$WS" >/dev/null
        case "$(mf gui input y n m escape 2>/dev/null || echo escape)" in
            y) apply "$1" "$2" tags "${ANCESTORS[@]}" ;;
            n) apply "$1" "$2" negative_tags "${DESCENDANTS[@]}" ;;
            m) set_mixed "$1"; QUEUE+=("$1") ;;
            *) return 1 ;;
        esac
    }
    handle_file() { # <uuid> <treepath> <abs>
        show "$3"
        mf gui message "'$2' has tag '$TAG'?   [y] oui   [n] non   [Esc] stop" \
            --workspace "$WS" >/dev/null
        case "$(mf gui input y n escape 2>/dev/null || echo escape)" in
            y) apply "$1" "$2" tags "${ANCESTORS[@]}" ;;
            n) apply "$1" "$2" negative_tags "${DESCENDANTS[@]}" ;;
            *) return 1 ;;
        esac
    }

    # Ask about the top folder; recurse into mixed folders breadth-first.
    QUEUE=()
    stop=""
    handle_dir "$FOLDER_UUID" "$FOLDER_TP" "$FOLDER_ABS" || stop=1

    local parent parent_tp child ctype cabs ctp
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
}

# Run only when executed directly (sourcing exposes the pure helpers for tests).
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    main "$@"
fi
