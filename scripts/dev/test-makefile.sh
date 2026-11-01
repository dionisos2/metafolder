#!/usr/bin/env bash
# Tests for the Makefile, the one entry point for building and installing.
#
# Compiling anything here would defeat the purpose, so the real Makefile runs in
# a fake checkout against stubs: a PATH shim for cargo and npm, and stub
# repo-local programs. What is asserted is the ORDER and the PATHS.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/dev/lib/assert.sh"

mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/makefile.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

fake="$tmp/repo"
bin="$tmp/bin"
log="$tmp/log"
mkdir -p "$fake/scripts/dev" "$fake/target/debug" "$fake/target/release" "$bin"
cp "$repo/Makefile" "$fake/"
cp "$repo/scripts/dev/check-deps.sh" "$fake/scripts/dev/"
touch "$fake/package.json" "$fake/package-lock.json"

# `npm ci` leaves what a real one leaves: the hidden lockfile make reads.
cat >"$bin/npm" <<STUB
#!/usr/bin/env bash
printf 'npm %s\n' "\$*" >>"$log"
if [ "\$1" = ci ]; then mkdir -p node_modules; touch node_modules/.package-lock.json; fi
STUB
cat >"$bin/cargo" <<STUB
#!/usr/bin/env bash
printf 'cargo %s\n' "\$*" >>"$log"
STUB
chmod +x "$bin/npm" "$bin/cargo"

cat >"$fake/scripts/dev/doc" <<STUB
#!/usr/bin/env bash
printf 'doc %s\n' "\$*" >>"$log"
STUB
chmod +x "$fake/scripts/dev/doc"

cat >"$fake/scripts/dev/prune-target.sh" <<STUB
#!/usr/bin/env bash
printf 'prune-target\n' >>"$log"
STUB
chmod +x "$fake/scripts/dev/prune-target.sh"

for profile in debug release; do
    cat >"$fake/target/$profile/metafolder-sync-config" <<STUB
#!/usr/bin/env bash
printf 'sync-config $profile\n' >>"$log"
STUB
    chmod +x "$fake/target/$profile/metafolder-sync-config"
    touch "$fake/target/$profile/metafolder-daemon" "$fake/target/$profile/mf" \
          "$fake/target/$profile/metafolder-gui"
done

# Forces every dependency present but those it lists (check-deps' testing seam).
stub="$tmp/deps"
: >"$stub"

run() {
    : >"$log"
    (cd "$fake" && PATH="$bin:$PATH" MF_CHECK_STUB="$stub" make --no-print-directory \
        PREFIX="$tmp/prefix" "$@" >"$tmp/out" 2>&1)
}
line_of() { grep -n -- "$1" "$log" | head -n1 | cut -d: -f1; }
assert_before() {
    local a b
    a=$(line_of "$2"); b=$(line_of "$3")
    assert "$1" [ "${a:-999999}" -lt "${b:-0}" ]
}

# ── 1. a fresh checkout: the npm deps go in at the workspace root ───────────
# `npm --prefix crates/gui/frontend install` ignored the root workspace, so
# `make docs` died on "Cannot find module 'tiddlywiki'".
run docs; rc=$?
assert_eq "docs succeeds on a fresh checkout" 0 "$rc"
assert_contains "installs from the root lockfile" "$(cat "$log")" "npm ci"
assert_before "installs the deps before rendering" "npm ci" "doc build"

run docs
assert "with the deps current it does not reinstall" [ "$(grep -c 'npm ci' "$log")" -eq 0 ]

# A pull that moves the lockfile moves the dependencies.
sleep 0.01; touch "$fake/package-lock.json"
run docs
assert_contains "a newer lockfile reinstalls" "$(cat "$log")" "npm ci"

# ── 2. make dev: the complete development build ─────────────────────────────
run dev; rc=$?
assert_eq "dev succeeds" 0 "$rc"
# Tauri embeds the bundle when cargo compiles the GUI: bundled after, the binary
# carries the previous one until the next run.
assert_before "bundles the frontend before cargo embeds it" "run build" "^cargo build$"
assert_contains "builds the test binaries" "$(cat "$log")" "cargo test --workspace --no-run"
assert_contains "scopes the feature to core" "$(cat "$log")" \
    "cargo build -p metafolder-core --features sync-config"
assert_before "renders the documentation before the sync" "doc build" "sync-config debug"
assert_before "builds the sync binary before running it" \
    "features sync-config" "sync-config debug"
# Pruning between two builds of different feature sets makes each look new and
# evict the other: all three go first, doc "Keeping the target directory small".
assert_before "builds the workspace before pruning" "^cargo build$" "prune-target"
assert_before "builds the test binaries before pruning" "test --workspace --no-run" "prune-target"
assert_before "builds the sync-config core before pruning" "features sync-config" "prune-target"
assert_eq "the sync is the last step" "sync-config debug" "$(tail -n1 "$log")"

# ── 3. install-headless: no GUI toolkit, and it says so ─────────────────────
printf 'webkit2gtk=0\ngtk3=0\nlibrsvg=0\nbwrap=0\n' >"$stub"
run install-headless; rc=$?
assert_eq "install-headless works without the GUI toolkit" 0 "$rc"
assert "installs the daemon" [ -x "$tmp/prefix/bin/metafolder-daemon" ]
assert "installs mf" [ -x "$tmp/prefix/bin/mf" ]
assert "does not install the GUI" [ ! -e "$tmp/prefix/bin/metafolder-gui" ]
assert_not "does not build the GUI" grep -q 'metafolder-gui' "$log"
assert_not "a partial build does not prune" grep -q 'prune-target' "$log"

run install; rc=$?
assert "install still requires the GUI toolkit" [ "$rc" -ne 0 ]
: >"$stub"

# The wiki needs node whichever way it is installed: sync-config refuses a
# checkout without the rendered documentation.
printf 'node=0\n' >"$stub"
run install-headless; rc=$?
assert "install-headless still requires node" [ "$rc" -ne 0 ]
: >"$stub"

assert_summary
