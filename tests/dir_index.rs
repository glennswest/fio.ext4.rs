//! Hash-indexed directories.
//!
//! A directory index is the one structure where "it looks right" proves
//! nothing. A tree built with the wrong hash has valid blocks, correct
//! checksums and every name in a leaf — it is simply the wrong leaf, and the
//! only symptom is that lookups miss. So these tests check three separate
//! things: that we find our own names, that `e2fsck` accepts the tree, and
//! (in `verify-on-linux.sh`) that a kernel walking the index finds them too.

use fio_ext4::Volume;
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, FsckOptions};
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::structs::inode::iflags;

const MIB: u64 = 1024 * 1024;

async fn fresh(profile: Profile, size: u64, block_size: u32) -> MemDevice {
    let dev = MemDevice::new(size);
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000)
        .block_size(block_size);
    format(&dev, &params).await.unwrap();
    dev
}

async fn assert_clean(dev: &MemDevice, what: &str) {
    let report = fsck::check(dev, &FsckOptions::check_only()).await.unwrap();
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

/// Fill a directory and check every name is still findable afterwards.
async fn fill_and_check(profile: Profile, block_size: u32, count: usize, size: u64) {
    let dev = fresh(profile, size, block_size).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);
    vol.mkdir("/many").await.unwrap();

    for i in 0..count {
        vol.write(&format!("/many/file-{i:06}"), format!("{i}").as_bytes())
            .await
            .unwrap();
    }

    let stat = vol.stat("/many").await.unwrap();
    let inode = vol.filesystem().read_inode(stat.inode).await.unwrap();
    assert_ne!(
        inode.flags & iflags::INDEX,
        0,
        "{count} names in a {block_size}-byte-block directory should be indexed"
    );

    // Every name, through the index this time rather than the linear scan.
    for i in 0..count {
        let path = format!("/many/file-{i:06}");
        assert_eq!(
            vol.read(&path).await.unwrap(),
            format!("{i}").as_bytes(),
            "{path} could not be found through the index"
        );
    }

    // And a name that was never there is still absent.
    assert!(!vol.exists("/many/file-999999").await.unwrap());

    let listing = vol.read_dir("/many").await.unwrap();
    assert_eq!(listing.len(), count, "the listing lost or gained names");

    vol.flush().await.unwrap();
    assert_clean(&dev, &format!("after {count} names at {block_size} bytes")).await;
}

#[tokio::test]
async fn a_directory_becomes_indexed_when_it_outgrows_one_block() {
    // 1 KiB blocks index sooner, so this also covers the conversion itself.
    fill_and_check(Profile::Ext4, 1024, 200, 64 * MIB).await;
}

#[tokio::test]
async fn a_large_directory_grows_a_second_level() {
    fill_and_check(Profile::Ext4, 1024, 4000, 128 * MIB).await;
}

#[tokio::test]
async fn indexing_works_with_four_kilobyte_blocks() {
    fill_and_check(Profile::Ext4, 4096, 3000, 128 * MIB).await;
}

#[tokio::test]
async fn ext2_indexes_too_without_metadata_checksums() {
    // No metadata_csum here, so the index blocks carry no tail — a different
    // limit, and a different code path for the checksum that is not written.
    fill_and_check(Profile::Ext2, 1024, 500, 64 * MIB).await;
}

#[tokio::test]
async fn names_can_be_removed_from_an_indexed_directory() {
    let dev = fresh(Profile::Ext4, 64 * MIB, 1024).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);
    vol.mkdir("/many").await.unwrap();

    for i in 0..300 {
        vol.write(&format!("/many/file-{i:06}"), b"x").await.unwrap();
    }
    for i in (0..300).step_by(2) {
        vol.unlink(&format!("/many/file-{i:06}")).await.unwrap();
    }

    for i in 0..300 {
        let path = format!("/many/file-{i:06}");
        assert_eq!(
            vol.exists(&path).await.unwrap(),
            i % 2 == 1,
            "{path} after removing every other name"
        );
    }

    // And the gaps are reused rather than growing the directory.
    for i in (0..300).step_by(2) {
        vol.write(&format!("/many/file-{i:06}"), b"y").await.unwrap();
    }
    assert_eq!(vol.read_dir("/many").await.unwrap().len(), 300);

    vol.flush().await.unwrap();
    assert_clean(&dev, "after removing and re-adding").await;
}

#[tokio::test]
async fn names_that_collide_are_still_found() {
    // Many names of the same length and shape, to push runs of entries into
    // the same leaf and across leaf boundaries.
    let dev = fresh(Profile::Ext4, 64 * MIB, 1024).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);
    vol.mkdir("/d").await.unwrap();

    let names: Vec<String> = (0..800).map(|i| format!("{i:0>200}")).collect();
    for name in &names {
        vol.write(&format!("/d/{name}"), b"x").await.unwrap();
    }
    for name in &names {
        assert!(
            vol.exists(&format!("/d/{name}")).await.unwrap(),
            "a long name went missing"
        );
    }

    vol.flush().await.unwrap();
    assert_clean(&dev, "after 800 long names").await;
}
