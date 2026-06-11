#!/usr/bin/env bash
# Interactive tagging for the metafolder GUI, built on the mf CLI
# (mf gui …, mf path, mf query --values; spec-gui "Scripting / GUI API").
#
# Asks for one tag name (with autocompletion over the existing tag
# entries), then walks every file of the focused workspace's repository
# that has no opinion on that tag yet, shows it in the left panel (its
# metadata in the right panel) and waits for one keypress:
#
#   y      -> the file has the tag      (adds a `tags` ref)
#   n      -> the file does NOT have it (adds a `negative_tags` ref)
#   s      -> skip this file
#   Escape -> stop the script
#
# Data model: each tag is its own entry (type = "tag", name = <tag>);
# files reference tag entries through the multi-map Ref fields `tags` and
# `negative_tags`. A tag hierarchy can be built later by giving tag
# entries a TreeRef field (e.g. tag_path); queries then compose as
#   mf query 'tags -> (tag_path ->* "genre/jazz")'
# (tree paths start with the root entry's name; only the filesystem root
# is named "", which is why mfr_path paths start with "/")
#
# Files already referencing the tag (either way) are excluded from the
# walk, so the script can be interrupted and resumed. Skipped files come
# back on the next run.
#
# Tag names must not contain double quotes (they are interpolated into
# the query DSL).

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

# Target the repository shown in the focused workspace.
REPO=$(mf gui repo) || die "no GUI running or no repository in the focused workspace"
export METAFOLDER_REPO="$REPO"

# Take over the layout: one workspace in both slots, file + entry-detail.
SAVED_LEFT=$(mf gui layout left)
SAVED_RIGHT=$(mf gui layout right)
WS=$(mf gui workspace new --repo "$REPO")
cleanup() {
    mf gui workspace rm "$WS" >/dev/null 2>&1 || true
    mf gui layout left "$SAVED_LEFT" >/dev/null 2>&1 || true
    mf gui layout right "$SAVED_RIGHT" >/dev/null 2>&1 || true
}
trap cleanup EXIT
mf gui layout left "$WS"
mf gui layout right "$WS"
mf gui view right entry-detail

# Ask for the tag name, completing over the existing tag entries.
TAG=$(mf query 'type = "tag"' --select name --values | sort -u |
      mf gui prompt "Tag name: " --completions-stdin) || die "cancelled"
[ -n "$TAG" ] || die "empty tag name"
case $TAG in *\"*) die "tag names must not contain double quotes" ;; esac

# Get or create the tag entry.
TAG_COND="(type = \"tag\" AND name = \"$TAG\")"
TAG_UUID=$(mf query "type = \"tag\" AND name = \"$TAG\"" | head -n1)
if [ -z "$TAG_UUID" ]; then
    TAG_UUID=$(mf create --field type:string=tag --field "name:string=$TAG")
    mf gui message "created tag entry '$TAG'" --workspace "$WS" --timeout-ms 3000
fi

# Files with no opinion on this tag yet. NOT() is a complement within the
# repository, so files where tags/negative_tags are unknown are included.
PREDICATE="mfr_path IS PRESENT AND mfr_type = \"file\" \
AND NOT (tags -> $TAG_COND OR negative_tags -> $TAG_COND)"

yes=0 no=0 skipped=0
while read -r uuid; do
    abs=$(mf path "$uuid") || continue # the file disappeared meanwhile
    rel=$(mf path --relative "$uuid")
    mf gui view left file --path "$abs"
    mf gui message "[y] $TAG   [n] not $TAG   [s] skip   [Esc] quit — $rel" \
       --workspace "$WS"
    key=$(mf gui input y n s escape) || break # timeout / GUI closed
    case $key in
        y) mf add "$uuid" "tags:ref=$TAG_UUID" >/dev/null; yes=$((yes + 1)) ;;
        n) mf add "$uuid" "negative_tags:ref=$TAG_UUID" >/dev/null; no=$((no + 1)) ;;
        s) skipped=$((skipped + 1)) ;;
        *) break ;; # escape
    esac
done < <(mf query "$PREDICATE")

SUMMARY="Tagging '$TAG' done: $yes yes, $no no, $skipped skipped"
mf gui message "$SUMMARY" --timeout-ms 5000
echo "$SUMMARY"
