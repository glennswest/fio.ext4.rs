# CLAUDE.md — fio-ext4

Async userspace file I/O into an ext2/ext3/ext4 filesystem. No kernel, no
mount, no loop device.

- **Crate:** `fio-ext4` (lib `fio_ext4`)
- **Version:** 1.3.2 — `Cargo.toml` is the single version location
- **Licence:** MIT OR Apache-2.0
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

## What lwext4 does not do

RouterOS reads these volumes with lwext4, and two of its limits look like bugs
here and are not. Both were established against the library itself, built from
`github.com/gkostka/lwext4` and pointed at our images (#2):

1. **It does not follow symlinks during path resolution.** `ext4_readlink` on
   `/lib64` returns `lib`, and `ext4_fopen` on `/lib64/ld-linux-x86-64.so.2`
   returns ENOENT. Since a dynamic ELF's `PT_INTERP` is usually reached through
   a symlink, `execve` fails with ENOENT for every binary while data files read
   perfectly — which reads as "large files are broken" and is nothing of the
   kind. **The control that settles it:** a filesystem made by real `mke2fs`
   and written by the real Linux kernel fails lwext4 in exactly the same way.
2. **It refuses to mount a default modern ext4 at all.** `metadata_csum_seed`
   (incompat `0x2000`) is outside `EXT4_SUPPORTED_FINCOM`, and unsupported
   incompat features are a hard `ENOTSUP`. Real `mke2fs` 1.47.3 sets that
   feature by default, so its images are refused identically to ours. Format
   with `-O ^metadata_csum_seed` for a volume stock lwext4 must mount.

Neither is worth working around by writing something other than what `mke2fs`
writes. Before concluding a foreign reader has found a defect, reproduce it
against a real `mke2fs` filesystem written by the real kernel — if that fails
too, the finding is about the reader.

## Work plan

- [x] Allocator, block maps, directory entries, the `Volume` API
- [x] `fio-ext4` binary
- [x] Round-trip tests and the Linux verification harness
- [ ] Hard links, symlinks, rename
- [ ] Extended attributes
- [ ] Triple indirection for very large files on ext2/ext3
- [ ] Maintain `dir_index` rather than appending linearly
- [ ] Partial writes at an offset, rather than whole-file replace
