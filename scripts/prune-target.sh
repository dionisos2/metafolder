#!/usr/bin/env bash
# Prunes superseded build artifacts from a cargo target/ directory.
#
# cargo never garbage-collects target/: every dependency bump, feature-set
# change or rustc update writes new hash-named artifacts NEXT TO the old ones
# (see rust-lang/cargo#13136 for the still-unimplemented native GC). This
# script removes what those events superseded, with three passes:
#
#   1. Diff pass — a state file remembers the artifact hashes seen per crate
#      name on the previous run. When a crate name gained a NEW hash since
#      then, the previously-seen hashes of that name are deleted. Anything
#      deleted by mistake is merely recompiled by the next build (artifacts
#      are always regenerable), so the worst case is compile time, never
#      corruption.
#   2. Lock pass (stateless) — a deps/*.d file whose sources live in the
#      registry identifies its crate version; if Cargo.lock no longer
#      contains that exact version, the artifact is orphaned and deleted.
#      Workspace crates and test binaries have local sources only and are
#      never touched by this pass.
#   3. Incremental pass — cargo keeps one incremental-compilation cache dir
#      per crate generation under <profile>/incremental/, keyed by a different
#      hash encoding than deps/ (c_metadata vs c_extra_filename), so a dir
#      cannot be matched to its artifacts by name; the count is what is
#      reliable. A name with no surviving generation in deps/ loses its whole
#      cache, and a name whose generation set is known to have changed (a
#      gain since the previous run, or a first run with no recorded state)
#      keeps one dir per surviving generation (the most recently used) and
#      loses the older ones. A deleted cache dir costs a recompile.
#   4. Session pass — inside a cache dir that stays, rustc writes each
#      compilation as a new session dir (s-<time>-<random>-<svh>, locked by
#      s-<time>-<random>.lock) and deletes the previous one only on the NEXT
#      successful compilation of that crate: a crate not rebuilt since, or a
#      build killed mid-way, keeps a full second copy (a third of incremental/
#      on a real target/). Every session older than the newest finalized one
#      is deleted with its lock; a newer one — a build in progress, still
#      named -working — is left alone.
#
# Usage: scripts/prune-target.sh [--dry-run] [TARGET_DIR]
#   TARGET_DIR defaults to ./target; Cargo.lock is expected next to it.
#   --dry-run reports what would be deleted without deleting (and without
#   updating the state file).
#
# Run it right after a successful build (the fresh artifacts are then the
# "new" generation). First run only records state. Generic: no metafolder
# assumption, works on any cargo project. A target/ that predates the
# incremental pass still holds cache dirs for generations this script's state
# never knew: delete the state file once, so the next run's first-run cap
# reclaims them.

set -euo pipefail

dry_run=0
target_dir=target
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) target_dir=$arg ;;
    esac
done

[ -d "$target_dir" ] || { echo "error: no such directory: $target_dir" >&2; exit 1; }
target_dir=$(realpath "$target_dir")
state_file="$target_dir/.prune-target-state"
lock_file="$(dirname "$target_dir")/Cargo.lock"

# ---- scan: every hash-named artifact, as "profile|stem|hash|kind|name" ------
# One find, parsed by awk: a bash loop matching a regex per entry took half a
# minute on a real target/ (10k entries). kind is deps, .fingerprint or build;
# stem is the name up to its first dot minus the -<hash> suffix. Only
# profiles holding a deps/ directory count. Stems are kept literal (libfoo and
# foo are tracked as separate stems); the deletion step bridges the lib prefix
# so one stale hash removes all kinds.
list_artifacts() {
    find "$target_dir" -mindepth 2 -maxdepth 3 -printf '%P\n' | awk -F/ '
        NF == 2 && $2 == "deps" { has_deps[$1] = 1; next }
        NF != 3 || ($2 != "deps" && $2 != ".fingerprint" && $2 != "build") { next }
        {
            base = $3; sub(/\..*/, "", base)
            if (!match(base, /-[0-9a-f]{16}$/) || RSTART == 1) next
            rows[++n] = $1 "|" substr(base, 1, RSTART - 1) "|" substr(base, RSTART + 1) "|" $2 "|" $3
            prof[n] = $1
        }
        END { for (i = 1; i <= n; i++) if (prof[i] in has_deps) print rows[i] }'
}

# "profile|stem|hash" for every hash-named artifact (the state file's form).
scan() {
    cut -d'|' -f1-3 <<<"$artifacts" | sort -u
}

# ---- deps-only scan (pass 3) ------------------------------------------------
# Like scan(), but deps/ only: a generation survives in deps/ or not at all —
# a leftover .fingerprint or build/ dir whose rlib is gone cannot use an
# incremental cache, so it must not keep one alive either.
deps_scan() {
    awk -F'|' '$4 == "deps" { print $1 "|" $2 "|" $3 }' <<<"$artifacts" | sort -u
}

# ---- deletion helper --------------------------------------------------------
# Removes every artifact kind for (profile, stem, hash), bridging the lib
# prefix both ways (libfoo rlibs vs foo .d/fingerprint/build entries).
declare -a doomed=()
declare -A doomed_gen=()    # "profile|name|hash" triples some pass doomed
# The crate name behind an artifact stem: drop the lib prefix and unify the
# separator, so libfoo_bar, foo_bar.d and an incremental/ foo_bar-h dir all
# land on one name. (A lib crate and a bin crate can then collide — e.g. a
# lib "rarian" and a bin "librarian" — which only ever over-counts the
# surviving generations: a few extra cache dirs survive. Never destructive.)
# Sets $norm rather than printing: it runs once per artifact, and a $(...)
# around it forked a subshell each time — thousands of forks per run.
norm_name() {
    norm=${1#lib}
    norm=${norm//_/-}
}
mark() {
    local profile=$1 stem=$2 hash=$3 alt s path
    if [[ $stem == lib?* ]]; then alt=${stem#lib}; else alt=lib$stem; fi
    norm_name "$stem"
    doomed_gen["$profile|$norm|$hash"]=1
    for s in "$stem" "$alt"; do
        while IFS= read -r path; do
            [ -n "$path" ] && doomed+=("$path")
        done <<<"${gen_paths[$profile|$s|$hash]:-}"
    done
    return 0
}

# Same, for a path that needs no stem/hash bridging (incremental cache dirs).
declare -A doomed_path=()
mark_path() {
    [ -e "$1" ] && doomed+=("$1") && doomed_path["$1"]=1
    return 0
}

artifacts=$(list_artifacts)
# The paths behind each "profile|stem|hash", for mark(): deps/ entries named
# <stem>-<hash> or <stem>-<hash>.<ext>, .fingerprint/ and build/ entries named
# exactly <stem>-<hash>. Looked up here rather than globbed per mark, which
# listed the whole deps/ directory each time.
declare -A gen_paths=()
while IFS='|' read -r profile stem hash kind name; do
    [ -n "$stem" ] || continue
    [ "$kind" = deps ] || [ "$name" = "$stem-$hash" ] || continue
    gen_paths["$profile|$stem|$hash"]+="$target_dir/$profile/$kind/$name"$'\n'
done <<<"$artifacts"
current=$(scan)

# ---- pass 1: diff against the previous run's state --------------------------
declare -A gained=()    # "profile|name": the name gained a hash absent from state
# One awk over both lists: a grep of the state file per artifact was one fork
# per artifact, and on a real target/ (10k artifacts) most of the run time.
# Emits "G|profile|name" for each name that gained a hash absent from the
# previous state, and "M|profile|stem|hash" for each previously-seen hash of
# that stem still present, to be pruned.
if [ -f "$state_file" ]; then
    while IFS='|' read -r kind profile a b; do
        case $kind in
            G) gained["$profile|$a"]=1 ;;
            M) mark "$profile" "$a" "$b" ;;
        esac
    done < <(printf '%s\n' "$current" | awk -F'|' '
        FNR == NR { if ($2 != "") { old[$0] = 1; hashes[$1 "|" $2] = hashes[$1 "|" $2] " " $3 }; next }
        $2 == "" { next }
        { cur[$0] = 1; lines[++n] = $0 }
        END {
            for (i = 1; i <= n; i++) {
                if (lines[i] in old) continue
                split(lines[i], f, "|")
                name = f[2]; sub(/^lib/, "", name); gsub(/_/, "-", name)
                print "G|" f[1] "|" name
                k = f[1] "|" f[2]
                m = split(hashes[k], hs, " ")
                for (j = 1; j <= m; j++) {
                    if (hs[j] == f[3]) continue
                    if ((k "|" hs[j]) in cur && !((k "|" hs[j]) in done)) {
                        done[k "|" hs[j]] = 1
                        print "M|" k "|" hs[j]
                    }
                }
            }
        }' "$state_file" -)
fi

# ---- pass 2: registry versions no longer in Cargo.lock ----------------------
# One awk over every .d file (it stops reading each at its first registry
# path): a grep|head|awk per file was four forks per artifact.
if [ -f "$lock_file" ]; then
    # shellcheck disable=SC2016  # an awk program, not a shell string
    while IFS='|' read -r profile stem hash; do
        mark "$profile" "$stem" "$hash"
    done < <(find "$target_dir" -mindepth 3 -maxdepth 3 -path '*/deps/*.d' -print0 |
             xargs -0 -r awk -v lock="$lock_file" '
        FILENAME == lock {
            if ($1 == "name" && $2 == "=")    { n = $3; gsub(/"/, "", n) }
            if ($1 == "version" && $2 == "=") { v = $3; gsub(/"/, "", v); sub(/\+.*/, "", v); inlock[n "-" v] = 1 }
            next
        }
        match($0, /\/registry\/src\/[^\/ ]+\/[^\/ ]+/) {
            m = split(substr($0, RSTART, RLENGTH), parts, "/")
            pkg = parts[m]; sub(/\+.*/, "", pkg)     # drop semver build metadata
            if (!(pkg in inlock)) {
                c = split(FILENAME, fp, "/")
                base = fp[c]; sub(/\.d$/, "", base)
                if (match(base, /-[0-9a-f]{16}$/) && RSTART > 1)
                    print fp[c - 2] "|" substr(base, 1, RSTART - 1) "|" substr(base, RSTART + 1)
            }
            nextfile                                # local sources: never pruned
        }' "$lock_file")
else
    echo "note: $lock_file not found, skipping the Cargo.lock pass" >&2
fi

# ---- pass 3: incremental/ cache dirs beyond the surviving generations -------
# incremental/ is keyed by a different hash encoding than deps/, so a dir
# cannot be matched to its artifacts by name; the COUNT is reliable — one dir
# per crate generation. Two rules doom a cache dir, counted from deps/ minus
# what the passes above just doomed (a doomed generation does not survive):
#   - the name has no surviving generation at all: nothing can use the cache;
#   - the name's generation set is known to have changed — it gained a
#     generation this run, or there is no previous state at all (first run):
#     it keeps one dir per surviving generation, the most recently used, and
#     loses the older ones. The gain trigger matters on later runs: a name
#     that merely LOST a generation (its artifacts were pruned, nothing
#     rebuilt yet) keeps its cache — the next build may still rejoin it, and
#     --dry-run must show that truth. A first run has no state to lose from:
#     any dir beyond the surviving count predates this script, which will
#     never observe the transition that superseded it.
declare -A ngen=()      # "profile|name" -> surviving generations in deps/
declare -A seen_gen=()  # "profile|name|hash" dedup across artifact kinds
while IFS='|' read -r profile stem hash; do
    [ -n "$stem" ] || continue
    norm_name "$stem"
    key="$profile|$norm|$hash"
    [ -n "${doomed_gen[$key]+x}" ] && continue
    [ -n "${seen_gen[$key]+x}" ] && continue
    seen_gen[$key]=1
    name_key=${key%|*}                          # "profile|name"
    ngen[$name_key]=$(( ${ngen[$name_key]:-0} + 1 ))
done < <(deps_scan)

# Group the cache dirs per (profile, name), remembering each one's mtime —
# "most recently used" is the only order the different hash encoding allows.
declare -A inc_dirs=()  # "profile|name" -> "mtime<TAB>dir" lines
for p in "$target_dir"/*/; do
    [ -d "${p}incremental" ] || continue
    profile=$(basename "$p")
    while IFS=$'\t' read -r mtime base; do
        [[ $base == *-* ]] || continue          # no hash suffix: leave it alone
        norm_name "${base%-*}"
        inc_dirs["$profile|$norm"]+="${mtime}"$'\t'"${base}"$'\n'
    done < <(find "${p}incremental" -mindepth 1 -maxdepth 1 -type d -printf '%T@\t%f\n')
done

if [ ${#inc_dirs[@]} -gt 0 ]; then
    for name_key in "${!inc_dirs[@]}"; do
        profile=${name_key%%|*}
        n=${ngen[$name_key]:-0}
        if [ "$n" -eq 0 ]; then
            # no surviving generation: no artifact of this name can use a cache
            while IFS=$'\t' read -r _ base; do
                [ -n "$base" ] || continue
                mark_path "$target_dir/$profile/incremental/$base"
            done <<<"${inc_dirs[$name_key]}"
            continue
        fi
        # The cap fires when the name's generation set is known to have
        # changed: a gain this run — or a first run, with no recorded state
        # (see the pass header above).
        [ -n "${gained[$name_key]+x}" ] || [ ! -f "$state_file" ] || continue
        keep=0
        while IFS=$'\t' read -r _ base; do
            [ -n "$base" ] || continue
            keep=$(( keep + 1 ))
            [ "$keep" -le "$n" ] || mark_path "$target_dir/$profile/incremental/$base"
        done < <(printf '%s' "${inc_dirs[$name_key]}" | sort -rn)
    done
fi

# ---- pass 4: superseded sessions inside the cache dirs that stay -------------
# Session names sort by their time field (fixed-width base 36), so the newest
# finalized session is the last non-working one in name order; everything
# before it is dead. Its lock is named by the session's first two fields.
# One find over every cache dir, grouped by awk: a find|sort|tail per cache
# dir and a cut per session were several forks each. Emits "cache<TAB>session".
while IFS=$'\t' read -r cache session; do
    [ -z "${doomed_path[$cache]+x}" ] || continue
    [[ $session =~ ^(s-[^-]*-[^-]*) ]] || continue
    mark_path "$cache/$session"
    mark_path "$cache/${BASH_REMATCH[1]}.lock"
done < <(find "$target_dir" -mindepth 4 -maxdepth 4 -type d -path '*/incremental/*/s-*' \
             -printf '%h\t%f\n' | awk -F'\t' '
    function key(s,   f) { split(s, f, "-"); return f[1] "-" f[2] "-" f[3] }
    { cache[NR] = $1; sess[NR] = $2 }
    $2 !~ /-working$/ && (!($1 in newest) || $2 > newest[$1]) { newest[$1] = $2 }
    END {
        for (i = 1; i <= NR; i++) {
            c = cache[i]
            if (!(c in newest)) continue
            if (key(sess[i]) < key(newest[c])) print c "\t" sess[i]
        }
    }')

# ---- execute and report ------------------------------------------------------
# The doomed list routinely runs to tens of thousands of paths, well past
# ARG_MAX: passing it as command-line arguments made both du and rm fail with
# "Argument list too long", so nothing was deleted and the size read 0 — a
# silent no-op on exactly the overgrown target/ this script exists for. The
# list is therefore piped rather than passed (`printf` is a shell builtin, so
# feeding it is not subject to the limit in the first place).
if [ ${#doomed[@]} -eq 0 ]; then
    echo "nothing to prune"
else
    # du reads the whole list in ONE pass (`--files0-from=-`) rather than in
    # xargs chunks: it only de-duplicates hard links it sees within a single
    # invocation, and cargo hard-links most of what it writes into deps/ — split
    # across chunks, the same bytes get counted once per chunk (an over-report
    # of 30x on a real target/).
    freed=$(printf '%s\0' "${doomed[@]}" | du -scb --files0-from=- 2>/dev/null |
            tail -1 | cut -f1 || true)
    freed=${freed:-0}
    human=$(numfmt --to=iec "$freed" 2>/dev/null || echo "${freed}B")
    if [ "$dry_run" -eq 1 ]; then
        printf '%s\n' "${doomed[@]}"
        echo "dry run: would prune ${#doomed[@]} paths, freeing $human"
    else
        printf '%s\0' "${doomed[@]}" | xargs -0 rm -rf --
        echo "pruned ${#doomed[@]} paths, freed $human"
    fi
fi

# Record the post-prune generation as the new baseline (not on dry runs, so
# the next real run still prunes what the dry run reported).
if [ "$dry_run" -eq 0 ]; then
    artifacts=$(list_artifacts)
    scan > "$state_file"
fi
