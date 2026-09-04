#!/usr/bin/env bash
# Summary: Interactive yes/no tagging of one tag across all files.
# Interactive y/n tagging for ONE tag over a repository's files, in the running
# metafolder GUI. Asks for a tag (autocompleting over the existing vocabulary),
# then walks every file with no opinion on it yet, shows it, and waits for a key:
#
#   y / →  -> the file HAS the tag      (mf tag add)
#   n / ←  -> the file does NOT have it (mf tag deny)
#   s / ↓  -> skip this file
#   Escape -> stop
#
# The tag model (entries + tag/negative_tag refs, TreeRef `path` hierarchy,
# exclusivity) is owned by `mf tag`; this script is just the display + y/n loop.
# Files already referencing the tag either way are excluded, so it is resumable.
# Skipped files come back on the next run.
#
# Usage: gui-tag-pair.sh

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

mf_gui_bind_repo

# Ask for the tag, completing over the existing vocabulary.
TAG=$(mf_gui_prompt_tag "Tag name: ") || mf_die "cancelled"
[ -n "$TAG" ] || mf_die "empty tag name"
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

mf_gui_session_open metarecord-detail

# The tag is identified by its hierarchy path: `path = "<TAG>"` is an exact-node
# match on the `path` TreeRef (a '/'-bearing path resolves to the one node). The
# entry need not pre-exist — a non-matching condition just leaves every file with
# "no opinion"; `mf tag add` creates the entry (and its ancestor chain) on apply.
TAG_COND="(mf_schema = \"tag\" AND path = \"$TAG\")"

# Files with no opinion on this tag yet (NOT() is a complement, so files where
# tag/negative_tag are unknown are included).
PREDICATE="mfr_path IS PRESENT AND mfr_type = \"file\" \
AND NOT (tag -> $TAG_COND OR negative_tag -> $TAG_COND)"

# Collect the whole worklist up front so the progress indicator has a total.
mapfile -t uuids < <(mf metarecord -q "$PREDICATE" get)
total=${#uuids[@]}

yes=0 no=0 skipped=0 i=0
for uuid in "${uuids[@]}"; do
    [ -n "$uuid" ] || continue
    i=$((i + 1))
    abs=$(mf path "$uuid") || continue # the file disappeared meanwhile
    rel=$(mf path --relative "$uuid")
    mf_gui_progress --done "$i" --total "$total" --phase "$rel"
    mf_gui_show_file "$abs"
    case "$(mf_gui_ask_answer \
        "[y →] $TAG   [n ←] not $TAG   [s ↓] skip   [Esc] quit — $rel" y n s escape)" in
        y) mf tag -i "$uuid" add "$TAG" >/dev/null; yes=$((yes + 1)) ;;
        n) mf tag -i "$uuid" deny "$TAG" >/dev/null; no=$((no + 1)) ;;
        s) skipped=$((skipped + 1)) ;;
        *) break ;; # escape
    esac
done

mf_gui_finish "Tagging '$TAG' done: $yes yes, $no no, $skipped skipped"
