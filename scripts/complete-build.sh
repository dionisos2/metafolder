#!/usr/bin/env bash
# Rebuilds the GUI frontend bundle, then runs the GUI.
#
# Tauri embeds `crates/gui/frontend/dist` into the binary at compile time, so
# the frontend must be (re)built BEFORE cargo — `npm run build` is separate from
# `cargo`. Skipping it leaves the app on a stale bundle and produces confusing
# runtime errors (e.g. "unknown metafolder API method: query.expand" after a
# bridge.ts change). This script does both in the right order.
#
# Any arguments are forwarded to mf-gui, e.g.:
#   scripts/run-gui.sh --gui-port 7524 --daemon-url http://127.0.0.1:7523

set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo"

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

# First run on a fresh checkout: install the frontend deps once.
if [ ! -d crates/gui/frontend/node_modules ]; then
    echo "==> Installing frontend dependencies (first run)…"
    npm --prefix crates/gui/frontend install
fi

echo "==> Building the GUI frontend bundle…"
npm --prefix crates/gui/frontend run build

metafolder-sync-config
