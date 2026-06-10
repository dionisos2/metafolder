#!/usr/bin/env bash
# Tests for gen_files.sh.
#
# Generates a small tree in a temp directory and checks the guarantees
# advertised by gen_files.sh: file count, content size range, random tree
# shape, duplicate-content files, magic headers (mime), permission variety,
# and seed reproducibility.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
GEN="$SCRIPT_DIR/gen_files.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAILED=0

check() { # check LABEL STATUS  (STATUS 0 = ok)
    local label="$1" status="$2"
    if [[ "$status" == "0" ]]; then
        printf 'ok   - %s\n' "$label"
    else
        printf 'FAIL - %s\n' "$label"
        FAILED=$((FAILED + 1))
    fi
}

N=300
MIN=100
MAX=200
NDIRS=15
DEPTH=4
DUP=20
SEED=42

OUT="$TMP/run1"
"$GEN" -o "$OUT" -n "$N" --min-size "$MIN" --max-size "$MAX" \
       --dirs "$NDIRS" --max-depth "$DEPTH" --dup-percent "$DUP" \
       --seed "$SEED" >/dev/null 2>&1
check "generator accepts all options and exits 0" $?

# ── File count ────────────────────────────────────────────────────────────────
n_files=$(find "$OUT" -type f 2>/dev/null | wc -l)
[[ "$n_files" -eq "$N" ]]
check "creates exactly $N files (got $n_files)" $?

# ── Content size range ───────────────────────────────────────────────────────
bad_sizes=$(find "$OUT" -type f -printf '%s\n' 2>/dev/null \
            | awk -v min="$MIN" -v max="$MAX" '$1 < min || $1 > max' | wc -l)
[[ "$bad_sizes" -eq 0 ]]
check "every file size is within [$MIN, $MAX] ($bad_sizes outside)" $?

# ── Tree shape ───────────────────────────────────────────────────────────────
n_dirs=$(find "$OUT" -mindepth 1 -type d 2>/dev/null | wc -l)
[[ "$n_dirs" -eq "$NDIRS" ]]
check "creates exactly $NDIRS directories (got $n_dirs)" $?

max_depth=$(find "$OUT" -mindepth 1 -type d 2>/dev/null \
            | sed "s|^$OUT/||" | awk -F/ '{print NF}' | sort -n | tail -1)
[[ -n "$max_depth" && "$max_depth" -le "$DEPTH" ]]
check "directory depth never exceeds $DEPTH (max seen: ${max_depth:-none})" $?

[[ -n "$max_depth" && "$max_depth" -ge 2 ]]
check "tree is not flat (max depth >= 2, seen: ${max_depth:-none})" $?

# ── Duplicate contents ───────────────────────────────────────────────────────
# With --dup-percent 20 on 300 files we expect ~60 duplicates; >= 30 is a
# safe lower bound (> 4 sigma below the mean of the binomial).
hashes=$(find "$OUT" -type f -exec md5sum {} + 2>/dev/null | awk '{print $1}')
total=$(echo "$hashes" | wc -l)
unique=$(echo "$hashes" | sort -u | wc -l)
n_dup=$((total - unique))
[[ "$n_dup" -ge 30 ]]
check "duplicate-content files exist (~${DUP}% requested, got $n_dup/$N)" $?

# ── Magic headers (mime) ─────────────────────────────────────────────────────
pdf=$(find "$OUT" -name '*.pdf' -type f 2>/dev/null | head -1)
if [[ -n "$pdf" ]]; then
    [[ "$(head -c 4 "$pdf")" == "%PDF" ]]
    check "pdf files start with %PDF magic" $?
else
    check "pdf files start with %PDF magic (no pdf generated)" 1
fi

png=$(find "$OUT" -name '*.png' -type f 2>/dev/null | head -1)
if [[ -n "$png" ]]; then
    [[ "$(head -c 8 "$png" | od -An -tx1 | tr -d ' \n')" == "89504e470d0a1a0a" ]]
    check "png files start with PNG signature" $?
else
    check "png files start with PNG signature (no png generated)" 1
fi

# ── Permission variety ───────────────────────────────────────────────────────
n_modes=$(find "$OUT" -type f -printf '%m\n' 2>/dev/null | sort -u | wc -l)
[[ "$n_modes" -ge 2 ]]
check "at least two distinct permission modes (got $n_modes)" $?

# ── Seed reproducibility ─────────────────────────────────────────────────────
OUT2="$TMP/run2"
"$GEN" -o "$OUT2" -n "$N" --min-size "$MIN" --max-size "$MAX" \
       --dirs "$NDIRS" --max-depth "$DEPTH" --dup-percent "$DUP" \
       --seed "$SEED" >/dev/null 2>&1

list1=$(find "$OUT"  -mindepth 1 2>/dev/null | sed "s|^$OUT/||"  | sort)
list2=$(find "$OUT2" -mindepth 1 2>/dev/null | sed "s|^$OUT2/||" | sort)
[[ -n "$list1" && "$list1" == "$list2" ]]
check "same seed reproduces the same tree and file paths" $?

echo
if [[ "$FAILED" -eq 0 ]]; then
    echo "All tests passed."
else
    echo "$FAILED test(s) failed."
    exit 1
fi
