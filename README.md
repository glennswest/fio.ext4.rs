# fio-ext4

Async **userspace file I/O** into an ext2 / ext3 / ext4 filesystem — read and
write files with no kernel, no mount and no loop device.

## Using it

Not on crates.io; take it by git, pinned to a tag. `fio-ext4` re-exports
`mkfs_ext4`, so this is the only dependency you need:

```toml
[dependencies]
fio-ext4 = { git = "https://github.com/glennswest/fio.ext4.rs", tag = "v1.2.0" }
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
fio-ext4 disk.img untar rootfs.tar
fio-ext4 disk.img tar -z backup.tar.gz
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

## Archives

A tar archive can be streamed straight into a filesystem, or read back out of
one. Everything an image needs survives: modes and the setuid bit, ownership,
symlinks, hard links, device nodes, timestamps, and extended attributes
including SELinux labels.

```rust
use fio_ext4::archive::{self, UnpackOptions};

// An image and an archive, by path.
archive::unpack("disk.img", Some("rootfs.tar"), &UnpackOptions::default()).await?;

// Or a pipe — None, or "-", is standard input. gzip is detected.
archive::unpack("disk.img", None::<&str>, &UnpackOptions::default()).await?;

// And back out.
archive::pack("disk.img", Some("backup.tar"), &PackOptions::default()).await?;
```

Nothing is held in memory. The stream can be a file, a pipe, a socket, or a
container layer arriving from a registry, and an archive larger than RAM is not
a problem — which is what makes this usable on a device that does not have much
of either.

Container layers stack:

```rust
vol.unpack_tar_layer(tar::Io::new(layer), "/").await?;
```

That obeys the whiteout markers, so a layer can delete as well as add:
`.wh.<name>` removes a name from the layers below, and `.wh..wh..opq` hides
everything already in its directory. A file that replaces one from a lower
layer gets a fresh inode, so it inherits none of the old mode, owner or labels.

For a byte-level interface — `Reader`, `Writer`, and a `Source`/`Sink` pair
that needs no runtime, no pinning and no allocation to implement — see the
[`tar`](src/tar.rs) module.

## Large directories

Directories that outgrow a single block become hash-indexed (`dir_index`), and
stay indexed as they change: a name is added to, found in, and removed from the
one leaf its hash selects. Filling a directory with *n* names costs *n* block
reads rather than *n²*.

The tree is grown the way `e2fsck -D` grows one — when a leaf will not take
another name the index is rebuilt and repacked — rather than by splitting a
leaf at a time as the kernel does. One code path serves the first conversion
and every growth after it, and leaves are left a fifth empty so a rebuild is
needed about once per two hundred names.

## Licence

`MIT OR Apache-2.0`, at your option — matching `mkfs-ext4` and the rest of the
Rust ecosystem. The MIT arm is GPLv2-compatible, so this imposes nothing on a
kernel or RHEL consumer.
