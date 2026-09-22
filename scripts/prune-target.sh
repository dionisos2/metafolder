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
        -h|--help) sed -n '2,39p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) target_dir=$arg ;;
    esac
done

[ -d "$target_dir" ] || { echo "error: no such directory: $target_dir" >&2; exit 1; }
target_dir=$(realpath "$target_dir")
state_file="$target_dir/.prune-target-state"
lock_file="$(dirname "$target_dir")/Cargo.lock"

# ---- scan: emit "profile|stem|hash" for every hash-named artifact ----------
# Stems are kept literal (libfoo and foo are tracked as separate stems); the
# deletion step bridges the lib prefix so one stale hash removes all kinds.
scan() {
    local p profile entry base
    for p in "$target_dir"/*/; do
        [ -d "$p/deps" ] || continue
        profile=$(basename "$p")
        for entry in "$p"deps/* "$p".fingerprint/* "$p"build/*; do
            [ -e "$entry" ] || continue
            base=$(basename "$entry")
            base=${base%%.*}
            if [[ $base =~ ^(.+)-([0-9a-f]{16})$ ]]; then
                echo "$profile|${BASH_REMATCH[1]}|${BASH_REMATCH[2]}"
            fi
        done
    done | sort -u
}

# ---- deps-only scan (pass 3) ------------------------------------------------
# Like scan(), but deps/ only: a generation survives in deps/ or not at all —
# a leftover .fingerprint or build/ dir whose rlib is gone cannot use an
# incremental cache, so it must not keep one alive either.
deps_scan() {
    local p profile entry base
    for p in "$target_dir"/*/; do
        [ -d "$p/deps" ] || continue
        profile=$(basename "$p")
        for entry in "$p"deps/*; do
            [ -e "$entry" ] || continue
            base=$(basename "$entry")
            base=${base%%.*}
            if [[ $base =~ ^(.+)-([0-9a-f]{16})$ ]]; then
                echo "$profile|${BASH_REMATCH[1]}|${BASH_REMATCH[2]}"
            fi
        done
    done | sort -u
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
norm_name() {
    local n=$1
    n=${n#lib}
    n=${n//_/-}
    printf '%s' "$n"
}
mark() {
    local profile=$1 stem=$2 hash=$3 alt s path
    if [[ $stem == lib?* ]]; then alt=${stem#lib}; else alt=lib$stem; fi
    doomed_gen["$profile|$(norm_name "$stem")|$hash"]=1
    for s in "$stem" "$alt"; do
        for path in "$target_dir/$profile/deps/$s-$hash" \
                    "$target_dir/$profile/deps/$s-$hash".* \
                    "$target_dir/$profile/.fingerprint/$s-$hash" \
                    "$target_dir/$profile/build/$s-$hash"; do
            [ -e "$path" ] && doomed+=("$path")
        done
    done
    return 0
}

# Same, for a path that needs no stem/hash bridging (incremental cache dirs).
mark_path() {
    [ -e "$1" ] && doomed+=("$1")
    return 0
}

current=$(scan)

# ---- pass 1: diff against the previous run's state --------------------------
declare -A gained=()    # "profile|name": the name gained a hash absent from state
if [ -f "$state_file" ]; then
    # Names that gained a hash absent from the previous state...
    while IFS='|' read -r profile stem hash; do
        [ -n "$stem" ] || continue
        if ! grep -qxF "$profile|$stem|$hash" "$state_file"; then
            gained["$profile|$(norm_name "$stem")"]=1
            # ...get their previously-seen, still-present hashes pruned.
            while IFS='|' read -r _ _ old_hash; do
                [ "$old_hash" != "$hash" ] || continue
                grep -qxF "$profile|$stem|$old_hash" <<<"$current" &&
                    mark "$profile" "$stem" "$old_hash"
            done < <(grep -F "$profile|$stem|" "$state_file" |
                     awk -F'|' -v s="$stem" -v p="$profile" '$1==p && $2==s')
        fi
    done <<<"$current"
fi

# ---- pass 2: registry versions no longer in Cargo.lock ----------------------
if [ -f "$lock_file" ]; then
    lock_versions=$(awk '
        /^name = /    { n=$3; gsub(/"/,"",n) }
        /^version = / { v=$3; gsub(/"/,"",v); sub(/\+.*/,"",v); print n "-" v }
    ' "$lock_file")
    for p in "$target_dir"/*/; do
        [ -d "$p/deps" ] || continue
        profile=$(basename "$p")
        for dfile in "$p"deps/*.d; do
            [ -e "$dfile" ] || continue
            pkgdir=$(grep -oE '/registry/src/[^/ ]+/[^/ ]+' "$dfile" |
                     head -1 | awk -F/ '{print $NF}' || true)
            [ -n "$pkgdir" ] || continue        # local sources: never pruned
            pkg=${pkgdir%%+*}                   # drop semver build metadata
            grep -qxF "$pkg" <<<"$lock_versions" && continue
            base=$(basename "$dfile" .d)
            if [[ $base =~ ^(.+)-([0-9a-f]{16})$ ]]; then
                mark "$profile" "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
            fi
        done
    done
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
    name=$(norm_name "$stem")
    key="$profile|$name|$hash"
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
        name=$(norm_name "${base%-*}")
        inc_dirs["$profile|$name"]+="${mtime}"$'\t'"${base}"$'\n'
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
    scan > "$state_file"
fi
