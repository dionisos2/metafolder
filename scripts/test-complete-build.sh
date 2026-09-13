#!/usr/bin/env bash
# Tests for scripts/complete-build.sh.
#
# complete-build.sh is the one entry point CLAUDE.md points at for a full build,
# and it had no test at all — which is exactly where its last line rotted into a
# path that does not exist, so every run did ten minutes of work and then died
# without ever applying the user configuration.
#
# Compiling anything here would defeat the purpose, so the script runs against
# stubs: a PATH shim for `cargo` and `npm`, a fake repository whose `git` answers
# with that directory, and stub target/ and scripts/ entries. What is asserted is
# the ORDER and the PATHS, which is all this script is.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/lib/assert.sh"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/complete-build.XXXXXX" 2>/dev/null) \
    || { mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
         tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/complete-build.XXXXXX"); }
trap 'rm -rf "$tmp"' EXIT

fake="$tmp/repo"
bin="$tmp/bin"
log="$tmp/log"
mkdir -p "$fake/scripts" "$fake/crates/gui/frontend" "$fake/target/debug" "$bin"

cp "$repo/scripts/complete-build.sh" "$fake/scripts/"

# Every external the script calls, logging its invocation and doing nothing.
for tool in cargo npm git; do
    cat >"$bin/$tool" <<STUB
#!/usr/bin/env bash
printf '%s %s\n' "$tool" "\$*" >>"$log"
case "$tool" in
    git) printf '%s\n' "$fake" ;;   # rev-parse --show-toplevel
esac
exit 0
STUB
    chmod +x "$bin/$tool"
done

# prune-target.sh and the sync-config binary are the two repo-local programs it
# runs; both are stubbed where the real script expects to find them.
cat >"$fake/scripts/prune-target.sh" <<STUB
#!/usr/bin/env bash
printf 'prune-target\n' >>"$log"
STUB
chmod +x "$fake/scripts/prune-target.sh"

cat >"$fake/target/debug/metafolder-sync-config" <<STUB
#!/usr/bin/env bash
printf 'sync-config\n' >>"$log"
STUB
chmod +x "$fake/target/debug/metafolder-sync-config"

run() {
    : >"$log"
    (cd "$fake" && PATH="$bin:$PATH" bash scripts/complete-build.sh >/dev/null 2>&1)
}

# ── 1. it runs to the end, and the end is the config sync ───────────────────
mkdir -p "$fake/node_modules"          # deps already installed
run; rc=$?
assert_eq "exits 0" 0 "$rc"
assert_contains "applies the user configuration" "$(cat "$log")" "sync-config"

# The sync-config binary is found at target/debug, relative to the repo root the
# script cd'd into. Read as ../target it was outside the repo and never existed.
assert "the sync-config binary is the repo's own" \
    [ "$(grep -c '^sync-config$' "$log")" -eq 1 ]

# ── 2. the order is the one the comments promise ─────────────────────────────
order=$(cat "$log")
assert "builds the workspace before pruning" \
    [ "$(printf '%s\n' "$order" | grep -n '^cargo build$' | head -n1 | cut -d: -f1)" \
      -lt "$(printf '%s\n' "$order" | grep -n '^prune-target$' | cut -d: -f1)" ]
assert "builds the test binaries before pruning" \
    [ "$(printf '%s\n' "$order" | grep -n 'test --workspace --no-run' | cut -d: -f1)" \
      -lt "$(printf '%s\n' "$order" | grep -n '^prune-target$' | cut -d: -f1)" ]
assert "builds the sync-config core before pruning" \
    [ "$(printf '%s\n' "$order" | grep -n 'features sync-config' | cut -d: -f1)" \
      -lt "$(printf '%s\n' "$order" | grep -n '^prune-target$' | cut -d: -f1)" ]
assert "bundles the frontend before applying the config" \
    [ "$(printf '%s\n' "$order" | grep -n 'run build' | cut -d: -f1)" \
      -lt "$(printf '%s\n' "$order" | grep -n '^sync-config$' | cut -d: -f1)" ]

# ── 3. node_modules is read at the repo root (the npm workspace member) ─────
rm -rf "$fake/node_modules"
run
assert_contains "a fresh checkout installs the frontend deps" "$(cat "$log")" "npm --prefix crates/gui/frontend install"

mkdir -p "$fake/node_modules"
run
assert "with node_modules present it does not reinstall" \
    [ "$(grep -c 'frontend install' "$log")" -eq 0 ]

assert_summary
