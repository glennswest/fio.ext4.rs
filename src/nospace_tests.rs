//! Finding 4b2834a8dd5e (#25): an operation that runs out of space gives back
//! every block it took. Before the fix, a failed multi-block allocation kept
//! the blocks it had, and so did `write_data`, the streaming unpack and the
//! map builders: a write bigger than the free space ate the free space while
//! failing, and the blocks stayed marked in use with no owner — fsck's "block
//! bitmap differences", and nothing a retry or a delete could reclaim.
//!
//! Each case compares the superblock's free-block count before and after the
//! failure, then ends in a forced fsck, which also catches a bitmap that
//! disagrees with the counters.

use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, CheckScope, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

use crate::error::Error;
use crate::tar::{self, Header};
use crate::volume::Volume;

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

fn free_blocks(vol: &Volume<&MemDevice>) -> u64 {
    vol.fs.superblock().free_blocks_count
}

fn free_inodes(vol: &Volume<&MemDevice>) -> u32 {
    vol.fs.superblock().free_inodes_count
}

fn bs(vol: &Volume<&MemDevice>) -> u64 {
    vol.fs.block_size() as u64
}

fn bytes(blocks: u64, block_size: u64) -> Vec<u8> {
    (0..blocks * block_size).map(|i| (i % 251) as u8).collect()
}

/// Fill the volume until exactly `leave` blocks are free, with files whose
/// size is the free space less what their own map needs. A try that runs out
/// of space is retried one block smaller — which works only because a failed
/// write takes nothing.
async fn fill(vol: &mut Volume<&MemDevice>, leave: u64) {
    let block_size = bs(vol);
    let mut i = 0;
    while free_blocks(vol) > leave {
        let mut n = free_blocks(vol) - leave;
        let path = format!("/fill{i}");
        loop {
            match vol.write(&path, &vec![0xa5; (n * block_size) as usize]).await {
                Ok(_) => break,
                Err(Error::NoSpace) if n > 1 => n -= 1,
                Err(e) => panic!("filling {path} with {n} blocks: {e}"),
            }
        }
        i += 1;
    }
    assert_eq!(free_blocks(vol), leave, "fill overshot");
}

#[tokio::test]
async fn alloc_blocks_is_all_or_nothing() {
    let dev = fresh(Profile::Ext4, 4096, 16 * MIB).await;
    {
        let mut vol = open(&dev).await;
        let before = free_blocks(&vol);
        let e = vol
            .alloc
            .alloc_blocks(&mut vol.fs, 0, before + 1)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::NoSpace), "{e}");
        assert_eq!(free_blocks(&vol), before, "a failed alloc_blocks kept blocks");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after a refused alloc_blocks").await;
}

async fn too_big_to_write(profile: Profile, block_size: u32, size: u64) {
    let dev = fresh(profile, block_size, size).await;
    {
        let mut vol = open(&dev).await;
        let bsz = bs(&vol);
        let keep = bytes(300, bsz);
        vol.write("/keep", &keep).await.unwrap();
        vol.write("/big", b"").await.unwrap();
        let before = free_blocks(&vol);

        // A new file's contents bigger than the free space.
        let e = vol.write("/big", &bytes(before + 8, bsz)).await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "write: {e}");
        assert_eq!(free_blocks(&vol), before, "a failed write kept blocks");
        assert!(vol.read("/big").await.unwrap().is_empty());

        // An append past the free space leaves the file as it was: its old
        // map and every block it owned.
        let e = vol.append("/keep", &bytes(before + 8, bsz)).await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "append: {e}");
        assert_eq!(free_blocks(&vol), before, "a failed append kept blocks");
        assert_eq!(vol.read("/keep").await.unwrap(), keep);

        // And so does a write_at past the end.
        let e = vol.write_at("/keep", 300 * bsz, &bytes(before + 8, bsz)).await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "write_at: {e}");
        assert_eq!(free_blocks(&vol), before, "a failed write_at kept blocks");
        assert_eq!(vol.read("/keep").await.unwrap(), keep);

        // The space is still there to be used, and comes back whole.
        vol.write("/big", &bytes(before / 2, bsz)).await.unwrap();
        vol.unlink("/big").await.unwrap();
        assert_eq!(free_blocks(&vol), before, "the space came back short");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after writes bigger than the free space").await;
}

#[tokio::test]
async fn a_write_bigger_than_the_free_space_takes_nothing_ext4() {
    too_big_to_write(Profile::Ext4, 4096, 16 * MIB).await;
}

#[tokio::test]
async fn a_write_bigger_than_the_free_space_takes_nothing_ext2() {
    too_big_to_write(Profile::Ext2, 1024, 8 * MIB).await;
}

/// The data fits and the map does not: on ext2 every block up to the free
/// space goes to data, then the single-indirect block is taken and the
/// double-indirect one is not there. Both the data and the indirect block
/// the map builder already held go back.
#[tokio::test]
async fn a_map_that_does_not_fit_gives_back_its_blocks_and_the_data() {
    let dev = fresh(Profile::Ext2, 1024, 8 * MIB).await;
    {
        let mut vol = open(&dev).await;
        let bsz = bs(&vol);
        vol.write("/f", b"").await.unwrap();
        let before = free_blocks(&vol);
        // Past 12 + 256 blocks the map needs indirect, double indirect and
        // leaves; one free block is left over for the first of them.
        assert!(before > 12 + 256 + 4, "test volume too small");
        let e = vol.write("/f", &bytes(before - 1, bsz)).await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "{e}");
        assert_eq!(free_blocks(&vol), before, "a failed map build kept blocks");
        assert!(vol.read("/f").await.unwrap().is_empty());
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after a map that did not fit").await;
}

async fn archive_of(path: &str, data: &[u8]) -> Vec<u8> {
    let mut header = Header::file(path, data.len() as u64);
    header.mtime = 1_700_000_000;
    let mut writer = tar::Writer::new(Vec::new());
    writer.append(&header, data).await.unwrap();
    writer.finish().await.unwrap();
    writer.into_inner()
}

/// A file over the 1 MiB gather limit is streamed block by block; running out
/// of space, or out of archive, partway gives back what it streamed.
#[tokio::test]
async fn a_streamed_unpack_that_fails_gives_back_what_it_streamed() {
    let dev = fresh(Profile::Ext4, 4096, 16 * MIB).await;
    {
        let mut vol = open(&dev).await;
        let bsz = bs(&vol);
        let data = bytes(3 * MIB / bsz, bsz);

        // A truncated archive: the header promises 3 MiB, 2 MiB follow.
        let mut cut = archive_of("short", &data).await;
        cut.truncate(512 + 2 * MIB as usize);
        let before = free_blocks(&vol);
        assert!(vol.unpack_tar(&cut).await.is_err(), "a truncated archive unpacked");
        assert_eq!(free_blocks(&vol), before, "a truncated unpack kept blocks");
        assert_eq!(vol.stat("/short").await.unwrap().size, 0);

        // Out of space: 2 MiB free, 3 MiB to place.
        fill(&mut vol, 2 * MIB / bsz).await;
        let whole = archive_of("big", &data).await;
        let before = free_blocks(&vol);
        let e = vol.unpack_tar(&whole).await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "{e}");
        assert_eq!(free_blocks(&vol), before, "a failed streamed unpack kept blocks");
        assert_eq!(vol.stat("/big").await.unwrap().size, 0);
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after streamed unpacks that failed").await;
}

/// No block for a new directory, or for a symlink's long target: the inode
/// taken for it goes back too.
#[tokio::test]
async fn a_full_volume_refuses_new_names_without_taking_inodes() {
    let dev = fresh(Profile::Ext4, 4096, 16 * MIB).await;
    {
        let mut vol = open(&dev).await;
        fill(&mut vol, 0).await;
        let (blocks, inodes) = (free_blocks(&vol), free_inodes(&vol));

        let e = vol.mkdir("/d").await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "mkdir: {e}");
        let long = "t".repeat(200);
        let e = vol.symlink("/l", &long).await.unwrap_err();
        assert!(matches!(e, Error::NoSpace), "symlink: {e}");
        assert_eq!((free_blocks(&vol), free_inodes(&vol)), (blocks, inodes));
        assert!(!vol.exists("/d").await.unwrap());
        assert!(!vol.exists("/l").await.unwrap());

        // Space again, and both work.
        vol.unlink("/fill0").await.unwrap();
        vol.mkdir("/d").await.unwrap();
        vol.symlink("/l", &long).await.unwrap();
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after names refused on a full volume").await;
}
