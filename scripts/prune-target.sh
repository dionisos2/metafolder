#!/usr/bin/env bash
# Prunes superseded build artifacts from a cargo target/ directory.
#
# cargo never garbage-collects target/: every dependency bump, feature-set
# change or rustc update writes new hash-named artifacts NEXT TO the old ones
# (see rust-lang/cargo#13136 for the still-unimplemented native GC). This
# script removes what those events superseded, with two passes:
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
#
# Usage: scripts/prune-target.sh [--dry-run] [TARGET_DIR]
#   TARGET_DIR defaults to ./target; Cargo.lock is expected next to it.
#   --dry-run reports what would be deleted without deleting (and without
#   updating the state file).
#
# Run it right after a successful build (the fresh artifacts are then the
# "new" generation). First run only records state. Generic: no metafolder
# assumption, works on any cargo project.

set -euo pipefail

dry_run=0
target_dir=target
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

# ---- deletion helper --------------------------------------------------------
# Removes every artifact kind for (profile, stem, hash), bridging the lib
# prefix both ways (libfoo rlibs vs foo .d/fingerprint/build entries).
declare -a doomed=()
mark() {
    local profile=$1 stem=$2 hash=$3 alt s
    if [[ $stem == lib?* ]]; then alt=${stem#lib}; else alt=lib$stem; fi
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

current=$(scan)

# ---- pass 1: diff against the previous run's state --------------------------
if [ -f "$state_file" ]; then
    # Names that gained a hash absent from the previous state...
    while IFS='|' read -r profile stem hash; do
        [ -n "$stem" ] || continue
        if ! grep -qxF "$profile|$stem|$hash" "$state_file"; then
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

# ---- execute and report ------------------------------------------------------
if [ ${#doomed[@]} -eq 0 ]; then
    echo "nothing to prune"
else
    freed=$(du -scb "${doomed[@]}" 2>/dev/null | tail -1 | cut -f1 || true)
    freed=${freed:-0}
    human=$(numfmt --to=iec "$freed" 2>/dev/null || echo "${freed}B")
    if [ "$dry_run" -eq 1 ]; then
        printf '%s\n' "${doomed[@]}"
        echo "dry run: would prune ${#doomed[@]} paths, freeing $human"
    else
        rm -rf -- "${doomed[@]}"
        echo "pruned ${#doomed[@]} paths, freed $human"
    fi
fi

# Record the post-prune generation as the new baseline (not on dry runs, so
# the next real run still prunes what the dry run reported).
if [ "$dry_run" -eq 0 ]; then
    scan > "$state_file"
fi
