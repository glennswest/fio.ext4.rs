# fio-ext4

Async **userspace file I/O** into an ext2 / ext3 / ext4 filesystem — read and
write files with no kernel, no mount and no loop device.

```rust
use fio_ext4::Volume;
use mkfs_ext4::device::FileDevice;

let device = FileDevice::open("disk.img").await?;
let mut vol = Volume::open(device).await?;

vol.mkdir_all("/etc/conf.d").await?;
vol.write("/etc/hostname", b"router\n").await?;
assert_eq!(vol.read("/etc/hostname").await?, b"router\n");

vol.flush().await?;
```

Or from the shell:

```
fio-ext4 disk.img put ./hostname /etc/hostname
fio-ext4 disk.img ls -l /etc
fio-ext4 disk.img cat /etc/hostname
```

## Why

Building the contents of a filesystem image from a program, on a machine that
cannot mount one. It works on a Mac, in an unprivileged container, and against
storage that is not a block device at all — anything implementing
`BlockDevice` from [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs),
which creates and checks the filesystem this crate fills in.

## What it maintains

Every write keeps in step the things `fsck` checks: block and inode bitmaps,
the group descriptors' free counts and directory tallies, the superblock's
totals, and every metadata checksum.

## Verified

`tests/verify-on-linux.sh` builds a filesystem entirely in userspace — format,
directories, files, a 120-entry directory, a 900 KB file needing indirect
blocks — then hands the image to a real Linux kernel and checks that

- `e2fsck -fn` is clean,
- the kernel mounts it and the tree is exactly what was written,
- file contents match **byte for byte** by SHA-256,
- the kernel can write to it afterwards, and it is *still* clean.

All three of ext2, ext3 and ext4 pass.

## Not yet

Hard links, symlinks, rename, extended attributes, and files large enough to
need triple indirection. `dir_index` is read but not maintained, so large
directories are appended linearly.

## Licence

GPL-2.0-or-later, matching `mkfs-ext4` and e2fsprogs.
