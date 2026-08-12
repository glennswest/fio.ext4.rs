# Changelog

## [Unreleased]

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
