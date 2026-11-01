#!/usr/bin/env bash
# Deletes from a cargo profile directory what cargo did not report using.
#
# cargo never collects target/ (rust-lang/cargo#13136). It does know what a build
# uses: `--message-format=json` lists every artifact, fresh ones too. So the live
# list is cargo's, and everything else named with a hash is dead. Inferring it
# from the artifacts instead does not work: one crate has several live generations
# (toml_edit has four), and a script taking a new hash for a replacement made
# builds evict each other (doc "Keeping the target directory small").
#
# Usage: prune-target.sh [--dry-run] PROFILE_DIR LIVE_LIST
#   PROFILE_DIR  e.g. target/debug
#   LIVE_LIST    the JSON messages of EVERY build whose artifacts should survive
#
# Deleting here can only cost a recompile.

set -euo pipefail

dry_run=0
if [ "${1:-}" = --dry-run ]; then dry_run=1; shift; fi
[ $# -eq 2 ] || { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
profile=$1
list=$2

# An empty or partial list reads as "all of this is dead".
[ -d "$profile" ] || { echo "error: no such directory: $profile" >&2; exit 1; }
[ -s "$list" ] || { echo "error: no live list: $list" >&2; exit 1; }
if grep -q '"reason":"build-finished","success":false' "$list"; then
    echo "error: $list holds a failed build: pruning nothing" >&2
    exit 1
fi

work=$(mktemp)
trap 'rm -f "$work"' EXIT

# "name-hash" of everything live, however the message spells it.
grep -oE '[A-Za-z0-9_]+-[0-9a-f]{16}' "$list" | sort -u >"$work"

# A binary is listed under its uplifted name, target/debug/mf, which holds no hash.
# Its originals are deps/mf-<hash>: as many as were reported, the most recent, not
# counting a test build of it, which is listed by hash already.
{ grep -oE '"executable":"[^"]*"' "$list" || true; } | sed -E 's#.*/##; s/"$//' |
    { grep -vE -- '-[0-9a-f]{16}$' || true; } | tr - _ | sort | uniq -c |
    while read -r count name; do
        find "$profile/deps" -mindepth 1 -maxdepth 1 -type f -name "$name-*" ! -name '*.*' \
                -printf '%T@ %f\n' 2>/dev/null | sort -rn | cut -d' ' -f2 |
            grep -vxFf "$work" | head -n "$count" || true
    done >>"$work"

doomed=$(
    # deps/, .fingerprint/, build/: an entry whose hash is not live.
    find "$profile/deps" "$profile/.fingerprint" "$profile/build" \
            -mindepth 1 -maxdepth 1 -printf '%p\n' 2>/dev/null |
        awk -v live="$work" '
            BEGIN { while ((getline l < live) > 0) { sub(/.*-/, "", l); hash[l] = 1 } }
            {
                base = $0; sub(/.*\//, "", base); sub(/\..*/, "", base)
                if (match(base, /-[0-9a-f]{16}$/) && !(substr(base, RSTART + 1) in hash)) print
            }'

    # incremental/: named with another hash, so only the count can be read: as
    # many caches per crate as it has live generations, the most recent ones.
    if [ -d "$profile/incremental" ]; then
        find "$profile/incremental" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' |
            sort -rn | awk -v live="$work" '
                function norm(s) { sub(/^lib/, "", s); gsub(/-/, "_", s); return s }
                BEGIN {
                    while ((getline l < live) > 0) {
                        h = l; sub(/.*-/, "", h); sub(/-[0-9a-f]{16}$/, "", l)
                        if (!((norm(l) SUBSEP h) in seen)) { seen[norm(l) SUBSEP h] = 1; gens[norm(l)]++ }
                    }
                }
                {
                    path = $2; base = path; sub(/.*\//, "", base); sub(/-[^-]*$/, "", base)
                    if (++kept[norm(base)] > gens[norm(base)] + 0) print path
                }'

        # Inside a cache: rustc removes the previous session only at the next
        # compilation of that crate. The newest finalized one is what it reads;
        # a newer one still named -working is a build in progress.
        find "$profile/incremental" -mindepth 2 -maxdepth 2 -type d -name 's-*' -printf '%h\t%f\n' |
            awk -F'\t' '
                function key(s, f) { split(s, f, "-"); return f[1] "-" f[2] "-" f[3] }
                { cache[NR] = $1; sess[NR] = $2 }
                $2 !~ /-working$/ && key($2) > newest[$1] { newest[$1] = key($2) }
                END {
                    for (i = 1; i <= NR; i++)
                        if (cache[i] in newest && key(sess[i]) < newest[cache[i]]) {
                            print cache[i] "/" sess[i]
                            print cache[i] "/" key(sess[i]) ".lock"
                        }
                }'
    fi
)

if [ -z "$doomed" ]; then
    echo "nothing to prune"
    exit 0
fi

count=$(wc -l <<<"$doomed")
# One du over the whole list: it only sees a hard link twice within one invocation.
freed=$(tr '\n' '\0' <<<"$doomed" | du -scb --files0-from=- 2>/dev/null | tail -1 | cut -f1 || true)
human=$(numfmt --to=iec "${freed:-0}" 2>/dev/null || echo "${freed:-0}B")
if [ "$dry_run" -eq 1 ]; then
    printf '%s\n' "$doomed"
    echo "dry run: would prune $count paths, freeing $human"
else
    tr '\n' '\0' <<<"$doomed" | xargs -0 rm -rf --
    echo "pruned $count paths, freed $human"
fi
