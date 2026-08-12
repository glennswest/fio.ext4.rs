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
