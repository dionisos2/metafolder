#!/usr/bin/env bash
# Interactive hierarchical-tag classification of ONE metarecord, in the running
# metafolder GUI. Shows the file (left `file` panel) and its metadata (right
# `metarecord-detail` panel), then asks a descending series of questions
# "add tag <path> ?" chosen by scripts/gui-tag-next.sh:
#
#   y      -> the file HAS the tag       (adds a `tags` ref)
#   n      -> the file does NOT have it  (adds a `negative_tags` ref)
#   Escape -> stop
#
# The question series starts at the top-level tags ("musique", "administratif"),
# then, for each tag the file ends up with, offers the deeper ones
# ("musique/jazz"). Adding a specific tag drops the more general ancestor
# (musique/jazz removes musique). A negative answer is symmetric: a generic
# negative ("not musique") makes the more specific negatives ("not musique/jazz")
# redundant, so they are removed and its whole subtree is never asked about.
#
# Data model (same tag entries as scripts/gui-tag-pair.sh):
#   - the tag universe = the repository's `type = "tag"` metarecords, whose
#     `name` field is the "/"-separated path;
#   - the file references them through the multi-map Ref fields `tags` and
#     `negative_tags`.
# Exclusivity is data-driven, read from the tag entries themselves:
#   - `partition = true`  on a parent tag -> its direct children are mutually
#     exclusive (a file gets at most one);
#   - `exclusive = true`  on a child tag  -> adding it forbids any sibling,
#     even if the parent is not a partition.
# When a chosen tag is exclusive, its already-present siblings are removed and
# the branch is closed (no more sibling questions).
#
# The flow is resumable: questions already answered (either way) are skipped, so
# the script can be interrupted and re-run on the same metarecord until there is
# nothing left to ask.
#
# Usage: gui-tag-classify.sh <metarecord-uuid>

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# Reuse the pure selector and its hierarchy helpers (tag_parent, is_ancestor,
# gui_tag_next). Its `main` guard keeps it inert when sourced.
# shellcheck source=gui-tag-next.sh
source "$HERE/gui-tag-next.sh"

die() { echo "error: $*" >&2; exit 1; }

[ $# -eq 1 ] || die "usage: $0 <metarecord-uuid>"
UUID=$1

# Target the repository shown in the focused workspace.
REPO=$(mf gui repo) || die "no GUI running or no repository in the focused workspace"
export METAFOLDER_REPO="$REPO"

# The metarecord must resolve to a file path (mfr_path present).
ABS=$(mf path "$UUID") || die "metarecord $UUID has no file path (mfr_path present?)"

TMP=$(mktemp -d)

# Take over the layout: one workspace in both slots, file + metarecord-detail.
SAVED_LEFT=$(mf gui layout left)
SAVED_RIGHT=$(mf gui layout right)
WS=$(mf gui workspace new --repo "$REPO")
cleanup() {
    mf gui workspace rm "$WS" >/dev/null 2>&1 || true
    mf gui layout left "$SAVED_LEFT" >/dev/null 2>&1 || true
    mf gui layout right "$SAVED_RIGHT" >/dev/null 2>&1 || true
    rm -rf "$TMP" 2>/dev/null || true
}
trap cleanup EXIT
mf gui layout left "$WS"
mf gui layout right "$WS"
mf gui view right metarecord-detail
# Setting the file view also publishes `selected_metarecord` for this workspace
# (the GUI resolves the path back to its metarecord), which the detail panel
# follows — no extra plumbing needed.
mf gui view left file --path "$ABS"

# --- build the tag universe once --------------------------------------------

declare -A NAME2UUID UUID2NAME IS_PARTITION IS_EXCLUSIVE
UNIVERSE="$TMP/universe"

# Exclusivity flag sets (by tag path).
while IFS= read -r p || [ -n "$p" ]; do
    [ -n "$p" ] && IS_PARTITION["$p"]=1
done < <(mf metarecord -q 'type = "tag" AND partition = true' get --select name --values)
while IFS= read -r p || [ -n "$p" ]; do
    [ -n "$p" ] && IS_EXCLUSIVE["$p"]=1
done < <(mf metarecord -q 'type = "tag" AND exclusive = true' get --select name --values)

: >"$UNIVERSE"
while IFS= read -r tuuid || [ -n "$tuuid" ]; do
    [ -n "$tuuid" ] || continue
    name=$(mf metarecord -i "$tuuid" field get name | head -n1)
    [ -n "$name" ] || continue
    NAME2UUID["$name"]=$tuuid
    UUID2NAME["$tuuid"]=$name
    printf '%s\t%s\t%s\n' "$name" "${IS_PARTITION[$name]:-0}" "${IS_EXCLUSIVE[$name]:-0}" >>"$UNIVERSE"
done < <(mf metarecord -q 'type = "tag"' get)

[ -s "$UNIVERSE" ] || die "no tag entries (type = \"tag\") in repository $REPO"

# drv_is_exclusive <path>: the tag excludes its siblings (own `exclusive`, or
# its parent is a `partition`). Mirrors gui-tag-next.sh's internal is_exclusive.
drv_is_exclusive() {
    local t=$1 par
    [ "${IS_EXCLUSIVE[$t]:-0}" = 1 ] && return 0
    par=$(tag_parent "$t")
    [ -n "$par" ] && [ "${IS_PARTITION[$par]:-0}" = 1 ]
}

# read_refs <field> <out-file>: write the metarecord's Ref values of <field> as
# tag paths (one per line), skipping refs that are not known tag entries.
read_refs() {
    local field=$1 out=$2 ru nm
    : >"$out"
    while IFS= read -r ru || [ -n "$ru" ]; do
        [ -n "$ru" ] || continue
        nm=${UUID2NAME[$ru]:-}
        [ -n "$nm" ] && printf '%s\n' "$nm" >>"$out"
    done < <(mf metarecord -i "$UUID" field get "$field" 2>/dev/null || true)
}

# --- question loop ----------------------------------------------------------

POS="$TMP/pos"
NEG="$TMP/neg"
yes=0 no=0

while :; do
    read_refs tags "$POS"
    read_refs negative_tags "$NEG"

    T=$(gui_tag_next "$UNIVERSE" "$POS" "$NEG") || break   # no question left
    tuuid=${NAME2UUID[$T]}

    mf gui message "add tag '$T' ?   [y] oui   [n] non   [Esc] stop" --workspace "$WS"
    key=$(mf gui input y n escape) || break                # timeout / GUI closed
    case $key in
        y)
            mf metarecord -i "$UUID" field add "tags:ref=$tuuid" >/dev/null
            # Adding a specific tag drops its more general ancestors.
            while IFS= read -r a || [ -n "$a" ]; do
                [ -n "$a" ] || continue
                is_ancestor "$a" "$T" &&
                    mf metarecord -i "$UUID" field delete "tags:ref=${NAME2UUID[$a]}" >/dev/null
            done <"$POS"
            # Exclusive tag: drop the siblings it forbids.
            if drv_is_exclusive "$T"; then
                parT=$(tag_parent "$T")
                while IFS= read -r s || [ -n "$s" ]; do
                    [ -n "$s" ] || continue
                    [ "$s" != "$T" ] && [ "$(tag_parent "$s")" = "$parT" ] &&
                        mf metarecord -i "$UUID" field delete "tags:ref=${NAME2UUID[$s]}" >/dev/null
                done <"$POS"
            fi
            yes=$((yes + 1)) ;;
        n)
            mf metarecord -i "$UUID" field add "negative_tags:ref=$tuuid" >/dev/null
            # A generic negative makes its more specific negatives redundant.
            while IFS= read -r d || [ -n "$d" ]; do
                [ -n "$d" ] || continue
                is_ancestor "$T" "$d" &&
                    mf metarecord -i "$UUID" field delete "negative_tags:ref=${NAME2UUID[$d]}" >/dev/null
            done <"$NEG"
            no=$((no + 1)) ;;
        *) break ;; # escape
    esac
done

SUMMARY="Classification de $UUID terminée : $yes oui, $no non"
mf gui message "$SUMMARY" --timeout-ms 5000
echo "$SUMMARY"
