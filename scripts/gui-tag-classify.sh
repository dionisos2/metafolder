#!/usr/bin/env bash
# Interactive hierarchical-tag classification of ONE metarecord, in the running
# metafolder GUI. Shows the file (left `file` panel) and its metadata (right
# `metarecord-detail` panel), then asks a descending series of questions
# "add tag <path> ?" chosen by scripts/gui-tag-next.sh:
#
#   y      -> the file HAS the tag       (mf tag add)
#   n      -> the file does NOT have it  (mf tag deny)
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
# Usage: gui-tag-classify.sh <metarecord-uuid>

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/mf-gui.sh
source "$HERE/lib/mf-gui.sh"
# Reuse the pure question selector and its hierarchy helpers (its `main` guard
# keeps it inert when sourced).
# shellcheck source=gui-tag-next.sh
source "$HERE/gui-tag-next.sh"

[ $# -eq 1 ] || mf_die "usage: $0 <metarecord-uuid>"
UUID=$1

mf_gui_bind_repo
ABS=$(mf path "$UUID") || mf_die "metarecord $UUID has no file path (mfr_path present?)"

mf_gui_session_open metarecord-detail
# Setting the file view also publishes selected_metarecord for this workspace,
# which the detail panel follows — no extra plumbing needed.
mf_gui_show_file "$ABS"

TMP=$(mf_gui_tmpdir)
UNIVERSE="$TMP/universe"
POS="$TMP/pos"
NEG="$TMP/neg"

mf tag list >"$UNIVERSE"
[ -s "$UNIVERSE" ] || mf_die "no tag entries (type = \"tag\") in repository $REPO"

yes=0 no=0
while :; do
    mf metarecord -i "$UUID" field get tag --resolve path >"$POS"
    mf metarecord -i "$UUID" field get negative_tag --resolve path >"$NEG"

    T=$(gui_tag_next "$UNIVERSE" "$POS" "$NEG") || break # no question left

    case "$(mf_gui_ask "add tag '$T' ?   [y] oui   [n] non   [Esc] stop" y n escape)" in
        y) mf tag -i "$UUID" add "$T" >/dev/null; yes=$((yes + 1)) ;;
        n) mf tag -i "$UUID" deny "$T" >/dev/null; no=$((no + 1)) ;;
        *) break ;; # escape
    esac
done

SUMMARY="Classification de $UUID terminée : $yes oui, $no non"
mf gui message "$SUMMARY" --timeout-ms 5000
echo "$SUMMARY"
