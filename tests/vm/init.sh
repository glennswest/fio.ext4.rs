#!/bin/busybox sh
# The init of the kernel-verification VM (#5), run as PID 1 from the
# initramfs tests/vm/build-image.sh makes. CLAUDE.md rule 4: the kernel is
# the judge. For each case, on a sparse image in tmpfs:
#
#   mkfs-ext4 + fio-ext4             formatted and filled in userspace
#   e2fsck -fn                       clean, as fio-ext4 wrote it
#   the kernel loop-mounts it        every file compared (sha256, modes,
#                                    symlinks, hard links, entry counts)
#   the kernel writes                a file, a directory, 200 names
#   e2fsck -fn                       clean, after the kernel wrote
#   fio-ext4 reads and writes        the kernel's file read back; fio-ext4
#                                    writes over the kernel's directories
#   e2fsck -fn, the kernel again     clean, and the kernel reads what
#                                    fio-ext4 wrote the second time
#
# Prints `VERIFY PASS` or `VERIFY FAIL <why>` on the serial console, then
# powers off. stormcentral's testhost boot watches for those lines.
/bin/busybox mount -t proc proc /proc
/bin/busybox --install -s /bin
export PATH=/bin:/sbin
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mkdir -p /mnt /work
mount -t tmpfs -o size=90% tmpfs /work
echo 1 > /proc/sys/kernel/printk 2>/dev/null

say() { echo "FIO-EXT4-VERIFY: $*"; }
fail() {
    say "FAIL: $*"
    dmesg | grep -iE 'ext[234]|jbd2|loop' | tail -20 | sed 's/^/FIO-EXT4-VERIFY dmesg: /'
    [ -s /work/check.log ] && head -40 /work/check.log | sed 's/^/FIO-EXT4-VERIFY e2fsck: /'
    echo "VERIFY FAIL $*"
    sync; poweroff -f; sleep 30
}

say "kernel $(uname -r), $(cat /build-info 2>/dev/null)"
for m in $(cat /modules.order 2>/dev/null); do
    insmod "/modules/$m" || fail "insmod $m"
done
for t in ext2 ext3 ext4; do
    grep -qw $t /proc/filesystems || fail "the kernel has no $t"
done

sha() { sha256sum "$1" | cut -d' ' -f1; }

check() { # image what — the real e2fsck, forced, read-only
    : > /work/check.log
    e2fsck -fn "$1" >/work/check.log 2>&1 || fail "e2fsck -fn $2 (exit $?)"
}

fio() { # fio-ext4 with its error in the failure
    fio-ext4 "$@" >/work/out 2>/work/err || fail "fio-ext4 $*: $(cat /work/err)"
}

# The payload, made here: urandom contents, so nothing compresses away.
printf 'router\n' > /work/hostname
printf 'welcome to the machine\n' > /work/motd
head -c 300000 /dev/urandom > /work/blob.bin
# Past the twelve direct blocks, so ext2 and ext3 exercise indirect blocks.
head -c 900000 /dev/urandom > /work/bigger.bin
printf 'x' > /work/tiny
BLOB_SHA=$(sha /work/blob.bin)
BIGGER_SHA=$(sha /work/bigger.bin)
LAYER_SHA=$(cat /data/layer-blob.sha256)

verify_files() {
    [ "$(cat /mnt/etc/hostname)" = router ] || fail "$1: etc/hostname differs"
    [ "$(cat /mnt/etc/motd)" = "welcome to the machine" ] || fail "$1: etc/motd differs"
    [ -d /mnt/usr/local/share ] || fail "$1: usr/local/share missing"
    [ "$(sha /mnt/var/lib/blob.bin)" = "$BLOB_SHA" ] || fail "$1: the 300 KB file differs"
    [ "$(sha /mnt/var/lib/bigger.bin)" = "$BIGGER_SHA" ] || fail "$1: the 900 KB file differs"
    n=$(ls /mnt/many | wc -l)
    [ "$n" -eq 120 ] || fail "$1: many/ lists $n names, want 120"
}

verify_tar() {
    [ "$(readlink /mnt/app/lib64)" = lib ] || fail "$1: app/lib64 is not a symlink to lib"
    [ "$(cat /mnt/app/lib64/libx.so)" = "a shared object" ] || fail "$1: app/lib64/libx.so differs through the symlink"
    [ "$(stat -c %i /mnt/app/bin/tool)" = "$(stat -c %i /mnt/app/bin/tool-alias)" ] \
        || fail "$1: app/bin/tool and tool-alias are not one inode"
    [ "$(stat -c %h /mnt/app/bin/tool)" -eq 2 ] || fail "$1: app/bin/tool has $(stat -c %h /mnt/app/bin/tool) links, want 2"
    [ "$(stat -c %a /mnt/app/bin/tool)" = 755 ] || fail "$1: app/bin/tool mode $(stat -c %a /mnt/app/bin/tool), want 755"
    [ "$(stat -c %a /mnt/app/private)" = 750 ] || fail "$1: app/private mode $(stat -c %a /mnt/app/private), want 750"
    [ "$(sha /mnt/app/data/blob.bin)" = "$LAYER_SHA" ] || fail "$1: app/data/blob.bin differs"
}

verify_large() {
    [ "$(sha /mnt/large.bin)" = "$LARGE_SHA" ] || fail "$1: the 200 MiB file differs"
}

# judge image type name verify — the kernel reads, writes; e2fsck; fio-ext4
# reads the kernel's work and writes over it; e2fsck; the kernel reads that.
judge() {
    img=$1 type=$2 name=$3 verify=$4
    check "$img" "$name: as fio-ext4 wrote it"
    mount -t "$type" -o loop,rw "$img" /mnt 2>/work/err || fail "$name: mount: $(cat /work/err)"
    grep -q " /mnt $type rw" /proc/mounts || fail "$name: not mounted $type read-write"
    $verify "$name"
    echo "written by the kernel" > /mnt/kernel.txt || fail "$name: kernel write"
    mkdir -p /mnt/kernel-dir/sub && echo x > /mnt/kernel-dir/sub/f || fail "$name: kernel mkdir"
    i=0; while [ $i -lt 200 ]; do echo $i > /mnt/kernel-dir/n$i || break; i=$((i + 1)); done
    [ $i -eq 200 ] || fail "$name: the kernel wrote only $i of 200 files"
    sync
    umount /mnt || fail "$name: umount"
    check "$img" "$name: after the kernel wrote"

    fio "$img" cat /kernel.txt
    [ "$(cat /work/out)" = "written by the kernel" ] || fail "$name: fio-ext4 reads the kernel's file as '$(cat /work/out)'"
    fio "$img" cat /kernel-dir/n199
    [ "$(cat /work/out)" = 199 ] || fail "$name: fio-ext4 reads kernel-dir/n199 as '$(cat /work/out)'"
    fio "$img" put /work/bigger.bin /kernel-dir/from-fio.bin
    fio "$img" rm /kernel-dir/n7
    check "$img" "$name: after fio-ext4 wrote over the kernel's work"
    mount -t "$type" -o loop,ro "$img" /mnt 2>/work/err || fail "$name: remount: $(cat /work/err)"
    $verify "$name (second mount)"
    [ "$(sha /mnt/kernel-dir/from-fio.bin)" = "$BIGGER_SHA" ] || fail "$name: the kernel reads fio-ext4's second write wrong"
    [ ! -e /mnt/kernel-dir/n7 ] && [ -f /mnt/kernel-dir/n199 ] || fail "$name: kernel-dir after fio-ext4's rm"
    [ "$(cat /mnt/kernel-dir/sub/f)" = x ] || fail "$name: the kernel's own file after fio-ext4 wrote"
    umount /mnt || fail "$name: umount after the second mount"
}

files_case() { # profile
    name=files-$1 img=/work/files-$1.img
    rm -f "$img"; truncate -s 64M "$img" || fail "$name: truncate"
    mkfs-ext4 -q -t "$1" -L "$1-userspace" "$img" >/work/err 2>&1 || fail "$name: mkfs-ext4: $(cat /work/err)"
    fio "$img" put /work/hostname /etc/hostname
    fio "$img" put /work/motd /etc/motd
    fio "$img" put /work/blob.bin /var/lib/blob.bin
    fio "$img" put /work/bigger.bin /var/lib/bigger.bin
    fio "$img" mkdir /usr/local/share
    i=1; while [ $i -le 120 ]; do fio "$img" put /work/tiny /many/f$i; i=$((i + 1)); done
    judge "$img" "$1" "$name" verify_files
    say "$name: fio-ext4 wrote it, e2fsck clean; the kernel read it, wrote, e2fsck clean; fio-ext4 read and wrote again, e2fsck clean, the kernel read it back"
    rm -f "$img"
}

tar_case() {
    name=tar-ext4 img=/work/tar.img
    rm -f "$img"; truncate -s 64M "$img" || fail "$name: truncate"
    mkfs-ext4 -q -t ext4 "$img" >/work/err 2>&1 || fail "$name: mkfs-ext4: $(cat /work/err)"
    fio "$img" untar /data/layer.tar.gz
    judge "$img" ext4 "$name" verify_tar
    say "$name: a gzipped layer (symlink, hard link, modes) unpacked by fio-ext4, judged as above"
    rm -f "$img"
}

large_case() { # profile — one file well past double indirection on 4 KiB blocks
    name=large-$1 img=/work/large-$1.img
    rm -f "$img"; truncate -s 256M "$img" || fail "$name: truncate"
    mkfs-ext4 -q -t "$1" "$img" >/work/err 2>&1 || fail "$name: mkfs-ext4: $(cat /work/err)"
    fio "$img" put /work/large.bin /large.bin
    judge "$img" "$1" "$name" verify_large
    say "$name: a 200 MiB file, judged as above"
    rm -f "$img"
}

files_case ext4
files_case ext3
files_case ext2
tar_case
head -c $((200 * 1024 * 1024)) /dev/urandom > /work/large.bin || fail "200 MiB of urandom"
LARGE_SHA=$(sha /work/large.bin)
large_case ext4
large_case ext3
large_case ext2

echo "VERIFY PASS"
sync; poweroff -f; sleep 30
