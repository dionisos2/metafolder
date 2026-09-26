#!/usr/bin/env bash
# End-to-end test of the fanotify broker against a real kernel, with real
# privileges — the one thing the test suite cannot do (docs/watcher-fanotify.md
# "Tests": handle resolution needs CAP_DAC_READ_SEARCH in the *initial* user
# namespace, which no container or user namespace grants).
#
# Run on a real machine, as your usual user (it uses sudo for the broker and to
# play a second user):
#
#   scripts/watchd-live-test.sh            # broker as `nobody` + the two caps
#   scripts/watchd-live-test.sh --root     # broker as plain root
#   scripts/watchd-live-test.sh --no-build # use the binaries already built
#
# The default mode reproduces the systemd unit: an unprivileged uid holding
# exactly CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH (setpriv). Everything happens
# under one scratch directory, on a private daemon port and runtime dir — your
# running daemon, GUI and configuration are not touched — and is removed at
# the end. Exit status: 0 when every check passed.
#
# --self-test-inotify runs the same scenario *without* a broker (the daemon
# falls back to inotify): it checks the harness itself where no privileged
# broker can run, and proves nothing about fanotify.

set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$here/.." && pwd)

mode=caps
build=1
for arg in "$@"; do
    case "$arg" in
        --root) mode=root ;;
        --no-build) build=0 ;;
        --self-test-inotify) mode=none ;;
        -h | --help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

# ── Output ───────────────────────────────────────────────────────────────────

passed=0
failed=0
pass() {
    passed=$((passed + 1))
    echo "  ok    $*"
}
fail() {
    failed=$((failed + 1))
    echo "  FAIL  $*"
}
die() {
    echo "error: $*" >&2
    exit 2
}

# ── Setup ────────────────────────────────────────────────────────────────────

[[ "$(uname -s)" == Linux ]] || die "fanotify is Linux-only"
command -v python3 >/dev/null || die "python3 is needed (free port, protocol probe)"
if [[ $mode != none ]]; then
    sudo -v || die "sudo is needed to start the broker"
fi

if ((build)); then
    echo "building…"
    (cd "$repo_root" && cargo build -q -p metafolder-watchd -p metafolder-daemon -p metafolder-cli) ||
        die "build failed"
fi
bin="$repo_root/target/debug"
for b in metafolder-watchd metafolder-daemon mf; do
    [[ -x "$bin/$b" ]] || die "$bin/$b is missing (drop --no-build)"
done

work="${TMPDIR:-/tmp}/metafolder-tests/watchd-live-$$"
mkdir -p "$work"
work=$(cd "$work" && pwd -P) # the kernel reports resolved paths
run="$work/run"         # the broker's socket (writable by its uid)
xdg="$work/xdg"         # the daemon's runtime dir: its token lives here
repo="$work/repo"       # the watched repository
stage="$work/stage"     # built here, moved in whole (no event half-way)
mkdir -p "$run" "$xdg" "$repo" "$stage" "$work/config"
chmod 0777 "$run"
chmod 0700 "$xdg"
sock="$run/watchd.sock"

broker_pid=
daemon_pid=
mounted=
# shellcheck disable=SC2329 # run by the trap below
cleanup() {
    [[ -n $daemon_pid ]] && kill "$daemon_pid" 2>/dev/null
    [[ -n $broker_pid ]] && sudo kill "$broker_pid" 2>/dev/null
    [[ -n $mounted ]] && sudo umount "$mounted" 2>/dev/null
    wait 2>/dev/null
    sudo rm -rf "$work" 2>/dev/null || rm -rf "$work"
}
trap cleanup EXIT

show_logs() {
    echo "── broker log (tail) ──"
    tail -n 30 "$work/broker.log" 2>/dev/null
    echo "── daemon log (tail) ──"
    tail -n 30 "$work/daemon.log" 2>/dev/null
}

# Waits up to 10 s for a command to succeed.
wait_for() {
    local i
    for ((i = 0; i < 40; i++)); do
        "$@" && return 0
        sleep 0.25
    done
    return 1
}

# ── The broker ───────────────────────────────────────────────────────────────

echo "broker mode: $mode"
case $mode in
    caps)
        caps=+sys_admin,+dac_read_search
        # The log belongs to us, not to the broker: the redirect is ours.
        # shellcheck disable=SC2024
        sudo setpriv --reuid=65534 --regid=65534 --clear-groups \
            --inh-caps=-all,$caps --ambient-caps=-all,$caps --bounding-set=-all,$caps \
            --no-new-privs -- "$bin/metafolder-watchd" --socket "$sock" \
            >"$work/broker.log" 2>&1 &
        broker_pid=$!
        ;;
    root)
        # shellcheck disable=SC2024
        sudo "$bin/metafolder-watchd" --socket "$sock" >"$work/broker.log" 2>&1 &
        broker_pid=$!
        ;;
    none) sock="$run/nothing-listens-here.sock" ;;
esac
if [[ $mode != none ]]; then
    # `$broker_pid` is sudo's: it relays the cleanup's SIGTERM to the broker.
    if ! wait_for test -S "$sock"; then
        show_logs
        die "the broker did not start (log above)"
    fi
    pass "the broker starts ($mode) and passes its preflight"
fi

# ── The daemon ───────────────────────────────────────────────────────────────

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
# A private configuration, from the shipped defaults: yours is not read.
for crate in core daemon cli; do
    mkdir -p "$work/config/metafolder/$crate"
    cp -r "$repo_root/crates/$crate/default-config/." "$work/config/metafolder/$crate/"
done
sed "s|^watchd-socket = .*|watchd-socket = \"$sock\"|" \
    "$repo_root/crates/daemon/default-config/config.toml" >"$work/daemon.toml"
export XDG_RUNTIME_DIR="$xdg" XDG_CONFIG_HOME="$work/config"
"$bin/metafolder-daemon" -p "$port" -c "$work/daemon.toml" >"$work/daemon.log" 2>&1 &
daemon_pid=$!

mf() { "$bin/mf" --no-config -p "$port" "$@"; }
if ! wait_for mf repo list >/dev/null 2>&1; then
    show_logs
    die "the daemon did not start (log above)"
fi

uuid=$(mf repo init "$repo" 2>"$work/init.err") || {
    cat "$work/init.err"
    show_logs
    die "mf repo init failed"
}
m() { mf -u "$uuid" "$@"; }
root_uuid=$(m metarecord get | head -n 1)
m metarecord -i "$root_uuid" field set mf_watch:bool=true >/dev/null || die "cannot set mf_watch"

status=$(m watch status --json)
if [[ $mode == none ]]; then
    echo "  (self-test: backend is $(grep -o '"backend": *"[a-z]*"' <<<"$status"))"
elif grep -q '"backend": *"fanotify"' <<<"$status"; then
    pass "the daemon took the broker (backend fanotify)"
else
    fail "the daemon fell back to another backend: $status"
    show_logs
    die "nothing below would test fanotify"
fi

# ── Scenario: what happens on disk reaches the metadata ─────────────────────

# The uuids of the metarecords at the exact repo-relative path $1.
at() { m metarecord -q "mfr_path = \"$1\"" get 2>/dev/null; }
# shellcheck disable=SC2329 # run through wait_for
exists_at() { [[ -n "$(at "$1")" ]]; }
absent_at() { [[ -z "$(at "$1")" ]]; }
is_at() { [[ "$(at "$2")" == "$1" ]]; }

echo "a file's life:"
echo hello >"$repo/a.txt"
if wait_for exists_at /a.txt; then
    pass "a created file gets a metarecord"
else
    fail "no metarecord for a created file"
fi
id=$(at /a.txt)

mv "$repo/a.txt" "$repo/b.txt"
if wait_for is_at "$id" /b.txt && absent_at /a.txt; then
    pass "a rename keeps the metarecord (same uuid at the new name)"
else
    fail "rename: expected $id at /b.txt, got '$(at /b.txt)'"
fi

mkdir "$repo/d"
mv "$repo/b.txt" "$repo/d/b.txt"
if wait_for is_at "$id" /d/b.txt; then
    pass "a move into a new directory keeps the metarecord"
else
    fail "move: expected $id at /d/b.txt, got '$(at /d/b.txt)'"
fi

echo more >>"$repo/d/b.txt"
sleep 1.5
if is_at "$id" /d/b.txt; then
    pass "a modification keeps the metarecord where it is"
else
    fail "modification lost the file's metarecord"
fi

mkdir -p "$stage/tree/sub"
echo x >"$stage/tree/sub/deep.txt"
mv "$stage/tree" "$repo/tree"
if wait_for exists_at /tree/sub/deep.txt; then
    pass "a tree moved in whole is picked up to its leaves"
else
    fail "a tree moved in: /tree/sub/deep.txt has no metarecord"
fi

rm "$repo/d/b.txt"
if wait_for absent_at /d/b.txt && m metarecord -i "$id" get >/dev/null 2>&1; then
    pass "a deleted file keeps its metarecord, without a path"
else
    fail "deletion: /d/b.txt still at '$(at /d/b.txt)', or the metarecord is gone"
fi

outside="$work/outside.txt"
echo bye >"$repo/leaving.txt"
wait_for exists_at /leaving.txt
leaving=$(at /leaving.txt)
mv "$repo/leaving.txt" "$outside"
if wait_for absent_at /leaving.txt && m metarecord -i "$leaving" get >/dev/null 2>&1; then
    pass "a file moved out of the repository leaves its metarecord behind"
else
    fail "a file moved out: /leaving.txt still at '$(at /leaving.txt)'"
fi

# ── Scenario: a filesystem mounted inside the repository ────────────────────

echo "mounts:"
mkdir "$repo/mnt"
if sudo mount -t tmpfs -o "mode=0777" metafolder-live "$repo/mnt"; then
    mounted="$repo/mnt"
    sleep 1 # the broker follows the mount table, then marks it
    echo x >"$repo/mnt/on-tmpfs.txt"
    if wait_for exists_at /mnt/on-tmpfs.txt; then
        pass "a filesystem mounted under the root after the subscription is covered"
    else
        fail "no metarecord for a file on a filesystem mounted under the root"
    fi
    sudo umount "$repo/mnt" && mounted=
else
    # Not the broker's failure: this machine will not let us mount (a
    # container without CAP_SYS_ADMIN, say). Said, not counted.
    echo "  skip  could not mount a tmpfs here: mounts are not tested"
fi

# ── Scenario: another user's private directories stay invisible ─────────────

echo "permissions (a directory of uid 65534):"
# Built outside, then moved in whole: the only event about them names the
# top directory, which the repository's root may list.
sudo mkdir -p "$stage/private" "$stage/half/known"
sudo chown -R 65534:65534 "$stage/private" "$stage/half"
sudo chmod 0700 "$stage/private"
sudo chmod 0711 "$stage/half"
sudo chmod 0755 "$stage/half/known"
sudo mv "$stage/private" "$stage/half" "$repo/"
sleep 1
sudo touch "$repo/private/secret" "$repo/half/known/file"
sleep 2
if absent_at /private/secret; then
    pass "nothing about a file in a 0700 directory of another user"
else
    fail "a file in another user's 0700 directory reached the daemon"
fi
if absent_at /half/known/file && absent_at /half/known; then
    pass "nothing under a --x directory: its entries' names stay unknown"
else
    fail "an entry under another user's --x directory reached the daemon"
fi

if [[ $mode != none ]]; then
    # The broker itself: a root spelled through a directory the subscriber
    # cannot enter must be refused whether or not what is behind it exists —
    # else the answer is an existence oracle.
    sudo mkdir -p "$work/locked/exists"
    sudo chmod 0700 "$work/locked"
    probe() {
        python3 - "$sock" "$1" <<'EOF'
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall((json.dumps({"op": "subscribe", "roots": [sys.argv[2]]}) + "\n").encode())
answer = json.loads(s.makefile().readline())
print("accepted" if answer.get("roots") else "denied")
EOF
    }
    a=$(probe "$work/locked/exists/../../run")
    b=$(probe "$work/locked/nothing/../../run")
    if [[ $a == denied && $b == denied ]]; then
        pass "a root through a private directory is refused, existing or not"
    else
        fail "the broker answers differently for an existing ($a) and a missing ($b) name"
    fi
    c=$(probe "$work/run")
    if [[ $c == accepted ]]; then
        pass "an accessible root is accepted"
    else
        fail "an accessible root was refused ($c)"
    fi
fi

# ── Result ───────────────────────────────────────────────────────────────────

echo
if ((failed)); then
    show_logs
    echo "$failed check(s) FAILED, $passed passed"
    exit 1
fi
echo "all $passed checks passed"
[[ $mode == none ]] && echo "(self-test without a broker: nothing was learnt about fanotify)"
exit 0
