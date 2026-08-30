#!/usr/bin/env bash
# INTEGRATION tests for the launchable shipped scripts: they run against a real
# `metafolder-daemon` (isolated, ephemeral port) with only the `mf gui …` half
# stubbed. This catches what the mocked-`mf` unit suites cannot — bugs in the
# actual queries the scripts build (e.g. a folder tree-path that the daemon
# resolves to the empty set). See scripts/lib/daemon-fixture.sh.
#
# SKIPs cleanly (exit 0) when the daemon/mf binaries are not built or the daemon
# cannot boot, so it is safe in `check.sh` before a full build.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/.." || exit 1                       # repo root: binaries are target/debug/*
# shellcheck source=lib/daemon-fixture.sh
source "$HERE/lib/daemon-fixture.sh"
# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

if ! df_available; then
    echo "SKIP: target/debug/metafolder-daemon or mf not built (run scripts/complete-build.sh)"
    exit 0
fi
if ! df_start; then
    echo "SKIP: the isolated daemon did not come up"
    exit 0
fi

FOLDER="$HERE/shipped/gui-tag-folder.sh"
PAIR="$HERE/shipped/gui-tag-pair.sh"
CLASSIFY="$HERE/shipped/gui-tag-classify.sh"
UNWATCH="$HERE/shipped/gui-unwatch-folder.sh"

# ── Build a fixture repo:  /  ├ top.txt   └ sub/ ├ inner.txt ──────────────────
REPO_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mf-fix.XXXXXX")
mkdir -p "$REPO_DIR/sub"
printf 'a' >"$REPO_DIR/top.txt"
printf 'b' >"$REPO_DIR/sub/inner.txt"
df_init_repo "$REPO_DIR"
df_hybrid "$DF_REPO"

# Map a repository-relative path (as `mf path --relative` prints it) to a uuid.
uuid_of() { # <relpath>
    local u
    for u in $(df_mf metarecord get 2>/dev/null); do
        [ "$(df_mf path --relative "$u" 2>/dev/null)" = "$1" ] && { printf '%s\n' "$u"; return 0; }
    done
    return 1
}
TOP=$(uuid_of /top.txt)
INNER=$(uuid_of /sub/inner.txt)
SUB=$(uuid_of /sub)

has_tag() { df_tags "$1" | grep -qxF -- "$2"; }   # <uuid> <tagpath>
has_neg() { df_neg  "$1" | grep -qxF -- "$2"; }

# Sanity: the fixture is what we think it is.
assert "fixture: top.txt tracked" [ -n "$TOP" ]
assert "fixture: inner.txt tracked" [ -n "$INNER" ]
assert "fixture: sub dir tracked" [ -n "$SUB" ]

# ── gui-tag-folder on a SUBFOLDER: whole subtree tagged (regression guard) ────
hy_reset
hy_prompt /sub
hy_input y
bash "$FOLDER" subtag >/dev/null 2>&1
assert "subfolder: the /sub node is tagged" has_tag "$SUB" subtag
assert "subfolder: a file under /sub is tagged" has_tag "$INNER" subtag
assert_not "subfolder: a file OUTSIDE /sub is NOT tagged" has_tag "$TOP" subtag

# ── gui-tag-folder on the ROOT: the WHOLE repository subtree must be tagged ───
# This is the bug the mocked suite could not see: the root's relative path is
# "/", but `mfr_path ->* "/"` / `mfr_path = "/"` resolve to the empty set, so a
# naive script tags nothing (or dies resolving the folder).
hy_reset
hy_prompt /
hy_input y
bash "$FOLDER" roottag >/dev/null 2>&1
assert "root: a top-level file is tagged" has_tag "$TOP" roottag
assert "root: a nested file is tagged" has_tag "$INNER" roottag
assert "root: the sub dir is tagged" has_tag "$SUB" roottag

# ── gui-tag-folder on the ROOT, answered "no": whole subtree denied ──────────
hy_reset
hy_prompt /
hy_input n
bash "$FOLDER" rootno >/dev/null 2>&1
assert "root-no: a top-level file is denied" has_neg "$TOP" rootno
assert "root-no: a nested file is denied" has_neg "$INNER" rootno

# ── gui-tag-folder MIXED on the root: descend into the direct children ───────
# Exercises the full breadth-first descent against the real daemon: the child
# listing (`mfr_path -> ""`), the per-child mfr_type read, and applying a
# "yes" child dir to its whole subtree.
hy_reset
hy_prompt /
hy_input m y y             # root=mixed, then both direct children = yes
bash "$FOLDER" mixtag >/dev/null 2>&1
assert "mixed: a top-level file is tagged" has_tag "$TOP" mixtag
assert "mixed: the sub dir is tagged" has_tag "$SUB" mixtag
assert "mixed: a file inside the 'yes' sub dir is tagged" has_tag "$INNER" mixtag

# ── gui-tag-pair: y then n over the two files ────────────────────────────────
hy_reset
hy_prompt ptag
hy_input y n
bash "$PAIR" >/dev/null 2>&1
# One file positive, one negative (order depends on the query, so count both).
pos=$({ has_tag "$TOP" ptag && echo 1; has_tag "$INNER" ptag && echo 1; } | grep -c 1)
neg=$({ has_neg "$TOP" ptag && echo 1; has_neg "$INNER" ptag && echo 1; } | grep -c 1)
assert_eq "pair: exactly one file tagged yes" 1 "$pos"
assert_eq "pair: exactly one file tagged no" 1 "$neg"

# ── gui-tag-classify: the descend loop runs end-to-end on the real daemon ────
# (exercises `mf tag list` + `field get … --resolve path` + gui_tag_next); the
# record already carries tags, so we assert the run COMPLETES rather than a
# specific delta.
hy_reset
hy_input escape            # stop at the first question
cout=$(bash "$CLASSIFY" "$TOP" 2>&1)
assert_contains "classify: completes against the real daemon" "$cout" "terminée"

# ── folder completion must not double-slash mfr_path (regression) ────────────
# `--resolve-tree mfr_path` already returns "/projets" and the root as "", so
# the completion helper must show the root as "/" and never "//projets".
comp=$(
    # shellcheck source=shipped/lib/mf-gui.sh
    source "$HERE/shipped/lib/mf-gui.sh"
    mf_gui_prompt() { cat; }          # echo the completions instead of prompting
    mf_gui_prompt_folder
)
dbl=$(printf '%s\n' "$comp" | grep -c '//' || true)
root=$(printf '%s\n' "$comp" | grep -xc '/' || true)
assert_eq "completion: no double-slashed folder path" 0 "$dbl"
assert "completion: the repository root is offered as /" [ "$root" -ge 1 ]

# ── gui-unwatch-folder on /sub: the folder survives, its contents do not ─────
# Runs LAST: it deletes metarecords the earlier cases rely on. Exercises the
# real `mfr_path ->* "<path>"` subtree query (strict descendants) and the
# mf_watch write, against the daemon.
hy_reset
export MF_UNWATCH_SETTLE=0
hy_prompt /sub
hy_input y
uout=$(bash "$UNWATCH" 2>&1)
assert_contains "unwatch: reports the deletion" "$uout" "deleted 1 metarecords"
assert "unwatch: the folder metarecord survives" [ "$(df_mf path --relative "$SUB" 2>/dev/null)" = /sub ]
assert_eq "unwatch: mf_watch is false on the folder" \
    "false" "$(df_mf metarecord -i "$SUB" field get mf_watch 2>/dev/null | head -1)"
inner_gone() { ! df_mf metarecord -i "$INNER" get >/dev/null 2>&1; }
assert "unwatch: the file inside is gone" inner_gone
assert "unwatch: a file OUTSIDE the folder survives" \
    [ "$(df_mf path --relative "$TOP" 2>/dev/null)" = /top.txt ]

assert_summary
