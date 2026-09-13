#!/usr/bin/env bash
# Summary: Interactively classify a query's files by hierarchical tags.
# Interactive hierarchical-tag classification, in the running metafolder GUI.
# Walks the metarecords a QUERY matches and, for each, shows the file (left
# `file` panel) and its metadata (right `metarecord-detail` panel), then asks a
# descending series of questions "add tag <path> ?" chosen by
# scripts/gui-tag-next.sh:
#
#   y / →  -> the file HAS the tag       (mf tag add)
#   n / ←  -> the file does NOT have it  (mf tag deny)
#   Escape -> stop
#
# The question ORDER (descend into a tag's children only once it is accepted,
# skip subtrees under a generic negative, honour exclusivity) is the pure
# selector in gui-tag-next.sh. The tag MODEL — creating entries, adding the
# ref, and the subsumption/exclusivity rewrites (drop ancestors on add,
# descendants on deny, siblings when exclusive) — is `mf tag`, so this driver no
# longer re-implements any of it.
#
# The vocabulary and exclusivity flags come from `mf tag list`
# (path<TAB>partition<TAB>exclusive), the metarecord's current tags from
# `mf … field get <field> --resolve path` — one round-trip each, no per-entry
# loops. Resumable: answered questions are skipped, so re-run until nothing is
# left to ask.
#
# THE QUERY IS THE SCOPE (spec-gui "A query is the scope"), as in the other
# shipped scripts: given as the argument, else what the GUI shows
# (`mf gui query`), else a folder chosen from the completion. `q` on any
# question stops the whole run, not just the record being classified.
#
# A bare UUID is a valid query (spec-query, the UUID-atom bullet), so the old
# single-metarecord invocation — `gui-tag-classify.sh <uuid>` — still works and
# means exactly what it did.
#
# Usage: gui-tag-classify.sh [<query>]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"
# Reuse the pure question selector and its hierarchy helpers (its `main` guard
# keeps it inert when sourced).
# shellcheck source=gui-tag-next.sh
source "$HERE/gui-tag-next.sh"

[ $# -le 1 ] || mf_die "usage: $0 [<query>]"
QUERY_GIVEN=0
[ $# -ge 1 ] && QUERY_GIVEN=1
QUERY_ARG=${1-}

mf_gui_bind_repo

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

# The whole scope up front, in path order, so the progress bar has a total and
# two runs walk it the same way. Through a file, not `< <(mf …)`: a process
# substitution discards the exit status, so a refused query or a stopped daemon
# read back as an empty scope and the run ended on "the query matches no tracked
# metarecord" — an answer, where there had been an error.
#
# Read before the session takes the screen over, so an empty scope does not
# flash the layout on its way to an error.
SCOPED=$(mktemp) || mf_die "cannot create a temporary file"
# Bounded: the whole scope is held in memory for the walk, so the scope is the
# run's memory. One past the cap is asked for, so "more than it" is
# distinguishable from "exactly it".
mf_gui_scope_into "$SCOPED" --sort mfr_path --limit "$((MF_GUI_MAX_ENTRIES + 1))"
mapfile -t UUIDS <"$SCOPED"
rm -f "$SCOPED"
mf_check_scope_size "${#UUIDS[@]}" "tracked metarecords"
TOTAL=0
for u in ${UUIDS+"${UUIDS[@]}"}; do [ -n "$u" ] && TOTAL=$((TOTAL + 1)); done
[ "$TOTAL" -gt 0 ] || mf_die "the query matches no tracked metarecord"

mf_gui_session_open metarecord-detail

TMP=$(mf_gui_tmpdir)
UNIVERSE="$TMP/universe"
POS="$TMP/pos"
NEG="$TMP/neg"

mf_into "$UNIVERSE" tag list
[ -s "$UNIVERSE" ] || mf_die "no tag entries (mf_schema = \"tag\") in repository $REPO"

yes=0 no=0 done_n=0 STOP=""
for UUID in ${UUIDS+"${UUIDS[@]}"}; do
    [ -z "$STOP" ] || break
    [ -n "$UUID" ] || continue
    done_n=$((done_n + 1))
    rel=$(mf path --relative "$UUID" 2>/dev/null || true)
    mf_gui_progress --done "$done_n" --total "$TOTAL" --phase "${rel:-$UUID}"
    # Setting the file view also publishes selected_metarecord for this
    # workspace, which the detail panel follows — no extra plumbing needed. A
    # record with no file behind it is classified all the same, without preview.
    mf_gui_show_file "$(mf path "$UUID" 2>/dev/null || true)"

    while :; do
        mf metarecord -i "$UUID" field get tag --resolve path >"$POS"
        mf metarecord -i "$UUID" field get negative_tag --resolve path >"$NEG"

        T=$(gui_tag_next "$UNIVERSE" "$POS" "$NEG") || break # no question left

        case "$(mf_gui_ask_answer "add tag '$T' ?   [y →] oui   [n ←] non   [q] stop" y n q)" in
            y) mf tag -i "$UUID" add "$T" >/dev/null; yes=$((yes + 1)) ;;
            n) mf tag -i "$UUID" deny "$T" >/dev/null; no=$((no + 1)) ;;
            # `q`, or a question that could not be answered: stop the RUN, not
            # just this record — the user asked to be let go.
            *) STOP=user; break ;;
        esac
    done
done

# One record keeps the wording it always had; a set says how many it walked.
if [ "$TOTAL" -eq 1 ]; then
    mf_gui_finish "Classification de ${UUIDS[0]} terminée : $yes oui, $no non"
else
    mf_gui_finish "Classification de $TOTAL metarecords terminée : $yes oui, $no non"
fi
