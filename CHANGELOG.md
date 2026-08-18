# Changelog

## [Unreleased]

- **chore(deps):** pinned to `mkfs-ext4` v1.3.0, which stops writing the
  bitmaps and reserved GDT blocks a reader never reads — a 1 TiB format writes
  133.9 MiB less in 45% fewer calls — and reads a `BLOCK_UNINIT` or
  `INODE_UNINIT` group's bitmap the way its flag says to, computing it from
  the geometry rather than trusting a block that was never written. The second
  is the one that reaches this crate: allocation reads those bitmaps, and on a
  medium that does not read back as zeros the old behaviour saw a bitmap full
  of whatever was there before.

## [v1.2.0] — 2026-08-14

### Added
- **Hash-indexed directories (`dir_index`) are now maintained**, not just read.
  A directory that outgrows one block becomes a tree, and stays one: names are
  added to the single leaf their hash selects, removed from it, and looked up
  through it. Filling a directory with *n* names was *n²* block reads and is now
  linear.

  The tree is grown the way `e2fsck -D` grows one rather than the way the
  kernel does: when a leaf will not take another name, the whole index is
  rebuilt from the directory's contents, sorted by hash and repacked. That is
  one code path for the first conversion and every growth after it, instead of
  a leaf split, a node split and a root promotion that each have to be right
  alone. Leaves are packed with a fifth of each block left free, so a rebuild
  happens about once per two hundred names.

  Verified three ways, because a directory index is the structure where
  "it looks right" proves least: our own lookups find every name, `e2fsck -fn`
  from e2fsprogs 1.47.3 accepts the tree, and `debugfs -R htree_dump` reads it
  back with the counts, limits and checksums it expects.

### Fixed
- Removing a name from an indexed directory walked its root block as though it
  were an ordinary directory block. The root's `..` entry runs to the very end
  of its block — an index block carries no dirent tail — so the walk failed on
  the first entry it read.

## [v1.1.0] — 2026-08-13

### Added
- **Tar archives, streamed in and out — `tar::Reader`, `tar::Writer`,
  `Volume::unpack_tar_from`, `Volume::pack_tar_to`. ustar, GNU long names and
  PAX, including `SCHILY.xattr.*`, so modes, owners, symlinks, hard links,
  device nodes and SELinux labels all survive. Nothing is held in memory, so
  the source or destination can be a file, a pipe, or a container layer moving
  to or from a registry.
- **OCI layer semantics** — `Volume::unpack_tar_layer` obeys `.wh.<name>`
  and `.wh..wh..opq` whiteout markers, so layers can be stacked in order.
- **`archive` module** — whole-archive operations in terms of paths:
  `archive::unpack` and `archive::pack` take an image and an archive, where
  `None` or `-` is standard input or output. gzip is detected and handled.
- **`untar` and `tar` subcommands** on the `fio-ext4` CLI, which are the
  above with argument parsing in front.
- `Volume::remove_all` and `Volume::device_numbers`.

### Fixed
- Deleting a file or directory whose extended attributes lived in a
  block of their own leaked that block. `e2fsck` reported it as "block bitmap
  differences" — a block marked in use and owned by nothing.
- Writing over an existing file kept the old inode, and with it the
  old mode, owner and extended attributes. Correct for an ordinary overwrite,
  wrong for a layer replacing a file: the replacement inherited the label of
  what it replaced. Unpacking now always creates a fresh inode.
- `pack` now walks depth-first in sorted order rather than emitting a
  stack's worth of directories first, so archives are conventional and the same
  tree always produces the same bytes.

### Added

- **Hard links** — `link()`, sharing one inode between names. Linking a
  directory is refused: it would turn the tree into a graph, which every
  checker treats as corruption.
- **`rename()`** — within a directory, across directories, and over an existing
  name. A directory move carries its `..` with it and adjusts both parents'
  link counts; moving a directory inside itself is refused, since the subtree
  would be unreachable.
- **`write_at()`** — write at an offset, leaving the rest of the file alone,
  extending the file if it runs past the end.
- **Triple indirection on write.** ext2 and ext3 files past ~4 GiB of double
  indirection now build the third level rather than being refused.

### Fixed

- A double indirect block was filled without bounding it to `per_block`
  pointers, so a file large enough to need triple indirection wrote past the
  end of the buffer.
- Dropping a `Volume` with unflushed changes now logs a warning. Directory
  blocks and inodes are written as they change but bitmaps are buffered, so a
  missed `flush()` leaves names pointing at inodes the bitmap never marked
  used — corruption rather than merely unsaved work. Found by a test of mine
  that omitted the flush.

## [v1.0.2] — 2026-08-12

### Changed

- **Licence is now `MIT OR Apache-2.0`**, matching `mkfs-ext4` and the Rust
  ecosystem. The MIT arm is GPLv2-compatible, so nothing here constrains a
  kernel or RHEL consumer.
- Pinned to `mkfs-ext4` v1.0.2, which detects a device's logical sector size
  rather than assuming 512 — a volume exporting 4 KiB sectors now gets a
  4 KiB-block filesystem.

## [v1.0.0] — 2026-08-12

First stable release, alongside `mkfs-ext4` v1.0.0. The public API is settled
and the full semver contract applies from here.

**What 1.0 claims.** A filesystem built entirely in userspace — no kernel, no
mount, no loop device, no root — that a real Linux kernel mounts, reads and
writes, with file contents matching byte for byte by SHA-256. Every write keeps
the bitmaps, group descriptor counts, superblock totals and metadata checksums
in step, so the result passes `e2fsck` afterwards.

**What 1.0 does not claim.** Hard links, rename, extended attributes (and the
POSIX ACLs that ride on them), writing files large enough to need triple
indirection, or maintaining `dir_index` on write. Adding any of them is
additive.

### Added

- **`Volume`** — the whole public surface: `read`, `write`, `append`, `mkdir`,
  `mkdir_all`, `unlink`, `rmdir`, `stat`, `read_dir`, `lookup` and `exists`,
  all async, against anything implementing `BlockDevice`.
- **Permissions and ownership as first-class.** `Attrs` carries mode, uid and
  gid through creation; `write_with`, `mkdir_with` and `mkdir_all_with` apply
  them; `chmod`, `chown` and `set_times` change them afterwards. Setuid, setgid
  and the sticky bit are ordinary permission bits, so `/usr/bin/su` at `4755`
  and `/tmp` at `1777` behave as expected. Ownership is 32-bit, which rootless
  containers need — they map into subuid ranges well past 65535.
- **Special files.** `mknod` creates character and block devices, FIFOs and
  sockets, with device numbers in the encoding the kernel reads — the classic
  16-bit slot below 256 and the wider encoding above. `symlink` and `read_link`
  handle symbolic links, storing short targets inside the inode where they cost
  no blocks at all.
- **Block maps in both shapes** — extent trees for ext4, direct, single and
  double indirection for ext2 and ext3 — rebuilt rather than patched, so a
  failed write leaves no half-updated tree.
- **Directories that grow.** Entries are inserted into the slack of an existing
  entry, or a new block is added and linked in.
- **`fio-ext4` binary** — `ls`, `cat`, `put`, `get`, `mkdir`, `rm`, `rmdir`,
  `stat` against an image, with no privileges required.

### Fixed

- Depend on `mkfs-ext4` by git rather than by path (#1). A path dependency
  inside a git dependency only resolves when the path is inside the same
  repository, so any consumer taking this crate by git failed to resolve before
  compiling anything. Now pinned to `v1.0.0`, with a `[patch]` section keeping
  local development against the sibling checkout unchanged.
- A newly created extent-mapped inode is given a valid, empty extent tree.
  A zeroed `i_block` has no magic in it, so anything walking the inode before
  its first write found a corrupt extent header rather than an empty file.

### Verified

- 18 round-trip tests, each ending by checking that the filesystem is still
  clean — a file writer that leaves `fsck` complaining has damaged the
  filesystem, not written a file.
- `tests/verify-on-linux.sh` — a filesystem built entirely in userspace, handed
  to a real Linux kernel: the tree is exactly what was written, a 900 KB file
  matches by SHA-256, the kernel writes to it, and `e2fsck` is clean before and
  after. ext2, ext3 and ext4 all pass.
- `examples/rootfs.rs` builds a root filesystem with modes, owners, `/dev/null`,
  `/dev/console`, a block device, a FIFO, large device numbers and merged-`/usr`
  symlinks. The kernel reports every mode and owner correctly — and *uses* the
  device nodes: reads through `/dev/null` and `/dev/zero` both work.

### Development history

### 2026-08-12
- **feat:** First cut of `fio-ext4` — async userspace read and write into an
  ext2/ext3/ext4 filesystem with no kernel, mount or loop device. Files,
  directories, `mkdir_all`, append, overwrite, unlink, rmdir, stat and listing.
  Extent trees for ext4 and indirect blocks for ext2/ext3; block and inode
  allocation keeping bitmaps, group descriptors, superblock counters and every
  metadata checksum in step.
- **feat:** `fio-ext4` binary — `ls`, `cat`, `put`, `get`, `mkdir`, `rm`,
  `rmdir`, `stat` against an image, with no privileges required.
- **test:** 12 round-trip tests, each asserting the filesystem still checks
  clean afterwards, plus `tests/verify-on-linux.sh`, which builds a filesystem
  entirely in userspace and has a real Linux kernel mount it, compare file
  contents by SHA-256, write to it, and re-check it. ext2, ext3 and ext4 pass.
- **feat:** Permissions and ownership are first-class. `Attrs` carries mode, uid
  and gid; `write_with`, `mkdir_with` and `mkdir_all_with` apply them, and
  `chmod`, `chown` and `set_times` change them afterwards. Setuid, setgid and
  the sticky bit are all just permission bits, so `/usr/bin/su` at `4755` and
  `/tmp` at `1777` work as expected. An image where every file is `0644
  root:root` is not a working root filesystem.
- **feat:** Special files — `mknod` creates character and block devices, FIFOs
  and sockets, with device numbers in the encoding the kernel reads; `symlink`
  and `read_link` handle symbolic links, storing short targets inside the inode
  where they cost no blocks at all.
- **test:** `examples/rootfs.rs` builds a root filesystem — modes, owners,
  `/dev/null`, `/dev/console`, a block device, a FIFO, large device numbers and
  merged-`/usr` symlinks. Verified against Linux: the kernel reports every mode
  and owner correctly, and *uses* the device nodes — `cat /dev/null` and reading
  `/dev/zero` both work.
- **test:** Ownership is verified 32-bit, not 16. A rootless container maps into
  subuid ranges past 65535, so truncating to the low half would hand every file
  to the wrong user. Verified against Linux at 165536 (a typical podman
  mapping) and at 4294967294.
- **fix:** Depend on `mkfs-ext4` by git rather than by path (#1). A path
  dependency inside a git dependency only resolves when the path is inside the
  same repository, so any consumer taking `fio-ext4` by git failed to resolve
  before compiling anything. A `[patch]` section keeps local development
  against the sibling checkout unchanged — patches apply only to the crate
  being built, so a downstream consumer never sees it.
