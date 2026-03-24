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
for dir in "$WORKSPACE_ROOT/target/debug" "$WORKSPACE_ROOT/target/release"; do
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
# One batch `set` call per extension/category value via MATCHES predicate.
# E.g.  set 'path MATCHES "\.mp3$"'  "ext:string=mp3"
#        set 'path MATCHES "/music/"' "category:string=music"

section "Tagging: ext + category (batch set via MATCHES)"

FILE_EXTS=(mp3 flac jpg png pdf txt epub mkv mp4 zip)
TOP_DIRS=(music images books videos documents archives misc)

N_ENTRIES=$(cli_repo list 2>/dev/null | grep -c . || true)

t_phase=$(date +%s%N)
n_ops=0

for ext in "${FILE_EXTS[@]}"; do
    cli_repo set "path MATCHES \"\.${ext}\$\"" "ext:string=$ext" >/dev/null
    n_ops=$((n_ops + 1))
done

for cat in "${TOP_DIRS[@]}"; do
    [[ -d "$BENCH_DIR/$cat" ]] || continue
    cli_repo set "path MATCHES \"/$cat/\"" "category:string=$cat" >/dev/null
    n_ops=$((n_ops + 1))
done

elapsed=$(ms_since "$t_phase")
per_op=0
[[ $n_ops -gt 0 ]] && per_op=$(( elapsed / n_ops ))
row "tag $N_ENTRIES entries — $n_ops batch set ops" "$elapsed" "   ($per_op ms/op)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Phase 4: Collection entries + parent Ref ──────────────────────────────────
#
# Create one "collection" entry per top-level directory that actually exists
# in bench_data, then link every file entry to its collection via a single
# batch `set` call per directory (using MATCHES).

section "Collection entries + parent Ref links"

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
n_ops=0

for dir in "${TOP_DIRS[@]}"; do
    [[ -d "$BENCH_DIR/$dir" ]] || continue
    col_uuid="${COLLECTION_UUID[$dir]:-}"
    [[ -z "$col_uuid" ]] && continue
    cli_repo set "path MATCHES \"/$dir/\"" "parent:ref=$col_uuid" >/dev/null
    n_ops=$((n_ops + 1))
done

elapsed=$(ms_since "$t_phase")
per_op=0
[[ $n_ops -gt 0 ]] && per_op=$(( elapsed / n_ops ))
row "link $N_ENTRIES entries — $n_ops batch set ops" "$elapsed" "   ($per_op ms/op)"
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

# ── Phase 6: Tag hierarchy + ->* traversal ───────────────────────────────────
#
# Build a 4-level tag tree:
#   media  (L1)
#     audio   (L2, parent->media)
#       jazz       (L3, parent->audio)   ← mp3, flac tagged here
#       classical  (L3, parent->audio)
#     visual  (L2, parent->media)
#       photo      (L3, parent->visual)  ← jpg, png tagged here
#       video      (L3, parent->visual)  ← mkv, mp4 tagged here
#   docs   (L1)
#     book    (L2, parent->docs)         ← pdf, epub tagged here
#     article (L2, parent->docs)         ← txt tagged here

section "Tag hierarchy (4 levels) + ->* traversal"

t=$(date +%s%N)
tag_media=$(cli_repo create --field "label:string=media" 2>/dev/null)
tag_docs=$(cli_repo create --field "label:string=docs" 2>/dev/null)
tag_audio=$(cli_repo create --field "label:string=audio" --field "parent:ref=$tag_media" 2>/dev/null)
tag_visual=$(cli_repo create --field "label:string=visual" --field "parent:ref=$tag_media" 2>/dev/null)
tag_jazz=$(cli_repo create --field "label:string=jazz" --field "parent:ref=$tag_audio" 2>/dev/null)
tag_classical=$(cli_repo create --field "label:string=classical" --field "parent:ref=$tag_audio" 2>/dev/null)
tag_photo=$(cli_repo create --field "label:string=photo" --field "parent:ref=$tag_visual" 2>/dev/null)
tag_video=$(cli_repo create --field "label:string=video" --field "parent:ref=$tag_visual" 2>/dev/null)
tag_book=$(cli_repo create --field "label:string=book" --field "parent:ref=$tag_docs" 2>/dev/null)
tag_article=$(cli_repo create --field "label:string=article" --field "parent:ref=$tag_docs" 2>/dev/null)
row "create 10 tags (4 levels)" "$(ms_since "$t")"

# Assign tags to file entries via batch set (reuses ext field from phase 3)
t_phase=$(date +%s%N)
cli_repo set 'ext = "mp3" OR ext = "flac"' "tag:ref=$tag_jazz"    >/dev/null
cli_repo set 'ext = "jpg" OR ext = "png"'  "tag:ref=$tag_photo"   >/dev/null
cli_repo set 'ext = "mkv" OR ext = "mp4"'  "tag:ref=$tag_video"   >/dev/null
cli_repo set 'ext = "pdf" OR ext = "epub"' "tag:ref=$tag_book"    >/dev/null
cli_repo set 'ext = "txt"'                 "tag:ref=$tag_article"  >/dev/null
elapsed=$(ms_since "$t_phase")
row "assign tags to $N_ENTRIES entries — 5 batch ops" "$elapsed" "   ($((elapsed/5)) ms/op)"
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

section "->* traversal queries"

time_query 'tag -> (label = "jazz")  [direct 1-hop]'                   'tag -> (label = "jazz")'
time_query 'tag -> (parent -> (label = "audio"))  [2-hop]'             'tag -> (parent -> (label = "audio"))'
time_query 'tag -> (parent ->* (label = "audio"))  [transitive L2]'    'tag -> (parent ->* (label = "audio"))'
time_query 'tag -> (parent ->* (label = "media"))  [transitive L1]'    'tag -> (parent ->* (label = "media"))'
time_query 'tag -> (parent ->* (label = "docs"))   [transitive L1]'    'tag -> (parent ->* (label = "docs"))'
echo "  --"
time_query 'tag -> (parent ->* (label = "media")) AND ext = "mp3"'     'tag -> (parent ->* (label = "media")) AND ext = "mp3"'
time_query 'NOT (tag -> (parent ->* (label = "media")))'               'NOT (tag -> (parent ->* (label = "media")))'
printf "  daemon RSS : %s\n" "$(daemon_rss "$DAEMON_PID")"

# ── Summary ───────────────────────────────────────────────────────────────────

section "Summary"

total=$(cli_repo list 2>/dev/null | grep -c . || true)
printf "  %-26s %d\n"  "Total entries in repo:"  "$total"
printf "  %-26s %s\n"  "Daemon RSS:"              "$(daemon_rss "$DAEMON_PID")"
printf "  %-26s %s\n"  "bench_data:"              "$BENCH_DIR"

echo
echo "=== done ==="

# trash ./bench_data/.metafolder/
