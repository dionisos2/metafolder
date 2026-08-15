#!/usr/bin/env bash
# Interactive y/n tagging for ONE tag over a repository's files, in the running
# metafolder GUI. Asks for a tag (autocompleting over the existing vocabulary),
# then walks every file with no opinion on it yet, shows it, and waits for a key:
#
#   y      -> the file HAS the tag      (mf tag add)
#   n      -> the file does NOT have it (mf tag deny)
#   s      -> skip this file
#   Escape -> stop
#
# The tag model (entries + tags/negative_tags refs, hierarchy, exclusivity) is
# owned by `mf tag`; this script is just the display + y/n loop. Files already
# referencing the tag either way are excluded, so it is resumable. Skipped files
# come back on the next run.
#
# Usage: gui-tag-pair.sh

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

mf_gui_bind_repo

# Ask for the tag, completing over the existing vocabulary (mf tag list).
TAG=$(mf tag list | cut -f1 | mf gui prompt "Tag name: " --completions-stdin) \
    || mf_die "cancelled"
[ -n "$TAG" ] || mf_die "empty tag name"
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

mf_gui_session_open metarecord-detail

# The tag entry (created if the vocabulary lacks it), used only to phrase the
# "no opinion yet" query. `--eq` builds it safely; `mf tag` would create the
# entry too, but only when applied to a record.
TAG_UUID=$(mf metarecord --eq type=tag --eq "name=$TAG" get | head -n1)
if [ -z "$TAG_UUID" ]; then
    TAG_UUID=$(mf metarecord add type:string=tag "name:string=$TAG")
    mf gui message "created tag entry '$TAG'" --workspace "$WS" --timeout-ms 3000
fi

# Files with no opinion on this tag yet (NOT() is a complement, so files where
# tags/negative_tags are unknown are included).
TAG_COND="(type = \"tag\" AND name = \"$TAG\")"
PREDICATE="mfr_path IS PRESENT AND mfr_type = \"file\" \
AND NOT (tags -> $TAG_COND OR negative_tags -> $TAG_COND)"

yes=0 no=0 skipped=0
while read -r uuid; do
    abs=$(mf path "$uuid") || continue # the file disappeared meanwhile
    rel=$(mf path --relative "$uuid")
    mf_gui_show_file "$abs"
    case "$(mf_gui_ask "[y] $TAG   [n] not $TAG   [s] skip   [Esc] quit — $rel" y n s escape)" in
        y) mf tag -i "$uuid" add "$TAG" >/dev/null; yes=$((yes + 1)) ;;
        n) mf tag -i "$uuid" deny "$TAG" >/dev/null; no=$((no + 1)) ;;
        s) skipped=$((skipped + 1)) ;;
        *) break ;; # escape
    esac
done < <(mf metarecord -q "$PREDICATE" get)

SUMMARY="Tagging '$TAG' done: $yes yes, $no no, $skipped skipped"
mf gui message "$SUMMARY" --timeout-ms 5000
echo "$SUMMARY"
