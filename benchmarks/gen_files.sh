#!/usr/bin/env bash
# Generate a directory tree of random files for benchmarking.
#
# Usage: gen_files.sh [OPTIONS]
#   -o, --output DIR      Output directory          (default: ./bench_data)
#   -n, --count N         Number of files to create (default: 1000)
#   -s, --size BYTES      Size of each file          (default: 4096)
#   -h, --help            Show this help

set -euo pipefail

OUTPUT="./bench_data"
COUNT=1000
SIZE=4096

usage() {
    grep '^#' "$0" | sed 's/^# \{0,2\}//'
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -o|--output) OUTPUT="$2"; shift 2 ;;
        -n|--count)  COUNT="$2";  shift 2 ;;
        -s|--size)   SIZE="$2";   shift 2 ;;
        -h|--help)   usage ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

EXTENSIONS=("mp3" "flac" "jpg" "png" "pdf" "txt" "epub" "mkv" "mp4" "zip")
DIRS=(
    "music/jazz"
    "music/rock"
    "music/classical"
    "images/photos"
    "images/art"
    "books"
    "videos"
    "documents"
    "archives"
    "misc"
    "music/jazz/albums/2020"
    "music/jazz/albums/2021"
    "music/rock/live/concerts/2019"
    "music/classical/composers/bach/cantatas"
    "images/photos/trips/japan/2023"
    "images/photos/trips/norway/2022/summer"
    "images/art/digital/portraits/sketches"
    "documents/work/projects/2024/reports"
    "documents/personal/finance/2023/receipts"
    "books/sci-fi/series/dune"
)

echo "Generating $COUNT files of ${SIZE} bytes in '$OUTPUT'..."

# Create subdirectories
for d in "${DIRS[@]}"; do
    mkdir -p "$OUTPUT/$d"
done

for i in $(seq 1 "$COUNT"); do
    dir="${DIRS[$((RANDOM % ${#DIRS[@]}))]}"
    ext="${EXTENSIONS[$((RANDOM % ${#EXTENSIONS[@]}))]}"
    name="$(cat /proc/sys/kernel/random/uuid | tr -d '-').$ext"
    dd if=/dev/urandom of="$OUTPUT/$dir/$name" bs="$SIZE" count=1 status=none
done

echo "Done. $(find "$OUTPUT" -type f | wc -l) files created in '$OUTPUT'."
