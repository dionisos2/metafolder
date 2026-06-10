#!/usr/bin/env bash
# Generate a random directory tree of files for benchmarking.
#
# The tree shape is random (not hardcoded): top-level directories are drawn
# from a pool of realistic category names (music, images, documents, ...) so
# bench_realistic.sh keeps working, and deeper levels are grown randomly up
# to --max-depth. Files get random text content within a size range, plus a
# magic header matching their extension so content-based mime detection
# works. A configurable fraction of files duplicates the content of an
# earlier file (to exercise hashing / duplicate detection), and permissions
# are varied (mostly 644, with some 755, 600 and 444).
#
# Usage: gen_files.sh [OPTIONS]
#   -o, --output DIR      Output directory                      (default: ./bench_data)
#   -n, --count N         Number of files to create             (default: 1000)
#   --min-size N          Minimum file size in bytes            (default: 100)
#   --max-size N          Maximum file size in bytes            (default: 200)
#   --dirs N              Number of directories in the tree     (default: 25)
#   --max-depth N         Maximum directory depth               (default: 5)
#   --dup-percent P       Percentage of duplicate-content files (default: 10)
#   --seed N              Seed for tree shape and file placement
#                         (content stays random; default: time-based)
#   -h, --help            Show this help

set -euo pipefail

OUTPUT="./bench_data"
COUNT=1000
MIN_SIZE=100
MAX_SIZE=200
N_DIRS=25
MAX_DEPTH=5
DUP_PERCENT=10
SEED=""

usage() {
    grep '^#' "$0" | sed 's/^# \{0,2\}//'
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -o|--output)   OUTPUT="$2";      shift 2 ;;
        -n|--count)    COUNT="$2";       shift 2 ;;
        --min-size)    MIN_SIZE="$2";    shift 2 ;;
        --max-size)    MAX_SIZE="$2";    shift 2 ;;
        --dirs)        N_DIRS="$2";      shift 2 ;;
        --max-depth)   MAX_DEPTH="$2";   shift 2 ;;
        --dup-percent) DUP_PERCENT="$2"; shift 2 ;;
        --seed)        SEED="$2";        shift 2 ;;
        -h|--help)     usage ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

(( MIN_SIZE <= MAX_SIZE )) || { echo "--min-size must be <= --max-size" >&2; exit 1; }
(( MAX_DEPTH >= 1 ))       || { echo "--max-depth must be >= 1"         >&2; exit 1; }
(( N_DIRS >= 1 ))          || { echo "--dirs must be >= 1"              >&2; exit 1; }

[[ -n "$SEED" ]] && RANDOM="$SEED"

# ── Name pools ────────────────────────────────────────────────────────────────

# Top-level names match the categories bench_realistic.sh tags on.
TOP_NAMES=(music images books videos documents archives misc projects downloads)
SUB_NAMES=(albums photos trips art drafts raw old new backup series live
           work personal reports receipts jazz rock classical
           2019 2020 2021 2022 2023 2024)
FILE_WORDS=(report photo track scan invoice draft sample backup export
            note clip mix album song image doc video archive)
EXTENSIONS=(mp3 flac jpg png pdf txt epub mkv mp4 zip)

# Magic headers per extension, as printf '%b' escape strings. These are the
# minimal prefixes libmagic needs to report the expected mime type even with
# random text after them (zip = empty end-of-central-directory record,
# mp3 = MPEG frame sync, mkv = EBML header with DocType "matroska").
declare -A MAGIC=(
    [pdf]='%PDF-1.4\n'
    [png]='\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x10\x00\x00\x00\x10\x08\x02\x00\x00\x00\x90\x91\x68\x36'
    [jpg]='\xff\xd8\xff\xe0'
    [zip]='PK\x05\x06\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00'
    [mp3]='\xff\xfb\x90\x44'
    [flac]='fLaC'
    [mkv]='\x1a\x45\xdf\xa3\x01\x00\x00\x00\x00\x00\x00\x1f\x42\x86\x81\x01\x42\xf7\x81\x01\x42\xf2\x81\x04\x42\xf3\x81\x08\x42\x82\x88matroska'
    [mp4]='\x00\x00\x00\x18ftypisom'
)
declare -A MAGIC_LEN
for ext in "${!MAGIC[@]}"; do
    MAGIC_LEN[$ext]=$(printf '%b' "${MAGIC[$ext]}" | wc -c)
done

# Mostly regular files, some executable, private or read-only.
PERM_POOL=(644 644 644 644 644 755 600 444)

# ── Build a random directory tree ─────────────────────────────────────────────

DIRS=()    # relative paths
DEPTHS=()  # parallel array: depth of DIRS[i]
declare -A SEEN

add_dir() { # add_dir PATH DEPTH
    DIRS+=("$1")
    DEPTHS+=("$2")
    SEEN[$1]=1
}

# Pick 3-6 top-level directories (fewer if --dirs is small).
n_top=$(( 3 + RANDOM % 4 ))
(( n_top > N_DIRS )) && n_top=$N_DIRS
top_pool=("${TOP_NAMES[@]}")
for (( i = 0; i < n_top; i++ )); do
    idx=$(( RANDOM % ${#top_pool[@]} ))
    add_dir "${top_pool[idx]}" 1
    unset 'top_pool[idx]'
    top_pool=("${top_pool[@]}")
done

# Grow the rest of the tree by attaching children to random existing dirs.
while (( ${#DIRS[@]} < N_DIRS )); do
    # Candidate parents are dirs strictly above max depth.
    candidates=()
    for (( i = 0; i < ${#DIRS[@]}; i++ )); do
        (( DEPTHS[i] < MAX_DEPTH )) && candidates+=("$i")
    done
    (( ${#candidates[@]} == 0 )) && break  # everything is at max depth

    p=${candidates[$(( RANDOM % ${#candidates[@]} ))]}
    name=${SUB_NAMES[$(( RANDOM % ${#SUB_NAMES[@]} ))]}
    child="${DIRS[p]}/$name"
    # Disambiguate collisions with a unique numeric suffix.
    [[ -n "${SEEN[$child]:-}" ]] && child="${child}-${#DIRS[@]}"
    add_dir "$child" $(( DEPTHS[p] + 1 ))
done

for d in "${DIRS[@]}"; do
    mkdir -p "$OUTPUT/$d"
done

# ── Generate files ────────────────────────────────────────────────────────────

echo "Generating $COUNT files (${MIN_SIZE}-${MAX_SIZE} bytes, ~${DUP_PERCENT}% duplicates)"
echo "in '$OUTPUT' across ${#DIRS[@]} directories (max depth $MAX_DEPTH)..."

# Random printable text of N bytes (|| true: head closing the pipe is fine).
gen_text() {
    tr -dc 'a-zA-Z0-9 .,\n' </dev/urandom 2>/dev/null | head -c "$1" || true
}

FILES=()
n_dup=0

for (( i = 1; i <= COUNT; i++ )); do
    dir="${DIRS[$(( RANDOM % ${#DIRS[@]} ))]}"
    word="${FILE_WORDS[$(( RANDOM % ${#FILE_WORDS[@]} ))]}"

    if (( ${#FILES[@]} > 0 && RANDOM % 100 < DUP_PERCENT )); then
        # Duplicate the content of an earlier file; keep its extension so
        # the magic header stays coherent with the file type.
        src="${FILES[$(( RANDOM % ${#FILES[@]} ))]}"
        path="$OUTPUT/$dir/${word}_$i.${src##*.}"
        cp "$src" "$path"
        n_dup=$(( n_dup + 1 ))
    else
        ext="${EXTENSIONS[$(( RANDOM % ${#EXTENSIONS[@]} ))]}"
        path="$OUTPUT/$dir/${word}_$i.$ext"
        size=$(( MIN_SIZE + RANDOM % (MAX_SIZE - MIN_SIZE + 1) ))
        hdr_len=${MAGIC_LEN[$ext]:-0}
        # The size range is the hard guarantee: skip the magic header when
        # it would not fit (only happens with very small --max-size).
        if (( hdr_len > 0 && hdr_len <= size )); then
            {
                printf '%b' "${MAGIC[$ext]}"
                gen_text $(( size - hdr_len ))
            } > "$path"
        else
            gen_text "$size" > "$path"
        fi
    fi

    chmod "${PERM_POOL[$(( RANDOM % ${#PERM_POOL[@]} ))]}" "$path"
    FILES+=("$path")
done

echo "Done. $(find "$OUTPUT" -type f | wc -l) files created in '$OUTPUT' ($n_dup duplicates)."
