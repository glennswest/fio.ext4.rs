//! Writing into a `meta_bg` filesystem (#18, mkfs.ext4.rs#33, stormblock#512).
//!
//! Under `meta_bg` the group descriptor table is not one run after each
//! superblock copy: each meta block group (the groups one descriptor block
//! describes) keeps its own descriptor block in its first, second and last
//! group. mkfs-ext4 up to v4.1.2 flushed descriptors as if the table were
//! classic, writing the whole table after every backup superblock — over that
//! group's bitmaps and inode table. `Volume::flush` calls that flush, so any
//! write into an ext4 big enough to be formatted `meta_bg` (past ~192 TiB at
//! 4 KiB blocks) damaged it. These tests write through `Volume` into `meta_bg`
//! filesystems of more than 64 groups — several meta block groups — reopen,
//! read back and end with a forced fsck; the 16 GiB case also asks real
//! `e2fsck -fn` when the host has it.

use std::process::Command;

use fio_ext4::Volume;
use mkfs_ext4::device::{BlockDevice, FileDevice, MemDevice};
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, CheckScope, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

async fn assert_clean<D: BlockDevice>(dev: &D, what: &str) {
    // `e2fsck -fn`: check even though the superblock says clean, as
    // mkfs-ext4 skips a clean filesystem without `force` (#7, #10).
    let forced = FsckOptions::check_only().force(true);
    let report = fsck::check(dev, &forced).await.unwrap();
    assert_eq!(report.scope, CheckScope::Forced, "fsck skipped the filesystem");
    // Only the passes count directories: a check that ran but read nothing.
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

fn params(block_size: u32) -> Params {
    Params::new(Profile::Ext4)
        .block_size(block_size)
        .features("meta_bg")
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ seed).collect()
}

/// Write files that reach groups well past the first meta block group, flush,
/// and return what was written where. Also asserts the writes did reach a
/// group outside meta block group 0, or the test would prove nothing.
async fn write_across_groups<D: BlockDevice>(dev: &D, big: usize) -> Vec<(String, Vec<u8>)> {
    let mut vol = Volume::open(dev).await.unwrap();
    vol.set_time(1_700_000_000);
    let fs = vol.filesystem();
    assert!(fs.meta_bg(), "the format did not set meta_bg");
    let per_desc_block = fs.desc_per_block();
    let groups = fs.group_count();
    assert!(groups > 64, "only {groups} groups");
    assert!(groups / per_desc_block >= 4, "fewer than four meta block groups");
    let before: Vec<u32> = fs.group_descs().iter().map(|d| d.free_blocks_count).collect();

    let mut files = Vec::new();
    vol.mkdir_all("/var/lib/data").await.unwrap();
    for d in 0..24 {
        let dir = format!("/d{d:02}");
        vol.mkdir(&dir).await.unwrap();
        let path = format!("{dir}/f");
        let data = pattern(256 * 1024 + d * 4097, d as u8);
        vol.write(&path, &data).await.unwrap();
        files.push((path, data));
    }
    let data = pattern(big, 0x5a);
    vol.write("/var/lib/data/big.bin", &data).await.unwrap();
    files.push(("/var/lib/data/big.bin".into(), data));
    vol.write("/etc-hostname", b"router\n").await.unwrap();
    files.push(("/etc-hostname".into(), b"router\n".to_vec()));
    vol.flush().await.unwrap();

    let after: Vec<u32> = vol.filesystem().group_descs().iter().map(|d| d.free_blocks_count).collect();
    let touched_far = (per_desc_block as usize..after.len()).any(|g| after[g] != before[g]);
    assert!(touched_far, "no write reached a group past meta block group 0");
    files
}

async fn read_back<D: BlockDevice>(dev: &D, files: &[(String, Vec<u8>)]) {
    let vol = Volume::open(dev).await.unwrap();
    for (path, data) in files {
        assert_eq!(&vol.read(path).await.unwrap(), data, "{path} reads back");
    }
}

/// 1 GiB with 1 KiB blocks: 128 groups of 8 MiB, 16 descriptors per block
/// with 64-byte descriptors, so eight meta block groups. In memory.
#[tokio::test]
async fn writes_into_a_meta_bg_filesystem_of_128_groups() {
    let dev = MemDevice::new(GIB);
    format(&dev, &params(1024)).await.unwrap();
    assert_clean(&dev, "fresh meta_bg format").await;

    // 160 MiB spans twenty-odd 8 MiB groups: into meta block group 1 and on.
    let files = write_across_groups(&dev, (160 * MIB) as usize).await;
    assert_clean(&dev, "after writing").await;
    read_back(&dev, &files).await;

    // Write again over what is there, through the cache, and check again.
    let mut vol = Volume::open_cached(&dev).await.unwrap();
    vol.set_time(1_700_000_100);
    vol.unlink("/d03/f").await.unwrap();
    vol.write_at("/var/lib/data/big.bin", 7 * MIB, b"over").await.unwrap();
    vol.write("/d05/g", &pattern(3 * MIB as usize, 9)).await.unwrap();
    vol.flush().await.unwrap();
    drop(vol);
    assert_clean(&dev, "after the second, cached write").await;
}

/// 16 GiB with 4 KiB blocks, sparse on disk: 128 groups, two descriptor
/// blocks — the 4 KiB layout a 256 TiB stormblock volume has, at a size a
/// build can hold. Checked by our fsck and, when present, real `e2fsck -fn`.
#[tokio::test]
async fn writes_into_a_16_gib_meta_bg_filesystem_and_e2fsck_agrees() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("fs.img");
    let dev = FileDevice::create(&img, 16 * GIB).await.unwrap();
    format(&dev, &params(4096)).await.unwrap();

    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);
    let fs = vol.filesystem();
    assert!(fs.meta_bg() && fs.group_count() >= 128);
    let before: Vec<u32> = fs.group_descs().iter().map(|d| d.free_blocks_count).collect();
    // 600 MiB: past the 128 MiB groups' first few, and with flex_bg its
    // metadata lands in group 0's flex group, so the data reaches later ones.
    let big = pattern((600 * MIB) as usize, 0x33);
    vol.mkdir("/data").await.unwrap();
    vol.write("/data/big.bin", &big).await.unwrap();
    vol.write("/data/small", b"meta_bg\n").await.unwrap();
    vol.flush().await.unwrap();
    let after: Vec<u32> = vol.filesystem().group_descs().iter().map(|d| d.free_blocks_count).collect();
    assert_ne!(before, after, "the write moved no group's counters");
    drop(vol);

    assert_clean(&dev, "16 GiB meta_bg after writing").await;
    let vol = Volume::open(&dev).await.unwrap();
    assert_eq!(vol.read("/data/big.bin").await.unwrap(), big);
    assert_eq!(vol.read("/data/small").await.unwrap(), b"meta_bg\n");
    drop(vol);
    drop(dev);

    // A build user's PATH may not hold /usr/sbin.
    let e2fsck = ["e2fsck", "/usr/sbin/e2fsck", "/sbin/e2fsck"]
        .into_iter()
        .find(|p| Command::new(p).arg("-V").output().is_ok())
        .unwrap_or("e2fsck");
    match Command::new(e2fsck).arg("-fn").arg(&img).output() {
        Err(e) => eprintln!("e2fsck not run ({e}): our forced fsck is the only check"),
        Ok(out) => {
            eprintln!("e2fsck -fn: rc {:?}", out.status.code());
            assert!(
                out.status.success(),
                "e2fsck -fn rc {:?}:\n{}{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
