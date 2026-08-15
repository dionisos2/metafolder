# shellcheck shell=bash
# Shared helpers for the gui-* scripts. Source it near the top of a script:
#
#     HERE=$(cd "$(dirname "$0")" && pwd)
#     source "$HERE/lib/mf-gui.sh"
#
# It centralises the boilerplate that every GUI script used to repeat — and,
# crucially, the layout takeover + guaranteed restore, so a script can never
# leave the GUI's layout wrecked or an orphan workspace behind. Requires the
# `mf` CLI on PATH and a running GUI with a repository in the focused workspace.

# Print an error and exit 1.
mf_die() { echo "error: $*" >&2; exit 1; }

# Bind METAFOLDER_REPO to the repository of the focused GUI workspace.
# Sets (and exports) REPO for the rest of the script.
mf_gui_bind_repo() {
    REPO=$(mf gui repo) || mf_die "no GUI running or no repository in the focused workspace"
    export METAFOLDER_REPO="$REPO"
}

# Internal: restore the saved layout and remove the scratch workspace. Installed
# as the EXIT trap by mf_gui_session_open; every step is best-effort.
_mf_gui_session_cleanup() {
    [ -n "${_MF_WS:-}" ] && mf gui workspace rm "$_MF_WS" >/dev/null 2>&1 || :
    [ -n "${_MF_SAVED_LEFT:-}" ] && mf gui layout left "$_MF_SAVED_LEFT" >/dev/null 2>&1 || :
    [ -n "${_MF_SAVED_RIGHT:-}" ] && mf gui layout right "$_MF_SAVED_RIGHT" >/dev/null 2>&1 || :
    [ -n "${_MF_TMP:-}" ] && rm -rf "$_MF_TMP" 2>/dev/null || :
    return 0
}

# A temp directory removed automatically when the session's EXIT trap fires
# (call after mf_gui_session_open). Prints the directory path.
mf_gui_tmpdir() {
    _MF_TMP=$(mktemp -d)
    printf '%s' "$_MF_TMP"
}

# Open a scratch GUI session: save the current layout, create a dedicated
# workspace shown in BOTH slots (left = file, right = <panel>, default
# metarecord-detail), and install an EXIT trap that restores everything —
# so the caller never has to remember the cleanup. Sets WS (the workspace id).
# Call mf_gui_bind_repo first.
mf_gui_session_open() {
    local right=${1:-metarecord-detail}
    _MF_SAVED_LEFT=$(mf gui layout left)
    _MF_SAVED_RIGHT=$(mf gui layout right)
    _MF_WS=$(mf gui workspace new --repo "$REPO") || mf_die "cannot create a workspace"
    trap _mf_gui_session_cleanup EXIT
    mf gui layout left "$_MF_WS" >/dev/null
    mf gui layout right "$_MF_WS" >/dev/null
    mf gui view right "$right" >/dev/null
    WS=$_MF_WS
}

# Show a file in the session's left `file` panel (also publishes
# selected_metarecord, which the right detail panel follows).
mf_gui_show_file() {
    [ -n "${1:-}" ] && mf gui view left file --path "$1" >/dev/null 2>&1 || :
}

# Ask a question in the GUI and wait for one key:
#     key=$(mf_gui_ask "message" y n escape)
# Prints the pressed key; 'escape' on timeout or a closed GUI. Targets the
# session workspace (WS) when one is open.
mf_gui_ask() {
    local msg=$1
    shift
    if [ -n "${WS:-}" ]; then
        mf gui message "$msg" --workspace "$WS" >/dev/null
    else
        mf gui message "$msg" >/dev/null
    fi
    mf gui input "$@" 2>/dev/null || echo escape
}
