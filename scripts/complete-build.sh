#!/usr/bin/env bash
# The complete build: every live artifact universe, the GUI frontend bundle,
# and the user configuration, in the one order that works.
#
# What it does, and why in this order:
#   1. the workspace, the test binaries and the sync-config core — the three
#      artifact "universes" that must all be current before pruning;
#   2. scripts/prune-target.sh, which deletes only what those builds superseded;
#   3. the frontend bundle. Tauri embeds `crates/gui/frontend/dist` into the
#      binary at compile time and `npm run build` is separate from `cargo`, so
#      skipping it leaves the app on a stale bundle with confusing runtime
#      errors (e.g. "unknown metafolder API method: query.expand" after a
#      bridge.ts change);
#   4. metafolder-sync-config, which applies crates/*/default-config/ to the
#      user's config repo at ~/.config/metafolder/.
#
# Takes no arguments.

set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo" || exit 1

# Build the workspace, then the sync-config binary with the feature scoped to
# core only: `--features sync-config` on the whole workspace would recompile
# every crate against a feature-enabled core, duplicating all artifacts in
# target/ (a second "universe" of rlibs cargo never garbage-collects).
cargo build
cargo test --workspace --no-run
cargo build -p metafolder-core --features sync-config
# Prune superseded artifacts now: every live "universe" (plain workspace,
# test binaries + dev-deps, sync-config core) was just (re)built, so
# everything current is in the fresh generation and only what they superseded
# gets deleted — including the stale 100-250 MB test executables. Never prune
# between two builds of different feature sets — each would look "new" and
# evict the other, forcing perpetual recompilation.
scripts/prune-target.sh

# First run on a fresh checkout: install the frontend deps once. node_modules
# lives at the repo root — the frontend is an npm workspace member — so that is
# where its presence is read (same test as scripts/check.sh).
if [ ! -d node_modules ]; then
    echo "==> Installing frontend dependencies (first run)…"
    npm --prefix crates/gui/frontend install
fi

echo "==> Building the GUI frontend bundle…"
npm --prefix crates/gui/frontend run build

# Apply the shipped defaults to ~/.config/metafolder/. The path is relative to
# the repo root, which is where this script has been since the `cd` above — it
# read `../target` for a while, which does not exist, so every run did the whole
# build and then died here without ever syncing the config.
echo "==> Applying the user configuration…"
target/debug/metafolder-sync-config
