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

# ── Failure reporting ─────────────────────────────────────────────────────────
# A GUI script that dies under `set -e` used to leave NOTHING on screen: its
# stdout/stderr go to the message log of the workspace it was launched from,
# which is not the scratch workspace it took over — and the session teardown
# removes that one, so the run just vanished mid-question. Sourcing this file
# therefore installs an ERR trap that names the failing command, and routes
# every report to the *launching* workspace, which outlives the session.
#
# `set -E` makes the trap fire inside functions and subshells too. Bash keeps
# both errexit and the ERR trap disabled wherever a failure is already being
# tested (`cmd || handler`, `if cmd`), so a handled failure — a cancelled
# prompt, say — never reports.
set -E

# The workspace to report to: the one the GUI launched the script from (it owns
# the script's message log), saved before the session takeover. Empty until
# mf_gui_session_open, where reports go to the focused workspace instead.
_MF_HOME_WS=""

# Print a report on stderr (→ the launching workspace's message log and the
# terminal) and raise it on that workspace's status bar. Never fatal.
mf_gui_report() { # <text>
    printf '%s\n' "$*" >&2
    if [ -n "$_MF_HOME_WS" ]; then
        mf gui message "$*" --workspace "$_MF_HOME_WS" >/dev/null 2>&1 || :
    else
        mf gui message "$*" >/dev/null 2>&1 || :
    fi
}

_mf_gui_on_err() { # <exit code> <line> <command>
    mf_gui_report "error: line $2: '$3' exited $1"
}
trap '_mf_gui_on_err "$?" "$LINENO" "$BASH_COMMAND"' ERR

# Print an error and exit 1.
mf_die() { mf_gui_report "error: $*"; exit 1; }

# Bind METAFOLDER_REPO to the repository of the focused GUI workspace.
# Sets (and exports) REPO for the rest of the script.
mf_gui_bind_repo() {
    REPO=$(mf gui repo) || mf_die "no GUI running or no repository in the focused workspace"
    export METAFOLDER_REPO="$REPO"
}

# Internal: restore the saved layout and remove the scratch workspace. Installed
# as the EXIT trap by mf_gui_session_open; every step is best-effort.
# Runs only in the top-level shell: a command substitution inherits the trap,
# and letting a subshell's exit tear the session down would kill the run.
_mf_gui_session_cleanup() {
    [ "${BASHPID:-$$}" = "$$" ] || return 0
    [ -n "${_MF_WS:-}" ] && mf gui workspace rm "$_MF_WS" >/dev/null 2>&1 || :
    [ -n "${_MF_SAVED_LEFT:-}" ] && mf gui layout left "$_MF_SAVED_LEFT" >/dev/null 2>&1 || :
    [ -n "${_MF_SAVED_RIGHT:-}" ] && mf gui layout right "$_MF_SAVED_RIGHT" >/dev/null 2>&1 || :
    [ -n "${_MF_TMP:-}" ] && rm -rf "$_MF_TMP" 2>/dev/null || :
    # The outcome is posted only now: while the session was open it would have
    # landed on the scratch workspace this very function removes.
    [ -n "${_MF_OUTCOME:-}" ] && mf_gui_report "$_MF_OUTCOME"
    return 0
}

# The script's final, user-visible outcome. Printed on stdout right away (the
# message log), and raised on the launching workspace's status bar once the
# session is torn down — a `mf gui message` sent before that goes to the scratch
# workspace and disappears with it.
mf_gui_finish() { # <text>
    _MF_OUTCOME=$1
    printf '%s\n' "$1"
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
    # Report to the workspace that was on screen before the takeover: the
    # scratch one below does not survive the run.
    [ "$_MF_SAVED_LEFT" = "-" ] || _MF_HOME_WS=$_MF_SAVED_LEFT
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

# ── Argument prompts (GUI completion) ─────────────────────────────────────────
# Let a script obtain a missing argument through the GUI's completion UI rather
# than only from the command line. Each prints the confirmed value on stdout and
# returns non-zero when the user cancels (Escape) — so a caller writes:
#     TAG=$(mf_gui_prompt_tag "Tag: ") || mf_die "cancelled"
# Call mf_gui_bind_repo first (the completion sources query the repository).

# Prompt for a value, completing over the lines read from stdin.
#     printf '%s\n' a b c | mf_gui_prompt "Pick one: "
mf_gui_prompt() { # <message>   (completions on stdin)
    mf gui prompt "$1" --completions-stdin
}

# Prompt for a tag, completing over the existing tag vocabulary (mf tag list).
mf_gui_prompt_tag() { # <message>
    mf tag list | cut -f1 | mf_gui_prompt "$1"
}

# Internal: prompt for an mfr_path tree-path, completing over the repository's
# tracked records matching <predicate>. `--resolve-tree mfr_path` already prints
# descendants with a leading slash (`/projets`) and the repository root as the
# EMPTY string, so we only turn that empty root line into "/" — prefixing every
# line (as this once did) double-slashes descendants ("//projets").
_mf_gui_prompt_mfr_path() { # <message> <predicate>
    mf metarecord -q "$2" get --resolve-tree mfr_path \
        | sed 's,^$,/,' | sort -u | mf_gui_prompt "$1"
}

# Prompt for a tracked folder / file, completing over the repository's tree.
# Prints the chosen root-relative path (leading slash, e.g. /music/jazz).
mf_gui_prompt_folder() { _mf_gui_prompt_mfr_path "${1:-Folder: }" 'mfr_type = "dir"'; }
mf_gui_prompt_file() { _mf_gui_prompt_mfr_path "${1:-File: }" 'mfr_type = "file"'; }

# Convert a repository-relative tree path — as `mf path --relative` prints it
# ("/" for the root, "/a/b" below) — to the form the `mfr_path` tree queries
# (`= "…"`, `-> "…"`, `->* "…"`) expect: the root is the EMPTY string, every
# descendant keeps its leading slash. Without this the root folder resolves to
# the empty set (`mfr_path = "/"` / `->* "/"` match nothing), so a whole-repo
# operation silently does nothing.
mf_gui_query_path() { # <relpath>
    if [ "$1" = "/" ]; then printf ''; else printf '%s' "$1"; fi
}

# Resolve an mfr_path tree-path (leading slash) to its metarecord uuid; prints
# the uuid, or nothing (empty) when no tracked record sits at that path.
mf_gui_path_uuid() { # <treepath>
    mf metarecord -q "mfr_path = \"$(mf_gui_query_path "$1")\"" get | head -n1
}

# Update this script's entry in the GUI task bar (spec-gui "Scripting"): a
# determinate progress bar with --done/--total, and/or a --phase label for the
# current step. A no-op outside the GUI (METAFOLDER_GUI_TASK unset) and never
# fatal, so scripts may call it unconditionally.
#     mf_gui_progress --done 3 --total 10 --phase "/music/x.mp3"
mf_gui_progress() { mf gui progress "$@" >/dev/null 2>&1 || :; }

# The arrow keys that double the letter answers, so a question can be answered
# without looking at the keyboard: → yes, ← no, ↑ mixed, ↓ skip. The mapping is
# fixed (spec-gui "Script session") so every script answers the same way.
_mf_gui_arrow_for() { # <letter> -> its arrow key, or nothing
    case $1 in
        y) printf 'right' ;;
        n) printf 'left' ;;
        m) printf 'up' ;;
        s) printf 'down' ;;
    esac
}

# Ask a question and print the answer as a LETTER: each of y/n/m/s in <letters>
# also awaits its arrow key, and the pressed arrow is folded back onto the
# letter — so a caller's `case` only ever deals with y/n/m/s and its quit key.
#
# `escape` is NOT a key a script may await: the GUI keeps it to stop the script
# outright, whatever keys the script grabbed (spec-gui "Reserved keys"), and
# refuses a wait that asks for it. A script with cleanup to do offers `q`
# instead — by convention the quit key of every shipped script.
#     case "$(mf_gui_ask_answer "$msg" y n m s q)" in ...
mf_gui_ask_answer() { # <message> <letter>...
    local msg=$1 letter arrow pressed
    shift
    local keys=()
    for letter in "$@"; do
        keys+=("$letter")
        arrow=$(_mf_gui_arrow_for "$letter")
        [ -n "$arrow" ] && keys+=("$arrow")
    done
    pressed=$(mf_gui_ask "$msg" "${keys[@]}")
    case $pressed in
        right) printf 'y\n' ;;
        left)  printf 'n\n' ;;
        up)    printf 'm\n' ;;
        down)  printf 's\n' ;;
        *)     printf '%s\n' "$pressed" ;;
    esac
}

# Ask a question in the GUI and wait for one key:
#     key=$(mf_gui_ask "message" y n q)
# Prints the pressed key; 'escape' when the question could not be answered at
# all — a timeout, a closed GUI, a refused wait — which every caller's default
# branch stops on, like the quit key. Targets the
# session workspace (WS) when one is open.
mf_gui_ask() {
    local msg=$1 key why err rc
    shift
    if [ -n "${WS:-}" ]; then
        mf gui message "$msg" --workspace "$WS" >/dev/null
    else
        mf gui message "$msg" >/dev/null
    fi
    # `mf gui input` fails on a timeout, a closed GUI, or a 409 (another wait
    # still holds the lock). All three end the run like Escape does — but the
    # reason must NOT be swallowed: silently answering "escape" is exactly what
    # made a script look like it "just stopped" for no reason.
    err=$(mktemp 2>/dev/null) || err=""
    key=$(mf gui input "$@" 2>"${err:-/dev/null}")
    rc=$?
    if [ "$rc" -ne 0 ]; then
        why=$([ -n "$err" ] && cat "$err" 2>/dev/null)
        mf_gui_report "the question could not be answered (${why:-no reason given}) — stopping"
        key=escape
    fi
    [ -n "$err" ] && rm -f "$err"
    printf '%s\n' "$key"
}
