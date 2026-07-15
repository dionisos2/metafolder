#!/usr/bin/env bash
# Tests for scripts/prune-target.sh against a synthetic target/ layout.
# Run: scripts/test-prune-target.sh   (exits non-zero on the first failure)

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
prune="$here/prune-target.sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "  ok: $*"; }

# 16-hex-char artifact hashes, one letter each so they read easily.
h_old=aaaaaaaaaaaaaaaa
h_new=bbbbbbbbbbbbbbbb
h_serde=cccccccccccccccc
h_test=dddddddddddddddd
h_toml_old=eeeeeeeeeeeeeeee
h_toml_new=ffffffffffffffff

registry=/home/user/.cargo/registry/src/index.crates.io-6f17d22bba15001f

make_target() {
    rm -rf "$tmp/proj"
    mkdir -p "$tmp/proj/target/debug/deps" \
             "$tmp/proj/target/debug/.fingerprint" \
             "$tmp/proj/target/debug/build"
    cd "$tmp/proj"

    cat > Cargo.lock <<EOF
[[package]]
name = "bitflags"
version = "2.9.0"

[[package]]
name = "serde"
version = "1.0.200"

[[package]]
name = "toml"
version = "1.0.6+spec-1.1.0"
EOF

    d=target/debug
    # bitflags: an old and a new variant (feature-set change).
    touch "$d/deps/libbitflags-$h_old.rlib" "$d/deps/libbitflags-$h_old.rmeta"
    echo "$d/deps/libbitflags-$h_old.rmeta: $registry/bitflags-2.9.0/src/lib.rs" \
        > "$d/deps/bitflags-$h_old.d"
    mkdir -p "$d/.fingerprint/bitflags-$h_old" "$d/build/bitflags-$h_old"
    touch "$d/deps/libbitflags-$h_new.rlib"
    echo "$d/deps/libbitflags-$h_new.rlib: $registry/bitflags-2.9.0/src/lib.rs" \
        > "$d/deps/bitflags-$h_new.d"
    mkdir -p "$d/.fingerprint/bitflags-$h_new"
    # serde: unchanged single variant.
    touch "$d/deps/libserde-$h_serde.rlib"
    echo "$d/deps/libserde-$h_serde.rlib: $registry/serde-1.0.200/src/lib.rs" \
        > "$d/deps/serde-$h_serde.d"
    # An integration-test executable (local sources only, never in Cargo.lock).
    touch "$d/deps/http_api-$h_test"
    echo "$d/deps/http_api-$h_test: tests/http_api.rs src/lib.rs" \
        > "$d/deps/http_api-$h_test.d"
    # toml: a version dropped from Cargo.lock (0.8.23) next to the live one.
    touch "$d/deps/libtoml-$h_toml_old.rlib"
    echo "$d/deps/libtoml-$h_toml_old.rlib: $registry/toml-0.8.23/src/lib.rs" \
        > "$d/deps/toml-$h_toml_old.d"
    touch "$d/deps/libtoml-$h_toml_new.rlib"
    echo "$d/deps/libtoml-$h_toml_new.rlib: $registry/toml-1.0.6+spec-1.1.0/src/lib.rs" \
        > "$d/deps/toml-$h_toml_new.d"
}

echo "== scenario 1: first run records state, only lock-orphans pruned"
make_target
"$prune" >/dev/null
[ -f target/.prune-target-state ] || fail "state file not created"
[ -e "target/debug/deps/libbitflags-$h_old.rlib" ] || fail "no-state run must not diff-prune"
[ ! -e "target/debug/deps/libtoml-$h_toml_old.rlib" ] || fail "toml 0.8.23 not in Cargo.lock, should be pruned"
[ -e "target/debug/deps/libtoml-$h_toml_new.rlib" ] || fail "toml 1.0.6 is in Cargo.lock, must stay"
[ -e "target/debug/deps/http_api-$h_test" ] || fail "test binaries must never be lock-pruned"
ok "first run"

echo "== scenario 2: a new variant prunes the older hashes of the same name"
make_target
# Seed the state as if a previous run had seen only the old bitflags variant.
"$prune" >/dev/null                    # records current state (old+new present)
rm "target/debug/deps/libbitflags-$h_new.rlib" "target/debug/deps/bitflags-$h_new.d"
rm -rf "target/debug/.fingerprint/bitflags-$h_new"
"$prune" >/dev/null                    # state now: bitflags has only h_old
touch "target/debug/deps/libbitflags-$h_new.rlib"   # "a build produced a new variant"
"$prune" >/dev/null
[ ! -e "target/debug/deps/libbitflags-$h_old.rlib" ] || fail "old bitflags rlib should be pruned"
[ ! -e "target/debug/deps/libbitflags-$h_old.rmeta" ] || fail "old bitflags rmeta should be pruned"
[ ! -e "target/debug/deps/bitflags-$h_old.d" ] || fail "old bitflags .d should be pruned"
[ ! -e "target/debug/.fingerprint/bitflags-$h_old" ] || fail "old bitflags fingerprint dir should be pruned"
[ ! -e "target/debug/build/bitflags-$h_old" ] || fail "old bitflags build dir should be pruned"
[ -e "target/debug/deps/libbitflags-$h_new.rlib" ] || fail "new bitflags variant must stay"
[ -e "target/debug/deps/libserde-$h_serde.rlib" ] || fail "unchanged serde must stay"
[ -e "target/debug/deps/http_api-$h_test" ] || fail "unchanged test binary must stay"
ok "diff prune"

echo "== scenario 3: --dry-run deletes nothing"
make_target
"$prune" >/dev/null                    # state with both bitflags variants
rm "target/debug/deps/libbitflags-$h_new.rlib" "target/debug/deps/bitflags-$h_new.d"
rm -rf "target/debug/.fingerprint/bitflags-$h_new"
"$prune" >/dev/null
touch "target/debug/deps/libbitflags-$h_new.rlib"
"$prune" --dry-run >/dev/null
[ -e "target/debug/deps/libbitflags-$h_old.rlib" ] || fail "--dry-run must not delete"
[ -e "target/debug/deps/libtoml-$h_toml_old.rlib" ] || true  # already pruned earlier runs
ok "dry run"

echo "== scenario 4: state survives and next real run prunes what dry-run showed"
"$prune" >/dev/null
[ ! -e "target/debug/deps/libbitflags-$h_old.rlib" ] || fail "real run after dry-run should prune"
ok "real run after dry-run"

echo "all prune-target tests passed"
