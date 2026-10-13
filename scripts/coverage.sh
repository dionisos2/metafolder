#!/usr/bin/env bash
# Line coverage of the Rust workspace, the GUI end-to-end run included
# (doc "Measuring coverage").
#
# `cargo llvm-cov` alone counts what `cargo test` reaches. The Tauri wrappers
# (`#[tauri::command]`, gui/src/lib.rs) are reached only by a running WebView,
# which is what scripts/gui-e2e.sh drives. This builds the binaries that run
# under the same instrumentation as the tests, points gui-e2e.sh at them, and
# merges every profile into one report.
#
# Usage:  scripts/coverage.sh [--no-gui] [--unsandboxed] [--html]
#   --no-gui       the tests alone (what `scripts/check.sh --coverage` counts)
#   --unsandboxed  passed to gui-e2e.sh (a container forbidding WebKit's sandbox)
#   --html         an HTML report under target/llvm-cov-target/llvm-cov/html
#                  instead of the summary table
#
# gui-e2e.sh runs the GUI on a private Xvfb when there is one; with neither
# Xvfb nor a display, the end-to-end run is a loud SKIP, not a failure. A failing
# test or scenario fails the run, but the report is written anyway: the
# coverage of a run with one red test is still worth reading.
#
# The instrumented artifacts live in target/llvm-cov-target (the directory
# `cargo llvm-cov` uses), so target/debug is left as it was. Needs
# cargo-llvm-cov; gui-e2e.sh's own prerequisites (the frontend built, the
# configuration installed) apply.

set -uo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)" || exit 2

gui=true
html=false
e2e_args=()
for arg in "$@"; do
    case "$arg" in
        --no-gui) gui=false ;;
        --unsandboxed) e2e_args+=(--unsandboxed) ;;
        --html) html=true ;;
        -h | --help) sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
        *) echo "error: unknown option $arg (try --help)" >&2; exit 2 ;;
    esac
done

export CARGO_TARGET_DIR="$PWD/target/llvm-cov-target"
env_script=$(cargo llvm-cov show-env --sh) || { echo "error: cargo llvm-cov is missing" >&2; exit 2; }
eval "$env_script"

status=0

# Stale profiles of an earlier run would be merged into this one's report.
cargo llvm-cov clean --workspace || exit 2

# --no-fail-fast: one failing test binary must not leave the ones after it
# unrun, and their code uncounted.
cargo test --workspace --no-fail-fast || status=1

if ! $gui; then
    :
elif ! command -v Xvfb >/dev/null && [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    echo "SKIP gui-e2e: no Xvfb and no display (\$DISPLAY / \$WAYLAND_DISPLAY) — the Tauri wrappers are not counted"
else
    if cargo build -p metafolder-daemon -p metafolder-cli -p metafolder-gui; then
        METAFOLDER_BIN_DIR="$CARGO_TARGET_DIR/debug" scripts/gui-e2e.sh "${e2e_args[@]}" \
            || status=1
    else
        status=1
    fi
fi

if $html; then
    cargo llvm-cov report --html || status=1
else
    cargo llvm-cov report --summary-only || status=1
fi

exit "$status"
