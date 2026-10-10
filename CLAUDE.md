# CLAUDE.md — fio-ext4

Async userspace file I/O into an ext2/ext3/ext4 filesystem. No kernel, no
mount, no loop device.

- **Crate:** `fio-ext4` (lib `fio_ext4`)
- **Version:** 1.8.0 — `Cargo.toml` is the single version location
- **Licence:** MIT OR Apache-2.0
- **Sibling:** `../mkfs.ext4.rs` provides the on-disk format, the `BlockDevice`
  seam, the read layer, `fsck` and the write-back `CachedDevice`. `fio-ext4`
  depends on it by git, pinned to a tag (currently `v4.1.0`, see
  `Cargo.toml`). There is no `[patch]` in `Cargo.toml`: the build box has no
  sibling checkout, so one breaks `sc-build` (#6, #8). To develop against the
  sibling, put the patch in the gitignored `.cargo/config.toml` (the snippet
  is in `Cargo.toml`), and bump the tag before pushing.
- **Ships as:** a library, taken by git at a tag, plus the `fio-ext4` binary
  (default `cli` feature; `gzip` is also default). It is not published to
  crates.io, and it has no service, ports, config file or container image.

## Shape

| Module | What it owns |
|---|---|
| `alloc` | block and inode allocation, and the four counters every allocation moves |
| `map` | an inode's block map — extent trees and indirect blocks (up to triple) alike |
| `dir` | directory entry insertion and removal within a block |
| `index` | hash-indexed (`dir_index`) directories: conversion, lookup, rebuild |
| `volume` | the public API: read, write, write_at, mkdir, link, rename, symlink, xattrs, tar unpack/pack, stat, list |
| `tar` | streaming tar `Reader`/`Writer` over a runtime-free `Source`/`Sink` |
| `archive` | path-level unpack/pack, gzip detection, stdin/stdout |
| `bin/fio_ext4` | the CLI: `ls cat put get mkdir rm rmdir stat untar tar` |

## Rules

1. **Every mutation goes through `alloc`.** A bitmap changed without its group
   descriptor and the superblock is a filesystem that fails `fsck`.
2. **Rebuild block maps, do not patch them.** One code path, and no
   half-updated tree if a write fails partway.
3. **Every test ends by checking the filesystem.** A file writer that leaves
   `fsck` complaining has damaged the filesystem, not written a file.
4. **The kernel is the judge.** The `fio-ext4-test` container (`test/`) is
   the test that counts: images written in userspace, then `e2fsck -fn` and
   a real kernel's loop mount, contents compared byte for byte. stormcentral
   runs it on the test machines (`stormcentral test run fio.ext4.rs
   short|medium|long`); it needs a privileged pod, and reports the kernel
   checks as skip where it gets none. `sc-build` runs `cargo test`, which
   checks every image with `mkfs-ext4`'s `fsck`.

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
- [x] Hard links, symlinks, rename
- [x] Extended attributes
- [x] Triple indirection for very large files on ext2/ext3
- [x] Maintain `dir_index` rather than appending linearly
- [x] Partial writes at an offset, rather than whole-file replace
- [x] Tar unpack/pack, OCI whiteouts, gzip
- [x] Issues #6/#8 — `sc-build` failed: the `[patch]` to `../mkfs.ext4.rs`
      has no sibling on the build box. The patch moved to a gitignored
      `.cargo/config.toml`; `Cargo.lock` resolves mkfs-ext4 from the v3.0.0 tag.
- [ ] Issue #5: move the kernel verification into a `test/` container
      (`stormcentral test run fio.ext4.rs <suite>`, image
      `test-fio-ext4-rs-<suite>`). `test/build.sh` builds static musl
      `fio-ext4` and the pinned `mkfs-ext4` (`-p mkfs-ext4 --features
      mkfs-ext4/cli`, so it is the version `Cargo.lock` pins);
      `test/Containerfile` is fedora-minimal + e2fsprogs + util-linux;
      `/test` (`test/test.sh`) formats, fills, `e2fsck -fn`, loop-mounts,
      compares sha256, lets the kernel write, `e2fsck -fn` again; JSON lines
      out. `requires.toml`: privileged. No loop device or no ext4 in the
      kernel: skip, never pass. short = ext4; medium = ext4/ext3/ext2 + a tar
      unpack (symlink, hard link); long = medium + a 200 MiB file.
      `tests/verify-on-linux.sh` goes; README/CLAUDE rule 4 point here.
- [x] Issue #7 — the five fsck assertions use `check_only()` without force;
      at mkfs-ext4 4.0.0 a clean filesystem is skipped and they check nothing.
      Fix: `FsckOptions { force: true, ..check_only() }` (the field exists in
      v3.0.0), plus `report.directories > 0`, which only the passes set, so a
      skipped check fails the test on any version. Verified on dev against
      mkfs.ext4.rs 89091cd (skip-when-clean): forced, all pass; with force
      stripped, 37 assertions fail "fsck skipped the filesystem". When the pin
      moves to v4.0.0, `force(true)` and `CheckScope::Forced` become usable.
- [ ] Issue #10 — move the mkfs-ext4 pin from v3.0.0 to **v4.1.0** (a6e4519;
      backward-compatible with v4.0.0, replays a dirty journal before
      repairing). Stays a tag (#9 waits on mkfs.ext4.rs#19; the master's
      recommendation there is a release tag). Then the five fsck assertions
      use `FsckOptions::check_only().force(true)` and assert
      `report.scope == CheckScope::Forced`. Release as fio-ext4 1.8.0 so
      stormblock (stormblock#300) can follow to the same tag.
- [x] Issue #4 — the superblock read at byte 1024 refused on a 4096-byte-
      block device (mkfs.ext4.rs#5 is the write side). Every byte this crate
      touches goes through `mkfs_ext4::fs::Filesystem`, so the fix is there:
      as of mkfs-ext4 v2.2.x every device operation is a whole filesystem
      block at a block boundary, and `open` reads whole sectors before the
      block size is known. Here: the pin, and `tests/strict_sector.rs`
      opening, writing, unpacking and checking on `MemDevice::strict` —
      the device that refuses what a stormblock thin volume refuses.
- [x] Issue #3 — kill the measured 280x–1065x write amplification
      (mkfs.ext4.rs#4). Three parts, in this order:
      1. `unpack_file` stops calling `write_at` per 64 KiB chunk. `write_at`
         reads the whole file and rewrites all of it every call, so a 55 MB
         file costs ~O(n²) device bytes — the measured superlinearity. The
         streaming path allocates and writes each data block exactly once and
         builds the block map once, at the end of the file.
      2. `alloc_block` scans the goal group from the goal's own bit instead of
         bit 0, so sequential allocation stops re-walking the bitmap from the
         start — O(1) per block on the streaming path.
      3. Adopt mkfs-ext4 v2.1.0's `CachedDevice`: bump the tag, add
         `Volume::open_cached`, use it in the CLI, re-export the type. Metadata
         blocks then settle in cache and reach the device once per flush.
