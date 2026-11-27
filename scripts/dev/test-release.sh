#!/usr/bin/env bash
# Tests for scripts/dev/release.sh and the install.sh it ships (doc "Building a
# release").
#
# Compiling here would defeat the purpose, so the real scripts run in a fake
# checkout against stubs: `make` and `rustc` on PATH, and a stub
# metafolder-sync-config. What is asserted is the LAYOUT of the archive and
# where install.sh puts it.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/dev/lib/assert.sh"

mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/release.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

fake="$tmp/repo"
bin="$tmp/bin"
log="$tmp/log"
mkdir -p "$fake/scripts/dev/release" "$fake/crates/core" "$bin"
cp "$repo/scripts/dev/release.sh" "$fake/scripts/dev/"
cp "$repo/scripts/dev/release/"* "$fake/scripts/dev/release/"
cp "$repo/scripts/dev/metafolder-watchd.service" "$repo/scripts/dev/metafolder-watchd.sysusers" \
    "$fake/scripts/dev/"
printf '[package]\nname = "metafolder-core"\nversion = "9.8.7"\n' >"$fake/crates/core/Cargo.toml"

# The build: what make leaves in target/release, the sync-config stub
# exporting one default (and, run installed, saying where it read them from).
cat >"$bin/make" <<STUB
#!/usr/bin/env bash
printf 'make %s\n' "\$*" >>"$log"
mkdir -p target/release
for b in metafolder-daemon mf metafolder-gui metafolder-watchd; do
    printf '#!/bin/sh\n' >target/release/\$b; chmod +x target/release/\$b
done
cat >target/release/metafolder-sync-config <<'INNER'
#!/usr/bin/env bash
if [ "\${3:-}" = --export ]; then
    mkdir -p "\$4/gui" && printf 'body{}' >"\$4/gui/style.css"
else
    defaults=\$(dirname "\$0")/../share/metafolder/defaults
    printf 'sync-config %s %s\n' "\$*" "\$(cat "\$defaults/gui/style.css")" >>"\$MF_TEST_LOG"
fi
INNER
chmod +x target/release/metafolder-sync-config
STUB
cat >"$bin/rustc" <<'STUB'
#!/usr/bin/env bash
printf 'rustc 1.0\nhost: x86_64-test-linux\n'
STUB
chmod +x "$bin/make" "$bin/rustc"

name=metafolder-9.8.7-x86_64-test-linux

# ── 1. the archive ──────────────────────────────────────────────────────────
(cd "$fake" && PATH="$bin:$PATH" bash scripts/dev/release.sh >"$tmp/out" 2>&1); rc=$?
assert_eq "release succeeds" 0 "$rc"
assert_contains "builds every program and the docs" "$(cat "$log")" "make build build-watchd docs"
archive="$fake/dist/$name.tar.gz"
assert "writes the archive named by version and target" [ -f "$archive" ]
assert_contains "says where" "$(cat "$tmp/out")" "$archive"

mkdir "$tmp/x"
tar -C "$tmp/x" -xzf "$archive"
r="$tmp/x/$name"
for b in metafolder-daemon mf metafolder-gui metafolder-watchd metafolder-sync-config; do
    assert "ships $b" [ -x "$r/bin/$b" ]
done
assert "ships the defaults laid out as the configuration" [ -f "$r/share/metafolder/defaults/gui/style.css" ]
assert "ships the watchd unit" [ -f "$r/share/metafolder/watchd/metafolder-watchd.service" ]
assert "ships the watchd user" [ -f "$r/share/metafolder/watchd/metafolder-watchd.sysusers" ]
assert "ships install.sh" [ -x "$r/install.sh" ]
assert_contains "ships a README naming the version" "$(cat "$r/README")" "9.8.7"

# A second run starts again: nothing is left from the first.
touch "$fake/dist/$name/stale"
(cd "$fake" && PATH="$bin:$PATH" bash scripts/dev/release.sh >"$tmp/out" 2>&1); rc=$?
assert_eq "a second release succeeds" 0 "$rc"
assert_not "a second release keeps nothing of the first" \
    sh -c "tar -tzf '$archive' | grep -q stale"

# ── 2. install.sh ───────────────────────────────────────────────────────────
: >"$log"
prefix="$tmp/prefix"
(PATH="$bin:$PATH" MF_TEST_LOG="$log" sh "$r/install.sh" --prefix "$prefix" >"$tmp/out" 2>&1); rc=$?
assert_eq "install succeeds" 0 "$rc"
for b in metafolder-daemon mf metafolder-gui metafolder-sync-config; do
    assert "installs $b" [ -x "$prefix/bin/$b" ]
done
# The broker is root's: under a prefix the user owns, a binary they can
# rewrite would run with CAP_SYS_ADMIN (doc "The fanotify broker").
assert "does not install the broker under the prefix" [ ! -e "$prefix/bin/metafolder-watchd" ]
assert "installs the defaults beside the binaries" [ -f "$prefix/share/metafolder/defaults/gui/style.css" ]
assert_eq "applies the installed defaults, with no argument" "sync-config  body{}" "$(cat "$log")"
assert_contains "says how to have the broker" "$(cat "$tmp/out")" "--watchd"

# An upgrade drops a default the new release no longer ships.
touch "$prefix/share/metafolder/defaults/gone.toml"
(PATH="$bin:$PATH" MF_TEST_LOG="$log" sh "$r/install.sh" --prefix "$prefix" >"$tmp/out" 2>&1)
assert "an upgrade drops a default no longer shipped" [ ! -e "$prefix/share/metafolder/defaults/gone.toml" ]

(sh "$r/install.sh" --bogus >"$tmp/out" 2>&1); rc=$?
assert_eq "an unknown option is a usage error" 2 "$rc"

assert_summary
