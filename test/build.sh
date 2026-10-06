#!/bin/sh
# Build fio-ext4's test image context for the commit checked out.
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, and stages static binaries in test/.stage/;
# test/Containerfile (context: the repo root) packages them.
#
# mkfs-ext4 is installed from the exact commit Cargo.lock pins — the
# formatter fio-ext4 is tested against. (Cargo will not turn on a
# dependency's `cli` feature from here, so it is not built in this workspace.)
# With STAGE_ONLY=1 it stops after staging and prints the stage path;
# otherwise it also runs `podman build` and tags fio-ext4-test.
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)
commit=$(git -C "$root" rev-parse HEAD)
manifest="$root/Cargo.toml"

cargo build --release --locked --target "$target" --manifest-path "$manifest" --bin fio-ext4
locked=$(sed -n '/^name = "mkfs-ext4"$/,/^$/s/^source = "git+\(.*\)"$/\1/p' "$root/Cargo.lock")
mkfs_repo=${locked%%\?*}
mkfs_rev=${locked##*#}
[ -n "$mkfs_repo" ] && [ -n "$mkfs_rev" ] || { echo "mkfs-ext4's source not found in Cargo.lock" >&2; exit 1; }
tools="$root/test/.tools"
cargo install --locked --target "$target" --root "$tools" \
    --git "$mkfs_repo" --rev "$mkfs_rev" --bin mkfs-ext4 mkfs-ext4
tdir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" |
    sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"
cp "$tdir/$target/release/fio-ext4" "$tools/bin/mkfs-ext4" "$stage/"
cp "$root/test/test.sh" "$stage/test"
chmod 755 "$stage/test"

if [ "${STAGE_ONLY:-0}" = 1 ]; then
    echo "$stage"
    exit 0
fi
podman build -f "$root/test/Containerfile" --build-arg COMMIT="$commit" -t fio-ext4-test "$root"
