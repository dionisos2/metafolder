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
# The tag flags (doc "Tag flags") change the questions: an `always` tag is
# added without one, unless it is answered or under a negative; the children of
# a `partition` or `sparse` tag are not asked one by one but PICKED, by
# completion over the candidates, once the tag is had — one for a partition,
# one after another for a sparse tag, until "(rien de plus)", which denies the
# candidates left.
#
# The question ORDER (descend into a tag's children only once it is accepted,
# skip subtrees under a generic negative) is the pure
# selector in gui-tag-next.sh. The tag MODEL — creating entries, adding the
# ref, and the subsumption rewrites (drop ancestors on add, descendants on
# deny, contradicted answers) — is `mf tag`, so this driver no
# longer re-implements any of it.
#
# The vocabulary comes from `mf tag list` (path, uuid and flags), the
# metarecord's current tags from
# `mf … field get <field> --resolve path` — one round-trip each, no per-entry
# loops. Resumable: answered questions are skipped, so re-run until nothing is
# left to ask.
#
# THE QUERY IS THE SCOPE (doc "A query is the scope"), as in the other
# shipped scripts: given as the argument, else ASKED — the checked selection,
# or the query the list shows, like the GUI's bulk commands; when the list has
# published nothing, a folder chosen from the completion. `q` on any
# question stops the whole run, not just the record being classified.
#
# A bare UUID is a valid query (doc "Query DSL grammar", the uuid-atom bullet), so the old
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

# The run is one label (doc "Labels and contexts"): what it writes reads, and
# reverts, as one piece of work.
RUN_LABEL=$(mf_label_begin "gui-tag-classify")
[ -z "$RUN_LABEL" ] || export METAFOLDER_LABEL="$RUN_LABEL"

TMP=$(mf_gui_tmpdir)

# Every relative path of the scope in one round-trip, and the root once: the
# preview's absolute path is built from both. `mf path` was two daemon calls per
# record for what the whole scope answers at once. A record with no position is
# absent from the rows, and is classified all the same, without a preview.
declare -A REL_OF
mf_gui_scope_into "$TMP/paths" --resolve-tree mfr_path --tsv
while IFS=$'\t' read -r u p; do
    [ -n "$u" ] || continue
    REL_OF[$u]=$p
done <"$TMP/paths"
mf_gui_read_root
UNIVERSE="$TMP/universe"
POS="$TMP/pos"
NEG="$TMP/neg"

mf_into "$UNIVERSE" tag list
[ -s "$UNIVERSE" ] || mf_die "no tag entries (mf_schema = \"tag\") in repository $REPO"

# The completion entry that ends a pick, denying the candidates left.
NOTHING_MORE="(rien de plus)"

# read_answers — the record's current answers into $POS / $NEG.
read_answers() {
    mf metarecord -i "$UUID" field get tag --resolve path >"$POS"
    mf metarecord -i "$UUID" field get negative_tag --resolve path >"$NEG"
}

# pick <tag> — one child of <tag>, by completion over the candidates; or
# nothing more, which denies them all. Fails when the user stopped the run.
pick() {
    local at=$1 chosen c
    local -a candidates=()
    local -A by_name=()
    mapfile -t candidates < <(gui_tag_children "$UNIVERSE" "$POS" "$NEG" "$at")
    for c in ${candidates+"${candidates[@]}"}; do by_name[${c##*/}]=$c; done
    while :; do
        chosen=$( { for c in ${candidates+"${candidates[@]}"}; do printf '%s\n' "${c##*/}"; done
                    printf '%s\n' "$NOTHING_MORE"; } \
            | mf_gui_prompt "Tag sous '$at' :") || return 1
        if [ "$chosen" = "$NOTHING_MORE" ]; then
            for c in ${candidates+"${candidates[@]}"}; do
                # Explicit: `pick` runs under `||`, where set -e is off.
                mf tag -i "$UUID" deny "$c" >/dev/null || mf_die "cannot deny $c"
                no=$((no + 1))
            done
            return 0
        fi
        if [ -n "${by_name[$chosen]+set}" ]; then
            mf tag -i "$UUID" add "${by_name[$chosen]}" >/dev/null \
                || mf_die "cannot add ${by_name[$chosen]}"
            yes=$((yes + 1))
            return 0
        fi
        mf_gui_report "'$chosen' n'est pas un tag sous '$at'"
    done
}

yes=0 no=0 done_n=0 STOP=""
for UUID in ${UUIDS+"${UUIDS[@]}"}; do
    [ -z "$STOP" ] || break
    [ -n "$UUID" ] || continue
    done_n=$((done_n + 1))
    rel="" abs=""
    if [ -n "${REL_OF[$UUID]+set}" ]; then
        rel=${REL_OF[$UUID]:-/}
        abs=$(mf_gui_abs_path "$rel")
    fi
    mf_gui_progress --done "$done_n" --total "$TOTAL" --phase "${rel:-$UUID}"
    # Setting the file view also publishes selected_metarecord for this
    # workspace, which the detail panel follows — no extra plumbing needed. A
    # record with no file behind it is classified all the same, without preview.
    mf_gui_show_file "$abs"

    # The always tags first: added without a question, so the questions below
    # already see them.
    read_answers
    while IFS= read -r T; do
        [ -n "$T" ] || continue
        mf tag -i "$UUID" add "$T" >/dev/null
        yes=$((yes + 1))
    done < <(gui_tag_always "$UNIVERSE" "$POS" "$NEG" || :)

    while :; do
        read_answers

        Q=$(gui_tag_next "$UNIVERSE" "$POS" "$NEG") || break # no question left
        KIND=${Q%%$'\t'*}
        T=${Q#*$'\t'}

        if [ "$KIND" = pick ]; then
            pick "$T" || { STOP=user; break; }
            continue
        fi

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
