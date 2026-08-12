# CLAUDE.md — fio-ext4

Async userspace file I/O into an ext2/ext3/ext4 filesystem. No kernel, no
mount, no loop device.

- **Crate:** `fio-ext4` (lib `fio_ext4`)
- **Version:** 1.0.0 — `Cargo.toml` is the single version location
- **Licence:** GPL-2.0-or-later
- **Sibling:** `../mkfs.ext4.rs` provides the on-disk format, the `BlockDevice`
  seam, the read layer and `fsck`. The two are developed together; `fio-ext4`
  depends on it by path.

## Shape

| Module | What it owns |
|---|---|
| `alloc` | block and inode allocation, and the four counters every allocation moves |
| `map` | an inode's block map — extent trees and indirect blocks alike |
| `dir` | directory entry insertion and removal within a block |
| `volume` | the public API: read, write, mkdir, unlink, stat, list |

## Rules

1. **Every mutation goes through `alloc`.** A bitmap changed without its group
   descriptor and the superblock is a filesystem that fails `fsck`.
2. **Rebuild block maps, do not patch them.** One code path, and no
   half-updated tree if a write fails partway.
3. **Every test ends by checking the filesystem.** A file writer that leaves
   `fsck` complaining has damaged the filesystem, not written a file.
4. **The kernel is the judge.** `tests/verify-on-linux.sh` is the test that
   counts: contents compared byte for byte after a real mount.

## Work plan

- [x] Allocator, block maps, directory entries, the `Volume` API
- [x] `fio-ext4` binary
- [x] Round-trip tests and the Linux verification harness
- [ ] Hard links, symlinks, rename
- [ ] Extended attributes
- [ ] Triple indirection for very large files on ext2/ext3
- [ ] Maintain `dir_index` rather than appending linearly
- [ ] Partial writes at an offset, rather than whole-file replace
