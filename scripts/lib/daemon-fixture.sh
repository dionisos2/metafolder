# shellcheck shell=bash
# Real-daemon fixture for the shipped-script INTEGRATION tests. Unlike the
# mocked `mf` (scripts/lib/mf-mock.sh, which cannot know the daemon's query
# semantics), this boots an actual `metafolder-daemon` and runs the scripts
# against it with only the `mf gui …` half stubbed — so a script that issues a
# query the daemon rejects, or one whose result is empty, is caught for real.
#
# Isolation: a private XDG_RUNTIME_DIR (own auth token) and an empty --config
# (no auto-load), on an ephemeral port — it never touches the user's daemon or
# repositories.
#
#   df_available            true iff the daemon + mf binaries are built
#   df_start                boot an isolated daemon; sets DF_PORT; EXIT-cleans up
#   df_init_repo <dir>      init+reconcile a repo under <dir>; exports
#                           METAFOLDER_REPO, sets DF_REPO + DF_ROOT (root uuid).
#                           Call it directly (NOT in $(…): the export must reach
#                           the caller's shell).
#   df_hybrid <repo-uuid>   install the gui-stubbing `mf` shim first on PATH
#   df_mf <args…>           call the real mf against the fixture daemon
#   df_tags <uuid>          the record's positive tag paths (one per line)
#   df_neg  <uuid>          the record's negative tag paths
#   hy_reset / hy_input / hy_prompt / hy_log   drive + inspect the gui stub

DF_PORT=""
DF_REPO=""
DF_ROOT=""
_DF_DPID=""
_DF_TMP=""

_DF_DAEMON="target/debug/metafolder-daemon"
_DF_MF="target/debug/mf"

df_available() { [ -x "$_DF_DAEMON" ] && [ -x "$_DF_MF" ]; }

df_mf() { "$_DF_MF" -p "$DF_PORT" "$@"; }

df_start() {
    _DF_TMP=$(mktemp -d "${TMPDIR:-/tmp}/mf-df.XXXXXX")
    mkdir -p "$_DF_TMP/run" "$_DF_TMP/cfg"
    : >"$_DF_TMP/cfg/config.toml"                 # empty: no repos auto-loaded
    export XDG_RUNTIME_DIR="$_DF_TMP/run"          # private auth-token dir
    DF_PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
    "$_DF_DAEMON" --port "$DF_PORT" --config "$_DF_TMP/cfg/config.toml" \
        >"$_DF_TMP/daemon.log" 2>&1 &
    _DF_DPID=$!
    trap _df_stop EXIT
    local _
    for _ in $(seq 1 40); do
        df_mf repo list >/dev/null 2>&1 && return 0
        kill -0 "$_DF_DPID" 2>/dev/null || return 1   # daemon died
        sleep 0.5
    done
    return 1
}

_df_stop() {
    [ -n "$_DF_DPID" ] && kill "$_DF_DPID" 2>/dev/null
    [ -n "$_DF_TMP" ] && rm -rf "$_DF_TMP" 2>/dev/null
    return 0
}

df_init_repo() { # <dir>  -> sets DF_REPO, DF_ROOT, exports METAFOLDER_REPO
    local dir=$1
    DF_REPO=$(df_mf repo init "$dir" 2>/dev/null | head -1)
    export METAFOLDER_REPO="$DF_REPO"
    DF_ROOT=$(df_mf metarecord get 2>/dev/null | head -1)
    df_mf metarecord -i "$DF_ROOT" field set mf_watch:bool=true >/dev/null 2>&1
    df_mf reconcile >/dev/null 2>&1
    # Quiesce the watcher so probing the DB is deterministic (no async churn).
    df_mf metarecord -i "$DF_ROOT" field set mf_watch:bool=false >/dev/null 2>&1
}

df_tags() { df_mf metarecord -i "$1" field get tag --resolve path 2>/dev/null; }
df_neg()  { df_mf metarecord -i "$1" field get negative_tag --resolve path 2>/dev/null; }

# ── the gui-stubbing hybrid `mf` shim ────────────────────────────────────────
df_hybrid() { # <repo-uuid>
    local bin="$_DF_TMP/bin"
    mkdir -p "$bin"
    HY_DIR="$_DF_TMP/hy"; mkdir -p "$HY_DIR"; : >"$HY_DIR/log"
    export HY_DIR
    export HYBRID_REAL_MF="$PWD/$_DF_MF"
    export HYBRID_PORT="$DF_PORT"
    export HYBRID_REPO="$1"
    cat >"$bin/mf" <<'SHIM'
#!/usr/bin/env bash
# Hybrid `mf`: `gui …` is stubbed from queue files; every other command is
# forwarded to the real mf against the fixture daemon (so the tag/query/path
# logic runs for real).
set -u
sig="$*"
printf '%s\n' "$sig" >>"$HY_DIR/log"
pop() { local qf=$1 first; [ -s "$qf" ] || return 1
    IFS= read -r first <"$qf"; tail -n +2 "$qf" >"$qf.r" 2>/dev/null || : >"$qf.r"
    mv "$qf.r" "$qf"; printf '%s' "$first"; }
case "$sig" in
    "gui repo"*)   printf '%s\n' "$HYBRID_REPO"; exit 0 ;;
    "gui input"*)  v=$(pop "$HY_DIR/input") || v=escape; [ -n "$v" ] || v=escape
                   printf '%s\n' "$v"; exit 0 ;;
    "gui prompt"*) case "$sig" in *--completions-stdin*) cat >/dev/null 2>&1 || : ;; esac
                   if v=$(pop "$HY_DIR/prompt"); then
                       [ "$v" = @cancel ] && exit 1
                       printf '%s\n' "$v"; exit 0
                   fi
                   exit 1 ;;
    "gui "*)       exit 0 ;;   # layout/view/message/workspace: no-op success
    *)             exec "$HYBRID_REAL_MF" -p "$HYBRID_PORT" "$@" ;;
esac
SHIM
    chmod +x "$bin/mf"
    PATH="$bin:$PATH"; export PATH
}

hy_reset() { : >"$HY_DIR/log"; rm -f "$HY_DIR/input" "$HY_DIR/prompt"; }
hy_input() { local k; for k in "$@"; do printf '%s\n' "$k" >>"$HY_DIR/input"; done; }
hy_prompt() { local v; for v in "$@"; do printf '%s\n' "$v" >>"$HY_DIR/prompt"; done; }
hy_log() { cat "$HY_DIR/log"; }
