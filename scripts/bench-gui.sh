#!/usr/bin/env bash
# JS profiling harness for the metafolder GUI (spec-gui "Bench harness").
#
# Drives the running GUI entirely through the mf CLI (mf gui command / view /
# bench) and prints, per scenario, the panel phase timings the panels record
# via performance.measure: mf:list:* / mf:detail:* / mf:fm:* and the
# auto-instrumented daemon round-trips (mf:daemon …). The same measures also
# show up, labelled, in the WebKit inspector Timeline (devtools:open) if you
# want a flame chart alongside these numbers.
#
# Prerequisites:
#   - the GUI is running with a repository loaded in the focused workspace;
#   - the panels in ~/.config/metafolder/gui/ are up to date — after editing the
#     shipped panel sources, run metafolder-sync-config so the instrumentation
#     reaches the config repo's `main` branch the GUI actually serves.
#
# Usage:  scripts/bench-gui.sh [scenario...]
#   with no arguments, runs every scenario. Names: open-list open-detail
#   open-fm list-detail-nav fm-nav paging.
#
# Tunables (env):  MF (cli command, default "mf"), STEP (per-navigation sleep,
#   default 0.25s), SETTLE (post-action settle, default 0.5s), STEPS (navigation
#   / paging iterations, default 10).

set -euo pipefail

MF=${MF:-mf}
STEP=${STEP:-0.25}
SETTLE=${SETTLE:-0.5}
STEPS=${STEPS:-10}

die() { echo "error: $*" >&2; exit 1; }
mfg() { $MF gui "$@"; }

REPO=$(mfg repo) || die "no GUI running, or no repository in the focused workspace"

# Save the current layout so the harness leaves the GUI as it found it.
SAVED_LEFT=$(mfg layout left)
SAVED_RIGHT=$(mfg layout right)
WORKSPACES=()
cleanup() {
    for ws in "${WORKSPACES[@]}"; do mfg workspace rm "$ws" >/dev/null 2>&1 || true; done
    mfg layout left "$SAVED_LEFT" >/dev/null 2>&1 || true
    mfg layout right "$SAVED_RIGHT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# A fresh workspace gives every "open" scenario a first, uncached panel load
# (PanelHost keeps one live iframe per workspace×panel-type, so re-showing a
# type in the same workspace does not reload it).
new_ws() {
    local ws
    ws=$(mfg workspace new --repo "$REPO")
    WORKSPACES+=("$ws")
    echo "$ws"
}

clear_bench() { mfg bench --clear; }

# Pretty per-name aggregation when jq is available, raw JSON otherwise.
report() {
    echo "── $1 ──"
    if command -v jq >/dev/null 2>&1; then
        mfg bench | jq -r '
          .records
          | group_by(.name)
          | map({
              name: .[0].name,
              n: length,
              total: (map(.duration_ms) | add),
              mean:  ((map(.duration_ms) | add) / length),
              max:   (map(.duration_ms) | max),
            })
          | sort_by(-.total)[]
          | "  \(.name)\tn=\(.n)\ttotal=\((.total*100|round)/100)ms\tmean=\((.mean*100|round)/100)ms\tmax=\((.max*100|round)/100)ms"
        ' | { column -t -s "$(printf '\t')" 2>/dev/null || cat; }
    else
        mfg bench
    fi
    echo
}

settle() { sleep "$SETTLE"; }

# ── Scenarios ─────────────────────────────────────────────────────────────

scenario_open_list() {
    local ws; ws=$(new_ws)
    clear_bench
    mfg layout left "$ws" >/dev/null
    mfg view left metarecord-list >/dev/null
    settle
    report "open metarecord-list"
}

scenario_open_detail() {
    # Detail loads `selected_metarecord`: pick a row in a list first, then
    # measure only the detail panel's first load of that metarecord.
    local ws; ws=$(new_ws)
    mfg layout left "$ws" >/dev/null
    mfg view left metarecord-list >/dev/null
    settle
    mfg command metarecord-list:first >/dev/null || true # sets selected_metarecord
    settle
    clear_bench
    mfg layout right "$ws" >/dev/null
    mfg view right metarecord-detail >/dev/null
    settle
    report "open metarecord-detail (selected row)"
}

scenario_open_fm() {
    local ws; ws=$(new_ws)
    clear_bench
    mfg layout left "$ws" >/dev/null
    mfg view left file-manager >/dev/null
    settle
    report "open file-manager"
}

scenario_list_detail_nav() {
    local ws; ws=$(new_ws)
    mfg layout left "$ws" >/dev/null
    mfg layout right "$ws" >/dev/null
    mfg view left metarecord-list >/dev/null
    mfg view right metarecord-detail >/dev/null
    settle
    mfg command metarecord-list:first >/dev/null || true
    settle
    clear_bench
    for ((i = 0; i < STEPS; i++)); do
        mfg command metarecord-list:next >/dev/null || true
        sleep "$STEP" # let the detail panel's async load settle before the next move
    done
    settle
    report "list+detail: ${STEPS}× selection down"
}

scenario_fm_nav() {
    # The file-manager drives the `file` viewer (not metarecord-detail);
    # this measures the file-manager's own re-render and directory loads.
    local ws; ws=$(new_ws)
    mfg layout left "$ws" >/dev/null
    mfg layout right "$ws" >/dev/null
    mfg view left file-manager >/dev/null
    mfg view right file >/dev/null
    settle
    clear_bench
    for ((i = 0; i < STEPS; i++)); do
        mfg command file-manager:next >/dev/null || true
        sleep "$STEP"
    done
    settle
    report "file-manager: ${STEPS}× selection down"
}

scenario_paging() {
    local ws; ws=$(new_ws)
    mfg layout left "$ws" >/dev/null
    mfg view left metarecord-list >/dev/null
    settle
    clear_bench
    for ((i = 0; i < STEPS; i++)); do
        mfg command metarecord-list:page-next >/dev/null || true
        sleep "$STEP"
    done
    settle
    report "paging: ${STEPS}× load next page"
}

ALL=(open-list open-detail open-fm list-detail-nav fm-nav paging)
declare -A FN=(
    [open-list]=scenario_open_list
    [open-detail]=scenario_open_detail
    [open-fm]=scenario_open_fm
    [list-detail-nav]=scenario_list_detail_nav
    [fm-nav]=scenario_fm_nav
    [paging]=scenario_paging
)

selected=("$@")
[ ${#selected[@]} -eq 0 ] && selected=("${ALL[@]}")
for name in "${selected[@]}"; do
    fn=${FN[$name]:-} || true
    [ -n "$fn" ] || die "unknown scenario '$name' (one of: ${ALL[*]})"
    "$fn"
done
