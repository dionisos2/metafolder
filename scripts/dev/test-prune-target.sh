#!/usr/bin/env bash
# Tests for scripts/dev/prune-target.sh against a synthetic target/debug. What it
# must never do is delete a live artifact: that costs a recompile, or, on a bad
# list, every recompile at once.

set -uo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# shellcheck source=lib/assert.sh
. "$repo/scripts/dev/lib/assert.sh"
prune="$repo/scripts/dev/prune-target.sh"

mkdir -p "${TMPDIR:-/tmp}/metafolder-tests"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/metafolder-tests/prune.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

live1=aaaaaaaaaaaaaaaa   # toml_edit, for the target
live2=bbbbbbbbbbbbbbbb   # toml_edit, a second LIVE generation: for build scripts
dead=cccccccccccccccc    # toml_edit, gone
test1=dddddddddddddddd   # a test binary
script=eeeeeeeeeeeeeeee  # a build script's compile
run=ffffffffffffffff     # the same build script's run
debug="$tmp/target/debug"
list="$tmp/live.json"

make_target() {
    rm -rf "$tmp/target"
    mkdir -p "$debug/deps" "$debug/.fingerprint" "$debug/build" "$debug/incremental"
    local h f
    for h in "$live1" "$live2" "$dead"; do
        for f in "libtoml_edit-$h.rlib" "libtoml_edit-$h.rmeta" "toml_edit-$h.d"; do
            echo x >"$debug/deps/$f"
        done
        mkdir "$debug/.fingerprint/toml_edit-$h"
    done
    echo x >"$debug/deps/bulk_trash-$test1"
    mkdir "$debug/.fingerprint/bulk_trash-$test1"
    mkdir "$debug/build/foo-$script" "$debug/build/foo-$run" \
          "$debug/.fingerprint/foo-$script" "$debug/.fingerprint/foo-$run"
    echo x >"$debug/mf"                 # no hash: not ours to judge
    echo x >"$debug/deps/no_hash.d"

    cat >"$list" <<JSON
{"reason":"compiler-artifact","fresh":true,"filenames":["$debug/deps/libtoml_edit-$live1.rlib","$debug/deps/libtoml_edit-$live1.rmeta"]}
{"reason":"compiler-artifact","fresh":true,"filenames":["$debug/deps/libtoml_edit-$live2.rlib"]}
{"reason":"compiler-artifact","fresh":true,"filenames":["$debug/deps/bulk_trash-$test1"]}
{"reason":"compiler-artifact","fresh":true,"filenames":["$debug/build/foo-$script/build-script-build"]}
{"reason":"build-script-executed","out_dir":"$debug/build/foo-$run/out"}
{"reason":"build-finished","success":true}
JSON
}
here() { [ -e "$debug/$1" ]; }

# ── 1. what cargo reports stays, whatever else shares its name ──────────────
make_target
"$prune" "$debug" "$list" >"$tmp/out"; rc=$?
assert_eq "exits 0" 0 "$rc"
assert "keeps a live generation" here "deps/libtoml_edit-$live1.rlib"
assert "keeps its .d file, which cargo does not list" here "deps/toml_edit-$live1.d"
assert "keeps its fingerprint" here ".fingerprint/toml_edit-$live1"
# The bug this script was rewritten for: several generations of one name are
# normal, and treating a new one as a replacement made builds evict each other.
assert "keeps a second live generation of the same name" here "deps/libtoml_edit-$live2.rlib"
assert "keeps a test binary" here "deps/bulk_trash-$test1"
assert "keeps a build script's compile" here "build/foo-$script"
assert "keeps a build script's run" here "build/foo-$run"
assert "keeps what has no hash" here "mf"
assert "keeps a dependency file without a hash" here "deps/no_hash.d"

# ── 1b. a binary is listed under its uplifted name, without a hash ──────────
# Deleting its original made every `make dev` link every binary again. The build
# and the test build each make one, so the name can have two live originals.
make_target
good=1234567890abcdef; good2=2234567890abcdef; stale=3234567890abcdef
for h in "$good" "$good2" "$stale"; do
    echo x >"$debug/deps/my_bin-$h"
    echo x >"$debug/deps/my_bin-$h.d"
    mkdir "$debug/.fingerprint/my-bin-$h"
done
touch -d '3 days ago' "$debug/deps/my_bin-$stale"
cat >>"$list" <<JSON
{"reason":"compiler-artifact","fresh":true,"executable":"$debug/my-bin","filenames":["$debug/my-bin"]}
{"reason":"compiler-artifact","fresh":true,"executable":"$debug/my-bin","filenames":["$debug/my-bin"]}
JSON
"$prune" "$debug" "$list" >/dev/null
assert "keeps the original of an uplifted binary" here "deps/my_bin-$good"
assert "and its .d" here "deps/my_bin-$good.d"
assert "and its fingerprint" here ".fingerprint/my-bin-$good"
assert "keeps the second one reported" here "deps/my_bin-$good2"
assert_not "not an older one" here "deps/my_bin-$stale"

# A test build of the same binary is listed by hash, and newer: it must not take
# the place of the one reported without a hash.
make_target
echo x >"$debug/deps/my_bin-$good"
echo x >"$debug/deps/my_bin-$stale"
touch -d '1 day ago' "$debug/deps/my_bin-$good"
touch -d '3 days ago' "$debug/deps/my_bin-$stale"
echo x >"$debug/deps/my_bin-$good2"
cat >>"$list" <<JSON
{"reason":"compiler-artifact","fresh":true,"executable":"$debug/deps/my_bin-$good2","filenames":["$debug/deps/my_bin-$good2"]}
{"reason":"compiler-artifact","fresh":true,"executable":"$debug/my-bin","filenames":["$debug/my-bin"]}
JSON
"$prune" "$debug" "$list" >/dev/null
assert "the test build stays" here "deps/my_bin-$good2"
assert "and so does the uplifted one's original" here "deps/my_bin-$good"
assert_not "an older one still goes" here "deps/my_bin-$stale"

make_target
"$prune" "$debug" "$list" >/dev/null

# ── 2. the rest goes ─────────────────────────────────────────────────────────
assert_not "removes a dead rlib" here "deps/libtoml_edit-$dead.rlib"
assert_not "removes a dead rmeta" here "deps/libtoml_edit-$dead.rmeta"
assert_not "removes a dead .d" here "deps/toml_edit-$dead.d"
assert_not "removes a dead fingerprint" here ".fingerprint/toml_edit-$dead"
assert_contains "reports what it did" "$(cat "$tmp/out")" "pruned"

"$prune" "$debug" "$list" >"$tmp/out"
assert_contains "a second run has nothing to do" "$(cat "$tmp/out")" "nothing to prune"

# ── 3. --dry-run reports and deletes nothing ────────────────────────────────
make_target
"$prune" --dry-run "$debug" "$list" >"$tmp/out"
assert "dry run keeps the dead" here "deps/libtoml_edit-$dead.rlib"
assert_contains "dry run names it" "$(cat "$tmp/out")" "libtoml_edit-$dead.rlib"

# ── 4. a bad list deletes nothing ───────────────────────────────────────────
# An empty list would read as "nothing is live": every artifact gone.
make_target
: >"$tmp/empty"
"$prune" "$debug" "$tmp/empty" >/dev/null 2>&1; rc=$?
assert "an empty list is an error" [ "$rc" -ne 0 ]
assert "and deletes nothing" here "deps/libtoml_edit-$live1.rlib"
"$prune" "$debug" "$tmp/absent" >/dev/null 2>&1; rc=$?
assert "a missing list is an error" [ "$rc" -ne 0 ]
# A failed build reports only part of what is live.
echo '{"reason":"build-finished","success":false}' >>"$list"
"$prune" "$debug" "$list" >/dev/null 2>&1; rc=$?
assert "a failed build's list is an error" [ "$rc" -ne 0 ]
assert "and deletes nothing either" here "deps/libtoml_edit-$dead.rlib"

# ── 5. incremental caches: one per live generation, the most recent ─────────
make_target
inc="$debug/incremental"
mkdir -p "$inc/toml_edit-old" "$inc/toml_edit-mid" "$inc/toml_edit-new" \
         "$inc/gone_crate-x" "$inc/bulk_trash-y"
touch -d '3 days ago' "$inc/toml_edit-old"
touch -d '2 days ago' "$inc/toml_edit-mid"
"$prune" "$debug" "$list" >/dev/null
assert "keeps the newest cache" here "incremental/toml_edit-new"
assert "and one per live generation" here "incremental/toml_edit-mid"
assert_not "not the older ones" here "incremental/toml_edit-old"
assert_not "a crate with no live generation loses its cache" here "incremental/gone_crate-x"
assert "a test binary keeps its cache" here "incremental/bulk_trash-y"

# ── 6. inside a cache: only the newest finalized session ────────────────────
make_target
c="$inc/bulk_trash-y"
mkdir -p "$c/s-aaa-111-oldsvh" "$c/s-bbb-222-newsvh" "$c/s-ccc-333-working"
touch "$c/s-aaa-111.lock" "$c/s-bbb-222.lock" "$c/s-ccc-333.lock"
"$prune" "$debug" "$list" >/dev/null
assert "keeps the newest session" here "incremental/bulk_trash-y/s-bbb-222-newsvh"
assert "and its lock" here "incremental/bulk_trash-y/s-bbb-222.lock"
assert_not "removes an older session" here "incremental/bulk_trash-y/s-aaa-111-oldsvh"
assert_not "and its lock" here "incremental/bulk_trash-y/s-aaa-111.lock"
assert "leaves a build in progress" here "incremental/bulk_trash-y/s-ccc-333-working"

assert_summary
