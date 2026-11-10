#!/usr/bin/env bash
# shellcheck source-path=SCRIPTDIR
# The demonstration repository (doc "The demonstration repository"): the
# demonstration home (/home/metafolder_user, "Camille Martin") as a repository
# of a throwaway daemon, with tags, ratings and comments, shown in a real GUI.
#
#   scripts/dev/demo.sh [--source DIR] [--unsandboxed] gui
#       the GUI on your display, on the demonstration repository, until it is
#       closed: to explore it, or to record a video
#   scripts/dev/demo.sh [--source DIR] [--unsandboxed] screenshots [OUT_DIR]
#       the GUI on a private X server (Xvfb), driven into each view of
#       scripts/dev/demo/views.sh in turn, one PNG per view in OUT_DIR
#       (default docs/wiki/images)
#
#   --source DIR   the home to show (default /home/metafolder_user)
#   --unsandboxed  pass --allow-unsandboxed-webview to the GUI (a container
#                  that forbids WebKit's sandbox; never on real data)
#
# Nothing is written into the source: the database lives in the run's work
# directory (mf repo init --metafolder), deleted at the end with the daemon. The
# GUI reads a private copy of the installed configuration, so your keybindings
# and commands do not show, and needs it installed (metafolder-sync-config).
#
# Prerequisites: the built binaries (make dev); for screenshots, Xvfb and
# ImageMagick's import.
set -uo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)" || exit 2

SOURCE=/home/metafolder_user
UNSANDBOXED=false
MODE=""
OUT=docs/wiki/images
while [ $# -gt 0 ]; do
    case "$1" in
        --source) SOURCE=${2:?--source needs a directory}; shift ;;
        --unsandboxed) UNSANDBOXED=true ;;
        -h | --help) sed -n '3,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
        gui) MODE=gui ;;
        screenshots) MODE=screenshots; [ -n "${2:-}" ] && { OUT=$2; shift; } ;;
        *) echo "error: unknown argument $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done
[ -n "$MODE" ] || { echo "error: say gui or screenshots (see --help)" >&2; exit 2; }

# shellcheck source=lib/daemon-fixture.sh
. scripts/dev/lib/daemon-fixture.sh
# shellcheck source=demo/lib.sh
. scripts/dev/demo/lib.sh

die() { echo "error: $*" >&2; exit 2; }

GUI_BIN="${METAFOLDER_BIN_DIR:-target/debug}/metafolder-gui"
df_available && [ -x "$GUI_BIN" ] || die "build first (make dev)"
[ -d "$SOURCE" ] || die "no demonstration home at $SOURCE"
if [ "$MODE" = screenshots ]; then
    command -v Xvfb >/dev/null || die "screenshots need Xvfb (Arch: xorg-server-xvfb)"
    for tool in import magick; do
        command -v "$tool" >/dev/null || die "screenshots need ImageMagick ($tool)"
    done
    command -v xdotool >/dev/null || die "screenshots need xdotool (to size the window)"
else
    [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || die "no display (\$DISPLAY / \$WAYLAND_DISPLAY)"
fi

# The fixture moves XDG_RUNTIME_DIR to a private directory (its own auth
# tokens); a relative Wayland socket name would then point into it.
if [ -n "${WAYLAND_DISPLAY:-}" ] && [ "${WAYLAND_DISPLAY#/}" = "$WAYLAND_DISPLAY" ] \
    && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    export WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
fi

df_start || die "the daemon did not start"
WORK=$_DF_TMP
GUI_PID=""
XVFB_PID=""
cleanup() {
    [ -n "$GUI_PID" ] && kill "$GUI_PID" 2>/dev/null && wait "$GUI_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null && wait "$XVFB_PID" 2>/dev/null
    _df_stop
}
trap cleanup EXIT

echo "building the demonstration repository over $SOURCE…"
demo_build_repo "$SOURCE" "$WORK/demo.metafolder" || die "the demonstration repository failed"
REPO=$(df_mf repo list | python3 -c 'import json, sys
print(next(r["repo_uuid"] for r in json.load(sys.stdin) if r["name"] == sys.argv[1]))' "$DEMO_NAME")

# The window is the size a README shows well, on a server of exactly that size.
if [ "$MODE" = screenshots ]; then
    Xvfb -displayfd 1 -screen 0 1440x900x24 -nolisten tcp \
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
fi

# A private copy of the installed configuration, as scripts/dev/gui-e2e.sh does;
# the rendered docs are only linked.
CONFIG_SRC="${XDG_CONFIG_HOME:-$HOME/.config}/metafolder"
CONFIG_HOME="$WORK/config"
mkdir -p "$CONFIG_HOME/metafolder"
for part in cli core daemon gui scripts; do
    [ -d "$CONFIG_SRC/$part" ] && cp -r "$CONFIG_SRC/$part" "$CONFIG_HOME/metafolder/"
done
[ -d "$CONFIG_SRC/docs" ] && ln -s "$CONFIG_SRC/docs" "$CONFIG_HOME/metafolder/docs"
[ -d "$CONFIG_HOME/metafolder/gui" ] || die "no installed configuration in $CONFIG_SRC (metafolder-sync-config)"

GUI_PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
gui_args=(--gui-port "$GUI_PORT" --daemon-port "$DF_PORT")
$UNSANDBOXED && gui_args+=(--allow-unsandboxed-webview)
XDG_CONFIG_HOME="$CONFIG_HOME" PATH="$(cd "$(dirname "$_DF_MF")" && pwd):$PATH" \
    "$GUI_BIN" "${gui_args[@]}" >"$WORK/gui.log" 2>&1 &
GUI_PID=$!

mfg() { "$_DF_MF" gui --gui-url "http://127.0.0.1:$GUI_PORT" "$@"; }
ready=false
for _ in $(seq 1 120); do
    kill -0 "$GUI_PID" 2>/dev/null || break
    if mfg command --timeout-ms 1000 status:clear >/dev/null 2>&1; then
        ready=true
        break
    fi
    sleep 0.5
done
$ready || die "the GUI did not become ready; its log: $(tail -n 20 "$WORK/gui.log")"

# The empty workspace the GUI starts with; closed once one on the repository
# is shown (close_initial), so that no tab says "no repository".
INITIAL_WS=$(mfg status | python3 -c 'import json, sys
print(" ".join(w["id"] for w in json.load(sys.stdin)["workspaces"]))')
close_initial() {
    local ws
    for ws in $INITIAL_WS; do mfg workspace rm "$ws" >/dev/null; done
    INITIAL_WS=""
}

if [ "$MODE" = gui ]; then
    WS=$(mfg workspace new --repo "$REPO" --left metarecord-list) || die "workspace new"
    mfg layout left "$WS" >/dev/null || die "layout left"
    close_initial
    echo "the GUI is on the repository $DEMO_NAME; close its window to end"
    echo "  XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR mf -p $DF_PORT -n $DEMO_NAME …   reaches its daemon"
    echo "  mf gui --gui-url http://127.0.0.1:$GUI_PORT …   drives the GUI"
    wait "$GUI_PID"
    GUI_PID=""
    exit 0
fi

mkdir -p "$OUT"
# shellcheck source=demo/views.sh
. scripts/dev/demo/views.sh
shoot_all "$OUT" || die "a view failed"
# The screenshots show the home's files: their authors and licences go with them.
cp "$SOURCE/CRÉDITS.md" "$OUT/CREDITS.md" 2>/dev/null \
    || echo "warning: no CRÉDITS.md in $SOURCE: credit the files shown by hand" >&2
mfg command quit >/dev/null 2>&1
echo "screenshots written to $OUT"
