//! The review findings of #23, one test or more each, named by finding id.
//!
//! Inside the crate rather than under `tests/`, because several cases need
//! a state no public writer makes — an attribute block shared by three
//! inodes, a journal holding a committed transaction — and build it through
//! the crate's own handles on the filesystem. Each case ends with a forced
//! fsck, as every test here does.

use mkfs_ext4::bytes::{get_u32, put_u32};
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, CheckScope, FsckOptions};
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::structs::xattr;

use crate::error::Error;
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
