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
#   with no scenario, runs them all: undo-redo duplicates orphans trash
#   reconcile ignore sync errors.
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
[ ${#SCENARIOS[@]} -gt 0 ] \
    || SCENARIOS=(undo-redo duplicates orphans trash reconcile ignore sync errors)

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
# files until the scenario writes some, shown in a fresh focused workspace. The
# workspace shows the message panel: the metarecord list would select a record
# of its own once loaded, and the selection is what several commands act on.
new_repo() {
    local dir="$WORK/repos/$1"
    mkdir -p "$dir"
    df_init_repo "$dir"
    REPO_DIR=$dir
    WS=$(mfg workspace new --repo "$DF_REPO" --left message) || die "workspace new"
    mfg layout left "$WS" >/dev/null || die "layout left $WS"
}

# answering <yes|no> <invocation…> — a command typed into the focused
# workspace, every confirmation it asks answered so. LAST_QUESTIONS holds the
# questions (one per line), LAST_ERR what `mf gui command` reported, LAST_RC
# its exit status. A marker is posted first, so a command that reports nothing
# leaves the marker on the bar instead of passing on a stale message.
answering() {
    local answer=$1
    shift
    mfg message --workspace "$WS" "e2e: before $*" >/dev/null
    LAST_QUESTIONS=$(mfg command --timeout-ms 60000 --confirm "$answer" "$@" 2>"$WORK/err")
    LAST_RC=$?
    LAST_ERR=$(cat "$WORK/err")
}

# run <invocation…> — a command expected to ask nothing. It is still answered
# (no): a dialog left open would block every command after it, so an
# unexpected question fails here, naming itself, instead.
run() {
    answering no "$@"
    if [ -n "$LAST_QUESTIONS" ]; then
        fail=$((fail + 1))
        printf 'FAIL %s asked an unexpected confirmation (refused):\n%s\n' "$*" "$LAST_QUESTIONS"
    fi
}

# select_path <abs path> — show it in the left slot's file panel, which makes
# its metarecord the workspace's selection.
select_path() { mfg view left file --path "$1" >/dev/null || die "view file $1"; }

# show_panel <slot> <type> <command> — shows a panel type and waits until it
# has mounted: until its own `command` (one that loads, so the panel's data is
# there too) is known and resolves.
show_panel() {
    local slot=$1 type=$2 command=$3
    mfg view "$slot" "$type" >/dev/null || die "view $slot $type"
    for _ in $(seq 1 40); do
        mfg command --timeout-ms 10000 "$command" >/dev/null 2>&1 && return 0
        sleep 0.25
    done
    die "the $type panel never answered $command"
}

# uuid_of <abs path> — the metarecord tracking a path.
uuid_of() { df_mf track "$1"; }

# ignore_count <uuid> — how many mf_ignore rows the metarecord has.
ignore_count() { df_mf metarecord -i "$1" field get mf_ignore 2>/dev/null | grep -c .; }

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

    answering no orphan:delete
    assert_contains "orphan delete: asks first" "$LAST_QUESTIONS" \
        "Delete 1 metarecord marked orphan = true?"
    status_is "orphan delete refused: nothing deleted" info "e2e: before *"

    answering yes orphan:delete
    status_is "orphan delete: deletes the marked set" info \
        "Deleted 1 orphaned metarecord — undo takes them back."

    printf 'x\n' >"$REPO_DIR/gone.txt"
    run orphan:detect
    status_is "orphan detect: the file is back" info \
        "No orphans — every tracked metarecord has its file."
}

scenario_trash() {
    new_repo trash
    printf 'x\n' >"$REPO_DIR/f.txt"
    df_mf track "$REPO_DIR/f.txt" >/dev/null
    select_path "$REPO_DIR/f.txt"

    answering no metarecord:trash
    assert_eq "trash: asks first" "Send the selected metarecord's file to the trash?" \
        "$LAST_QUESTIONS"
    status_is "trash refused: nothing said" info "e2e: before *"
    assert "trash refused: the file stays" test -e "$REPO_DIR/f.txt"

    answering yes metarecord:trash
    status_is "trash: trashes the file" info "Trashed f.txt — restore it from the trash panel"
    assert_not "trash: the file is gone" test -e "$REPO_DIR/f.txt"

    show_panel left trash trash:refresh
    run trash:first
    run trash:restore
    status_is "trash restore: says where" info "Restored $REPO_DIR/f.txt"
    assert "trash restore: the file is back" test -e "$REPO_DIR/f.txt"
}

scenario_reconcile() {
    new_repo reconcile
    mkdir -p "$REPO_DIR/sub"
    printf 'a\n' >"$REPO_DIR/a.txt"
    printf 'b\n' >"$REPO_DIR/sub/b.txt"
    # Eligible for the walk; the watcher, placed after the files exist, hears
    # nothing of them — finding them is the reconcile's job.
    df_mf metarecord -i "$DF_ROOT" field set mf_watch:bool=true >/dev/null

    run reconcile:run
    command_ok "reconcile: the command resolves"
    status_is "reconcile: creates the new files' records" info \
        "Reconcile: 3 created, 0 moved, 0 candidates."

    run reconcile:run
    status_is "reconcile again: nothing new" info "Reconcile: 0 created, 0 moved, 0 candidates."
    df_mf metarecord -i "$DF_ROOT" field set mf_watch:bool=false >/dev/null
}

scenario_ignore() {
    new_repo ignore
    mkdir -p "$REPO_DIR/kept" "$REPO_DIR/fresh"
    local root_count kept fresh
    root_count=$(ignore_count "$DF_ROOT")
    kept=$(uuid_of "$REPO_DIR/kept")
    fresh=$(uuid_of "$REPO_DIR/fresh")

    select_path "$REPO_DIR"
    run ignore:list
    assert_contains "ignore list: the presets" "$(mfg messages --workspace "$WS")" "Ignore presets:"
    assert_contains "ignore list: the root's own set" "$(mfg messages --workspace "$WS")" \
        "Patterns of / (its own):"

    # The target is the selected directory, else the root: name it anyway.
    select_path "$REPO_DIR"
    # Every shipped preset is part of the root's default set: take one out
    # and put it back.
    run ignore:remove git
    status_is "ignore remove on the root" info \
        "Ignore: git removed from / — $((root_count - 1)) pattern(s)"
    run ignore:add git
    status_is "ignore add on the root" info "Ignore: git applied to / — $root_count pattern(s)"

    # A directory inheriting the root's set: adding to it asks whether to copy
    # the inherited patterns first, or the new set would shadow them.
    select_path "$REPO_DIR/kept"
    answering yes ignore:add git
    assert_contains "ignore add under the root: asks about the inherited set" \
        "$LAST_QUESTIONS" "/kept has no ignore patterns of its own"
    status_is "ignore add, inherited set copied" info \
        "Ignore: git applied to /kept — $root_count pattern(s)"
    assert_eq "ignore add, inherited set copied: written" "$root_count" "$(ignore_count "$kept")"

    select_path "$REPO_DIR/fresh"
    answering no ignore:add git
    status_is "ignore add, starting empty" info "Ignore: git applied to /fresh — 1 pattern(s)"
    assert_eq "ignore add, starting empty: written" "1" "$(ignore_count "$fresh")"
}

scenario_sync() {
    new_repo sync-b
    local b=$DF_REPO b_dir=$REPO_DIR
    new_repo sync-a
    local a=$DF_REPO intents="$WORK/sync-intents.toml"
    printf 'world\n' >"$REPO_DIR/hello.txt"
    df_mf track "$REPO_DIR/hello.txt" >/dev/null
    printf "[[intents]]\nrepo = '%s'\nquery = 'mfr_type = \"file\"'\n" "$a" >"$intents"

    # The pair is named, not left to the toolbar's defaults: B would be
    # whichever other repository the daemon lists first.
    show_panel left sync "sync:pair $a $b"
    run sync:pair "$a" "$b"
    status_is "sync pair: says which" info "Sync: sync-a ⇄ sync-b"

    run sync:status
    status_is "sync status: no link yet" info "Sync status: 0 links"

    # A file B lacks: create-link, sync, copy and chmod (doc "The sync plan").
    run sync:plan "$intents"
    status_is "sync plan: one file to create in B" info "Plan: 4 operation(s)"

    answering no sync:run
    assert_eq "sync run: asks first" "Run the sync plan for sync-a ⇄ sync-b?" "$LAST_QUESTIONS"
    assert_not "sync run refused: nothing copied" test -e "$b_dir/hello.txt"

    answering yes sync:run
    status_is "sync run: carries the plan out" info "Ran: 4 done, 0 skipped"
    assert_eq "sync run: the file is in B" "world" "$(cat "$b_dir/hello.txt" 2>/dev/null)"

    run sync:status
    status_is "sync status: the link is in sync" info "Sync status: 1 link (1 in_sync)"
}

scenario_errors() {
    new_repo errors

    run nope:nope
    assert_eq "unknown command: fails" "1" "$LAST_RC"
    assert_contains "unknown command: says so" "$LAST_ERR" "unknown command"

    run metarecord:trash
    status_is "trash with nothing selected: refused" error "no metarecord is selected"

    show_panel left sync sync:status
    run sync:pair errors nowhere
    assert_contains "sync pair: an unknown repository is refused" "$LAST_ERR" \
        'no loaded repository named "nowhere"'
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
