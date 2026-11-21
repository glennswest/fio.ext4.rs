//! The review findings of #23, one test or more each, named by finding id.
//!
//! Inside the crate rather than under `tests/`, because several cases need
//! a state no public writer makes — an attribute block shared by three
//! inodes, a journal holding a committed transaction — and build it through
//! the crate's own handles on the filesystem. Each case that writes ends with
//! a forced fsck; the one that refuses to open writes nothing.

use mkfs_ext4::bytes::{get_u32, put_u32};
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::fsck::{self, CheckScope, FsckOptions};
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::structs::xattr;
use mkfs_ext4::IncompatFeatures;

use crate::error::Error;
use crate::map;
use crate::tar::{self, EntryKind, Header};
use crate::volume::{Special, Volume};
use crate::Attrs;

const MIB: u64 = 1024 * 1024;

async fn fresh(profile: Profile, block_size: u32, size: u64) -> MemDevice {
    let dev = MemDevice::new(size);
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000)
        .block_size(block_size);
    format(&dev, &params).await.unwrap();
    dev
}

async fn assert_clean(dev: &MemDevice, what: &str) {
    let forced = FsckOptions::check_only().force(true);
    let report = fsck::check(dev, &forced).await.unwrap();
    assert_eq!(report.scope, CheckScope::Forced, "fsck skipped the filesystem");
    assert!(report.directories > 0, "fsck read no directories");
    assert!(
        report.is_clean(),
        "{what}: filesystem is not clean:\n{}",
        report
            .problems
            .iter()
            .map(|p| format!("  [pass {} {}] {}", p.pass, p.code, p.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

async fn open(dev: &MemDevice) -> Volume<&MemDevice> {
    let mut vol = Volume::open(dev).await.unwrap();
    vol.set_time(1_700_000_000);
    vol
}

/// Free blocks and inodes, as the superblock counts them right now.
fn free(vol: &Volume<&MemDevice>) -> (u64, u32) {
    let sb = vol.fs.superblock();
    (sb.free_blocks_count, sb.free_inodes_count)
}

// ---- names (3235dae01a21, 48f2ceb27e92) ----

#[tokio::test]
async fn dot_and_dotdot_are_never_a_name_to_change() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.mkdir("/d").await.unwrap();
        vol.mkdir("/d/empty").await.unwrap();
        vol.write("/f", b"file").await.unwrap();
        let before = free(&vol);

        for bad in ["/d/empty/.", "/d/empty/..", "/d/."] {
            let e = vol.rmdir(bad).await.unwrap_err();
            assert!(matches!(e, Error::InvalidName(_)), "rmdir {bad}: {e}");
        }
        for (from, to) in [("/d/empty/.", "/moved"), ("/f", "/d/.."), ("/f", "/d/.")] {
            let e = vol.rename(from, to).await.unwrap_err();
            assert!(matches!(e, Error::InvalidName(_)), "rename {from} {to}: {e}");
        }
        assert!(vol.write("/d/.", b"x").await.is_err());
        assert!(vol.mkdir("/d/..").await.is_err());
        assert!(vol.symlink("/d/.", "x").await.is_err());
        assert!(vol.unlink("/d/..").await.is_err());

        // Refused before an inode is taken, not after.
        let long = format!("/{}", "n".repeat(256));
        assert!(matches!(vol.write(&long, b"x").await.unwrap_err(), Error::InvalidName(_)));
        assert!(matches!(vol.mkdir("/a\0b").await.unwrap_err(), Error::InvalidName(_)));
        assert_eq!(free(&vol), before, "a refused name allocated something");

        assert!(vol.stat("/d/empty").await.unwrap().is_dir());
        assert_eq!(vol.read("/f").await.unwrap(), b"file");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after refused dot names").await;
}

// ---- rename across types (ed975422b071) ----

#[tokio::test]
async fn rename_will_not_swap_a_directory_for_a_file() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.write("/f", b"keep me").await.unwrap();
        vol.mkdir("/d").await.unwrap();

        let e = vol.rename("/d", "/f").await.unwrap_err();
        assert!(matches!(e, Error::NotADirectory(_)), "dir onto file: {e}");
        let e = vol.rename("/f", "/d").await.unwrap_err();
        assert!(matches!(e, Error::IsADirectory(_)), "file onto dir: {e}");

        assert_eq!(vol.read("/f").await.unwrap(), b"keep me");
        assert!(vol.stat("/d").await.unwrap().is_dir());

        // Like onto like still replaces.
        vol.write("/g", b"new").await.unwrap();
        vol.rename("/g", "/f").await.unwrap();
        assert_eq!(vol.read("/f").await.unwrap(), b"new");
        vol.mkdir("/e").await.unwrap();
        vol.rename("/e", "/d").await.unwrap();
        assert!(!vol.exists("/e").await.unwrap());
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after typed renames").await;
}

// ---- data writes to special inodes (9534db6f345d, 978776e2fe03) ----

#[tokio::test]
async fn file_writes_leave_symlinks_and_devices_alone() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.symlink("/s", "target").await.unwrap();
        vol.mknod("/null", Special::CharDevice { major: 1, minor: 3 }, &Attrs::mode(0o666))
            .await
            .unwrap();
        vol.mknod("/pipe", Special::Fifo, &Attrs::mode(0o600)).await.unwrap();
        let before = free(&vol);

        for path in ["/s", "/null", "/pipe"] {
            assert!(vol.write(path, b"x").await.is_err(), "write {path}");
            assert!(vol.append(path, b"x").await.is_err(), "append {path}");
            assert!(vol.write_at(path, 3, b"x").await.is_err(), "write_at {path}");
        }
        assert_eq!(free(&vol), before);
        assert_eq!(vol.read_link("/s").await.unwrap(), "target");
        assert_eq!(vol.device_numbers("/null").await.unwrap(), (1, 3));
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after refused special writes").await;
}

// ---- symlink target length (f737f3d6e9e4) ----

async fn symlink_limits(block_size: u32) {
    let dev = fresh(Profile::Ext4, block_size, 32 * MIB).await;
    let max = (block_size as usize).min(4096) - 1;
    {
        let mut vol = open(&dev).await;
        let longest = "t".repeat(max);
        vol.symlink("/longest", &longest).await.unwrap();
        assert_eq!(vol.read_link("/longest").await.unwrap(), longest);

        let before = free(&vol);
        let e = vol.symlink("/too-long", &"t".repeat(max + 1)).await.unwrap_err();
        assert!(matches!(e, Error::InvalidPath(_)), "{block_size}: {e}");
        assert!(vol.symlink("/empty", "").await.is_err());
        assert_eq!(free(&vol), before, "{block_size}: a refused symlink allocated");
        assert!(!vol.exists("/too-long").await.unwrap());
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, &format!("symlinks at {block_size}")).await;
}

#[tokio::test]
async fn symlink_targets_are_bounded_before_allocating_1k() {
    symlink_limits(1024).await;
}

#[tokio::test]
async fn symlink_targets_are_bounded_before_allocating_4k() {
    symlink_limits(4096).await;
}

// ---- shared attribute blocks (12afcde622bc) ----

/// Point `path`'s inode at attribute block `block`, as the kernel does when
/// two inodes carry the same attribute set.
async fn share_xattr_block(vol: &mut Volume<&MemDevice>, path: &str, block: u64) {
    let inum = vol.lookup(path).await.unwrap();
    let mut inode = vol.fs.read_inode(inum).await.unwrap();
    assert_eq!(inode.file_acl, 0);
    inode.file_acl = block;
    inode.blocks += vol.fs.block_size() as u64 / 512;
    vol.fs.write_inode(inum, &inode).await.unwrap();
}

async fn set_refcount(vol: &Volume<&MemDevice>, block: u64, refs: u32) {
    let mut buf = vol.fs.read_block(block).await.unwrap();
    put_u32(&mut buf, xattr::block_off::H_REFCOUNT, refs);
    if vol.fs.has_metadata_csum() {
        xattr::stamp_block_csum(&mut buf, vol.fs.csum_seed(), block);
    }
    vol.fs.write_block(block, &buf).await.unwrap();
}

async fn refcount(vol: &Volume<&MemDevice>, block: u64) -> u32 {
    get_u32(&vol.fs.read_block(block).await.unwrap(), xattr::block_off::H_REFCOUNT)
}

async fn file_acl(vol: &Volume<&MemDevice>, path: &str) -> u64 {
    let inum = vol.lookup(path).await.unwrap();
    vol.fs.read_inode(inum).await.unwrap().file_acl
}

async fn names(vol: &Volume<&MemDevice>, path: &str) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = vol
        .list_xattrs(path)
        .await
        .unwrap()
        .into_iter()
        .map(|x| (x.name, x.value))
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_shared_attribute_block_is_cloned_and_counted_down() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    // Too big for the inode, so it spills into a block.
    let label = vec![b'L'; 300];
    let shared;
    {
        let mut vol = open(&dev).await;
        for p in ["/a", "/b", "/c"] {
            vol.write(p, p.as_bytes()).await.unwrap();
        }
        vol.set_xattr("/a", "user.big", &label).await.unwrap();
        shared = file_acl(&vol, "/a").await;
        assert_ne!(shared, 0, "the attribute did not spill");
        share_xattr_block(&mut vol, "/b", shared).await;
        share_xattr_block(&mut vol, "/c", shared).await;
        set_refcount(&vol, shared, 3).await;
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "three inodes sharing one block").await;

    let only_label = vec![("user.big".to_string(), label.clone())];
    {
        let mut vol = open(&dev).await;
        // Changing b's set gives b its own block; a and c keep theirs.
        vol.set_xattr("/b", "user.more", &[b'M'; 200]).await.unwrap();
        assert_ne!(file_acl(&vol, "/b").await, shared, "b rewrote the shared block");
        assert_eq!(refcount(&vol, shared).await, 2);
        assert_eq!(names(&vol, "/a").await, only_label, "a's attributes changed");
        assert_eq!(names(&vol, "/c").await, only_label, "c's attributes changed");
        assert_eq!(names(&vol, "/b").await.len(), 2);
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after cloning for b").await;

    {
        let mut vol = open(&dev).await;
        // Deleting c drops its reference; the block stays for a.
        vol.unlink("/c").await.unwrap();
        assert_eq!(refcount(&vol, shared).await, 1);
        assert_eq!(names(&vol, "/a").await, only_label);
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after unlinking c").await;

    {
        let mut vol = open(&dev).await;
        let (blocks_before, _) = free(&vol);
        // The last reference: moving a's set inline frees the block.
        vol.remove_xattr("/a", "user.big").await.unwrap();
        assert_eq!(file_acl(&vol, "/a").await, 0);
        assert_eq!(free(&vol).0, blocks_before + 1, "the last reference did not free");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after the last reference").await;
}

#[tokio::test]
async fn removing_a_shared_set_inline_counts_down_without_freeing() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    let shared;
    {
        let mut vol = open(&dev).await;
        vol.write("/a", b"a").await.unwrap();
        vol.write("/b", b"b").await.unwrap();
        vol.set_xattr("/a", "user.big", &[b'L'; 300]).await.unwrap();
        shared = file_acl(&vol, "/a").await;
        share_xattr_block(&mut vol, "/b", shared).await;
        set_refcount(&vol, shared, 2).await;
        let (blocks_before, _) = free(&vol);
        vol.remove_xattr("/b", "user.big").await.unwrap();
        assert_eq!(free(&vol).0, blocks_before, "a shared block was freed");
        assert_eq!(refcount(&vol, shared).await, 1);
        assert_eq!(vol.get_xattr("/a", "user.big").await.unwrap().unwrap(), vec![b'L'; 300]);
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after b dropped its share").await;
}

// ---- i_blocks keeps the attribute block (16cc8481732e, abb26904f7ff) ----

async fn spilled_rewrites(profile: Profile, block_size: u32) {
    let what = format!("{profile:?} {block_size}");
    let dev = fresh(profile, block_size, 32 * MIB).await;
    let bs = block_size as u64;
    let sectors = bs / 512;
    {
        let mut vol = open(&dev).await;
        vol.write("/f", &[1u8; 3000]).await.unwrap();
        vol.set_xattr("/f", "user.big", &[b'L'; 300]).await.unwrap();
        assert_ne!(file_acl(&vol, "/f").await, 0, "{what}: did not spill");

        vol.write("/f", &[2u8; 5000]).await.unwrap();
        let data = 5000u64.div_ceil(bs);
        let stat = vol.stat("/f").await.unwrap();
        // Only ext2's indirect map can need a block of its own here, and
        // 5000 bytes stay within the direct pointers.
        assert_eq!(stat.blocks, (data + 1) * sectors, "{what}: rewrite");
        vol.append("/f", b"tail").await.unwrap();
        vol.write_at("/f", 10, b"mid").await.unwrap();
        vol.write("/f", b"").await.unwrap();
        assert_eq!(vol.stat("/f").await.unwrap().blocks, sectors, "{what}: emptied");
        vol.write("/f", &[3u8; 100]).await.unwrap();

        // A directory with a spilled label, grown past one block: the
        // conversion to an index and its rebuilds recompute `i_blocks` too.
        vol.mkdir("/d").await.unwrap();
        vol.set_xattr("/d", "user.big", &[b'D'; 300]).await.unwrap();
        for i in 0..200 {
            vol.write(&format!("/d/a-rather-long-name-to-fill-blocks-{i:04}"), b"x")
                .await
                .unwrap();
        }
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, &format!("{what}: rewrites with a spilled attribute")).await;
}

#[tokio::test]
async fn rewrites_keep_the_attribute_block_in_i_blocks_ext4() {
    spilled_rewrites(Profile::Ext4, 4096).await;
}

#[tokio::test]
async fn rewrites_keep_the_attribute_block_in_i_blocks_ext2() {
    spilled_rewrites(Profile::Ext2, 1024).await;
}

// ---- archives ----

fn tar_file(path: &str) -> Header {
    Header {
        path: path.into(),
        kind: EntryKind::File,
        mode: 0o644,
        mtime: 1_700_000_000,
        ..Default::default()
    }
}

async fn archive(entries: &[(Header, &[u8])]) -> Vec<u8> {
    let mut writer = tar::Writer::new(Vec::new());
    for (header, data) in entries {
        writer.append(header, data).await.unwrap();
    }
    writer.finish().await.unwrap();
    writer.into_inner()
}

/// Findings f12ada4fdef6, d3df6a641775, 285fc60e938a: nothing in an archive
/// reaches outside the directory it is unpacked into.
#[tokio::test]
async fn archive_paths_cannot_climb_out_of_the_destination() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.write("/victim", b"untouched").await.unwrap();
        vol.mkdir("/sandbox").await.unwrap();
        vol.flush().await.unwrap();

        let link = Header {
            path: "link".into(),
            kind: EntryKind::HardLink,
            link: "../victim".into(),
            ..Default::default()
        };
        let climbs: Vec<(&str, Vec<u8>)> = vec![
            ("entry", archive(&[(tar_file("../victim"), &b"pwned"[..])]).await),
            ("nested", archive(&[(tar_file("a/../../victim"), &b"pwned"[..])]).await),
            ("hard link", archive(&[(link, &b""[..])]).await),
            ("whiteout", archive(&[(tar_file("../.wh.victim"), &b""[..])]).await),
        ];
        for (what, bytes) in &climbs {
            for layer in [false, true] {
                let res = if layer {
                    vol.unpack_tar_layer(tar::Bytes::new(bytes), "/sandbox").await
                } else {
                    vol.unpack_tar_into(tar::Bytes::new(bytes), "/sandbox").await
                };
                let e = res.unwrap_err();
                assert!(matches!(e, Error::InvalidPath(_)), "{what} (layer {layer}): {e}");
                assert_eq!(vol.read("/victim").await.unwrap(), b"untouched", "{what}");
            }
        }

        // A marker that names the directory itself, or its parent.
        for marker in [".wh..", ".wh...", "d/.wh."] {
            let bytes = archive(&[(tar_file(marker), &b""[..])]).await;
            let e = vol
                .unpack_tar_layer(tar::Bytes::new(&bytes), "/sandbox")
                .await
                .unwrap_err();
            assert!(matches!(e, Error::InvalidName(_)), "{marker}: {e}");
        }
        assert!(vol.exists("/sandbox").await.unwrap());
        assert_eq!(vol.read("/victim").await.unwrap(), b"untouched");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after refused climbs").await;
}

/// Finding 2f271dfded9e: a marker that arrives after this layer's own
/// entries hides only the lower layers.
#[tokio::test]
async fn late_whiteouts_spare_the_layer_s_own_entries() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        let lower = archive(&[
            (tar_file("d/old"), &b"lower"[..]),
            (tar_file("d/sub/lowerfile"), b"lower"),
            (tar_file("g/lowerg"), b"lower"),
        ])
        .await;
        vol.unpack_tar_layer(tar::Bytes::new(&lower), "/").await.unwrap();

        let upper = archive(&[
            (tar_file("d/new"), &b"upper"[..]),
            (tar_file("d/sub/upper"), b"upper"),
            (tar_file("d/.wh..wh..opq"), b""),
            (tar_file("f/kept"), b"upper"),
            (tar_file("f/.wh.kept"), b""),
            (tar_file("g/.wh.lowerg"), b""),
        ])
        .await;
        let report = vol.unpack_tar_layer(tar::Bytes::new(&upper), "/").await.unwrap();

        assert_eq!(vol.read("/d/new").await.unwrap(), b"upper", "opaque took this layer's file");
        assert_eq!(vol.read("/d/sub/upper").await.unwrap(), b"upper");
        assert!(!vol.exists("/d/old").await.unwrap(), "opaque kept a lower file");
        assert!(!vol.exists("/d/sub/lowerfile").await.unwrap(), "opaque kept a lower file below");
        assert_eq!(vol.read("/f/kept").await.unwrap(), b"upper", "a late whiteout took this layer's file");
        assert!(!vol.exists("/g/lowerg").await.unwrap(), "an ordinary whiteout did nothing");
        assert_eq!(report.removed, 3, "d/old, d/sub/lowerfile, g/lowerg");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after late whiteouts").await;
}

/// Finding 4d4d5c59ed24: a symlink keeps its owner, time and labels through
/// unpack and pack.
#[tokio::test]
async fn symlink_metadata_survives_the_round_trip() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    let label = b"system_u:object_r:bin_t:s0".to_vec();
    {
        let mut vol = open(&dev).await;
        let link = Header {
            path: "usr/lib64".into(),
            kind: EntryKind::Symlink,
            link: "lib".into(),
            mode: 0o777,
            uid: 1000,
            gid: 1001,
            mtime: 1_600_000_000,
            xattrs: vec![("security.selinux".into(), label.clone())],
            ..Default::default()
        };
        let bytes = archive(&[(link, &b""[..])]).await;
        vol.unpack_tar(&bytes).await.unwrap();

        let stat = vol.stat("/usr/lib64").await.unwrap();
        assert_eq!((stat.uid, stat.gid, stat.mtime), (1000, 1001, 1_600_000_000));
        assert_eq!(vol.read_link("/usr/lib64").await.unwrap(), "lib");
        assert_eq!(
            vol.get_xattr("/usr/lib64", "security.selinux").await.unwrap(),
            Some(label.clone())
        );

        let packed = vol.pack_tar("/").await.unwrap();
        let entries = tar::read(&packed).await.unwrap();
        let back = entries
            .iter()
            .find(|e| e.header.path == "usr/lib64")
            .expect("the symlink was not packed");
        assert_eq!(back.header.kind, EntryKind::Symlink);
        assert_eq!((back.header.uid, back.header.gid), (1000, 1001));
        assert_eq!(back.header.xattrs, vec![("security.selinux".to_string(), label.clone())]);
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after a labelled symlink").await;
}

/// A sink that keeps what it is given and remembers the largest write.
#[derive(Default)]
struct Recording {
    bytes: Vec<u8>,
    largest: usize,
}

impl tar::Sink for &mut Recording {
    async fn write_all(&mut self, buf: &[u8]) -> crate::Result<()> {
        self.largest = self.largest.max(buf.len());
        self.bytes.extend_from_slice(buf);
        Ok(())
    }
}

/// Finding f06f9c5e6605: packing streams a file in bounded pieces rather
/// than reading it whole, and holes still read as zeroes.
#[tokio::test]
async fn pack_streams_file_contents() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    let big: Vec<u8> = (0..3 * MIB as usize + 123).map(|i| (i % 253) as u8).collect();
    {
        let mut vol = open(&dev).await;
        vol.write("/big", &big).await.unwrap();
        vol.write("/small", b"tiny").await.unwrap();
        vol.write("/empty", b"").await.unwrap();

        let mut sink = Recording::default();
        let report = vol.pack_tar_to(&mut sink, "/").await.unwrap();
        assert_eq!(report.files, 3);
        assert!(
            sink.largest <= 64 * 1024,
            "a write of {} bytes reached the sink: the file was gathered",
            sink.largest
        );

        let entries = tar::read(&sink.bytes).await.unwrap();
        let get = |p: &str| entries.iter().find(|e| e.header.path == p).unwrap().data.clone();
        assert_eq!(get("big"), big);
        assert_eq!(get("small"), b"tiny");
        assert!(get("empty").is_empty());
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after packing").await;
}

// ---- moving an indexed directory (78d76493e7c8, 83c24c7dbb2d) ----

/// The inode a directory's `..` names, read from its first block — for an
/// indexed directory, the htree root, which is where the kernel reads it.
/// Not `lookup("…/..")`: at the pinned mkfs-ext4 (v4.1.3) an indexed lookup
/// hashes `..` like any name and misses it (fixed in mkfs.ext4.rs#41).
async fn dotdot_of(vol: &Volume<&MemDevice>, dir: &str) -> u32 {
    use mkfs_ext4::structs::dirent;
    let inode = vol.fs.read_inode(vol.lookup(dir).await.unwrap()).await.unwrap();
    let first = vol.fs.resolve_block(&inode, 0).await.unwrap().unwrap();
    let block = vol.fs.read_block(first).await.unwrap();
    let entries = dirent::parse_block(&block).unwrap();
    assert_eq!(entries[0].name, b".", "{dir}: first entry");
    assert_eq!(entries[1].name, b"..", "{dir}: second entry");
    entries[1].inode
}

async fn move_indexed(block_size: u32) {
    let what = format!("ext4 {block_size}");
    let dev = fresh(Profile::Ext4, block_size, 32 * MIB).await;
    let names: Vec<String> = (0..400)
        .map(|i| format!("an-entry-with-a-longish-name-so-the-directory-grows-{i:05}"))
        .collect();
    {
        let mut vol = open(&dev).await;
        assert!(vol.fs.has_metadata_csum(), "{what}: want checksums on");
        vol.mkdir("/a").await.unwrap();
        vol.mkdir("/b").await.unwrap();
        vol.mkdir("/a/big").await.unwrap();
        for n in &names {
            vol.write(&format!("/a/big/{n}"), n.as_bytes()).await.unwrap();
        }
        let big = vol.lookup("/a/big").await.unwrap();
        let inode = vol.fs.read_inode(big).await.unwrap();
        assert!(vol.is_indexed(&inode), "{what}: the directory never became indexed");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, &format!("{what}: before the move")).await;
    {
        let mut vol = open(&dev).await;
        vol.rename("/a/big", "/b/big").await.unwrap();
        assert!(!vol.exists("/a/big").await.unwrap());
        assert_eq!(
            dotdot_of(&vol, "/b/big").await,
            vol.lookup("/b").await.unwrap(),
            "{what}: .. still names the old parent"
        );
        assert_eq!(vol.stat("/a").await.unwrap().links, 2, "{what}: old parent's links");
        assert_eq!(vol.stat("/b").await.unwrap().links, 3, "{what}: new parent's links");
        for n in names.iter().step_by(37) {
            assert_eq!(vol.read(&format!("/b/big/{n}")).await.unwrap(), n.as_bytes());
        }
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, &format!("{what}: after the move")).await;
}

#[tokio::test]
async fn an_indexed_directory_moves_between_parents_4k() {
    move_indexed(4096).await;
}

#[tokio::test]
async fn an_indexed_directory_moves_between_parents_1k() {
    move_indexed(1024).await;
}

// ---- allocation goal (402731625099) ----

/// Writing a file allocates each block next to the last, without re-walking
/// the bits already taken — linear, across a block-group boundary too.
#[tokio::test]
async fn ordinary_writes_allocate_in_linear_time() {
    let dev = fresh(Profile::Ext4, 1024, 32 * MIB).await;
    // 10,000 blocks of 1 KiB: past the 8,192-block first group.
    let blocks = 10_000u64;
    let data: Vec<u8> = (0..blocks as usize * 1024).map(|i| (i % 249) as u8).collect();
    {
        let mut vol = open(&dev).await;
        let before = vol.alloc.bits_scanned;
        vol.write("/big", &data).await.unwrap();
        let scanned = vol.alloc.bits_scanned - before;
        // Once per block, plus a pass over each group's metadata and the
        // journal — not the ~50 million a fixed goal costs.
        assert!(
            scanned < blocks + 4 * 8192,
            "{scanned} bits scanned for {blocks} blocks: allocation is not linear"
        );
        assert_eq!(vol.read("/big").await.unwrap(), data);
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after a large ordinary write").await;
}

// ---- a journal that needs recovery (27a761f4df5e) ----

fn be32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(buf[at..at + 4].try_into().unwrap())
}

/// Leave one committed transaction in the journal, logging `data` for block
/// `home`, and the needs_recovery flag set: what a crash after the commit
/// and before the checkpoint leaves. No checksums, 32-bit tags — the
/// simplest log JBD2 writes.
async fn leave_unreplayed(dev: &MemDevice, home: u64, data: &[u8], jbd2_incompat: u32) {
    use mkfs_ext4::device::BlockDevice;
    use mkfs_ext4::journal::{off, JBD2_MAGIC};
    use mkfs_ext4::structs::superblock::ino;

    let mut fs = Filesystem::open(dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let jinode = fs.read_inode(ino::JOURNAL).await.unwrap();
    let jmap = map::read_block_list(&fs, &jinode).await.unwrap();
    let mut jsb = fs.read_block(jmap[0]).await.unwrap();
    let first = be32(&jsb, off::S_FIRST) as usize;
    let seq = be32(&jsb, off::S_SEQUENCE);

    let header = |kind: u32| {
        let mut b = vec![0u8; bs];
        b[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&kind.to_be_bytes());
        b[8..12].copy_from_slice(&seq.to_be_bytes());
        b
    };
    // Descriptor: one 8-byte tag (block, checksum, flags = LAST_TAG), then
    // the journal's UUID, since the tag does not say SAME_UUID.
    let mut desc = header(1);
    desc[12..16].copy_from_slice(&(home as u32).to_be_bytes());
    desc[18..20].copy_from_slice(&8u16.to_be_bytes());
    desc[20..36].copy_from_slice(&jsb[off::S_UUID..off::S_UUID + 16]);
    fs.write_block(jmap[first], &desc).await.unwrap();
    fs.write_block(jmap[first + 1], data).await.unwrap();
    fs.write_block(jmap[first + 2], &header(2)).await.unwrap();
    // Whatever follows must not continue the log.
    fs.write_block(jmap[first + 3], &vec![0u8; bs]).await.unwrap();

    jsb[off::S_FEATURE_INCOMPAT..off::S_FEATURE_INCOMPAT + 4]
        .copy_from_slice(&jbd2_incompat.to_be_bytes());
    jsb[off::S_START..off::S_START + 4].copy_from_slice(&(first as u32).to_be_bytes());
    fs.write_block(jmap[0], &jsb).await.unwrap();

    fs.superblock_mut().feature_incompat.insert(IncompatFeatures::RECOVER);
    fs.flush_superblock().await.unwrap();
    fs.device().flush().await.unwrap();
}

async fn first_block_of(vol: &Volume<&MemDevice>, path: &str) -> u64 {
    let inode = vol.fs.read_inode(vol.lookup(path).await.unwrap()).await.unwrap();
    vol.fs.resolve_block(&inode, 0).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_journal_needing_recovery_is_replayed_on_open() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    let home;
    {
        let mut vol = open(&dev).await;
        vol.write("/f", &[b'o'; 4096]).await.unwrap();
        vol.flush().await.unwrap();
        home = first_block_of(&vol, "/f").await;
    }
    leave_unreplayed(&dev, home, &[b'n'; 4096], 0).await;
    {
        let mut vol = open(&dev).await;
        assert!(
            !vol.fs.superblock().feature_incompat.contains(IncompatFeatures::RECOVER),
            "needs_recovery still set after open"
        );
        assert_eq!(
            vol.read("/f").await.unwrap(),
            vec![b'n'; 4096],
            "the committed transaction was not replayed"
        );
        vol.write("/after", b"written after the replay").await.unwrap();
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after replay and a write").await;
}

#[tokio::test]
async fn a_journal_that_cannot_be_replayed_is_refused() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    let home;
    {
        let mut vol = open(&dev).await;
        vol.write("/f", &[b'o'; 4096]).await.unwrap();
        vol.flush().await.unwrap();
        home = first_block_of(&vol, "/f").await;
    }
    // fast_commit: a separate replay protocol, not done here.
    leave_unreplayed(&dev, home, &[b'n'; 4096], 0x20).await;
    // Bound first: a match on it as the block's tail would keep the
    // borrow of `dev` alive past `dev` itself.
    let opened = Volume::open(&dev).await;
    match opened {
        Err(Error::Unsupported(why)) => assert!(why.contains("recovery"), "{why}"),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("opened for writing over an unreplayed journal"),
    };
}

#[tokio::test]
async fn a_recovery_flag_without_a_journal_is_cleared() {
    let dev = fresh(Profile::Ext2, 1024, 32 * MIB).await;
    {
        let mut fs = Filesystem::open(&dev).await.unwrap();
        fs.superblock_mut().feature_incompat.insert(IncompatFeatures::RECOVER);
        fs.flush_superblock().await.unwrap();
    }
    {
        let mut vol = open(&dev).await;
        assert!(!vol.fs.superblock().feature_incompat.contains(IncompatFeatures::RECOVER));
        vol.write("/f", b"x").await.unwrap();
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after clearing a stale flag").await;
}
