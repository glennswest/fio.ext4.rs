# fio-ext4

Async **userspace file I/O** into an ext2 / ext3 / ext4 filesystem — read and
write files with no kernel, no mount and no loop device.

## Using it

Not on crates.io; take it by git, pinned to a tag. `fio-ext4` re-exports
`mkfs_ext4`, so this is the only dependency you need:

```toml
[dependencies]
fio-ext4 = { git = "https://github.com/glennswest/fio.ext4.rs", tag = "v1.0.2" }
```

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

## Attributes

Permissions and ownership are first-class, because an image without them is not
a working root filesystem:

```rust
vol.write_with("/etc/shadow", data, &Attrs::mode(0o600)).await?;
vol.write_with("/usr/bin/su", elf, &Attrs::mode(0o4755)).await?;   // setuid
vol.mkdir_with("/tmp", &Attrs::mode(0o1777)).await?;               // sticky
vol.chown("/home/gw", 165536, 165536).await?;                      // 32-bit
vol.mknod("/dev/null", Special::CharDevice { major: 1, minor: 3 },
          &Attrs::mode(0o666)).await?;
vol.symlink("/bin", "usr/bin").await?;
```

## Not yet

Hard links, rename, extended attributes (and the POSIX ACLs that ride on them),
and writing files large enough to need triple indirection. `dir_index` is read
but not maintained, so large directories are appended linearly.

## Licence

`MIT OR Apache-2.0`, at your option — matching `mkfs-ext4` and the rest of the
Rust ecosystem. The MIT arm is GPLv2-compatible, so this imposes nothing on a
kernel or RHEL consumer.
