#!/usr/bin/env bash
# bench_realistic.sh — end-to-end realistic metafolder benchmark (HTTP API).
#
# Assumes bench_data/ already exists and is populated (run gen_files.sh first).
# To rerun cleanly: rm -rf bench_data/.metafolder && ./bench_realistic.sh
#
# Workflow:
#   1. Start daemon, init repo, enable tracking (mf_watch) on the root
#   2. Reconcile (all file entries created)
#   3. Tag every entry: ext (MATCHES on the mfr_path name component) and
#      category (follows_transitive on the top-level directory)
#   4. Create "collection" entries per top-level directory, link files via
#      a parent Ref (batch set)
#   5. Build a TreeRef tag hierarchy (4 levels) and assign tag Refs
#   6. Run a query suite (eq/or/not, presence, ref traversal, tree
#      traversal, regex, sort, pagination) and report timing + counts
#   7. Event log: read timing, rollback/redo of the last batch revision
#   8. Report daemon memory and database size
#
# Requirements: curl, python3 (JSON parsing). The daemon binary is taken
# from target/release if present, else target/debug.
#
# Usage:
#   ./bench_realistic.sh [--keep]
#
#   --keep   Leave daemon running and repo intact after the benchmark

set -euo pipefail

# ── Options ───────────────────────────────────────────────────────────────────

KEEP=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --keep)   KEEP=true; shift ;;
        -h|--help)
            grep '^#' "$0" | sed 's/^# \{0,2\}//'
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# ── Paths ─────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$SCRIPT_DIR/bench_data"
PORT=7524
B="http://localhost:${PORT}"
DAEMON_PID=""

# ── Find the daemon binary (prefer release) ───────────────────────────────────

DAEMON_BIN=""
for dir in "$WORKSPACE_ROOT/target/debug" "$WORKSPACE_ROOT/target/release"; do
    [[ -x "$dir/metafolder-daemon" ]] && DAEMON_BIN="$dir/metafolder-daemon"
done
[[ -z "$DAEMON_BIN" ]] && { echo "metafolder-daemon not found — run 'cargo build --release' first" >&2; exit 1; }

# ── Cleanup ───────────────────────────────────────────────────────────────────

cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID"
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
}
$KEEP || trap cleanup EXIT

# ── Helpers ───────────────────────────────────────────────────────────────────

# api METHOD PATH [JSON_BODY] → response body
api() {
    local method="$1" path="$2" body="${3:-}"
    if [[ -n "$body" ]]; then
        curl -s -X "$method" "$B$path" -H 'content-type: application/json' -d "$body"
    else
        curl -s -X "$method" "$B$path"
    fi
}

# jfield KEY ← reads JSON on stdin, prints the value of a top-level key
jfield() { python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }

# jcount ← reads a JSON array (or a {results: []} page) on stdin, prints its length
jcount() {
    python3 -c 'import json,sys
d = json.load(sys.stdin)
print(len(d["results"]) if isinstance(d, dict) else len(d))'
}

# jfirst ← reads a JSON array of strings on stdin, prints the first element
jfirst() { python3 -c 'import json,sys; print(json.load(sys.stdin)[0])'; }

# elapsed milliseconds since a date +%s%N timestamp
ms_since() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

# Print a formatted result row:  LABEL  N ms  [optional suffix]
row() {
    local label="$1" ms="$2" suffix="${3:-}"
    printf "  %-56s %6d ms%s\n" "$label" "$ms" "$suffix"
}

# daemon_rss PID → "N kB" from /proc
daemon_rss() {
    awk '/VmRSS/ { printf "%d %s", $2, $3 }' "/proc/$1/status" 2>/dev/null || echo "N/A"
}

section() { echo; echo "--- $1 ---"; }

# batch_set QUERY_JSON NAME VALUE_JSON → number of updated entries
batch_set() {
    api POST "/repos/$REPO/set" \
        "{\"query\": $1, \"name\": \"$2\", \"value\": $3}" | jfield updated
}

# run_query LABEL QUERY_JSON — times the query and prints a row with the count
run_query() {
    local label="$1" query="$2"
    local t n
    t=$(date +%s%N)
    n=$(api POST "/repos/$REPO/query" "{\"query\": $query}" | jcount)
    row "$label" "$(ms_since "$t")" "   ($n results)"
}

# create_entry FIELDS_JSON → uuid of the new entry
create_entry() {
    api POST "/repos/$REPO/metadata" "{\"fields\": $1}" | jfield uuid
}

# tag_node NAME PARENT_UUID_OR_EMPTY → uuid (TreeRef tag tree on field "parent")
tag_node() {
    local name="$1" parent="${2:-}"
    local parent_json="null"
    [[ -n "$parent" ]] && parent_json="\"$parent\""
    create_entry "[{\"name\": \"parent\",
                    \"value\": {\"type\": \"tree_ref\",
                                \"value\": {\"parent\": $parent_json, \"name\": \"$name\"}}},
                   {\"name\": \"label\", \"value\": {\"type\": \"string\", \"value\": \"$name\"}}]"
}

# ── Phase 0: Preconditions ────────────────────────────────────────────────────

N_FILES=$(find "$BENCH_DIR" -type f 2>/dev/null | wc -l)

echo "=== metafolder realistic benchmark (daemon HTTP API) ==="
printf "  daemon     : %s\n" "$DAEMON_BIN"
printf "  bench_data : %s  (%d files)\n" "$BENCH_DIR" "$N_FILES"

[[ -d "$BENCH_DIR" ]] || { echo "bench_data not found — run gen_files.sh first" >&2; exit 1; }
[[ -d "$BENCH_DIR/.metafolder" ]] && { echo "bench_data already has a .metafolder — remove it first (rm -rf bench_data/.metafolder)" >&2; exit 1; }

# ── Phase 1: Daemon + repo init + watch scope ─────────────────────────────────

section "Daemon + repo init"

"$DAEMON_BIN" --port "$PORT" >/dev/null 2>&1 &
DAEMON_PID=$!

for i in $(seq 1 50); do
    sleep 0.1
    curl -sf "$B/health" >/dev/null 2>&1 && break
    [[ $i -eq 50 ]] && { echo "Daemon did not start in time" >&2; exit 1; }
done

t=$(date +%s%N)
REPO=$(api POST /repos/init "{\"root\": \"$BENCH_DIR\"}" | jfield repo_uuid)
row "repo init" "$(ms_since "$t")"

# Tracking is opt-in: enable mf_watch on the root entry.
ROOT_ENTRY=$(api POST "/repos/$REPO/query" \
    '{"query": {"type": "is_present", "field": "mf_watch"}}' | jfirst)
api PATCH "/repos/$REPO/metadata/$ROOT_ENTRY" \
    '{"name": "mf_watch", "value": {"type": "bool", "value": true}}' >/dev/null

printf "  daemon PID : %s  (RSS: %s)\n" "$DAEMON_PID" "$(daemon_rss "$DAEMON_PID")"
printf "  repo UUID  : %s\n"             "$REPO"

# ── Phase 2: Reconcile ────────────────────────────────────────────────────────

section "Reconcile (populate entries from filesystem)"

t=$(date +%s%N)
recon=$(api POST "/repos/$REPO/reconcile")
elapsed=$(ms_since "$t")
created=$(echo "$recon" | jfield created)
row "reconcile" "$elapsed" "   (created: $created)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 3: Tag entries (ext + category) ─────────────────────────────────────
#
# ext: one batch set per extension, MATCHES on the mfr_path name component.
# category: one batch set per top-level directory, follows_transitive on
# its path (the natural replacement for the old full-path MATCHES).

section "Tagging: ext (MATCHES) + category (follows_transitive)"

FILE_EXTS=(mp3 flac jpg png pdf txt epub mkv mp4 zip)
# Top-level directories actually present in bench_data (the generator draws
# them from a random pool, so they vary between datasets).
mapfile -t TOP_DIRS < <(find "$BENCH_DIR" -mindepth 1 -maxdepth 1 -type d \
                            -not -name '.metafolder' -printf '%f\n' | sort)
CAT_A="${TOP_DIRS[0]}"
CAT_B="${TOP_DIRS[1]:-${TOP_DIRS[0]}}"
printf "  categories : %s\n" "${TOP_DIRS[*]}"

N_ENTRIES=$(api GET "/repos/$REPO/metadata" | jcount)

t_phase=$(date +%s%N)
n_ops=0
for ext in "${FILE_EXTS[@]}"; do
    batch_set "{\"type\": \"matches\", \"field\": \"mfr_path\", \"pattern\": \"\\\\.${ext}\$\"}" \
              ext "{\"type\": \"string\", \"value\": \"$ext\"}" >/dev/null
    n_ops=$((n_ops + 1))
done
elapsed=$(ms_since "$t_phase")
row "ext tags — $n_ops batch set (MATCHES)" "$elapsed" "   ($((elapsed / n_ops)) ms/op)"

t_phase=$(date +%s%N)
n_ops=0
for cat in "${TOP_DIRS[@]}"; do
    [[ -d "$BENCH_DIR/$cat" ]] || continue
    batch_set "{\"type\": \"follows_transitive\", \"field\": \"mfr_path\", \"path\": \"/$cat\"}" \
              category "{\"type\": \"string\", \"value\": \"$cat\"}" >/dev/null
    n_ops=$((n_ops + 1))
done
elapsed=$(ms_since "$t_phase")
row "category tags — $n_ops batch set (->* path)" "$elapsed" "   ($((elapsed / n_ops)) ms/op)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 4: Collection entries + parent Ref ──────────────────────────────────

section "Collection entries + parent Ref links"

declare -A COLLECTION_UUID

t=$(date +%s%N)
n_col=0
for dir in "${TOP_DIRS[@]}"; do
    [[ -d "$BENCH_DIR/$dir" ]] || continue
    COLLECTION_UUID[$dir]=$(create_entry \
        "[{\"name\": \"kind\", \"value\": {\"type\": \"string\", \"value\": \"$dir\"}}]")
    n_col=$((n_col + 1))
done
row "create $n_col collection entries" "$(ms_since "$t")"

t_phase=$(date +%s%N)
n_ops=0
for dir in "${TOP_DIRS[@]}"; do
    [[ -d "$BENCH_DIR/$dir" ]] || continue
    batch_set "{\"type\": \"follows_transitive\", \"field\": \"mfr_path\", \"path\": \"/$dir\"}" \
              parent "{\"type\": \"ref\", \"value\": \"${COLLECTION_UUID[$dir]}\"}" >/dev/null
    n_ops=$((n_ops + 1))
done
elapsed=$(ms_since "$t_phase")
row "link entries — $n_ops batch set ops" "$elapsed" "   ($((elapsed / n_ops)) ms/op)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 5: Query suite ──────────────────────────────────────────────────────

section "Queries: fields, presence, ref traversal"

run_query 'ext = "pdf"' \
          '{"type": "eq", "field": "ext", "value": {"type": "string", "value": "pdf"}}'
run_query 'ext = "mp3" OR ext = "flac"' \
          '{"type": "or", "operands": [
             {"type": "eq", "field": "ext", "value": {"type": "string", "value": "mp3"}},
             {"type": "eq", "field": "ext", "value": {"type": "string", "value": "flac"}}]}'
run_query "category = \"$CAT_A\"" \
          '{"type": "eq", "field": "category", "value": {"type": "string", "value": "'"$CAT_A"'"}}'
echo "  --"
run_query 'ext IS PRESENT' '{"type": "is_present", "field": "ext"}'
run_query 'ext IS UNKNOWN' '{"type": "is_unknown", "field": "ext"}'
run_query 'parent IS PRESENT' '{"type": "is_present", "field": "parent"}'
echo "  --"
run_query "parent -> (kind = \"$CAT_A\")  [ref 1-hop]" \
          '{"type": "follows", "field": "parent", "target":
             {"type": "eq", "field": "kind", "value": {"type": "string", "value": "'"$CAT_A"'"}}}'
run_query "parent -> (kind = \"$CAT_B\") AND ext = \"pdf\"" \
          '{"type": "and", "operands": [
             {"type": "follows", "field": "parent", "target":
               {"type": "eq", "field": "kind", "value": {"type": "string", "value": "'"$CAT_B"'"}}},
             {"type": "eq", "field": "ext", "value": {"type": "string", "value": "pdf"}}]}'
echo "  --"
run_query "category = \"$CAT_A\" AND (ext = \"mp3\" OR ext = \"flac\")" \
          '{"type": "and", "operands": [
             {"type": "eq", "field": "category", "value": {"type": "string", "value": "'"$CAT_A"'"}},
             {"type": "or", "operands": [
               {"type": "eq", "field": "ext", "value": {"type": "string", "value": "mp3"}},
               {"type": "eq", "field": "ext", "value": {"type": "string", "value": "flac"}}]}]}'
run_query 'NOT (ext = "mp3" OR ext = "jpg")' \
          '{"type": "not", "operand": {"type": "or", "operands": [
             {"type": "eq", "field": "ext", "value": {"type": "string", "value": "mp3"}},
             {"type": "eq", "field": "ext", "value": {"type": "string", "value": "jpg"}}]}}'

section "Queries: filesystem tree + regex"

run_query "mfr_path ->* \"/$CAT_A\"  [transitive, tree cache]" \
          '{"type": "follows_transitive", "field": "mfr_path", "path": "/'"$CAT_A"'"}'
run_query "mfr_path ->* \"/$CAT_A\"  [again, cache warm]" \
          '{"type": "follows_transitive", "field": "mfr_path", "path": "/'"$CAT_A"'"}'
run_query "mfr_path -> \"/$CAT_A\"  [direct children]" \
          '{"type": "follows", "field": "mfr_path", "target": "/'"$CAT_A"'"}'
run_query 'mfr_path MATCHES "\.pdf$"  [REGEXP scan]' \
          '{"type": "matches", "field": "mfr_path", "pattern": "\\.pdf$"}'
run_query 'mfr_path MATCHES "^a.*\.(mp3|flac)$"' \
          '{"type": "matches", "field": "mfr_path", "pattern": "^a.*\\.(mp3|flac)$"}'
run_query 'mfr_size > 150' \
          '{"type": "gt", "field": "mfr_size", "value": {"type": "int", "value": 150}}'

# ── Phase 6: TreeRef tag hierarchy + traversal ────────────────────────────────
#
# Tag tree (TreeRef field "parent", roots without leading slash):
#   media/audio/{jazz,classical}   media/visual/{photo,video}
#   docs/{book,article}
# Files point to tag entries with a `tag` Ref; "every file under the media
# tag" composes Follows(tag) with FollowsTransitive on the tag tree.

section "TreeRef tag hierarchy (4 levels) + traversal"

t=$(date +%s%N)
tag_media=$(tag_node media)
tag_docs=$(tag_node docs)
tag_audio=$(tag_node audio "$tag_media")
tag_visual=$(tag_node visual "$tag_media")
tag_jazz=$(tag_node jazz "$tag_audio")
tag_classical=$(tag_node classical "$tag_audio")
tag_photo=$(tag_node photo "$tag_visual")
tag_video=$(tag_node video "$tag_visual")
tag_book=$(tag_node book "$tag_docs")
tag_article=$(tag_node article "$tag_docs")
row "create 10 tag nodes (4 levels)" "$(ms_since "$t")"

assign_tag() { # EXTS_REGEX_ALTERNATION TAG_UUID
    batch_set "{\"type\": \"matches\", \"field\": \"mfr_path\", \"pattern\": \"\\\\.($1)\$\"}" \
              tag "{\"type\": \"ref\", \"value\": \"$2\"}" >/dev/null
}

t_phase=$(date +%s%N)
assign_tag 'mp3|flac' "$tag_jazz"
assign_tag 'jpg|png'  "$tag_photo"
assign_tag 'mkv|mp4'  "$tag_video"
assign_tag 'pdf|epub' "$tag_book"
assign_tag 'txt'      "$tag_article"
elapsed=$(ms_since "$t_phase")
row "assign tag refs — 5 batch ops" "$elapsed" "   ($((elapsed / 5)) ms/op)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

echo
run_query 'tag -> (label = "jazz")  [direct]' \
          '{"type": "follows", "field": "tag", "target":
             {"type": "eq", "field": "label", "value": {"type": "string", "value": "jazz"}}}'
run_query 'tag -> (descendant of "media/audio")' \
          "{\"type\": \"follows\", \"field\": \"tag\", \"target\":
             {\"type\": \"follows_transitive\", \"field\": \"parent\", \"path\": \"media/audio\"}}"
run_query 'tag -> (descendant of "media")' \
          "{\"type\": \"follows\", \"field\": \"tag\", \"target\":
             {\"type\": \"follows_transitive\", \"field\": \"parent\", \"path\": \"media\"}}"
run_query 'tag -> (descendant of "docs")' \
          "{\"type\": \"follows\", \"field\": \"tag\", \"target\":
             {\"type\": \"follows_transitive\", \"field\": \"parent\", \"path\": \"docs\"}}"
run_query 'tag under "media" AND ext = "mp3"' \
          "{\"type\": \"and\", \"operands\": [
             {\"type\": \"follows\", \"field\": \"tag\", \"target\":
               {\"type\": \"follows_transitive\", \"field\": \"parent\", \"path\": \"media\"}},
             {\"type\": \"eq\", \"field\": \"ext\", \"value\": {\"type\": \"string\", \"value\": \"mp3\"}}]}"

# ── Phase 7: Sort + pagination ────────────────────────────────────────────────

section "Sort + keyset pagination"

t=$(date +%s%N)
n=$(api POST "/repos/$REPO/query" \
    '{"query": {"type": "is_present", "field": "ext"},
      "sort": [{"field": "mfr_size", "order": "desc"}], "limit": 100}' | jcount)
row 'top 100 by mfr_size desc' "$(ms_since "$t")" "   ($n results)"

t=$(date +%s%N)
pages=0
total=0
cursor=""
while :; do
    if [[ -z "$cursor" ]]; then
        body='{"query": {"type": "is_present", "field": "mfr_path"}, "limit": 5000}'
    else
        body="{\"query\": {\"type\": \"is_present\", \"field\": \"mfr_path\"}, \"limit\": 5000, \"cursor\": \"$cursor\"}"
    fi
    page=$(api POST "/repos/$REPO/query" "$body")
    total=$((total + $(echo "$page" | jcount)))
    pages=$((pages + 1))
    cursor=$(echo "$page" | python3 -c 'import json,sys; print(json.load(sys.stdin)["next_cursor"] or "")')
    [[ -z "$cursor" ]] && break
done
row "paginate all entries (pages of 5000)" "$(ms_since "$t")" "   ($pages pages, $total results)"

# ── Phase 8: Event log ────────────────────────────────────────────────────────

section "Event log"

t=$(date +%s%N)
api GET "/repos/$REPO/log?limit=1" >/dev/null
row 'GET /log?limit=1  [linear: full HEAD ancestry walk]' "$(ms_since "$t")"

t=$(date +%s%N)
nav=$(api POST "/repos/$REPO/rollback" '{"target": {"prev_revision": true}}')
elapsed=$(ms_since "$t")
unapplied=$(echo "$nav" | jfield operations_unapplied)
prev_head=$(echo "$nav" | jfield previous_head)
row 'rollback last revision' "$elapsed" "   ($unapplied ops unapplied)"

t=$(date +%s%N)
api POST "/repos/$REPO/rollback" "{\"target\": {\"id\": $prev_head}}" >/dev/null
row 'redo (navigate forward)' "$(ms_since "$t")" "   ($unapplied ops reapplied)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Summary ───────────────────────────────────────────────────────────────────

section "Summary"

total=$(api GET "/repos/$REPO/metadata" | jcount)
db_size=$(du -h "$BENCH_DIR/.metafolder/db.sqlite" 2>/dev/null | cut -f1)
printf "  %-26s %d\n"  "Total entries in repo:"  "$total"
printf "  %-26s %s\n"  "Daemon RSS:"              "$(daemon_rss "$DAEMON_PID")"
printf "  %-26s %s\n"  "db.sqlite size:"          "${db_size:-N/A}"
printf "  %-26s %s\n"  "bench_data:"              "$BENCH_DIR"

echo
echo "=== done ==="
