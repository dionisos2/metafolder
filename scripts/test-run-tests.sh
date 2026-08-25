#!/usr/bin/env bash
# Tests for scripts/run-tests.sh.
#
# run-tests.sh runs the workspace suite and adds the one thing cargo never
# prints: a GLOBAL report over every test binary. The aggregation is exposed as
# a seam — `--stdin` reads a cargo run's output instead of producing one — so
# the tests feed it recorded cargo output and assert the totals, with no
# compilation and no daemon.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/lib/assert.sh"

script="$repo/scripts/run-tests.sh"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/run-tests.XXXXXX" 2>/dev/null) \
    || { mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"; tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/run-tests.XXXXXX"); }
trap 'rm -rf "$tmp"' EXIT

# ── fixtures: recorded cargo test output ─────────────────────────────────────

cat >"$tmp/green" <<'OUT'
   Compiling metafolder-core v0.3.0 (/home/u/metafolder/crates/core)
    Finished `test` profile [unoptimized] target(s) in 12.34s
     Running unittests src/lib.rs (target/debug/deps/metafolder_core-e7ea0dab0094f874)

running 295 tests
test trash::tests::entries_lists_all ... ok

test result: ok. 295 passed; 0 failed; 2 ignored; 0 measured; 3 filtered out; finished in 1.42s

     Running tests/dsl.rs (target/debug/deps/dsl-aabbccdd00112233)

running 1 test
test test_parse_query_is_exposed_by_core ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s

   Doc-tests metafolder_core

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
OUT

cat >"$tmp/red" <<'OUT'
     Running unittests src/lib.rs (target/debug/deps/metafolder_daemon-1122334455667788)

running 3 tests
test db::tests::opens ... ok
test db::tests::writes ... FAILED
test db::tests::reads ... FAILED

failures:
    db::tests::reads
    db::tests::writes

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s

     Running tests/storage.rs (target/debug/deps/storage-99aabbccddeeff00)

running 2 tests
test test_reconcile ... ok
test test_orphans ... FAILED

failures:
    test_orphans

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.80s
OUT

cat >"$tmp/nothing" <<'OUT'
   Compiling metafolder-core v0.3.0 (/home/u/metafolder/crates/core)
error[E0425]: cannot find value `nope` in this scope
error: could not compile `metafolder-core` (lib test) due to 1 previous error
OUT

summarize() { "$script" --stdin <"$1" 2>&1; }

# ── 1. a green run: one global line covering every suite ─────────────────────
out=$(summarize "$tmp/green"); rc=$?
assert_eq  "green run exits 0" 0 "$rc"
assert_contains "green totals the passed tests over all suites" "$out" "296 passed"
assert_contains "green reports 0 failed"                        "$out" "0 failed"
assert_contains "green totals the ignored tests"                "$out" "2 ignored"
assert_contains "green counts the suites (incl. doc-tests)"     "$out" "3 suites"

# The per-suite output must still reach the terminal: this wraps cargo, it does
# not replace it.
assert_contains "input is passed through" "$out" "Running tests/dsl.rs"

# ── 2. a failing run: totals, exit code, and WHICH tests failed ──────────────
out=$(summarize "$tmp/red"); rc=$?
assert_eq  "failing run exits 1" 1 "$rc"
assert_contains "failing run totals the failures"    "$out" "3 failed"
assert_contains "failing run totals the passes"      "$out" "2 passed"
assert_contains "failing run names the failed tests" "$out" "db::tests::writes"
assert_contains "failing run names every failed test" "$out" "test_orphans"
assert_contains "failing run names the suite"        "$out" "storage"

# ── 3. no test ever ran (compile error): say so, do not report success ───────
out=$(summarize "$tmp/nothing"); rc=$?
assert_eq "a run with no test results exits 1" 1 "$rc"
assert_contains "a run with no test results says so" "$out" "no test"

# ── 4. --help works ─────────────────────────────────────────────────────────
help=$("$script" --help 2>&1); rc=$?
assert_eq "--help exits 0" 0 "$rc"
assert_contains "--help mentions the script" "$help" "run-tests"

assert_summary
