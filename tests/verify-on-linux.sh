#!/bin/bash
# verify-on-linux.sh — does a real kernel see what we wrote?
#
# Everything here happens in userspace on the host: format a filesystem, fill
# it with files and directories, never touching a mount, a loop device or a
# kernel. Then hand the image to Linux and ask whether the tree is there, the
# contents match byte for byte, and e2fsck is happy — before and after the
# kernel does some writing of its own.
#
#   ./tests/verify-on-linux.sh [user@host]
#
# Defaults to root@dev.g8.lo.

set -uo pipefail

HOST="${1:-root@dev.g8.lo}"
REMOTE=/root/fio-ext4-verify
FAILURES=0

GREEN='' RED='' CYAN='' BOLD='' RESET=''
if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'
    BOLD='\033[1m'; RESET='\033[0m'
fi
ok()  { echo -e "  ${GREEN}OK${RESET}: $1"; }
bad() { echo -e "  ${RED}FAIL${RESET}: $1"; FAILURES=$((FAILURES+1)); }
hdr() { echo; echo -e "${BOLD}${CYAN}-- $1 --${RESET}"; }

W="$(mktemp -d)"
trap 'rm -rf "$W"' EXIT

MKFS=../mkfs.ext4.rs/target/debug/mkfs-ext4
FIO=target/debug/fio-ext4

hdr "building"
cargo build --quiet --bin fio-ext4 || { echo "build failed"; exit 1; }
(cd ../mkfs.ext4.rs && cargo build --quiet --bin mkfs-ext4) || { echo "mkfs build failed"; exit 1; }
ok "binaries built"

for profile in ext4 ext3 ext2; do
    hdr "$profile: building a filesystem entirely in userspace"
    IMG="$W/$profile.img"
    dd if=/dev/zero of="$IMG" bs=1M count=64 status=none
    "$MKFS" -q -t "$profile" -L "$profile-userspace" "$IMG" || { bad "$profile: mkfs"; continue; }

    printf 'router\n' > "$W/hostname"
    printf 'welcome to the machine\n' > "$W/motd"
    head -c 300000 /dev/urandom > "$W/blob.bin"
    # Past the twelve direct blocks, so ext2 and ext3 exercise indirect blocks.
    head -c 900000 /dev/urandom > "$W/bigger.bin"

    "$FIO" "$IMG" put "$W/hostname" /etc/hostname >/dev/null || bad "$profile: put hostname"
    "$FIO" "$IMG" put "$W/motd" /etc/motd >/dev/null || bad "$profile: put motd"
    "$FIO" "$IMG" put "$W/blob.bin" /var/lib/blob.bin >/dev/null || bad "$profile: put blob"
    "$FIO" "$IMG" put "$W/bigger.bin" /var/lib/bigger.bin >/dev/null || bad "$profile: put bigger"
    "$FIO" "$IMG" mkdir /usr/local/share >/dev/null || bad "$profile: mkdir"

    # A directory big enough to need more than one block.
    for i in $(seq 1 120); do
        printf 'x' > "$W/tiny"
        "$FIO" "$IMG" put "$W/tiny" "/many/f$i" >/dev/null 2>&1
    done
    ok "$profile: wrote files, directories and a 120-entry directory"

    blob_sha=$(shasum -a 256 "$W/blob.bin" 2>/dev/null | cut -d' ' -f1 \
        || sha256sum "$W/blob.bin" | cut -d' ' -f1)
    bigger_sha=$(shasum -a 256 "$W/bigger.bin" 2>/dev/null | cut -d' ' -f1 \
        || sha256sum "$W/bigger.bin" | cut -d' ' -f1)

    ssh "$HOST" "rm -rf $REMOTE && mkdir -p $REMOTE" >/dev/null 2>&1
    scp -q "$IMG" "$HOST:$REMOTE/fs.img" || { bad "$profile: copy"; continue; }

    out=$(ssh "$HOST" "bash -s" <<EOF 2>&1
set -uo pipefail
IMG=$REMOTE/fs.img
MNT=\$(mktemp -d)

e2fsck -fn "\$IMG" >/dev/null 2>&1 && echo "FSCK1_OK" || echo "FSCK1_FAIL"

LOOP=\$(losetup --find --show "\$IMG" 2>/dev/null)
[ -n "\$LOOP" ] || { echo "LOOP_FAIL"; exit 1; }
mount "\$LOOP" "\$MNT" 2>/dev/null && echo "MOUNT_OK" || { echo "MOUNT_FAIL"; exit 1; }

[ "\$(cat \$MNT/etc/hostname)" = "router" ] && echo "HOSTNAME_OK" || echo "HOSTNAME_FAIL"
[ "\$(cat \$MNT/etc/motd)" = "welcome to the machine" ] && echo "MOTD_OK" || echo "MOTD_FAIL"
[ -d "\$MNT/usr/local/share" ] && echo "MKDIR_OK" || echo "MKDIR_FAIL"
echo "BLOB_SHA \$(sha256sum \$MNT/var/lib/blob.bin | cut -d' ' -f1)"
echo "BIGGER_SHA \$(sha256sum \$MNT/var/lib/bigger.bin | cut -d' ' -f1)"
echo "MANY_COUNT \$(ls \$MNT/many | wc -l)"

# The kernel's turn to write.
echo "written by the kernel" > "\$MNT/etc/kernel.txt" 2>/dev/null && echo "KERNEL_WRITE_OK" || echo "KERNEL_WRITE_FAIL"
umount "\$MNT" && echo "UMOUNT_OK" || echo "UMOUNT_FAIL"
losetup -d "\$LOOP" 2>/dev/null

e2fsck -fn "\$IMG" >/dev/null 2>&1 && echo "FSCK2_OK" || echo "FSCK2_FAIL"
rmdir "\$MNT" 2>/dev/null
EOF
)

    for check in FSCK1_OK MOUNT_OK HOSTNAME_OK MOTD_OK MKDIR_OK KERNEL_WRITE_OK UMOUNT_OK FSCK2_OK; do
        grep -q "^$check\$" <<< "$out" && ok "$profile: ${check%_OK}" || bad "$profile: ${check%_OK}"
    done

    got_blob=$(grep '^BLOB_SHA ' <<< "$out" | awk '{print $2}')
    [ "$got_blob" = "$blob_sha" ] && ok "$profile: 300 KB file matches byte for byte" \
        || bad "$profile: blob differs (host $blob_sha, kernel $got_blob)"

    got_bigger=$(grep '^BIGGER_SHA ' <<< "$out" | awk '{print $2}')
    [ "$got_bigger" = "$bigger_sha" ] && ok "$profile: 900 KB file matches byte for byte" \
        || bad "$profile: bigger differs (host $bigger_sha, kernel $got_bigger)"

    many=$(grep '^MANY_COUNT ' <<< "$out" | awk '{print $2}')
    [ "$many" = "120" ] && ok "$profile: all 120 directory entries present" \
        || bad "$profile: directory has $many entries, expected 120"
done

hdr "result"
if [ "$FAILURES" -eq 0 ]; then
    echo -e "${GREEN}${BOLD}all checks passed${RESET}"
    exit 0
fi
echo -e "${RED}${BOLD}$FAILURES check(s) failed${RESET}"
exit 1
