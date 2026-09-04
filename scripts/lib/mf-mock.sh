# shellcheck shell=bash
# A fake `mf` CLI for the shipped-script tests.
#
# The launchable scripts under `scripts/shipped/` (gui-tag-pair.sh,
# gui-tag-folder.sh, gui-tag-classify.sh, example-gui-sort-folder.sh) are
# orchestrators: every side effect goes through the real `mf` binary — the
# tag/metarecord/path CLI *and* the `mf gui …` scripting API. Driving them
# end-to-end would need a running daemon AND a running GUI (bwrap + WebKit),
# which is neither hermetic nor fast.
#
# Instead we put a scripted `mf` shim first on PATH. It:
#   • logs every invocation (so a test asserts the exact commands issued),
#   • answers `mf gui input`/`mf gui prompt` from FIFO answer queues (the keys
#     the "user" presses, the values they type), and
#   • answers reads (mf tag list, mf metarecord -q … get, mf path, …) from a
#     small response table the test fills in.
# It also keeps a tiny per-uuid tag store so `mf tag -i U add/deny <path>`
# followed by `mf … field get tag --resolve path` behaves like the real thing —
# enough to drive gui-tag-classify.sh's descend-until-exhausted loop.
#
# Source this file, call `mock_init` once, then `mock_reset` before each case.
# Public API:
#   mock_init                         install the shim on PATH (idempotent)
#   mock_reset                        clear the log, table, queues and tag store
#   mock_respond "<glob>" "<out>"     add a table row (first match wins); <out>
#                                     is a printf %b template (\n → newline),
#                                     or @exit:<n>, or @queue:<name>
#   mock_input   <key> [<key>…]       enqueue answers for `mf gui input`
#                                     (@fail = the call itself fails, as a
#                                     closed GUI or a 409 does)
#   mock_prompt  <val> [<val>|@cancel] enqueue answers for `mf gui prompt`
#                                     (@cancel or an empty queue = user Escaped)
#   mock_queue   <name> <val> [<val>…] enqueue values for a @queue:<name> row
#   mf_log                            print the invocation log (one line/call)
#   mock_calls_matching "<glob>"      print only the log lines matching a glob
#   mock_count "<glob>"               count log lines matching a glob

# Absolute path to the mock's private directory (log, table, queues, state).
MF_MOCK_DIR=""
export MF_MOCK_DIR

_mf_mock_bin=""

# Install the shim. Safe to call more than once; the first call wins.
mock_init() {
    [ -n "$MF_MOCK_DIR" ] && return 0
    MF_MOCK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mf-mock.XXXXXX")
    _mf_mock_bin="$MF_MOCK_DIR/bin"
    mkdir -p "$_mf_mock_bin" "$MF_MOCK_DIR/q" "$MF_MOCK_DIR/state"
    _mf_mock_write_shim "$_mf_mock_bin/mf"
    chmod +x "$_mf_mock_bin/mf"
    PATH="$_mf_mock_bin:$PATH"
    export PATH
    mock_reset
}

# Forget everything from the previous case but keep the shim installed.
mock_reset() {
    : >"$MF_MOCK_DIR/log"
    : >"$MF_MOCK_DIR/responses"
    rm -f "$MF_MOCK_DIR/q/"* 2>/dev/null || :
    rm -f "$MF_MOCK_DIR/state/"* 2>/dev/null || :
}

mock_respond() { # <glob-pattern> <output-template>
    # The table is one row per physical line, TAB-separated, so real newlines
    # and tabs in the output would corrupt it. Encode them as \n / \t escapes;
    # the shim expands them again with printf %b. A value may equally be written
    # with literal \n / \t escapes — both reach the script as newlines/tabs.
    local out=${2//$'\n'/\\n}
    out=${out//$'\t'/\\t}
    printf '%s\t%s\n' "$1" "$out" >>"$MF_MOCK_DIR/responses"
}

mock_input() { # <key>...
    local k
    for k in "$@"; do printf '%s\n' "$k" >>"$MF_MOCK_DIR/q/__input"; done
}

mock_prompt() { # <value>...
    local v
    for v in "$@"; do printf '%s\n' "$v" >>"$MF_MOCK_DIR/q/__prompt"; done
}

mock_queue() { # <name> <value>...
    local name=$1; shift
    local v
    for v in "$@"; do printf '%s\n' "$v" >>"$MF_MOCK_DIR/q/$name"; done
}

mf_log() { cat "$MF_MOCK_DIR/log"; }

mock_calls_matching() { # <glob>
    local pat=$1 line
    while IFS= read -r line || [ -n "$line" ]; do
        # shellcheck disable=SC2254  # glob match is intentional
        case "$line" in $pat) printf '%s\n' "$line" ;; esac
    done <"$MF_MOCK_DIR/log"
}

mock_count() { # <glob>
    mock_calls_matching "$1" | grep -c . || true
}

# --- the shim itself, written out as a standalone bash script -----------------
_mf_mock_write_shim() {
    cat >"$1" <<'SHIM'
#!/usr/bin/env bash
# Scripted `mf` — see scripts/lib/mf-mock.sh. Driven entirely by files under
# $MF_MOCK_DIR; never touches a daemon or a GUI.
set -u
dir=${MF_MOCK_DIR:?MF_MOCK_DIR unset}
sig="$*"

# 1. Log the call verbatim (one line = the joined args).
printf '%s\n' "$sig" >>"$dir/log"

# Pop the first line off a queue file. Prints it (no trailing newline) and
# succeeds; prints nothing and fails when the queue is empty/absent.
pop() {
    local qf=$1 first
    [ -s "$qf" ] || return 1
    IFS= read -r first <"$qf" || true
    tail -n +2 "$qf" >"$qf.rest" 2>/dev/null || : >"$qf.rest"
    mv "$qf.rest" "$qf"
    printf '%s' "$first"
}

# 2. GUI key/value prompts come from their queues.
case "$sig" in
    "gui input"*)
        # The GUI refuses a wait that asks for one of the keys it keeps for the
        # user (spec-gui "Reserved keys"): escape stops the script, tab toggles
        # the script keys, ":" opens the command input. Refuse them here too, or
        # a script asking for one would pass its tests and fail in the GUI.
        for a in "$@"; do
            case "$a" in
                escape|tab|:)
                    echo "'$a' is reserved by the GUI and cannot be awaited by a script" >&2
                    exit 1 ;;
            esac
        done
        if v=$(pop "$dir/q/__input"); then
            # @fail: the wait could not even be registered (closed GUI, 409).
            [ "$v" = "@fail" ] && { echo "input wait ended: closed" >&2; exit 1; }
            [ -n "$v" ] || v=escape
            printf '%s\n' "$v"
        else
            printf 'escape\n'   # exhausted queue = as if the GUI closed
        fi
        exit 0 ;;
    "gui prompt"*)
        case "$sig" in *--completions-stdin*) cat >/dev/null 2>&1 || : ;; esac
        if v=$(pop "$dir/q/__prompt"); then
            case "$v" in
                @cancel) exit 1 ;;              # user pressed Escape
                *) printf '%s\n' "$v"; exit 0 ;;
            esac
        fi
        exit 1 ;;                              # nothing queued = cancelled
esac

# 3. The response table: first matching glob wins. A test overrides ANY default
#    behaviour by adding a row here.
if [ -s "$dir/responses" ]; then
    while IFS=$'\t' read -r pat out || [ -n "$pat" ]; do
        [ -n "$pat" ] || continue
        case "$pat" in \#*) continue ;; esac
        # shellcheck disable=SC2254
        case "$sig" in
            $pat)
                case "$out" in
                    @exit:*)   exit "${out#@exit:}" ;;
                    @queue:*)
                        qn=${out#@queue:}
                        pop "$dir/q/$qn" && printf '\n' || :
                        exit 0 ;;
                    "") exit 0 ;;                     # empty output = nothing
                    *) printf '%b\n' "$out"; exit 0 ;;
                esac
                ;;
        esac
    done <"$dir/responses"
fi

# 4. Built-in tag store: make `mf tag -i U add/deny/mixed <path>` observable to
#    a later `mf … field get {tag,negative_tag,mixed_tag} --resolve path`, so
#    the classify loop terminates like the real CLI. Static rows above win.
case "$sig" in
    "tag -i "*)
        read -r _ _ st_uuid st_verb st_path <<<"$sig"
        case "$st_verb" in
            add)   field=tag ;;
            deny)  field=negative_tag ;;
            mixed) field=mixed_tag ;;
            *) exit 0 ;;
        esac
        [ -n "$st_path" ] || exit 0
        f="$dir/state/$st_uuid.$field"
        grep -qxF -- "$st_path" "$f" 2>/dev/null || printf '%s\n' "$st_path" >>"$f"
        exit 0 ;;
    "metarecord -i "*" field get "*)
        st_uuid=$(printf '%s' "$sig" | awk '{print $3}')
        st_field=$(printf '%s' "$sig" | sed -n 's/.*field get \([^ ]*\).*/\1/p')
        case "$st_field" in
            tag|negative_tag|mixed_tag)
                cat "$dir/state/$st_uuid.$st_field" 2>/dev/null || :
                exit 0 ;;
        esac
        ;;
esac

# 5. Anything unmatched: succeed silently (mutations whose output is discarded).
exit 0
SHIM
}
