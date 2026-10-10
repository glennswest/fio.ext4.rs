#!/bin/bash
# /test <short|medium|long> — does a real kernel see what fio-ext4 wrote?
#
# fio-ext4-test's test program (stormcentral docs/test-standard.md). Every
# image is built in userspace — mkfs-ext4 formats it, fio-ext4 fills it, with
# no mount, loop device or kernel — and then judged by the two things that
# count: the real e2fsck (`e2fsck -fn`), and the real kernel, which mounts it
# through a loop device, compares every file byte for byte, writes a file of
# its own, and hands it back to e2fsck.
#
#   short   ext4: files, a 900 KB file, a 120-entry directory
#   medium  short on ext4, ext3 and ext2, and a tar unpack (symlink, hard
#           link, modes) on ext4
#   long    medium, and a 200 MiB file on each of ext4, ext3 and ext2
#
# The kernel checks need a loop device and a mount. This pod is not
# privileged (owner, #5), so here they report skip, never pass; the e2fsck
# checks still run. The kernel's verdict comes from the testhost boot VM,
# tests/vm/ (`stormcentral testhost boot`), which runs these cases as PID 1.
#
# Output: one JSON object per test on stdout, then a summary. Exit 0 all
# passed (or skipped), 1 a test failed, 2 the test could not run.

set -uo pipefail

SUITE="${1:-${STORM_SUITE:-}}"
case "$SUITE" in
    short | medium | long) ;;
    *)
        echo "usage: /test short|medium|long" >&2
        exit 2
        ;;
esac

PASS=0 FAIL=0 SKIP=0
MOUNTS=() LOOPS=()

for tool in mkfs-ext4 fio-ext4 e2fsck losetup mount umount sha256sum tar gzip; do
    command -v "$tool" >/dev/null || {
        echo "fio-ext4-test: $tool is missing from the image" >&2
        exit 2
    }
done

W=$(mktemp -d "${TMPDIR:-/tmp}/fio-ext4-test.XXXXXX") || exit 2
cleanup() {
    local m l
    for m in "${MOUNTS[@]}"; do umount "$m" 2>/dev/null; done
    for l in "${LOOPS[@]}"; do losetup -d "$l" 2>/dev/null; done
    rm -rf "$W"
}
trap cleanup EXIT

now_ms() { date +%s%3N; }

# report NAME STATUS START_MS [DETAIL]
report() {
    local detail
    detail=$(printf '%s' "${4:-}" | tr '\n\t' '  ' | sed 's/\\/\\\\/g; s/"/\\"/g')
    printf '{"test": "%s", "status": "%s", "ms": %d, "detail": "%s"}\n' \
        "$1" "$2" "$(($(now_ms) - $3))" "$detail"
    case "$2" in
        pass) PASS=$((PASS + 1)) ;;
        fail) FAIL=$((FAIL + 1)) ;;
        skip) SKIP=$((SKIP + 1)) ;;
    esac
}

sha() { sha256sum "$1" | cut -d' ' -f1; }

# Why the kernel checks cannot run here, or nothing. Asked once.
KERNEL_SKIP=""
probe_kernel() {
    [ -e /dev/loop-control ] || mknod /dev/loop-control c 10 237 2>/dev/null ||
        KERNEL_SKIP="no /dev/loop-control and cannot create it (pod not privileged?)"
    # ext4 missing from /proc/filesystems may only be not loaded yet: whether
    # the kernel has a driver is decided at the mount.
}

# attach IMAGE — attach it to a free loop device, named in $LOOP; on
# failure $LOOP is why. Not called in a subshell, so cleanup sees $LOOPS.
LOOP=""
attach() {
    local n
    if LOOP=$(losetup --find --show "$1" 2>/dev/null); then
        LOOPS+=("$LOOP")
        return 0
    fi
    # A privileged container's /dev is a copy taken when it started: loop
    # devices the driver creates afterwards have no node here. Make some.
    for n in $(seq 0 63); do
        [ -e "/dev/loop$n" ] || mknod "/dev/loop$n" b 7 "$n" 2>/dev/null
    done
    LOOP=$(losetup --find --show "$1" 2>&1) || return 1
    LOOPS+=("$LOOP")
}

detach() {
    local keep=() l
    losetup -d "$1" 2>/dev/null
    for l in "${LOOPS[@]}"; do [ "$l" = "$1" ] || keep+=("$l"); done
    LOOPS=("${keep[@]}")
}

unmount() {
    local keep=() m rc
    umount "$1"
    rc=$?
    for m in "${MOUNTS[@]}"; do [ "$m" = "$1" ] || keep+=("$m"); done
    MOUNTS=("${keep[@]}")
    return $rc
}

# e2fsck_check NAME IMAGE — the real e2fsck, forced, read-only.
e2fsck_check() {
    local t out
    t=$(now_ms)
    if out=$(e2fsck -fn "$2" 2>&1); then
        report "$1" pass "$t"
    else
        report "$1" fail "$t" "$(tail -n 20 <<<"$out")"
    fi
}

# kernel_judge PREFIX IMAGE VERIFY_FN — mount, run VERIFY_FN MNT (it prints
# what is wrong, nothing when right), let the kernel write, unmount, e2fsck.
kernel_judge() {
    local p=$1 img=$2 verify=$3 t dev mnt out wrong
    t=$(now_ms)
    if [ -n "$KERNEL_SKIP" ]; then
        report "$p-kernel-read" skip "$t" "$KERNEL_SKIP"
        report "$p-kernel-write" skip "$t" "$KERNEL_SKIP"
        return
    fi
    if ! attach "$img"; then
        report "$p-kernel-read" skip "$t" "no loop device: $LOOP"
        report "$p-kernel-write" skip "$t" "no loop device: $LOOP"
        return
    fi
    dev=$LOOP
    mnt="$W/mnt-$p"
    mkdir -p "$mnt"
    if ! out=$(mount -o rw "$dev" "$mnt" 2>&1); then
        detach "$dev"
        if grep -qi "unknown filesystem type" <<<"$out"; then
            report "$p-kernel-read" skip "$t" "this kernel has no driver for it: $out"
            report "$p-kernel-write" skip "$t" "this kernel has no driver for it: $out"
        else
            report "$p-kernel-read" fail "$t" "mount refused it: $out"
            report "$p-kernel-write" skip "$t" "not mounted"
        fi
        return
    fi
    MOUNTS+=("$mnt")

    wrong=$("$verify" "$mnt" 2>&1)
    if [ -z "$wrong" ]; then
        report "$p-kernel-read" pass "$t"
    else
        report "$p-kernel-read" fail "$t" "$wrong"
    fi

    # The kernel's turn to write, then e2fsck judges what it left.
    t=$(now_ms)
    wrong=""
    echo "written by the kernel" >"$mnt/kernel.txt" 2>&1 || wrong="write failed"
    mkdir "$mnt/kernel-dir" 2>/dev/null || wrong="$wrong; mkdir failed"
    unmount "$mnt" || wrong="$wrong; umount failed"
    detach "$dev"
    if [ -n "$wrong" ]; then
        report "$p-kernel-write" fail "$t" "$wrong"
    elif out=$(e2fsck -fn "$img" 2>&1); then
        report "$p-kernel-write" pass "$t"
    else
        report "$p-kernel-write" fail "$t" "e2fsck after the kernel wrote: $(tail -n 20 <<<"$out")"
    fi
}

# --- files: the original verify-on-linux.sh tree --------------------------

printf 'router\n' >"$W/hostname"
printf 'welcome to the machine\n' >"$W/motd"
head -c 300000 /dev/urandom >"$W/blob.bin"
# Past the twelve direct blocks, so ext2 and ext3 exercise indirect blocks.
head -c 900000 /dev/urandom >"$W/bigger.bin"
printf 'x' >"$W/tiny"
BLOB_SHA=$(sha "$W/blob.bin")
BIGGER_SHA=$(sha "$W/bigger.bin")

verify_files() {
    local m=$1
    [ "$(cat "$m/etc/hostname")" = "router" ] || echo "etc/hostname differs"
    [ "$(cat "$m/etc/motd")" = "welcome to the machine" ] || echo "etc/motd differs"
    [ -d "$m/usr/local/share" ] || echo "usr/local/share missing"
    [ "$(sha "$m/var/lib/blob.bin")" = "$BLOB_SHA" ] || echo "300 KB file differs"
    [ "$(sha "$m/var/lib/bigger.bin")" = "$BIGGER_SHA" ] || echo "900 KB file differs"
    local n
    n=$(ls "$m/many" | wc -l)
    [ "$n" = 120 ] || echo "many/ has $n entries, expected 120"
}

files_case() {
    local profile=$1 p="files-$1" img="$W/files-$1.img" t err
    t=$(now_ms)
    truncate -s 64M "$img"
    err=$({
        mkfs-ext4 -q -t "$profile" -L "$profile-userspace" "$img" &&
            fio-ext4 "$img" put "$W/hostname" /etc/hostname &&
            fio-ext4 "$img" put "$W/motd" /etc/motd &&
            fio-ext4 "$img" put "$W/blob.bin" /var/lib/blob.bin &&
            fio-ext4 "$img" put "$W/bigger.bin" /var/lib/bigger.bin &&
            fio-ext4 "$img" mkdir /usr/local/share &&
            for i in $(seq 1 120); do
                fio-ext4 "$img" put "$W/tiny" "/many/f$i" || exit 1
            done
    } 2>&1 >/dev/null) || {
        report "$p-write" fail "$t" "$err"
        return
    }
    report "$p-write" pass "$t"
    e2fsck_check "$p-e2fsck" "$img"
    kernel_judge "$p" "$img" verify_files
}

# --- tar: what an image layer brings ---------------------------------------

verify_tar() {
    local m=$1
    [ "$(readlink "$m/app/lib64")" = "lib" ] || echo "app/lib64 is not a symlink to lib"
    [ "$(cat "$m/app/lib64/libx.so")" = "a shared object" ] || echo "app/lib64/libx.so differs through the symlink"
    [ "$(stat -c %i "$m/app/bin/tool")" = "$(stat -c %i "$m/app/bin/tool-alias")" ] ||
        echo "app/bin/tool and tool-alias are not one inode"
    [ "$(stat -c %h "$m/app/bin/tool")" = 2 ] || echo "app/bin/tool link count is $(stat -c %h "$m/app/bin/tool"), expected 2"
    [ "$(stat -c %a "$m/app/bin/tool")" = 755 ] || echo "app/bin/tool mode is $(stat -c %a "$m/app/bin/tool"), expected 755"
    [ "$(stat -c %a "$m/app/private")" = 750 ] || echo "app/private mode is $(stat -c %a "$m/app/private"), expected 750"
    [ "$(sha "$m/app/data/blob.bin")" = "$BIGGER_SHA" ] || echo "app/data/blob.bin differs"
}

tar_case() {
    local p="tar-ext4" img="$W/tar.img" t err src="$W/tree"
    t=$(now_ms)
    mkdir -p "$src/app/lib" "$src/app/bin" "$src/app/private" "$src/app/data"
    printf 'a shared object' >"$src/app/lib/libx.so"
    ln -s lib "$src/app/lib64"
    printf '#!/bin/sh\necho tool\n' >"$src/app/bin/tool"
    chmod 755 "$src/app/bin/tool"
    ln "$src/app/bin/tool" "$src/app/bin/tool-alias"
    chmod 750 "$src/app/private"
    cp "$W/bigger.bin" "$src/app/data/blob.bin"
    tar -C "$src" -czf "$W/layer.tar.gz" app
    truncate -s 64M "$img"
    err=$({ mkfs-ext4 -q -t ext4 "$img" && fio-ext4 "$img" untar "$W/layer.tar.gz"; } 2>&1 >/dev/null) || {
        report "$p-write" fail "$t" "$err"
        return
    }
    report "$p-write" pass "$t"
    e2fsck_check "$p-e2fsck" "$img"
    kernel_judge "$p" "$img" verify_tar
}

# --- large: one file well past double indirection on 4 KiB blocks ----------

large_case() {
    local profile=$1 p="large-$1" img="$W/large-$1.img" t err
    t=$(now_ms)
    [ -f "$W/large.bin" ] || head -c $((200 * 1024 * 1024)) /dev/urandom >"$W/large.bin"
    LARGE_SHA=${LARGE_SHA:-$(sha "$W/large.bin")}
    truncate -s 256M "$img"
    err=$({ mkfs-ext4 -q -t "$profile" "$img" && fio-ext4 "$img" put "$W/large.bin" /large.bin; } 2>&1 >/dev/null) || {
        report "$p-write" fail "$t" "$err"
        return
    }
    report "$p-write" pass "$t"
    e2fsck_check "$p-e2fsck" "$img"
    kernel_judge "$p" "$img" verify_large
    rm -f "$img"
}

verify_large() {
    [ "$(sha "$1/large.bin")" = "$LARGE_SHA" ] || echo "200 MiB file differs"
}

probe_kernel

files_case ext4
if [ "$SUITE" != short ]; then
    files_case ext3
    files_case ext2
    tar_case
fi
if [ "$SUITE" = long ]; then
    large_case ext4
    large_case ext3
    large_case ext2
fi

printf '{"summary": {"pass": %d, "fail": %d, "skip": %d}}\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" -eq 0 ]
