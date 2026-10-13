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
# METAFOLDER_BIN_DIR (default target/debug) names the directory of the three
# binaries; scripts/coverage.sh points it at instrumented ones, which is why
# the GUI is quit through its own `quit` command at the end and never killed:
# a killed process writes no coverage profile.
#
# Usage:  scripts/gui-e2e.sh [--unsandboxed] [--keep-logs] [scenario...]
#   with no scenario, runs them all: undo-redo duplicates orphans trash
#   reconcile ignore sync repos list layout script errors.
#   --unsandboxed  pass --allow-unsandboxed-webview to the GUI (a container
#                  that forbids WebKit's sandbox; never on real data)
#   --keep-logs    keep the work directory (daemon.log, gui.log) on exit
#
# Exit status: 0 iff every assertion passed.

set -uo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)" || exit 2

GUI_BIN="${METAFOLDER_BIN_DIR:-target/debug}/metafolder-gui"
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
    || SCENARIOS=(undo-redo duplicates orphans trash reconcile ignore sync repos list layout script errors)

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

# The GUI reads a private copy of the installed configuration, so a scenario can
# install a script of its own and the user's edits to theirs (keybindings, a
# user command) cannot change an outcome. The rendered docs are only linked.
CONFIG_SRC="${XDG_CONFIG_HOME:-$HOME/.config}/metafolder"
CONFIG_HOME="$WORK/config"
mkdir -p "$CONFIG_HOME/metafolder"
for part in cli core daemon gui scripts; do
    [ -d "$CONFIG_SRC/$part" ] && cp -r "$CONFIG_SRC/$part" "$CONFIG_HOME/metafolder/"
done
[ -d "$CONFIG_SRC/docs" ] && ln -s "$CONFIG_SRC/docs" "$CONFIG_HOME/metafolder/docs"
[ -d "$CONFIG_HOME/metafolder/gui" ] || die "no installed configuration in $CONFIG_SRC (metafolder-sync-config)"
mkdir -p "$CONFIG_HOME/metafolder/scripts"
# A script for the `script` scenario: one question, and what it got is said.
cat >"$CONFIG_HOME/metafolder/scripts/e2e-ask.sh" <<'SCRIPT'
#!/usr/bin/env bash
# Summary: e2e: asks for a key and says which one it got
if key=$(mf gui input y n); then
    mf gui message "e2e-ask: got $key"
else
    mf gui message "e2e-ask: no answer"
fi
SCRIPT

gui_args=(--gui-port "$GUI_PORT" --daemon-port "$DF_PORT")
$UNSANDBOXED && gui_args+=(--allow-unsandboxed-webview)
# What the GUI runs (scripts) finds this mf first, and this GUI with it.
XDG_CONFIG_HOME="$CONFIG_HOME" PATH="$(cd "$(dirname "$_DF_MF")" && pwd):$PATH" \
    METAFOLDER_GUI_URL="http://127.0.0.1:$GUI_PORT" \
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

# record <abs path> — create the metarecord of a path (its parent recorded), print it.
record() { df_mf metarecord add --path "$1"; }

# ignore_count <uuid> — how many mf_ignore rows the metarecord has.
ignore_count() { df_mf metarecord -i "$1" field get mf_ignore 2>/dev/null | grep -c .; }

# gui_status <python expression> — evaluated over `mf gui status` as `s`.
gui_status() {
    mfg status | python3 -c 'import json, sys
s = json.load(sys.stdin)
print(eval(sys.argv[1]))' "$1"
}

# ws_count — how many workspaces the GUI holds.
ws_count() { gui_status 'len(s["workspaces"])'; }

# slot_ws <left|right> — the workspace a slot shows, None when hidden.
slot_ws() { gui_status "(s['layout']['$1'] or {}).get('workspace_id')"; }

# repo_named <name> — the uuid of the loaded repository so named, or nothing.
repo_named() {
    df_mf repo list | python3 -c 'import json, sys
print(next((r["repo_uuid"] for r in json.load(sys.stdin)
            if r["name"] == sys.argv[1] and not r.get("lost")), ""))' "$1"
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
    status_is "undo: takes back the label of the last change" info "Undid label * (1 operations)*"

    run log:redo
    status_is "redo: takes the undo back" info "Redid label * (1 operations)*"

    run log:redo
    status_is "redo at the tip: nothing to redo" info "Nothing to redo."
}

scenario_duplicates() {
    new_repo dup
    printf 'same bytes\n' >"$REPO_DIR/a.txt"
    printf 'same bytes\n' >"$REPO_DIR/b.txt"
    printf 'other\n' >"$REPO_DIR/c.txt"
    for f in a b c; do record "$REPO_DIR/$f.txt" >/dev/null; done

    run mf:duplicate scan
    command_ok "duplicate scan: the command resolves"
    status_is "duplicate scan: one group of two" info "Duplicates: 1 groups, 2 files, 11B reclaimable."
    assert_contains "duplicate scan: the detail is in the log" \
        "$(mfg messages --workspace "$WS" --last 1)" '"groups": 1'
}

scenario_orphans() {
    new_repo orphan
    printf 'x\n' >"$REPO_DIR/gone.txt"
    record "$REPO_DIR/gone.txt" >/dev/null
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
    record "$REPO_DIR/f.txt" >/dev/null
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
    kept=$(record "$REPO_DIR/kept")
    fresh=$(record "$REPO_DIR/fresh")

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
    record "$REPO_DIR/hello.txt" >/dev/null
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

scenario_repos() {
    new_repo repos-host
    local dir="$WORK/repos/created" uuid root
    mkdir -p "$dir"
    show_panel left repos repos:refresh

    # The workspace has its repository: the new one is announced, not adopted.
    run repos:init "$dir" made
    command_ok "repos init: the command resolves"
    status_is "repos init: announces the repository" info "Repository ready: *"
    uuid=$(repo_named made)
    assert "repos init: the daemon holds it, under its name" test -n "$uuid"
    root=$(df_mf -u "$uuid" metarecord get | head -1)
    assert "repos init: the default ignore preset is on its root" \
        test "$(df_mf -u "$uuid" metarecord -i "$root" field get mf_ignore | grep -c .)" -gt 0

    run repos:init "$dir" again
    assert_eq "repos init on a repository: fails" "1" "$LAST_RC"
    assert_contains "repos init on a repository: says why" "$LAST_ERR" "already"

    run repos:unload made
    status_is "repos unload: says so" info "repository unloaded"
    assert_eq "repos unload: the daemon let it go" "" "$(repo_named made)"
}

scenario_list() {
    new_repo list
    df_mf metarecord add note:string=hello >/dev/null
    df_mf metarecord add note:string=bye >/dev/null

    # The list reads its query from the workspace when it mounts: a simplified
    # text, expanded then parsed by the GUI backend.
    run workspace:send "$WS" metarecord-list:query note=hello
    command_ok "list: the query is sent to the workspace"
    show_panel left metarecord-list metarecord-list:refresh
    assert_eq "list: the simplified query is expanded" 'note = "hello"' "$(mfg query)"

    run workspace:send "$WS" metarecord-list:query 'note = = ='
    run metarecord-list:refresh
    assert_eq "list: an invalid query is not run" 'note = "hello"' "$(mfg query)"
}

scenario_layout() {
    new_repo layout
    local n0 forked
    n0=$(ws_count)

    run workspace:rename Renamed tab
    assert_eq "workspace rename" "Renamed tab" \
        "$(gui_status "next(w['name'] for w in s['workspaces'] if w['id'] == '$WS')")"

    run workspace:fork
    assert_eq "workspace fork: a new tab" $((n0 + 1)) "$(ws_count)"
    forked=$(gui_status "s['workspaces'][-1]['id']")
    assert_eq "workspace fork: on the same repository" "$DF_REPO" \
        "$(gui_status "s['workspaces'][-1]['active_repo']")"

    run workspace:new
    assert_eq "workspace new: a new tab" $((n0 + 2)) "$(ws_count)"

    run workspace:goto 1
    assert_eq "workspace goto 1" "$(gui_status "s['workspaces'][0]['id']")" "$(slot_ws left)"
    run workspace:next
    assert_eq "workspace next" "$(gui_status "s['workspaces'][1]['id']")" "$(slot_ws left)"
    run workspace:prev
    assert_eq "workspace prev" "$(gui_status "s['workspaces'][0]['id']")" "$(slot_ws left)"

    mfg layout left "$WS" >/dev/null
    run workspace:goto nope
    status_is "workspace goto: a non-number is refused" error 'not a workspace number: "nope"'
    run panel:split
    assert_not "panel split: the right slot shows" test "$(slot_ws right)" = None
    run panel:focus right
    assert_eq "panel focus right" True "$(gui_status "s['layout']['right']['focused']")"
    run panel:focus next
    assert_eq "panel focus next: back left" True "$(gui_status "s['layout']['left']['focused']")"
    run panel:focus up
    status_is "panel focus: an unknown slot is refused" error 'unknown slot: "up"*'

    local right_before
    right_before=$(slot_ws right)
    run workspace:next slot
    assert_eq "workspace next slot: the other slot stays" "$right_before" "$(slot_ws right)"
    run workspace:prev slot
    assert_eq "workspace prev slot: back" "$WS" "$(slot_ws left)"

    mfg view right help >/dev/null
    run panel:swap
    assert_eq "panel swap: the types change sides" "help message" \
        "$(gui_status "s['layout']['left']['panel_type'] + ' ' + s['layout']['right']['panel_type']")"
    run panel:swap

    run panel:toggle split
    assert_eq "panel toggle split: hides the right slot" None "$(slot_ws right)"
    run panel:toggle split
    assert_not "panel toggle split: shows it again" test "$(slot_ws right)" = None
    run panel:toggle sideways
    status_is "panel toggle: an unknown flag is refused" error 'unknown flag: "sideways"*'
    run panel:unsplit
    assert_eq "panel unsplit" None "$(slot_ws right)"
    run panel:toggle fullscreen
    command_ok "panel toggle fullscreen: on"
    run panel:toggle fullscreen
    command_ok "panel toggle fullscreen: off"

    mfg message --workspace "$WS" "e2e: to be cleared" >/dev/null
    run message:clear
    assert_not "message clear: the log is emptied" \
        grep -q "to be cleared" <<<"$(mfg messages --workspace "$WS")"
    run shell:clear
    command_ok "shell clear"

    run config:reload keybindings
    status_is "config reload keybindings: reports" info "reloaded keybindings"
    run config:reload all
    status_is "config reload all: reports" info \
        "reloaded keybindings, style, grammar, commands"
    run config:reload nothing
    status_is "config reload: an unknown target is refused" error 'unknown reload target: "nothing"*'
    run daemon:set url "http://127.0.0.1:$DF_PORT"
    command_ok "daemon set url: the same daemon"
    assert_eq "daemon set url: still connected" True "$(gui_status "s['daemon_connected']")"

    mfg layout left "$forked" >/dev/null
    run workspace:close
    assert_eq "workspace close: the tab goes" $((n0 + 1)) "$(ws_count)"
    mfg layout left "$WS" >/dev/null
}

# until <seconds> <command…> — polls until the command succeeds.
until_ok() {
    local tries=$(($1 * 4))
    shift
    for _ in $(seq 1 "$tries"); do
        "$@" && return 0
        sleep 0.25
    done
    return 1
}

# says <text> — the last status message of the scenario's workspace is <text>.
says() { [ "$(mfg status-bar --workspace "$WS")" = "info"$'\t'"$1" ]; }

asking() { [ "$(gui_status "s['input_wait_active']")" = True ]; }
not_asking() { ! asking; }

scenario_script() {
    new_repo script
    local runner

    # script:run resolves when the script ends: it runs beside the scenario,
    # which answers its question meanwhile.
    mfg command --timeout-ms 60000 script:run e2e-ask >/dev/null 2>&1 &
    runner=$!
    assert "script run: the script asks" until_ok 20 asking

    run script-keys:toggle
    status_is "script keys toggle: off" info "script keys disabled"
    run script-keys:toggle
    status_is "script keys toggle: on again" info "script keys enabled"

    run answer:send y
    wait "$runner"
    assert_eq "script run: resolves once the script ends" 0 "$?"
    # Its message may land just after the run resolves: waited for, not assumed.
    until_ok 10 says "e2e-ask: got y"
    status_is "script run: the script got the answer" info "e2e-ask: got y"

    mfg command --timeout-ms 60000 script:run e2e-ask >/dev/null 2>&1 &
    runner=$!
    assert "script run again: the script asks" until_ok 20 asking
    run script:stop
    wait "$runner"
    assert "script stop: the question is withdrawn" until_ok 10 not_asking
    assert_not "script stop: no answer was given" \
        grep -q "e2e-ask: got" <<<"$(mfg status-bar --workspace "$WS")"

    run script:run no-such-script
    status_is "script run: an unknown script is refused" error \
        'no installed script matches "no-such-script"'
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

# Last, and not a scenario: the GUI quits on its own command. A clean exit is
# what writes an instrumented binary's coverage profile.
echo "── quit"
mfg command --timeout-ms 2000 quit >/dev/null 2>&1
exited=false
for _ in $(seq 1 40); do
    kill -0 "$GUI_PID" 2>/dev/null || { exited=true; break; }
    sleep 0.25
done
if $exited; then
    wait "$GUI_PID" 2>/dev/null
    GUI_PID=""
fi
assert_eq "quit: the GUI exits" true "$exited"

assert_summary
