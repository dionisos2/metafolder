#!/usr/bin/env bash
# Builds a release archive: dist/metafolder-<version>-<target>.tar.gz, the
# programs and their defaults, installed without a source checkout by the
# install.sh it holds (doc "Building a release").
#
#   scripts/dev/release.sh        # DIST=<dir> to write elsewhere than dist/
#
# Layout of the archive:
#   bin/                        every program, the broker included
#   share/metafolder/defaults/  the shipped defaults, laid out as the
#                               configuration is (metafolder-sync-config --export)
#   share/metafolder/watchd/    the broker's systemd unit and system user
#   install.sh  README
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/core/Cargo.toml | head -n1)
target=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$version" ] || { echo "release: no version in crates/core/Cargo.toml" >&2; exit 1; }
[ -n "$target" ] || { echo "release: rustc names no host target" >&2; exit 1; }

name="metafolder-$version-$target"
dist=${DIST:-$repo/dist}
stage="$dist/$name"
archive="$dist/$name.tar.gz"

# Release builds of every program (the frontend bundled first) and the
# rendered documentation, which the defaults carry.
make build build-watchd docs

rm -rf "$stage" "$archive"
mkdir -p "$stage/bin" "$stage/share/metafolder/watchd"
for b in metafolder-daemon mf metafolder-gui metafolder-sync-config metafolder-watchd; do
    install -m 0755 "target/release/$b" "$stage/bin/$b"
done
target/release/metafolder-sync-config --source "$repo" --export "$stage/share/metafolder/defaults"
install -m 0644 scripts/dev/metafolder-watchd.service scripts/dev/metafolder-watchd.sysusers \
    "$stage/share/metafolder/watchd/"
install -m 0755 scripts/dev/release/install.sh "$stage/install.sh"
sed "s/@VERSION@/$version/g; s/@TARGET@/$target/g" scripts/dev/release/README >"$stage/README"

tar -C "$dist" -czf "$archive" "$name"
echo "release: $archive"
