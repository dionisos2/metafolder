#!/usr/bin/env bash
# bench_realistic.sh — end-to-end realistic metafolder benchmark.
#
# Assumes bench_data/ already exists and is populated (run gen_files.sh first).
# To rerun cleanly: rm -rf bench_data/.metafolder && ./bench_realistic.sh
#
# Workflow:
#   1. Start daemon, init repo, reconcile (all file entries created)
#   2. Tag every entry: ext (file extension) + category (top-level directory)
#   3. Create "collection" entries for each top-level directory, then set a
#      parent Ref on every file entry pointing to its collection entry
#   4. Run a query suite and report timing + result counts
#   5. Report daemon memory usage
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
DAEMON_URL="http://localhost:${PORT}"
DAEMON_PID=""

# ── Find binaries (prefer release) ────────────────────────────────────────────

DAEMON_BIN=""
CLI_BIN=""
for dir in "$WORKSPACE_ROOT/target/release" "$WORKSPACE_ROOT/target/debug"; do
    [[ -x "$dir/metafolder-daemon" ]] && DAEMON_BIN="$dir/metafolder-daemon"
    [[ -x "$dir/metafolder"        ]] && CLI_BIN="$dir/metafolder"
done

[[ -z "$DAEMON_BIN" ]] && { echo "metafolder-daemon not found — run 'cargo build' first" >&2; exit 1; }
[[ -z "$CLI_BIN"    ]] && { echo "metafolder not found — run 'cargo build' first"        >&2; exit 1; }

# ── Cleanup ───────────────────────────────────────────────────────────────────

cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID"
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
}
$KEEP || trap cleanup EXIT

# ── Helpers ───────────────────────────────────────────────────────────────────

cli()      { "$CLI_BIN" --daemon-url "$DAEMON_URL" "$@"; }
cli_repo() { cli --repo "$REPO" "$@"; }

# elapsed milliseconds since a date +%s%N timestamp
ms_since() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

# Print a formatted result row:  LABEL  N ms  [optional suffix]
row() {
    local label="$1" ms="$2" suffix="${3:-}"
    printf "  %-46s %6d ms%s\n" "$label" "$ms" "$suffix"
}

# daemon_rss PID → "N kB" from /proc
daemon_rss() {
    awk '/VmRSS/ { printf "%d %s", $2, $3 }' "/proc/$1/status" 2>/dev/null || echo "N/A"
}

# section TITLE
section() { echo; echo "--- $1 ---"; }

# Extract a string field value from the JSON output of `get`.
# Usage: get_field_value <json> <field_name>
get_field_value() {
    python3 -c "
import json, sys
data = json.loads(sys.argv[1])
for f in (data[0]['fields'] if data else []):
    if f['name'] == sys.argv[2] and f['value']['type'] == 'string':
        print(f['value']['value'])
        break
" "$1" "$2" 2>/dev/null
}

# ── Phase 0: File generation ──────────────────────────────────────────────────

N_FILES=$(find "$BENCH_DIR" -type f | wc -l)

echo "=== metafolder realistic benchmark ==="
printf "  daemon     : %s\n" "$DAEMON_BIN"
printf "  cli        : %s\n" "$CLI_BIN"
printf "  bench_data : %s  (%d files)\n" "$BENCH_DIR" "$N_FILES"

[[ -d "$BENCH_DIR" ]] || { echo "bench_data not found — run gen_files.sh first" >&2; exit 1; }
[[ -d "$BENCH_DIR/.metafolder" ]] && { echo "bench_data already has a .metafolder — remove it first (rm -rf bench_data/.metafolder)" >&2; exit 1; }

# ── Phase 1: Daemon + repo init ───────────────────────────────────────────────

section "Daemon + repo init"

"$DAEMON_BIN" --port "$PORT" >/dev/null 2>&1 &
DAEMON_PID=$!

# Poll /health until the daemon is ready (up to 5 s)
for i in $(seq 1 50); do
    sleep 0.1
    curl -sf "$DAEMON_URL/health" >/dev/null 2>&1 && break
    [[ $i -eq 50 ]] && { echo "Daemon did not start in time" >&2; exit 1; }
done

REPO=$(cli init "$BENCH_DIR" 2>/dev/null)
printf "  daemon PID : %s  (RSS: %s)\n" "$DAEMON_PID" "$(daemon_rss "$DAEMON_PID")"
printf "  repo UUID  : %s\n"             "$REPO"

# ── Phase 2: Reconcile ────────────────────────────────────────────────────────

section "Reconcile (populate entries from filesystem)"

t=$(date +%s%N)
recon=$(cli_repo reconcile 2>/dev/null)
elapsed=$(ms_since "$t")
created=$(echo "$recon" | grep -o 'created: [0-9]*' | awk '{print $2}')
cleared=$(echo "$recon" | grep -o 'cleared: [0-9]*' | awk '{print $2}')
row "reconcile" "$elapsed" "   (created: $created  cleared: $cleared)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 3: Tag entries (ext + category) ─────────────────────────────────────
#
# For each entry: read its path, derive ext and category, then set both fields.
# We also cache uuid→category locally to avoid re-querying in Phase 4.

section "Tagging: ext + category (one CLI round-trip per entry)"

ALL_UUIDS=$(cli_repo list 2>/dev/null)
N_ENTRIES=$(echo "$ALL_UUIDS" | grep -c .)

declare -A ENTRY_CATEGORY   # uuid -> category (used in Phase 4)

t_phase=$(date +%s%N)
tagged=0

while IFS= read -r uuid; do
    [[ -z "$uuid" ]] && continue

    # Fetch entry path via JSON output
    entry_json=$(cli_repo get "$uuid" --fields path 2>/dev/null)
    raw_path=$(get_field_value "$entry_json" "path")
    [[ -z "$raw_path" ]] && continue     # entry has no string path (deleted file)

    # Extension: last component after the last '.'
    filename="${raw_path##*/}"
    ext="${filename##*.}"
    [[ "$ext" == "$filename" ]] && ext="none"    # file has no extension

    # Category: first path component below bench_data
    rel="${raw_path#"$BENCH_DIR"/}"
    category="${rel%%/*}"
    [[ "$category" == "$rel" ]] && category="root"   # file directly at repo root

    cli_repo set "$uuid" "ext:string=$ext"           >/dev/null
    cli_repo set "$uuid" "category:string=$category" >/dev/null

    ENTRY_CATEGORY[$uuid]=$category
    tagged=$((tagged + 1))
done <<< "$ALL_UUIDS"

elapsed=$(ms_since "$t_phase")
per_entry=0
[[ $tagged -gt 0 ]] && per_entry=$(( elapsed / tagged ))
row "tag $tagged entries (ext + category)" "$elapsed" "   ($per_entry ms/entry)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 4: Collection entries + parent Ref ──────────────────────────────────
#
# Create one "collection" entry per top-level directory that actually exists
# in bench_data, then link every file entry to its collection via a Ref field.
# This enables graph-traversal queries: parent -> (kind = "music")

section "Collection entries + parent Ref links"

TOP_DIRS=(music images books videos documents archives misc)
declare -A COLLECTION_UUID   # category -> collection entry uuid

t=$(date +%s%N)
n_col=0
for dir in "${TOP_DIRS[@]}"; do
    [[ -d "$BENCH_DIR/$dir" ]] || continue
    uuid=$(cli_repo create \
            --field "kind:string=$dir" \
            --field "path:string=$BENCH_DIR/$dir" \
            2>/dev/null)
    COLLECTION_UUID[$dir]="$uuid"
    n_col=$((n_col + 1))
done
row "create $n_col collection entries" "$(ms_since "$t")"

t_phase=$(date +%s%N)
linked=0

while IFS= read -r uuid; do
    [[ -z "$uuid" ]] && continue
    category="${ENTRY_CATEGORY[$uuid]:-}"
    [[ -z "$category" ]] && continue
    col_uuid="${COLLECTION_UUID[$category]:-}"
    [[ -z "$col_uuid" ]] && continue

    cli_repo set "$uuid" "parent:ref=$col_uuid" >/dev/null
    linked=$((linked + 1))
done <<< "$ALL_UUIDS"

elapsed=$(ms_since "$t_phase")
per_entry=0
[[ $linked -gt 0 ]] && per_entry=$(( elapsed / linked ))
row "link $linked entries to collections" "$elapsed" "   ($per_entry ms/entry)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 5: Query suite ──────────────────────────────────────────────────────

section "Queries"

time_query() {
    local label="$1" query="$2"
    local t; t=$(date +%s%N)
    local n; n=$(cli_repo query "$query" 2>/dev/null | grep -c . || true)
    row "$label" "$(ms_since "$t")" "   ($n results)"
}

# ── Simple field queries ──────────────────────────────────────────────────────
time_query 'ext = "pdf"'                     'ext = "pdf"'
time_query 'ext = "mp3" OR ext = "flac"'     'ext = "mp3" OR ext = "flac"'
time_query 'ext = "jpg" OR ext = "png"'      'ext = "jpg" OR ext = "png"'
time_query 'category = "music"'              'category = "music"'
time_query 'category = "documents"'          'category = "documents"'
echo "  --"

# ── Presence / absence ───────────────────────────────────────────────────────
time_query 'ext IS PRESENT'                  'ext IS PRESENT'
time_query 'ext IS ABSENT'                   'ext IS ABSENT'
time_query 'parent IS PRESENT'               'parent IS PRESENT'
echo "  --"

# ── Graph traversal (Ref follow) ─────────────────────────────────────────────
time_query 'parent -> (kind = "music")'              'parent -> (kind = "music")'
time_query 'parent -> (kind = "images")'             'parent -> (kind = "images")'
time_query 'parent -> (kind = "documents")'          'parent -> (kind = "documents")'
time_query 'parent -> (kind = "documents") AND ext = "pdf"' \
           'parent -> (kind = "documents") AND ext = "pdf"'
echo "  --"

# ── Combined ─────────────────────────────────────────────────────────────────
time_query 'category = "music" AND (ext = "mp3" OR ext = "flac")' \
           'category = "music" AND (ext = "mp3" OR ext = "flac")'
time_query 'NOT (ext = "mp3" OR ext = "flac" OR ext = "jpg" OR ext = "png")' \
           'NOT (ext = "mp3" OR ext = "flac" OR ext = "jpg" OR ext = "png")'

# ── MATCHES predicate ─────────────────────────────────────────────────────────
time_query 'path MATCHES "\.pdf$"'           'path MATCHES "\.pdf$"'
time_query 'path MATCHES "music/.*\.mp3$"'   'path MATCHES "music/.*\.mp3$"'

# ── Summary ───────────────────────────────────────────────────────────────────

section "Summary"

total=$(cli_repo list 2>/dev/null | grep -c . || true)
printf "  %-26s %d\n"  "Total entries in repo:"  "$total"
printf "  %-26s %s\n"  "Daemon RSS:"              "$(daemon_rss "$DAEMON_PID")"
printf "  %-26s %s\n"  "bench_data:"              "$BENCH_DIR"

echo
echo "=== done ==="
