#!/usr/bin/env bash
# shellcheck source-path=SCRIPTDIR
# End-to-end tests of the GUI's commands (doc "Testing the GUI end to end").
#
# Boots an isolated daemon (scripts/lib/daemon-fixture.sh: private auth-token
# dir, empty config, ephemeral port) and a real `metafolder-gui` on top of it,
# then runs each scenario through the scripting API: `mf gui command` types a
# command as the command input would, and `mf gui status-bar` / `mf gui
# messages` read back what it *said*. Nothing here looks at pixels, so a change
# of style never breaks a scenario; a change of wording or behaviour does.
#
# What it covers that `cargo test` cannot: the whole chain shell dispatcher →
# Tauri `invoke` → `#[tauri::command]` → daemon → state.
#
# Not part of check.sh: it needs a display, the WebView and a built frontend.
#
# Prerequisites:
#   - built binaries: cargo build -p metafolder-daemon -p metafolder-cli
#     -p metafolder-gui, and the frontend (npm --prefix crates/gui/frontend run
#     build) — scripts/complete-build.sh does all of it;
#   - the user configuration installed (metafolder-sync-config): the GUI reads
#     its panels and keybindings from ~/.config/metafolder/gui/;
#   - a display ($DISPLAY or $WAYLAND_DISPLAY).
#
# Usage:  scripts/gui-e2e.sh [--unsandboxed] [--keep-logs] [scenario...]
#   with no scenario, runs them all: undo-redo duplicates orphans errors.
#   --unsandboxed  pass --allow-unsandboxed-webview to the GUI (a container
#                  that forbids WebKit's sandbox; never on real data)
#   --keep-logs    keep the work directory (daemon.log, gui.log) on exit
#
# Exit status: 0 iff every assertion passed.

set -uo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)" || exit 2

GUI_BIN="target/debug/metafolder-gui"
UNSANDBOXED=false
KEEP_LOGS=false
SCENARIOS=()
for arg in "$@"; do
    case "$arg" in
        --unsandboxed) UNSANDBOXED=true ;;
        --keep-logs) KEEP_LOGS=true ;;
        -h | --help) sed -n '3,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
        -*) echo "error: unknown option $arg" >&2; exit 2 ;;
        *) SCENARIOS+=("$arg") ;;
    esac
done
[ ${#SCENARIOS[@]} -gt 0 ] || SCENARIOS=(undo-redo duplicates orphans errors)

# shellcheck source=lib/daemon-fixture.sh
. scripts/lib/daemon-fixture.sh
# shellcheck source=lib/assert.sh
. scripts/lib/assert.sh

die() { echo "error: $*" >&2; exit 2; }

df_available && [ -x "$GUI_BIN" ] \
    || die "build first: cargo build -p metafolder-daemon -p metafolder-cli -p metafolder-gui"
[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || die "no display (\$DISPLAY / \$WAYLAND_DISPLAY)"

# The fixture moves XDG_RUNTIME_DIR to a private directory (its own auth
# tokens); a relative Wayland socket name would then point into it.
if [ -n "${WAYLAND_DISPLAY:-}" ] && [ "${WAYLAND_DISPLAY#/}" = "$WAYLAND_DISPLAY" ] \
    && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    export WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
fi

df_start || die "the daemon did not start"
WORK=$_DF_TMP
GUI_PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
GUI_PID=""

cleanup() {
    [ -n "$GUI_PID" ] && kill "$GUI_PID" 2>/dev/null && wait "$GUI_PID" 2>/dev/null
    if $KEEP_LOGS; then
        [ -n "$_DF_DPID" ] && kill "$_DF_DPID" 2>/dev/null
        echo "logs kept in $WORK"
    else
        _df_stop
    fi
    return 0
}
trap cleanup EXIT

mfg() { "$_DF_MF" gui --gui-url "http://127.0.0.1:$GUI_PORT" "$@"; }

gui_args=(--gui-port "$GUI_PORT" --daemon-port "$DF_PORT")
$UNSANDBOXED && gui_args+=(--allow-unsandboxed-webview)
"$GUI_BIN" "${gui_args[@]}" >"$WORK/gui.log" 2>&1 &
GUI_PID=$!

# Ready = the shell dispatches commands (its listener is the last thing it
# sets up), not merely the HTTP server answering.
ready=false
for _ in $(seq 1 120); do
    kill -0 "$GUI_PID" 2>/dev/null || break
    if mfg command --timeout-ms 1000 status:clear >/dev/null 2>&1; then
        ready=true
        break
    fi
    sleep 0.5
done
if ! $ready; then
    echo "error: the GUI did not become ready; its log:" >&2
    tail -n 20 "$WORK/gui.log" >&2
    exit 2
fi

# ── helpers ──────────────────────────────────────────────────────────────────

WS=""

# new_repo <name> — a repository of its own under the work directory, empty of
# files until the scenario writes some, shown in a fresh focused workspace.
new_repo() {
    local dir="$WORK/repos/$1"
    mkdir -p "$dir"
    df_init_repo "$dir"
    REPO_DIR=$dir
    WS=$(mfg workspace new --repo "$DF_REPO" --left metarecord-list) || die "workspace new"
    mfg layout left "$WS" >/dev/null || die "layout left $WS"
}

# run <invocation…> — a command typed into the focused workspace. A marker is
# posted first, so a command that reports nothing leaves the marker on the bar
# instead of passing on a stale message.
run() {
    mfg message --workspace "$WS" "e2e: before $*" >/dev/null
    LAST_OUT=$(mfg command --timeout-ms 60000 "$@" 2>&1)
    LAST_RC=$?
}

# status_is <name> <kind> <glob> — the last status message, kind and text.
status_is() {
    local name=$1 kind=$2 glob=$3 got
    got=$(mfg status-bar --workspace "$WS")
    # shellcheck disable=SC2053 # the right-hand side is a glob on purpose
    if [[ $got == "$kind"$'\t'$glob ]]; then
        pass=$((pass + 1)); printf 'ok   %s\n' "$name"
    else
        fail=$((fail + 1)); printf 'FAIL %s\n       want: %s\t%s\n       got:  %s\n' \
            "$name" "$kind" "$glob" "$got"
    fi
}

# command_ok <name> — the last `run` resolved without an error.
command_ok() { assert_eq "$1" "0" "$LAST_RC"; }

# ── scenarios ────────────────────────────────────────────────────────────────

scenario_undo_redo() {
    new_repo undo
    df_mf metarecord add note:string=hello >/dev/null

    run log:undo
    command_ok "undo: the command resolves"
    status_is "undo: rolls the last revision back" info "Undo: revision * rolled back (1 operations)."

    run log:redo
    status_is "redo: re-applies it" info "Redo: revision * re-applied (1 operations)."

    run log:redo
    status_is "redo at the tip: nothing to redo" info "Nothing to redo."
}

scenario_duplicates() {
    new_repo dup
    printf 'same bytes\n' >"$REPO_DIR/a.txt"
    printf 'same bytes\n' >"$REPO_DIR/b.txt"
    printf 'other\n' >"$REPO_DIR/c.txt"
    for f in a b c; do df_mf track "$REPO_DIR/$f.txt" >/dev/null; done

    run mf:duplicate scan
    command_ok "duplicate scan: the command resolves"
    status_is "duplicate scan: one group of two" info "Duplicates: 1 groups, 2 files, 11B reclaimable."
    assert_contains "duplicate scan: the detail is in the log" \
        "$(mfg messages --workspace "$WS" --last 1)" '"groups": 1'
}

scenario_orphans() {
    new_repo orphan
    printf 'x\n' >"$REPO_DIR/gone.txt"
    df_mf track "$REPO_DIR/gone.txt" >/dev/null
    # Nothing watches this repository (the fixture turns mf_watch off), so the
    # deletion goes unrecorded: exactly an orphan.
    rm "$REPO_DIR/gone.txt"

    run orphan:detect
    status_is "orphan detect: marks the orphan" info \
        "1 orphaned metarecord marked orphan = true (1 new, 0 no longer orphaned)."

    printf 'x\n' >"$REPO_DIR/gone.txt"
    run orphan:detect
    status_is "orphan detect: the file is back" info \
        "No orphans — every tracked metarecord has its file."
}

scenario_errors() {
    new_repo errors

    run nope:nope
    assert_eq "unknown command: fails" "1" "$LAST_RC"
    assert_contains "unknown command: says so" "$LAST_OUT" "unknown command"

    run metarecord:trash
    status_is "trash with nothing selected: refused" error "no metarecord is selected"
}

for scenario in "${SCENARIOS[@]}"; do
    fn="scenario_${scenario//-/_}"
    if ! declare -F "$fn" >/dev/null; then
        fail=$((fail + 1)); printf 'FAIL unknown scenario %s\n' "$scenario"
        continue
    fi
    echo "── $scenario"
    "$fn"
done

assert_summary
