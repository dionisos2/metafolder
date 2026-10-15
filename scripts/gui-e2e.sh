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
# The GUI runs on a private X server (Xvfb) when there is one, so its window
# never shows on the user's desktop. The keyboard scenarios (prompt history
# keybindings open-with) need it and xdotool: they press keys where no command
# reaches — the settings view, the command input — and on a real display they
# would type into whatever the user is doing, or into the desktop's own
# shortcuts. Without both they are a loud SKIP.
#
# What only a mouse reaches (a tab's close button, the field editor's picker
# button, the settings view's Remove) is clicked through WebKit's remote
# inspector (scripts/lib/webview.mjs, node): by selector and text, never by
# coordinates. The inspector listens on loopback for the run's duration, open
# to any local process — a throwaway daemon, but the GUI runs shell commands.
# Without node, or an inspector that does not answer, those scenarios are a
# loud SKIP.
#
# Not part of check.sh: it needs a display, the WebView and a built frontend.
#
# Prerequisites:
#   - built binaries: cargo build -p metafolder-daemon -p metafolder-cli
#     -p metafolder-gui, and the frontend (npm --prefix crates/gui/frontend run
#     build) — scripts/complete-build.sh does all of it;
#   - the user configuration installed (metafolder-sync-config): the GUI reads
#     its panels and keybindings from ~/.config/metafolder/gui/;
#   - Xvfb (Arch: xorg-server-xvfb), else a display ($DISPLAY or
#     $WAYLAND_DISPLAY) and no keyboard scenario;
#   - xdotool, for the keyboard scenarios;
#   - node, for the clicking ones.
#
# METAFOLDER_BIN_DIR (default target/debug) names the directory of the three
# binaries; scripts/coverage.sh points it at instrumented ones, which is why
# the GUI is quit through its own `quit` command at the end and never killed:
# a killed process writes no coverage profile.
#
# Usage:  scripts/gui-e2e.sh [--unsandboxed] [--keep-logs] [scenario...]
#   with no scenario, runs them all: undo-redo duplicates orphans trash
#   reconcile ignore sync repos list layout script errors picker refs hierarchy
#   hierarchy-form prompt history keybindings open-with tab-close pick-button
#   suggested-key walk slideshow devtools.
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
    || SCENARIOS=(undo-redo duplicates orphans trash remove field-keys reconcile ignore sync repos list layout script script-workspaces
        errors picker refs hierarchy hierarchy-form prompt history keybindings open-with tab-close
        pick-button suggested-key walk slideshow devtools shell-line)

# shellcheck source=lib/daemon-fixture.sh
. scripts/lib/daemon-fixture.sh
# shellcheck source=lib/assert.sh
. scripts/lib/assert.sh

die() { echo "error: $*" >&2; exit 2; }

df_available && [ -x "$GUI_BIN" ] \
    || die "build first: cargo build -p metafolder-daemon -p metafolder-cli -p metafolder-gui"
command -v Xvfb >/dev/null || [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] \
    || die "no Xvfb and no display (\$DISPLAY / \$WAYLAND_DISPLAY)"

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
XVFB_PID=""
INSPECTOR_PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')

cleanup() {
    [ -n "$GUI_PID" ] && kill "$GUI_PID" 2>/dev/null && wait "$GUI_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null && wait "$XVFB_PID" 2>/dev/null
    if $KEEP_LOGS; then
        [ -n "$_DF_DPID" ] && kill "$_DF_DPID" 2>/dev/null
        echo "logs kept in $WORK"
    else
        _df_stop
    fi
    return 0
}
trap cleanup EXIT

# The private X server: a free display number of its own choosing (-displayfd),
# a US keymap, nothing else on it. The GUI is told to use X even under Wayland.
KEYBOARD=false
if command -v Xvfb >/dev/null; then
    Xvfb -displayfd 1 -screen 0 1280x800x24 -nolisten tcp \
        >"$WORK/xvfb.display" 2>"$WORK/xvfb.log" &
    XVFB_PID=$!
    for _ in $(seq 1 40); do
        [ -s "$WORK/xvfb.display" ] && break
        sleep 0.25
    done
    [ -s "$WORK/xvfb.display" ] || die "Xvfb did not start: $(tail -n 3 "$WORK/xvfb.log")"
    DISPLAY=":$(head -n 1 "$WORK/xvfb.display")"
    export DISPLAY GDK_BACKEND=x11
    unset WAYLAND_DISPLAY
    command -v xdotool >/dev/null && KEYBOARD=true
fi

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
# A panel for the `keybindings` scenario: it suggests a key for a command of
# its own, the way a panel's `addKeybinding` does.
mkdir -p "$CONFIG_HOME/metafolder/gui/panel-types/e2e-keys"
cat >"$CONFIG_HOME/metafolder/gui/panel-types/e2e-keys/index.html" <<'PANEL'
<p>e2e: a panel that suggests a key</p>
PANEL
cat >"$CONFIG_HOME/metafolder/gui/panel-types/e2e-keys/main.js" <<'PANEL'
export function mount(_root, metafolder) {
  void metafolder.commands.register('e2e-keys:ping', {
    label: 'e2e: say pinged',
    handler: () => metafolder.statusBar.message('e2e: pinged', 60000),
  });
  void metafolder.addKeybinding('e2e-keys:ping', 'ctrl+alt+y');
}
PANEL

gui_args=(--gui-port "$GUI_PORT" --daemon-port "$DF_PORT")
$UNSANDBOXED && gui_args+=(--allow-unsandboxed-webview)
# What the GUI runs (scripts) finds this mf first, and this GUI with it.
WEBKIT_INSPECTOR_HTTP_SERVER="127.0.0.1:$INSPECTOR_PORT" \
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
# The window the keyboard scenarios type into: the GUI's one visible window.
GUI_WIN=""
$KEYBOARD && GUI_WIN=$(xdotool search --onlyvisible --pid "$GUI_PID" 2>/dev/null | head -n 1)
[ -n "$GUI_WIN" ] || KEYBOARD=false

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

# show_panel <slot> <type> <invocation…> — shows a panel type and waits until it
# has mounted: until the `invocation` (one that loads, so the panel's data is
# there too) is known and resolves.
show_panel() {
    local slot=$1 type=$2
    shift 2
    # The rest is the invocation, whatever its number of words
    # (`view:refresh repos`).
    local command="$*"
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

# needs_keyboard <scenario> — false, and a loud SKIP, when keys cannot be
# pressed: no private X server or no xdotool (see the header).
needs_keyboard() {
    $KEYBOARD && return 0
    printf 'SKIP %s: needs Xvfb and xdotool (keys are only pressed on a private display)\n' "$1"
    return 1
}

# keys <key…> — presses keys (xdotool names: colon, Return, ctrl+p) in the
# GUI's window. typed <text> — types text there.
keys() { xdotool windowfocus --sync "$GUI_WIN" key --delay 30 "$@"; }
typed() { xdotool windowfocus --sync "$GUI_WIN" type --delay 30 -- "$1"; }

# needs_clicks <scenario> — false, and a loud SKIP, when nothing can click.
CLICKS=""
needs_clicks() {
    # Asked once: the inspector answers for the shell's page.
    if [ -z "$CLICKS" ]; then
        CLICKS=false
        command -v node >/dev/null && webview ready >/dev/null 2>&1 && CLICKS=true
    fi
    $CLICKS && return 0
    printf 'SKIP %s: needs node and the WebKit inspector (clicks go through it)\n' "$1"
    return 1
}

# webview <action> <args…> — scripts/lib/webview.mjs against this GUI.
# click <selector> [<text> [<inner selector> [<inner text>]]] clicks the one
# visible element so named, and fails, clicking nothing, unless exactly one
# matches: wrapped in until_ok, it waits for the element to appear.
webview() { node scripts/lib/webview.mjs "127.0.0.1:$INSPECTOR_PORT" "$@"; }
click() { webview click "$@" >/dev/null 2>&1; }
# shows <selector> [<text>] — exactly one such element is visible.
shows() { webview text "$@" >/dev/null 2>&1; }
# reads <value> <selector…> — the element's text (an input's value) is <value>.
reads() {
    local want=$1
    shift
    [ "$(webview text "$@" 2>/dev/null)" = "$want" ]
}

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

    show_panel left trash view:refresh trash
    run cursor:first trash
    run trash:restore
    status_is "trash restore: says where" info "Restored $REPO_DIR/f.txt"
    assert "trash restore: the file is back" test -e "$REPO_DIR/f.txt"
}

has_record() { df_mf metarecord -i "$1" get >/dev/null 2>&1; }

# Delete on a metarecord with a file asks three ways in the command input
# (doc "metarecord:remove-file"): Escape keeps everything, `trash` trashes the
# file, `delete` keeps the file and deletes the metarecord.
scenario_remove() {
    needs_keyboard remove || return 0
    new_repo remove
    printf 'x\n' >"$REPO_DIR/f.txt"
    printf 'y\n' >"$REPO_DIR/g.txt"
    local f g runner
    f=$(record "$REPO_DIR/f.txt")
    g=$(record "$REPO_DIR/g.txt")
    select_path "$REPO_DIR/f.txt"

    mfg command --timeout-ms 60000 metarecord:remove >/dev/null 2>&1 &
    runner=$!
    sleep 1
    keys Escape
    wait "$runner"
    assert "remove, escape: the file stays" test -e "$REPO_DIR/f.txt"
    assert "remove, escape: the metarecord stays" has_record "$f"

    mfg command --timeout-ms 60000 metarecord:remove >/dev/null 2>&1 &
    runner=$!
    sleep 1
    typed "trash"
    keys Return
    wait "$runner"
    assert "remove, trash: the file is trashed" until_ok 10 test ! -e "$REPO_DIR/f.txt"

    select_path "$REPO_DIR/g.txt"
    answering yes metarecord:remove-file delete
    assert "remove, delete: the file stays" test -e "$REPO_DIR/g.txt"
    assert_not "remove, delete: the metarecord is gone" has_record "$g"
}

# The per-record field keys work wherever a metarecord is selected, the file
# panel included — not only in the list and the detail.
scenario_field_keys() {
    needs_keyboard field-keys || return 0
    new_repo field-keys
    printf 'x\n' >"$REPO_DIR/f.txt"
    local f
    f=$(record "$REPO_DIR/f.txt")
    select_path "$REPO_DIR/f.txt"
    sleep 1
    keys m a
    sleep 1
    typed "e2e_tag"
    keys Return
    sleep 0.5
    typed "string"
    keys Return
    sleep 0.5
    typed "from-the-file-panel"
    keys Return
    assert "field keys: m a adds a field from the file panel" \
        until_ok 10 field_is "$f" e2e_tag from-the-file-panel
}
field_is() { [ "$(df_mf metarecord -i "$1" field get "$2" 2>/dev/null)" = "$3" ]; }

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
    show_panel left repos view:refresh repos

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
    show_panel left metarecord-list view:refresh metarecord-list
    assert_eq "list: the simplified query is expanded" 'note = "hello"' "$(mfg query)"

    run workspace:send "$WS" metarecord-list:query 'note = = ='
    run view:refresh metarecord-list
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

# A question belongs to the workspace it is asked in, one per workspace (doc
# "Script sessions"): two scripts wait side by side, and answer:send answers
# the question of the workspace on screen.
scenario_script_workspaces() {
    new_repo questions
    local first=$WS second a b

    mfg command --timeout-ms 60000 script:run e2e-ask >/dev/null 2>&1 &
    a=$!
    assert "two questions: the first script asks" until_ok 20 asking

    second=$(mfg workspace new --repo "$DF_REPO" --left message) || die "workspace new"
    mfg layout left "$second" >/dev/null || die "layout left $second"
    mfg command --timeout-ms 60000 script:run e2e-ask >/dev/null 2>&1 &
    b=$!
    # Refused, the second script would say "no answer" at once: its answer
    # below is the proof that it waits.
    sleep 1

    WS=$second
    run answer:send n
    wait "$b"
    until_ok 10 says "e2e-ask: got n"
    status_is "two questions: the second workspace's question got the answer" info \
        "e2e-ask: got n"
    assert "two questions: the first is still waiting" asking

    mfg layout left "$first" >/dev/null || die "layout left $first"
    WS=$first
    run answer:send y
    wait "$a"
    until_ok 10 says "e2e-ask: got y"
    status_is "two questions: back in the first workspace, its own question" info \
        "e2e-ask: got y"
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

# picking [<name>] — a value picker is open (its workspace is named after the
# pick: 'Pick a repository' by default).
picking() {
    [ "$(gui_status "any(w['name'] == '${1:-Pick a repository}' for w in s['workspaces'])")" = True ]
}

# picker_slot [<name>] — the slot showing the picker.
picker_slot() {
    gui_status "next(k for k in ('left', 'right') if s['layout'][k] and next(
        w['name'] for w in s['workspaces'] if w['id'] == s['layout'][k]['workspace_id'])
        == '${1:-Pick a repository}')"
}
not_picking() { ! picking "$@"; }

scenario_picker() {
    new_repo picker
    local picked="$WORK/repos/picked" uuid runner
    mkdir -p "$picked"
    uuid=$(df_mf repo init --name picked "$picked" | head -n 1)
    df_mf -u "$uuid" repo unload >/dev/null
    show_panel left repos view:refresh repos

    # repos:load resolves once the pick does: it runs beside the scenario.
    mfg command --timeout-ms 60000 repos:load >/dev/null 2>&1 &
    runner=$!
    assert "picker: repos load opens one" until_ok 10 picking
    run pick:cancel
    wait "$runner"
    assert_not "pick cancel: the picker closes" picking
    assert_eq "pick cancel: nothing is loaded" "" "$(repo_named picked)"

    mfg command --timeout-ms 60000 repos:load >/dev/null 2>&1 &
    runner=$!
    assert "picker: opens again" until_ok 10 picking
    # The picker's selection is what its panel shows: the folder, here.
    mfg view "$(picker_slot)" file --path "$picked" >/dev/null || die "view file $picked"
    run pick:confirm
    wait "$runner"
    assert_not "pick confirm: the picker closes" picking
    assert_eq "pick confirm: the picked folder is loaded" "$uuid" "$(repo_named picked)"
    # Unloaded once its load has finished (refused while it warms up).
    until_ok 30 df_mf -u "$uuid" repo unload >/dev/null 2>&1
}

scenario_refs() {
    new_repo refs
    local file="$REPO_DIR/a.txt" a bob
    echo a >"$file"
    a=$(record "$file")
    bob=$(df_mf metarecord add name:string=bob)
    select_path "$file"
    show_panel left metarecord-detail metarecord:cancel

    run metarecord:field add friend ref "$bob"
    assert_eq "ref: a uuid is taken as it is" "$bob" "$(df_mf metarecord -i "$a" field get friend)"
    # The shipped '*' rule of [ref-seeds] names a target by its name too.
    run metarecord:field add buddy ref bob
    assert_eq "ref: a seed label is resolved to its uuid" "$bob" \
        "$(df_mf metarecord -i "$a" field get buddy)"
}

# A tag of several parents (doc "Hierarchy references"): a second parent is
# added from the detail panel by typing the parent's path — the node keeps its
# name — and the node then answers under both.
scenario_hierarchy() {
    new_repo hierarchy
    local maraton
    df_mf metarecord add mf_schema:string=tag path:hierarchy_ref=/sport >/dev/null
    df_mf metarecord add mf_schema:string=tag path:hierarchy_ref=/évènement >/dev/null
    show_panel left metarecord-detail metarecord:cancel

    # Made in the panel, which selects it; each parent typed by its path.
    run metarecord:create tag
    command_ok "hierarchy: a tag is created"
    run metarecord:field set path hierarchy_ref sport/maraton
    command_ok "hierarchy: placed under a first parent"
    # The field's type is settled now, and the node's name: the parent alone.
    run metarecord:field add path évènement
    command_ok "hierarchy: a second parent is added by its path"
    maraton=$(df_mf metarecord -q 'path = "évènement/maraton"' get)
    assert_eq "hierarchy: the node has both parents" \
        "$(printf 'sport/maraton\névènement/maraton')" \
        "$(df_mf metarecord -i "$maraton" get --resolve-tree path --tsv | cut -f2 | LC_ALL=C sort)"
    assert_eq "hierarchy: it is below either parent" "$maraton" \
        "$(df_mf metarecord -q 'path ->* "évènement"' get)"
    assert_eq "hierarchy: and below the other" "$maraton" "$(df_mf metarecord -q 'path ->* "sport"' get)"
    run metarecord:field add path nowhere
    assert "hierarchy: a parent that does not exist is refused" [ "$LAST_RC" != 0 ]
}

# The same with the mouse (doc "metarecord-detail panel"): the + of the field
# opens the add form on it, and the new parent alone is typed, by its path.
scenario_hierarchy_form() {
    needs_clicks hierarchy-form || return 0
    needs_keyboard hierarchy-form || return 0
    new_repo hierarchy-form
    local maraton
    df_mf metarecord add mf_schema:string=tag path:hierarchy_ref=/sport >/dev/null
    df_mf metarecord add mf_schema:string=tag path:hierarchy_ref=/évènement >/dev/null
    show_panel left metarecord-detail metarecord:cancel
    run metarecord:create tag
    run metarecord:field set path hierarchy_ref sport/maraton
    command_ok "hierarchy form: placed under a first parent"
    maraton=$(df_mf metarecord -q 'path = "sport/maraton"' get)

    assert "hierarchy form: the field's + is clicked" \
        until_ok 10 click 'button.add-value[title*=path]'
    assert "hierarchy form: the form is on that field" until_ok 10 reads path '#add-name'
    # The value input has the focus; the node has its name, so the parent alone.
    typed "évènement"
    assert "hierarchy form: added" click '#add-append'
    assert "hierarchy form: the node has both parents" until_ok 10 paths_are "$maraton" \
        "$(printf 'sport/maraton\névènement/maraton')"
    # Shown as one field: the name once, a line per parent.
    assert "hierarchy form: the node's name is shown once" \
        until_ok 10 shows '#field-rows td.name[rowspan="2"]' maraton
    assert "hierarchy form: a line names the new parent" \
        until_ok 10 shows '#field-rows td.value' évènement
}
# paths_are <uuid> <paths> — the node's paths, one a line, sorted.
paths_are() {
    [ "$(df_mf metarecord -i "$1" get --resolve-tree path --tsv | cut -f2 | LC_ALL=C sort)" = "$2" ]
}

scenario_prompt() {
    needs_keyboard prompt || return 0
    new_repo prompt
    local runner

    mfg prompt "e2e: name?" >"$WORK/prompt.out" 2>&1 &
    runner=$!
    assert "prompt: asks in the command input" until_ok 10 asking
    typed "typed answer"
    keys Return
    wait "$runner"
    assert_eq "prompt: enter confirms" 0 "$?"
    assert_eq "prompt: the typed text is the answer" "typed answer" "$(cat "$WORK/prompt.out")"

    mfg prompt "e2e: name?" >"$WORK/prompt.out" 2>&1 &
    runner=$!
    assert "prompt: asks again" until_ok 10 asking
    typed "abandoned"
    keys Escape
    wait "$runner"
    assert_eq "prompt: escape cancels" 1 "$?"
    assert_contains "prompt: the script is told" "$(cat "$WORK/prompt.out")" "cancel"
}

# ws_name — the name of the scenario's workspace.
ws_name() { gui_status "next(w['name'] for w in s['workspaces'] if w['id'] == '$WS')"; }
named() { [ "$(ws_name)" = "$1" ]; }

scenario_history() {
    needs_keyboard history || return 0
    new_repo history
    local file="$REPO_DIR/.metafolder/gui/history/shell:command"

    # Escape first: with no text field focused, ':' is the binding that
    # focuses the command input, not a character typed somewhere.
    keys Escape colon
    typed "workspace:rename Typed name"
    keys Return
    assert "history: the typed command runs" until_ok 10 named "Typed name"
    assert "history: and is kept for the repository" \
        grep -qx "workspace:rename Typed name" "$file"

    run workspace:rename Other
    # ctrl+p reads the history before it recalls: return waits a second for
    # it. Typed first, `status:clear` is what return runs if the recall is
    # late — a failure that changes nothing, where an empty line would run
    # whatever the completion list offers first.
    keys Escape colon
    typed "status:clear"
    keys ctrl+p
    sleep 1
    keys Return
    assert "history: ctrl+p recalls it" until_ok 10 named "Typed name"
}

# bound <combo> — keybindings.toml has an entry for the combo.
bound() { grep -q "^\"$1\" =" "$CONFIG_HOME/metafolder/gui/keybindings.toml"; }
unbound() { ! bound "$1"; }

scenario_keybindings() {
    needs_keyboard keybindings || return 0
    new_repo keybindings
    local file="$CONFIG_HOME/metafolder/gui/keybindings.toml"

    run config:open
    command_ok "settings: config open"
    # The key field has the focus; tab walks the form — command, when, focus,
    # text-input, Save — and runs no binding while the view is open.
    typed "!"
    keys Tab
    typed "status:clear"
    keys Tab Tab Tab Tab Return
    assert "settings: Save writes the binding" \
        until_ok 10 grep -qx '"!" = { command = "status:clear" }' "$file"
    # The focus is back on the key field; Save, disabled while it is empty, is
    # skipped: the next stops are the first row's Edit and Remove, and "!"
    # sorts first.
    keys Tab Tab Tab Tab Tab Tab Return
    assert "settings: Remove unbinds it" until_ok 10 unbound "!"
    keys Escape
    status_is "settings: tab ran no binding behind the view" info "e2e: before config:open"

    show_panel left e2e-keys e2e-keys:ping
    mfg message --workspace "$WS" "e2e: before ctrl+alt+y" >/dev/null
    keys ctrl+alt+y
    assert "settings closed: a key a panel suggested runs its command" \
        until_ok 10 says "e2e: pinged"
}

# running <pid> — the process has not exited.
running() { kill -0 "$1" 2>/dev/null; }

scenario_open_with() {
    needs_keyboard open-with || return 0
    new_repo open-with
    local file="$REPO_DIR/a.txt" runner tries=0
    echo a >"$file"
    record "$file" >/dev/null
    select_path "$file"

    # Without a program, it is asked in the command input, completing over the
    # configured `open-with` list. The prompt leaves no trace to wait on, so
    # escape is pressed until the command ends: harmless before the prompt
    # opens (no binding), it cancels the prompt once it is there.
    mfg command --timeout-ms 60000 file:open-with >/dev/null 2>&1 &
    runner=$!
    while running "$runner" && [ "$tries" -lt 40 ]; do
        keys Escape
        sleep 0.25
        tries=$((tries + 1))
    done
    assert_not "open with: escape cancels the program prompt" running "$runner"
    wait "$runner"
    assert_not "open with: a cancelled prompt runs nothing" test -e "$WORK/opened"

    # With one, it runs on the selection.
    # shellcheck disable=SC2016 # $1 is the script's own argument
    printf '#!/bin/sh\necho "$1" >"%s"\n' "$WORK/opened" >"$WORK/e2e-open"
    chmod +x "$WORK/e2e-open"
    run file:open-with "$WORK/e2e-open"
    command_ok "open with: a program given runs"
    assert "open with: the program runs" until_ok 10 test -s "$WORK/opened"
    assert_eq "open with: on the selected file" "$file" "$(cat "$WORK/opened" 2>/dev/null)"
}

scenario_shell_line() {
    needs_keyboard shell-line || return 0
    new_repo shell-line
    local out="$WORK/shell-line.out" cut="$WORK/shell-line-cut.out"

    # The line at the bottom of the shell panel runs what is typed in it, and
    # keeps the keyboard for the next one (doc "shell panel").
    run panel:open here shell
    run shell:focus line
    command_ok "shell line: focus reaches the panel's line"
    typed "echo typed-in-panel > $out"
    keys Return
    assert "shell line: Enter runs the line" until_ok 10 grep -qx typed-in-panel "$out"

    # ctrl+k (editing:clear line-end) drops what follows the caret: without
    # it, echo would also print the trailing word. Typed without leaving the
    # field — the line still has the keyboard after the run.
    typed "echo kept > $cut dropped"
    keys Left Left Left Left Left Left Left Left ctrl+k Return
    assert "shell line: still focused, ctrl+k cuts the end" until_ok 10 grep -qx kept "$cut"
}

# has_ws <id> — the GUI holds that workspace.
has_ws() { [ "$(gui_status "any(w['id'] == '$1' for w in s['workspaces'])")" = True ]; }
lacks_ws() { ! has_ws "$1"; }

scenario_tab_close() {
    needs_clicks tab-close || return 0
    new_repo tab-close
    local n0 doomed
    run workspace:fork
    doomed=$(gui_status "s['workspaces'][-1]['id']")
    mfg layout left "$doomed" >/dev/null
    run workspace:rename Doomed tab
    mfg layout left "$WS" >/dev/null
    n0=$(ws_count)

    assert "tab close: its button is clicked" until_ok 10 click '.tab-bar .tab' 'Doomed tab' '.close'
    assert "tab close: the workspace goes" until_ok 10 lacks_ws "$doomed"
    assert_eq "tab close: only that one" $((n0 - 1)) "$(ws_count)"
    assert_eq "tab close: the shown one stays" "$WS" "$(slot_ws left)"
}

# field_is <uuid> <field> <value> — the daemon holds that value.
field_is() { [ "$(df_mf metarecord -i "$1" field get "$2" 2>/dev/null)" = "$3" ]; }

scenario_pick_button() {
    needs_clicks pick-button || return 0
    new_repo pick-button
    local a b c f
    for f in a b c; do echo "$f" >"$REPO_DIR/$f.txt"; done
    a=$(record "$REPO_DIR/a.txt")
    b=$(record "$REPO_DIR/b.txt")
    c=$(record "$REPO_DIR/c.txt")
    df_mf metarecord -i "$a" field set "friend:ref=$b" >/dev/null
    select_path "$REPO_DIR/a.txt"
    show_panel left metarecord-detail metarecord:cancel

    # The row's Edit opens its inline editor, whose ref widget has the button.
    assert "pick button: the field is edited" until_ok 10 click '#field-rows tr' friend button Edit
    assert "pick button: is clicked" until_ok 10 click '#field-rows tr' friend .pick-btn
    assert "pick button: opens a picker" until_ok 10 picking "Pick: friend"
    # The picker shows a metarecord list, which selects its first row once
    # loaded: a selection made before would be overwritten by it.
    assert "pick button: the picker's list has selected a row" \
        until_ok 10 shows 'tr.row.cursor, .card.cursor'
    mfg view "$(picker_slot "Pick: friend")" file --path "$REPO_DIR/c.txt" >/dev/null \
        || die "view file c.txt"
    run pick:confirm
    assert "pick button: the picker closes" until_ok 10 not_picking "Pick: friend"
    # The pick's result reaches the editor after the command resolves.
    assert "pick button: the editor holds the pick" \
        until_ok 10 reads "$c" '#field-rows tr' friend input
    assert "pick button: the edit is confirmed" until_ok 10 click '#field-rows tr' friend button OK
    assert "pick button: the picked metarecord is written" until_ok 10 field_is "$a" friend "$c"
}

scenario_suggested_key() {
    needs_clicks suggested-key || return 0
    new_repo suggested-key
    # The panel suggests ctrl+alt+y when it mounts.
    show_panel left e2e-keys e2e-keys:ping
    run config:open

    assert "suggested key: its Remove is clicked" \
        until_ok 10 click '.binding-table tr' ctrl+alt+y button Remove
    # The settings view says its errors itself, not on the status bar.
    assert_contains "suggested key: the removal is refused, saying why" \
        "$(until_ok 10 webview text '.dialog .error' 2>/dev/null)" \
        "is suggested by a panel or by commands.js"
    assert "suggested key: its row stays" shows '.binding-table tr' ctrl+alt+y
    assert "settings: closed by its button" click '.dialog header button'

    mfg message --workspace "$WS" "e2e: before ctrl+alt+y" >/dev/null
    run e2e-keys:ping
    status_is "suggested key: the command still runs" info "e2e: pinged"
}

# Last: the inspector docks into the window and may take the keyboard.
# ws_var <key> — a variable of the scenario's workspace, as JSON.
ws_var() {
    curl -sf -H "Authorization: Bearer $(cat "$XDG_RUNTIME_DIR/metafolder/gui.token")" \
        "http://127.0.0.1:$GUI_PORT/gui/workspaces/$WS/vars/$1" |
        python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin)["value"]))'
}
# fm_at <dir> [<cursor>] — the file manager shows <dir> (under the
# repository), its cursor on <cursor> — on nothing when none is given.
fm_at() {
    local cursor=null
    [ $# -gt 1 ] && cursor="\"$2\""
    [ "$(ws_var file-manager:dir)" = "\"$REPO_DIR$1\"" ] &&
        [ "$(ws_var file-manager:cursor)" = "$cursor" ]
}

# doc "Jumping to an entry": `cursor:find file-manager walk` enters each directory
# named and asks again from inside; a file ends the walk, selected, and
# shift+Return stops on a directory, selected.
scenario_walk() {
    needs_keyboard walk || return 0
    new_repo walk
    mkdir -p "$REPO_DIR/a/b"
    touch "$REPO_DIR/a/b/c.txt" "$REPO_DIR/a/other.txt"
    show_panel left file-manager view:refresh file-manager

    run cursor:find file-manager walk a/
    assert "walk: a directory named is entered" until_ok 10 fm_at /a
    typed "b"
    keys Return
    assert "walk: and the question asked again inside it" until_ok 10 fm_at /a/b
    typed "c.txt"
    keys Return
    assert "walk: a file named is selected" until_ok 10 fm_at /a/b c.txt

    run cursor:parent file-manager
    run cursor:parent file-manager
    run cursor:find file-manager walk a/
    assert "walk: again from the root" until_ok 10 fm_at /a
    typed "b"
    keys shift+Return
    assert "walk: shift+return stops on the directory, selected" until_ok 10 fm_at /a b

    # The tree explorer walks the same way, over the mfr_path forest: a node
    # with children is gone into, a leaf is selected.
    record "$REPO_DIR/a" >/dev/null && record "$REPO_DIR/a/b" >/dev/null &&
        record "$REPO_DIR/a/b/c.txt" >/dev/null || die "record a/b/c.txt"
    show_panel left treeref view:refresh treeref
    run treeref:set field mfr_path
    run cursor:find treeref walk /
    for name in a b; do
        sleep 1
        typed "$name"
        keys Return
    done
    sleep 1
    typed "c.txt"
    keys Return
    assert "walk (treeref): a leaf named is selected" until_ok 10 tree_at /a/b/c.txt

    run cursor:root treeref
    run cursor:find treeref walk /
    sleep 1
    typed "a"
    keys Return
    sleep 1
    typed "b"
    keys shift+Return
    assert "walk (treeref): shift+return stops on the node, selected" until_ok 10 tree_at /a/b
}

# The slideshow (doc "Slideshow"): `file:play query` walks what the list
# shows — the images for their time, the rest passed over — and says when it is
# done. The file panel must be on screen: a hidden one shows nothing, so a
# slideshow there waits.
scenario_slideshow() {
    new_repo slideshow
    # Two 1×1 PNGs and a text: two images to wait on, the rest to skip.
    for name in a b; do
        printf '\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\x9cc\xf8\x0f\x00\x00\x01\x01\x00\x05\x18\xd8N\0\0\0\0IEND\xaeB`\x82' \
            >"$REPO_DIR/$name.png"
        record "$REPO_DIR/$name.png" >/dev/null || die "record $name.png"
    done
    echo hello >"$REPO_DIR/c.txt"
    record "$REPO_DIR/c.txt" >/dev/null || die "record c.txt"

    show_panel left metarecord-list view:refresh metarecord-list
    # Assigned rather than split: an earlier scenario may have left the right
    # slot open, and `panel:split` would then close it.
    mfg layout right "$WS" >/dev/null || die "layout right $WS"
    mfg view right file >/dev/null || die "view right file"

    run file:play selection
    status_is "slideshow: an empty selection plays nothing" info \
        'No metarecords are checked — nothing to play.'

    run file:play query
    command_ok "slideshow: file:play query starts"
    assert "slideshow: the query shown is played to its end" until_ok 30 says 'Slideshow finished.'
    # The text and the root folder are passed over, never left on screen.
    assert_eq "slideshow: it ends on an image" png \
        "$(ws_var selected_paths | python3 -c 'import json, sys; print(json.load(sys.stdin)[0].rsplit(".", 1)[-1])')"

    run file:play stop
    status_is "slideshow: stop with none running says so" info 'No slideshow is running.'
    mfg layout right - >/dev/null
}

# tree_at <path> — the tree explorer's selected node is <path>.
tree_at() {
    [ "$(ws_var selected_treeref | python3 -c 'import json, sys
print((json.load(sys.stdin) or {}).get("path"))')" = "$1" ]
}

scenario_devtools() {
    new_repo devtools
    run devtools:open
    command_ok "devtools open"
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
