#!/bin/sh
# Build fio-ext4's test image context for the commit checked out.
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, and stages static binaries in test/.stage/;
# test/Containerfile (context: the repo root) packages them.
#
# mkfs-ext4 is built from this crate's own dependency graph, so it is exactly
# the version Cargo.lock pins — the formatter fio-ext4 is tested against.
# With STAGE_ONLY=1 it stops after staging and prints the stage path;
# otherwise it also runs `podman build` and tags fio-ext4-test.
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)
commit=$(git -C "$root" rev-parse HEAD)
manifest="$root/Cargo.toml"

cargo build --release --locked --target "$target" --manifest-path "$manifest" --bin fio-ext4
cargo build --release --locked --target "$target" --manifest-path "$manifest" \
    -p mkfs-ext4 --features mkfs-ext4/cli --bin mkfs-ext4
tdir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" |
    sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"
cp "$tdir/$target/release/fio-ext4" "$tdir/$target/release/mkfs-ext4" "$stage/"
cp "$root/test/test.sh" "$stage/test"
chmod 755 "$stage/test"

if [ "${STAGE_ONLY:-0}" = 1 ]; then
    echo "$stage"
    exit 0
fi
podman build -f "$root/test/Containerfile" --build-arg COMMIT="$commit" -t fio-ext4-test "$root"
