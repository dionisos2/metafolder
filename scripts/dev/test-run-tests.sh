#!/usr/bin/env bash
# Tests for scripts/dev/run-tests.sh.
#
# run-tests.sh runs the workspace suite and adds the one thing cargo never
# prints: a GLOBAL report over every test binary. The aggregation is exposed as
# a seam — `--stdin` reads a cargo run's output instead of producing one — so
# the tests feed it recorded cargo output and assert the totals, with no
# compilation and no daemon.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/dev/lib/assert.sh"

script="$repo/scripts/dev/run-tests.sh"

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

# ── 4. the real (non---stdin) path reports the aggregator's verdict ─────────
# The `--stdin` seam above exercises the aggregator alone. This exercises the
# branch that actually runs, through the CARGO seam: cargo succeeds while
# printing no suite report at all — a filter that matched nothing, a `--list`.
# The aggregator's "no test ran" must survive to the exit code.
#
# It did not: the status was read with `$?` *after* an assignment from
# PIPESTATUS, which is the status of the assignment — always 0. Every such run
# reported success, which is the one thing this script promises never to do.
cat >"$tmp/fake-cargo-quiet" <<'FAKE'
#!/usr/bin/env bash
echo "   Compiling metafolder-core v0.3.0 (/home/u/metafolder/crates/core)"
exit 0
FAKE
chmod +x "$tmp/fake-cargo-quiet"
out=$(CARGO="$tmp/fake-cargo-quiet" "$script" 2>&1); rc=$?
assert_eq "a real run with no test results exits 1" 1 "$rc"
assert_contains "a real run with no test results says so" "$out" "no test"

# And cargo's own failure still wins over the aggregate's.
cat >"$tmp/fake-cargo-broken" <<'FAKE'
#!/usr/bin/env bash
echo "error[E0433]: failed to resolve" >&2
exit 101
FAKE
chmod +x "$tmp/fake-cargo-broken"
out=$(CARGO="$tmp/fake-cargo-broken" "$script" 2>&1); rc=$?
assert_eq "cargo's own status wins" 101 "$rc"

# A clean run reports success, so the check above is not just "always 1".
cat >"$tmp/fake-cargo-green" <<FAKE
#!/usr/bin/env bash
cat "$tmp/green"
exit 0
FAKE
chmod +x "$tmp/fake-cargo-green"
out=$(CARGO="$tmp/fake-cargo-green" "$script" 2>&1); rc=$?
assert_eq "a real green run exits 0" 0 "$rc"
assert_contains "a real green run totals the passes" "$out" "296 passed"

# ── 4b. timing tests run in a pass of their own ─────────────────────────────
# Tests that depend on the clock are #[ignore]d and run second, alone and in
# series, so the rest of the suite cannot load the machine under them.
cat >"$tmp/fake-cargo-args" <<FAKE
#!/usr/bin/env bash
echo "ARGS: \$*" >>"$tmp/calls"
cat "$tmp/green"
exit 0
FAKE
chmod +x "$tmp/fake-cargo-args"
: >"$tmp/calls"
out=$(CARGO="$tmp/fake-cargo-args" "$script" 2>&1); rc=$?
assert_eq "two passes exit 0 when both are green" 0 "$rc"
assert_eq "cargo is run twice" 2 "$(wc -l <"$tmp/calls")"
assert_contains "the second pass runs the ignored tests" "$(sed -n 2p "$tmp/calls")" "--ignored"
assert_contains "the second pass is serial" "$(sed -n 2p "$tmp/calls")" "--test-threads=1"
assert_not "the first pass leaves them out" grep -q -e --ignored <<<"$(sed -n 1p "$tmp/calls")"
assert_contains "the timing pass is announced" "$out" "timing tests"

: >"$tmp/calls"
CARGO="$tmp/fake-cargo-args" "$script" --no-timing >/dev/null 2>&1
assert_eq "--no-timing runs cargo once" 1 "$(wc -l <"$tmp/calls")"

# A failure of the timing pass fails the run, and says it was the timing pass.
cat >"$tmp/fake-cargo-timing-red" <<FAKE
#!/usr/bin/env bash
case "\$*" in
    *--ignored*) cat "$tmp/red" ;;
    *) cat "$tmp/green" ;;
esac
exit 0
FAKE
chmod +x "$tmp/fake-cargo-timing-red"
out=$(CARGO="$tmp/fake-cargo-timing-red" "$script" 2>&1); rc=$?
assert_eq "a red timing pass fails the run" 1 "$rc"
assert_contains "the failing pass is the timing one" "$out" "timing tests"

# ── 5. --help works ─────────────────────────────────────────────────────────
help=$("$script" --help 2>&1); rc=$?
assert_eq "--help exits 0" 0 "$rc"
assert_contains "--help mentions the script" "$help" "run-tests"

assert_summary
