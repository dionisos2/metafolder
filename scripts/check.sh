#!/usr/bin/env bash
# Runs every static check the project has, in increasing order of cost, and
# prints one summary line per check.
#
#   scripts/check.sh              # the standard pass
#   scripts/check.sh --lax        # clippy warnings stay warnings (for work in progress)
#   scripts/check.sh --coverage   # also measure test coverage (slow, ~9 GiB in target/)
#
# Checks, and what each is for:
#
#   clippy      lints rustc cannot see. Run with -D warnings: the tree is clean,
#               and the only way it stays clean is if the first new warning
#               fails the build. --lax downgrades them while you work.
#   test        the Rust workspace suite.
#   types       tsc --noEmit + svelte-check over the GUI frontend (the latter
#               is the only thing that reads .svelte at all). Nothing else runs
#               a compiler: vite strips the types without checking them.
#               svelte-check runs --fail-on-warnings: the tree is clean, and an
#               a11y or dead-CSS regression is worth failing on.
#   frontend    the vitest suite + JS coverage. The thresholds are a ratchet at
#               the measured floor; raise them as the panels get tested.
#   deny        dependency vulnerabilities, licenses and sources (deny.toml).
#               Every ignored advisory is justified in that file.
#   semgrep     the project invariants no type checker can express
#               (.semgrep/invariants.yml) — writes go through log::Writer,
#               background tasks hold Weak<RepoState>, decoders run sandboxed,
#               panels query their Shadow root, reconcile never writes
#               mfr_path = Nothing.
#
# A missing optional tool (cargo-deny, semgrep, cargo-llvm-cov) is reported as
# SKIP with its install line, not as a failure — but a skipped check has
# verified nothing, so the summary says so out loud.

set -uo pipefail   # NOT -e: every check must run, even after one fails.

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo"

strict=true
coverage=false
for arg in "$@"; do
    case "$arg" in
        --lax) strict=false ;;
        --coverage) coverage=true ;;
        -h|--help) sed -n '2,31p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
    esac
done

# Terminal colours, but only when writing to one.
if [ -t 1 ]; then
    red=$'\e[31m'; green=$'\e[32m'; yellow=$'\e[33m'; bold=$'\e[1m'; off=$'\e[0m'
else
    red=''; green=''; yellow=''; bold=''; off=''
fi

log=$(mktemp -d)
trap 'rm -rf "$log"' EXIT

failed=()
skipped=()

# run <name> <command...> — runs the command, keeps its output, prints a verdict.
run() {
    local name=$1; shift
    printf '%s┄ %-10s%s' "$bold" "$name" "$off"
    if "$@" >"$log/$name" 2>&1; then
        printf '%s ok%s\n' "$green" "$off"
        return 0
    fi
    printf '%s FAILED%s\n' "$red" "$off"
    failed+=("$name")
    return 1
}

skip() {
    local name=$1 reason=$2
    printf '%s┄ %-10s%s%s skipped%s — %s\n' "$bold" "$name" "$off" "$yellow" "$off" "$reason"
    skipped+=("$name")
}

echo "${bold}metafolder — static checks${off}"
echo

# ── clippy ───────────────────────────────────────────────────────────────────
if $strict; then
    run clippy cargo clippy --workspace --all-targets -- -D warnings
else
    run clippy cargo clippy --workspace --all-targets
    # Surface the warning count even when the check passes: silent drift is how
    # a codebase ends up with hundreds of them.
    # Count the warnings themselves, not cargo's "generated N warnings" recaps.
    warnings=$(grep -E '^warning: ' "$log/clippy" | grep -cvE 'generated [0-9]+ warning' || true)
    if [ "${warnings:-0}" -gt 0 ]; then
        # cargo only re-emits warnings for the crates it recompiled, so this is
        # a floor, not a total. `cargo clean` first for the real number.
        echo "             ${yellow}${warnings} warning(s)${off} in the crates rebuilt just now — list them with: cargo clippy --workspace --all-targets"
    fi
fi

# ── tests ────────────────────────────────────────────────────────────────────
run test cargo test --workspace

# node_modules lives at the repo root: the frontend is an npm workspace member.
if [ -d node_modules ]; then
    run types npm --prefix crates/gui/frontend run typecheck
    run frontend npm --prefix crates/gui/frontend test
else
    skip types "run: npm install"
    skip frontend "run: npm install"
fi

# ── dependency audit ─────────────────────────────────────────────────────────
if cargo deny --version >/dev/null 2>&1; then
    run deny cargo deny check
else
    skip deny "run: cargo install cargo-deny"
fi

# ── project invariants ───────────────────────────────────────────────────────
if command -v semgrep >/dev/null 2>&1; then
    # semgrep exits 0 with findings unless told otherwise.
    run semgrep semgrep scan --error --quiet --metrics=off \
        --config .semgrep/invariants.yml --exclude=target
else
    skip semgrep "run: pipx install semgrep"
fi

# ── coverage (opt-in: slow, and it doubles the size of target/) ───────────────
if $coverage; then
    if cargo llvm-cov --version >/dev/null 2>&1; then
        if run coverage cargo llvm-cov --workspace --summary-only; then
            grep -E '^TOTAL' "$log/coverage" | awk '{printf "             regions %s, lines %s\n", $4, $10}'
        fi
    else
        skip coverage "run: cargo install cargo-llvm-cov"
    fi
fi

# ── summary ──────────────────────────────────────────────────────────────────
echo
if [ ${#failed[@]} -gt 0 ]; then
    echo "${red}${bold}${#failed[@]} check(s) failed: ${failed[*]}${off}"
    for name in "${failed[@]}"; do
        echo
        echo "${bold}── $name ──${off}"
        tail -30 "$log/$name"
    done
    exit 1
fi

if [ ${#skipped[@]} -gt 0 ]; then
    echo "${green}all checks passed${off}, but ${yellow}${#skipped[@]} were skipped (${skipped[*]}) and verified nothing${off}"
    exit 0
fi

echo "${green}${bold}all checks passed${off}"
