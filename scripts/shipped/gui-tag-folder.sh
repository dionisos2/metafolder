#!/usr/bin/env bash
# Summary: Bulk-apply one tag over a query's metarecords (yes/no/mixed walk).
# Bulk-apply one tag over a set of metarecords, in the running metafolder GUI.
# Given a TAG (a "/"-separated tag path) and a QUERY, walks what the query
# matches and asks, per entry, whether it carries the tag:
#
#   y (oui)   -> `mf tag add` on the entry; for a folder, on its whole subtree
#                too — *intersected with the query*, which is the scope.
#   n (non)   -> `mf tag deny` on the same scope.
#   m (mixed) -> `mf tag mixed` on the folder only; the walk then descends into
#                it and asks its children in turn.
#   s (skip)  -> leave this entry alone: no tag op, and for a folder nothing
#                under it is asked either. The skip is RECORDED (a
#                `gui_tag_skipped` marker), so it survives the run and is
#                offered for cleanup at the end and at the next run's start.
#
# The arrow keys answer as well: → yes, ← no, ↑ mixed, ↓ skip. Backspace (or
# `u`, for undo) takes the previous answer back — including a skip.
#
# THE QUERY IS THE WALK STATE. An answer is already a write, so "what is left
# to ask" is a *query*, not bookkeeping: each step asks ONE question with
# `--limit 1` over
#
#     <scope> AND <not yet decided> AND <not skipped>
#
# and every answer removes its subject from that set — a y/n on a folder writes
# the tag over `(scope) AND mfr_path ->* "<path>"`, so the whole subtree leaves
# at once. Nothing is read up front, nothing is held in bash, and there is no
# scope cap: the cost is two or three small round-trips per question, each
# behind a human keypress. See docs/gui-tag-folder-rework.md.
#
# THE QUERY IS ALSO THE SCOPE. A "yes" on a folder never reaches a metarecord
# the query excludes: the subtree op is `(<query>) AND mfr_path ->* "<path>"`.
# So narrowing the list in the GUI narrows what this script can touch.
#
# Where the query comes from, in order: the QUERY argument; else what the GUI
# is showing (`mf gui query` — the checkbox selection, else the list's query
# with its finder narrowing); else a folder chosen from the completion, turned
# into `mfr_path =>* "<folder>"` (the folder and its whole subtree). An empty
# query means every metarecord, and is left as such rather than wrapped.
#
# ORDER. Depth first, a folder before what it holds. At the current folder P:
#   1. the first undecided direct child in scope — folders first, each kind by
#      `order_dir`/`order_file` then by path, so a folder `mf order` has
#      numbered is walked in its own order (an album by track number);
#   2. none? the first undecided in-scope *descendant* of P, by path: the walk
#      moves to its parent and descends there without asking. This is what
#      reaches a SCATTERED scope — the query may exclude /a and hold
#      /a/b/c.txt — and it skips every empty level in one step;
#   3. neither? up one component. Above the repository root, the walk is done.
# Coming back up finds the siblings left behind. The script never runs
# `mf order` itself: numbering is a deliberate act, and its date-based fallback
# would be a worse walking order than the alphabetical one.
#
# Resumable, and that too is the query: an entry already decided is not in it.
# The record carries the tag (`tag`, exactly or through a more specific one),
# carries its negation (`negative_tag`, exactly or through a more general one),
# or is a `mixed_tag` folder — the walk enters the last by descending into it,
# so it is never re-asked. `--redo` asks everything again, decided or not — the
# way to revise a wrong answer over a subtree — and ignores the skip markers.
#
# `mf tag` owns the tag model: it creates the entry if the vocabulary lacks it,
# adds the ref idempotently, and applies the subsumption/exclusivity rewrites
# (add drops the more general ancestor tags, deny drops the more specific
# descendant negatives). So this script is only the walk + the y/n/m questions.
#
# Operates on TRACKED metarecords only — reconcile first if you want everything
# covered.
#
# Usage: gui-tag-folder.sh [--redo] [<tag> [<query>]]

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"

# `--redo`: ask every entry again, ignoring what is already decided or skipped.
# (`&&` here would end the script under `set -e` whenever the flag is absent.)
REDO=0
if [ "${1:-}" = "--redo" ]; then
    REDO=1
    shift
fi
[ $# -le 2 ] || mf_die "usage: $0 [--redo] [<tag> [<query>]]"
TAG=${1:-}
QUERY_GIVEN=0
[ $# -ge 2 ] && QUERY_GIVEN=1
QUERY_ARG=${2-}

mf_gui_bind_repo

# Tag: from the command line, or prompted with completion over the vocabulary.
[ -n "$TAG" ] || TAG=$(mf_gui_prompt_tag "Tag: ") || mf_die "cancelled"
[ -n "$TAG" ] || mf_die "empty tag name"
case $TAG in *\"*) mf_die "tag names must not contain double quotes" ;; esac

# The scope. Resolved BEFORE the session takeover: `mf gui query` answers for
# the focused workspace, and the scratch workspace the session opens publishes
# nothing.
# SCOPE is read by mf_gui_scoped, in lib/mf-gui.sh (a sourced file this check
# does not follow from here).
# shellcheck disable=SC2034
if [ "$QUERY_GIVEN" = 1 ]; then
    SCOPE=$QUERY_ARG
else
    SCOPE=$(mf_gui_default_scope "Folder: ") || mf_die "cancelled"
fi

mf_gui_session_open metarecord-detail

TMP=$(mf_gui_tmpdir)

# ── The question set, as a predicate ─────────────────────────────────────────
# Everything below builds one predicate: "in scope, and still to be asked". It
# is the walk's whole memory, which is why it is spelled once here.

# TAG and its ancestors, as an alternation over a tag entry's path: a more
# general "no" denies the tag we are asking about.
neg_paths() { # <tag> -> path = "a/b" OR path = "a"
    local t=$1 out=""
    while [ -n "$t" ]; do
        out+="path = \"$t\" OR "
        case $t in */*) t=${t%/*} ;; *) t="" ;; esac
    done
    printf '%s' "${out% OR }"
}

TAG_ESC=$(mf_dsl_str "$TAG")
# Subsumption is spelled in the query, so the daemon owns it: a positive on TAG
# *or any tag below it* implies TAG; a negative on TAG *or any tag above it*
# denies it; only an exact mixed marker is one.
DECIDED_PRED="NOT (tag -> (mf_schema = \"tag\" AND path =>* \"$TAG_ESC\")"
DECIDED_PRED+=" OR negative_tag -> (mf_schema = \"tag\" AND ($(neg_paths "$TAG_ESC")))"
DECIDED_PRED+=" OR mixed_tag -> (mf_schema = \"tag\" AND path = \"$TAG_ESC\"))"
# The skip marker: a ref to the tag's own vocabulary entry, not a boolean.
# Fields are a multi-map, so one record carries one marker per tag and a skip
# left by a run on `music` does not silently skip the same entry for `jazz`.
# One clause, whatever the number of skips.
SKIP_PRED="gui_tag_skipped -> (mf_schema = \"tag\" AND path = \"$TAG_ESC\")"

# Folder subtrees this run leaves alone: skipped folders (their subtree is
# still undecided and in scope, so nothing else would take it out), and under
# `--redo` the folders answered whole (where the decided clause is not there to
# do it). Bounded by how many times a human presses a key, not by the scope.
CLOSED_DIRS=()
# Entries this run has settled that the query cannot see: everything under
# `--redo`, and a skip that could not be recorded for want of a vocabulary
# entry. Also bounded by keypresses.
EXCLUDED=()

# "In scope and still to be asked", as one predicate. Ends with `mfr_path IS
# PRESENT`: a deleted file keeps its metarecord with `mfr_path = Nothing`
# (spec-file-tracking), which has no place in a tree walk — it cannot be shown,
# and it would be counted as something left to ask about for ever.
open_pred() {
    local parts=() uuid dir joined=""
    if [ "$REDO" = 0 ]; then
        parts+=("$DECIDED_PRED" "NOT $SKIP_PRED")
    fi
    if [ "${#EXCLUDED[@]}" -gt 0 ]; then
        local list=""
        for uuid in "${EXCLUDED[@]}"; do list+="$uuid, "; done
        parts+=("NOT uuid_in(${list%, })")
    fi
    for dir in ${CLOSED_DIRS+"${CLOSED_DIRS[@]}"}; do
        parts+=("NOT mfr_path ->* \"$(mf_dsl_str "$dir")\"")
    done
    parts+=("mfr_path IS PRESENT")
    for uuid in "${parts[@]}"; do joined+="$uuid AND "; done
    printf '%s' "${joined% AND }"
}

# ── The walk's four questions to the daemon ──────────────────────────────────
# Each is bounded — a `--limit 1`, a `--count`, or one record by uuid — and each
# goes through mf_into, so a refused query or a stopped daemon is fatal instead
# of reading back as "nothing left to ask".

first_line() { head -n1 "$1"; }

# One component up. A path with no separator left is directly under the
# repository root, whose own path is the empty string — spelled as a `case` and
# not as `${p%/*}`, which leaves such a path UNCHANGED and would walk up for
# ever.
parent_path() { # <path>
    case $1 in */*) printf '%s' "${1%/*}" ;; *) printf '' ;; esac
}

# The direct children of the current folder. The repository root is nobody's
# child, so at the walk root the forest root is asked for beside them; its path
# is the empty string, which sorts first, so it is the first question.
parent_clause() {
    if [ -z "$CURRENT" ]; then
        printf '(mfr_path:parent = "" OR mfr_path:parent IS ABSENT)'
    else
        printf 'mfr_path:parent = "%s"' "$(mf_dsl_str "$CURRENT")"
    fi
}

# Folders first, then everything else. A LEAF is spelled "not a folder" rather
# than `mfr_type = "file"`: a symlink is tracked as `mfr_type = "symlink"`
# (fs_meta.rs, which never dereferences one), so the narrow form would leave it
# reachable by rule 2 and by nothing else — the walk would descend to its parent,
# find no child to ask, and descend again, for ever, with no key pressed.
first_child() { # <dir|leaf>
    local kind=$1 order=order_dir type='mfr_type = "dir"'
    if [ "$kind" != dir ]; then
        order=order_file
        type='NOT mfr_type = "dir"'
    fi
    mf_into "$TMP/step" metarecord \
        -q "$(mf_gui_scoped "$(open_pred) AND $type AND $(parent_clause)")" \
        get --sort "$order" --sort mfr_path --limit 1
    first_line "$TMP/step"
}

# Rule 2: the first open descendant, by path. Sorting by path is sorting in DFS
# order, so it is the first entry the walk should reach — and its parent is the
# level to descend to, every empty level above it skipped in one step.
first_descendant() {
    mf_into "$TMP/step" metarecord \
        -q "$(mf_gui_scoped "$(open_pred) AND mfr_path ->* \"$(mf_dsl_str "$CURRENT")\"")" \
        get --sort mfr_path --limit 1
    first_line "$TMP/step"
}

# How many entries are left to ask about: exact, O(1) on the index, and it drops
# by a whole subtree the moment a folder is answered whole.
open_count() {
    mf_into "$TMP/count" metarecord -q "$(mf_gui_scoped "$(open_pred)")" get --count
    first_line "$TMP/count"
}

tree_path() { # <uuid> -> its root-relative path ("" for the repository root)
    mf_into "$TMP/path" metarecord -i "$1" get --resolve-tree mfr_path
    first_line "$TMP/path"
}

num() { case ${1:-} in '' | *[!0-9]*) printf 0 ;; *) printf '%s' "$1" ;; esac; }

# ── The skip markers ─────────────────────────────────────────────────────────

# The tag's vocabulary entry, which a marker refs. It may not exist yet (`mf
# tag` creates it on the first write), and "no such tag" must read the same as
# "no skips" — not as an error.
read_tag_uuid() {
    mf_into "$TMP/tag" metarecord -q "mf_schema = \"tag\" AND path = \"$TAG_ESC\"" get --limit 1
    first_line "$TMP/tag"
}

marker_count() {
    mf_into "$TMP/markers" metarecord -q "$(mf_gui_scoped "$SKIP_PRED")" get --count
    first_line "$TMP/markers"
}

# Skipped FOLDERS from an earlier run: their subtree is still undecided and in
# scope, so it has to be closed again or the files under a folder the user
# deliberately left alone are asked one by one.
load_closed_dirs() {
    local dir
    mf_into "$TMP/skipped" metarecord \
        -q "$(mf_gui_scoped "mfr_type = \"dir\" AND $SKIP_PRED")" get --resolve-tree mfr_path
    while read -r dir; do
        if [ -n "$dir" ]; then CLOSED_DIRS+=("$dir"); fi
    done <"$TMP/skipped"
}

# `remove` takes the row out by value, so only this tag's marker goes; `unset`
# would remove every tag's.
clear_markers() {
    [ -n "$TAG_UUID" ] || return 0
    mf metarecord -q "$(mf_gui_scoped "$SKIP_PRED")" \
        field remove "gui_tag_skipped:ref=$TAG_UUID" >/dev/null \
        || mf_gui_report "could not clear the skip markers"
    MARKERS=0
}

TAG_UUID=$(read_tag_uuid)
MARKERS=0        # skip markers this tag is known to carry

# Leftover markers are offered at the START, which is where the choice is
# actually informed: resume where you were, or ask those entries again.
if [ "$REDO" = 0 ]; then
    LEFT=$(num "$(marker_count)")
    if [ "$LEFT" -gt 0 ]; then
        MARKERS=$LEFT
        case "$(mf_gui_ask_answer \
            "$LEFT entries were skipped in an earlier run — ask them again?   [y] ask again   [n] keep them skipped" \
            y n)" in
            y) clear_markers ;;
            n) load_closed_dirs ;;
            *) mf_die "cancelled" ;;
        esac
    fi
fi

# ── The walk ─────────────────────────────────────────────────────────────────

# The two empty cases read differently, and the run must not confuse them: a
# query that matches nothing is a mistake to report, while a query whose every
# entry is already decided or skipped is a run that has nothing left to do —
# which is what a resumed walk looks like once it is finished. The second count
# is only asked when the first is zero, so the ordinary run pays nothing for it.
scope_count() {
    mf_into "$TMP/scope" metarecord -q "$(mf_gui_scoped "mfr_path IS PRESENT")" get --count
    first_line "$TMP/scope"
}

TOTAL=$(num "$(open_count)")
if [ "$TOTAL" -eq 0 ]; then
    [ "$(num "$(scope_count)")" -gt 0 ] || mf_die "the query matches no tracked metarecord"
    if [ "$MARKERS" -gt 0 ]; then
        case "$(mf_gui_ask_answer \
            "nothing left to ask about '$TAG' — forget the $MARKERS skips, so the next run asks them again?   [y] forget   [n] keep" \
            y n)" in
            y) clear_markers ;;
        esac
    fi
    mf_gui_finish "nothing left to ask about '$TAG': the query is fully decided."
    exit 0
fi

# Apply TAG over a node and its subtree, the subtree narrowed to the scope.
apply_tree() { # <uuid> <path> <verb: add|deny>
    mf tag -i "$1" "$3" "$TAG" >/dev/null \
        && mf tag -q "$(mf_gui_scoped "mfr_path ->* \"$(mf_dsl_str "$2")\"")" "$3" "$TAG" \
            >/dev/null
}

# Record a skip: the marker (so it survives the run) and, for a folder, the
# subtree it closes. The marker needs the vocabulary entry — when the tag has
# never been written anywhere there is nothing to point at, and the skip then
# holds for this run only.
record_skip() { # <uuid> <path> <dir|file>
    [ -n "$TAG_UUID" ] || TAG_UUID=$(read_tag_uuid)
    if [ -n "$TAG_UUID" ]; then
        if mf metarecord -i "$1" field add "gui_tag_skipped:ref=$TAG_UUID" >/dev/null; then
            MARKERS=$((MARKERS + 1))
        else
            STOP="cannot record the skip of '$2'"
            return 0
        fi
    else
        EXCLUDED+=("$1")
        [ "$WARNED_SKIP" = 1 ] || mf_gui_report \
            "'$TAG' is not in the tag vocabulary yet: skips hold for this run only"
        WARNED_SKIP=1
    fi
    if [ "$3" = dir ]; then CLOSED_DIRS+=("$2"); fi
}

# How the walk ended: "" = still going, "user" = stopped, anything else is an
# ERROR MESSAGE. The two must stay apart: bash disables `set -e` wherever a
# failure is tested, so a failed `mf tag` inside a handler would otherwise be
# indistinguishable from Escape and end the run with a cheerful "stopped."
STOP=""
SKIPPED=0
WARNED_SKIP=0
CURRENT=""       # the current folder — THE PATH IS THE STACK: going up is
                 # trimming one component, so the walk holds nothing else.

# One frame per answer, pushed before it is applied: the history's position (so
# the writes can be undone exactly), where the walk stood, and the sizes of the
# two lists an answer can grow. Going back pops one and restores all of it —
# the writes through the event log, which puts back the exact field rows and
# versions, the rest by assignment. Undoing the write is what re-opens the
# entry: it comes back into the query on its own.
BACK_STACK=()

while :; do
    [ -z "$STOP" ] || break

    kind="dir"
    uuid=$(first_child dir)
    if [ -z "$uuid" ]; then
        kind="leaf"
        uuid=$(first_child leaf)
    fi

    if [ -z "$uuid" ]; then
        # Rule 2: descend to the parent of the first open descendant, asking
        # nothing on the way. Rule 3: up one component, and above the
        # repository root the walk is done.
        deep=$(first_descendant)
        if [ -n "$deep" ]; then
            deep_path=$(tree_path "$deep")
            # Descending has to move, or the same query answers the same entry
            # for ever — and nothing in this branch waits for a key, so the run
            # would spin in silence. It cannot happen while the two queries
            # agree (an open descendant of P whose parent is P is a direct
            # child, which the step above asks for); if they ever stop agreeing,
            # this says so instead of hanging.
            next_folder=$(parent_path "$deep_path")
            if [ -z "$deep_path" ] || [ "$next_folder" = "$CURRENT" ]; then
                STOP="'${deep_path:-?}' is in the query but not in the walk"
                break
            fi
            CURRENT=$next_folder
            continue
        fi
        [ -n "$CURRENT" ] || break
        CURRENT=$(parent_path "$CURRENT")
        continue
    fi

    path=$(tree_path "$uuid")
    disp=${path:-/}
    remaining=$(num "$(open_count)")
    done_now=$((TOTAL - remaining + 1))
    [ "$done_now" -ge 1 ] || done_now=1
    [ "$done_now" -le "$TOTAL" ] || done_now=$TOTAL
    mf_gui_progress --done "$done_now" --total "$TOTAL" --phase "$disp"
    mf_gui_show_file "$(mf path "$uuid" 2>/dev/null || true)"
    counter="$((remaining - 1)) left"

    back_hint=""
    [ "${#BACK_STACK[@]}" -gt 0 ] && back_hint="   [u ⌫] back"
    if [ "$kind" = dir ]; then
        answer=$(mf_gui_ask_answer \
            "'$disp' has tag '$TAG'?   [y →] oui   [n ←] non   [m ↑] mixed   [s ↓] skip$back_hint   [q] stop   — $counter" \
            y n m s u q)
    else
        answer=$(mf_gui_ask_answer \
            "'$disp' has tag '$TAG'?   [y →] oui   [n ←] non   [s ↓] skip$back_hint   [q] stop   — $counter" \
            y n s u q)
    fi

    # Back: undo the previous answer and let the query bring its entry back.
    # Nothing to go back to on the first question, so the key is re-asked for.
    if [ "$answer" = u ]; then
        if [ "${#BACK_STACK[@]}" -eq 0 ]; then
            mf_gui_report "nothing to go back to"
            continue
        fi
        IFS=$'\t' read -r b_head b_current b_closed b_excluded b_markers b_skipped \
            <<<"${BACK_STACK[-1]}"
        unset "BACK_STACK[-1]"
        mf_log_back_to "$b_head"
        CURRENT=${b_current#.}
        while [ "${#CLOSED_DIRS[@]}" -gt "$b_closed" ]; do unset "CLOSED_DIRS[-1]"; done
        while [ "${#EXCLUDED[@]}" -gt "$b_excluded" ]; do unset "EXCLUDED[-1]"; done
        MARKERS=$b_markers
        SKIPPED=$b_skipped
        continue
    fi

    frame_head=$(mf_log_head)
    # The folder is prefixed so the field is never empty: a tab is IFS
    # *whitespace*, so `read` collapses two consecutive ones, and the walk
    # root's path — the empty string — would shift every field after it.
    BACK_STACK+=("$frame_head	.$CURRENT	${#CLOSED_DIRS[@]}	${#EXCLUDED[@]}	$MARKERS	$SKIPPED")

    case $answer in
        y | n)
            verb=add
            [ "$answer" = y ] || verb=deny
            if [ "$kind" = dir ]; then
                if apply_tree "$uuid" "$path" "$verb"; then
                    # Under --redo the decided clause is not there to take the
                    # subtree out of the walk, so the run remembers it itself.
                    if [ "$REDO" = 1 ]; then CLOSED_DIRS+=("$path"); fi
                else
                    STOP="cannot tag '$disp'"
                fi
            else
                mf tag -i "$uuid" "$verb" "$TAG" >/dev/null || STOP="cannot tag '$disp'"
            fi
            ;;
        m)
            # Only the marker is written; the walk then descends into it, which
            # is where its remaining questions are.
            if mf tag -i "$uuid" mixed "$TAG" >/dev/null; then
                CURRENT=$path
            else
                STOP="cannot mark '$disp' mixed"
            fi
            ;;
        s)
            SKIPPED=$((SKIPPED + 1))
            record_skip "$uuid" "$path" "$kind"
            ;;
        *) STOP=user ;;
    esac
    # Under --redo nothing the answer wrote narrows the query (the decided
    # clause is gone), so the entry is held out by uuid.
    if [ "$REDO" = 1 ] && [ "$answer" != s ]; then EXCLUDED+=("$uuid"); fi
done

mf_gui_progress --done "$TOTAL" --total "$TOTAL"

# The markers are offered for cleanup on every exit that can still ask. Escape
# kills the script, so nothing runs there; that is fine.
if [ "$MARKERS" -gt 0 ]; then
    case "$(mf_gui_ask_answer \
        "$MARKERS entries are skipped — forget the skips, so the next run asks them again?   [y] forget   [n] keep" \
        y n)" in
        y) clear_markers ;;
    esac
fi

case $STOP in
    "")   mf_gui_finish "done tagging '$TAG' ($SKIPPED skipped)." ;;
    user) mf_gui_finish "stopped tagging '$TAG' ($SKIPPED skipped)." ;;
    *)    mf_gui_finish "tagging '$TAG' aborted: $STOP"; exit 1 ;;
esac
