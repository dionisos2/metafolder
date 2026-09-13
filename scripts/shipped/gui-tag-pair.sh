#!/usr/bin/env bash
# Summary: Interactive yes/no tagging of one tag across all files.
# Interactive y/n tagging for ONE tag over a set of files, in the running
# metafolder GUI. Takes a tag (autocompleting over the existing vocabulary) and
# a QUERY as the scope, then walks every file of that scope with no opinion on
# the tag yet, shows it, and waits for a key:
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
# THE QUERY IS THE SCOPE (spec-gui "A query is the scope"), as in
# gui-tag-folder.sh: given as the argument, else what the GUI shows
# (`mf gui query`), else a folder chosen from the completion. An empty query is
# every file, which is what this script used to do unconditionally.
#
# Both arguments are optional: a missing tag is asked in the GUI with
# completion over the vocabulary, a missing query resolved as above.
#
# Usage: gui-tag-pair.sh [<tag> [<query>]]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

[ $# -le 2 ] || mf_die "usage: $0 [<tag> [<query>]]"
TAG=${1:-}
QUERY_GIVEN=0
[ $# -ge 2 ] && QUERY_GIVEN=1
QUERY_ARG=${2-}

mf_gui_bind_repo

# The tag: from the command line, or completed over the existing vocabulary.
[ -n "$TAG" ] || TAG=$(mf_gui_prompt_tag "Tag name: ") || mf_die "cancelled"
[ -n "$TAG" ] || mf_die "empty tag name"
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

# The scope, resolved before the session takeover (the scratch workspace it
# opens publishes nothing).
# SCOPE is read by mf_gui_scoped / mf_gui_scope_get, in lib/mf-gui.sh (a
# sourced file this check does not follow from here).
# shellcheck disable=SC2034
if [ "$QUERY_GIVEN" = 1 ]; then
    SCOPE=$QUERY_ARG
else
    SCOPE=$(mf_gui_default_scope "Folder: ") || mf_die "cancelled"
fi

mf_gui_session_open metarecord-detail

TMP=$(mf_gui_tmpdir)

# The tag is identified by its hierarchy path: `path = "<TAG>"` is an exact-node
# match on the `path` TreeRef (a '/'-bearing path resolves to the one node). The
# entry need not pre-exist — a non-matching condition just leaves every file with
# "no opinion"; `mf tag add` creates the entry (and its ancestor chain) on apply.
TAG_COND="(mf_schema = \"tag\" AND path = \"$(mf_dsl_str "$TAG")\")"

# Files of the scope with no opinion on this tag yet (NOT() is a complement, so
# files where tag/negative_tag are unknown are included).
PREDICATE=$(mf_gui_scoped "mfr_path IS PRESENT AND mfr_type = \"file\" \
AND NOT (tag -> $TAG_COND OR negative_tag -> $TAG_COND)")

# Collect the whole worklist up front so the progress indicator has a total.
# Through a file: `< <(mf …)` discards the exit status, so a refused query or a
# stopped daemon came back as an empty worklist and the run reported "nothing to
# do" instead of the error.
mf_into "$TMP/worklist" metarecord -q "$PREDICATE" get
mapfile -t uuids <"$TMP/worklist"
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
        "[y →] $TAG   [n ←] not $TAG   [s ↓] skip   [q] quit — $rel" y n s q)" in
        y) mf tag -i "$uuid" add "$TAG" >/dev/null; yes=$((yes + 1)) ;;
        n) mf tag -i "$uuid" deny "$TAG" >/dev/null; no=$((no + 1)) ;;
        s) skipped=$((skipped + 1)) ;;
        *) break ;; # q, or the question could not be answered
    esac
done

mf_gui_finish "Tagging '$TAG' done: $yes yes, $no no, $skipped skipped"
