//! A volume on a device that refuses anything but whole sectors at a sector
//! boundary — a stormblock thin volume with a 4096-byte logical block (#4).
//!
//! The superblock lives at byte 1024. Asked for on its own it is one sector
//! in on such a device, and refused; the same for a lone inode. Every byte
//! this crate touches goes through `mkfs_ext4::fs::Filesystem`, which as of
//! mkfs-ext4 v2.2.2 reads and writes whole blocks only — so the test here is
//! that the whole `Volume` surface, cached and uncached, works on a device
//! that enforces it, and leaves a filesystem that checks clean.

use fio_ext4::Volume;
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

async fn strict_4k(size: u64) -> MemDevice {
    let dev = MemDevice::strict(size, 4096);
    let params = Params::new(Profile::Ext4)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&dev, &params)
        .await
        .expect("formatting on a device enforcing 4 KiB sectors");
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

/// The state-volume check from the issue: open, and read what is there.
#[tokio::test]
async fn opens_and_reads_an_empty_volume_on_4k_sectors() {
    let dev = strict_4k(64 * MIB).await;
    let vol = Volume::open(&dev).await.expect("opening reads byte 1024 through its sector");
    let root = vol.read_dir("/").await.unwrap();
    assert!(
        root.iter().any(|e| e.name == "lost+found"),
        "a fresh volume lists lost+found: {root:?}"
    );
}

#[tokio::test]
async fn writes_and_reads_back_uncached_on_4k_sectors() {
    let dev = strict_4k(64 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir_all("/etc/stormblock").await.unwrap();
    vol.mkdir("/var").await.unwrap();
    vol.write("/etc/stormblock/state.json", b"{\"epoch\":1}\n").await.unwrap();
    let big: Vec<u8> = (0..(3 * MIB) as usize).map(|i| (i % 251) as u8).collect();
    vol.write("/var/big.bin", &big).await.unwrap_or_else(|e| panic!("{e}"));
    vol.flush().await.unwrap();

    assert_eq!(vol.read("/etc/stormblock/state.json").await.unwrap(), b"{\"epoch\":1}\n");
    assert_eq!(vol.read("/var/big.bin").await.unwrap(), big);
    drop(vol);

    // Reopen: the superblock and inodes written above are read back whole.
    let vol = Volume::open(&dev).await.unwrap();
    assert_eq!(vol.stat("/var/big.bin").await.unwrap().size, 3 * MIB);
    drop(vol);

    assert_clean(&dev, "after uncached writes on 4 KiB sectors").await;
}

/// The path stormblock actually takes: through `CachedDevice`, whose
/// write-back reaches the device in coalesced runs of whole blocks.
#[tokio::test]
async fn writes_and_reads_back_cached_on_4k_sectors() {
    let dev = strict_4k(64 * MIB).await;
    let mut vol = Volume::open_cached(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir_all("/usr/lib").await.unwrap();
    for i in 0..64 {
        let body = vec![i as u8; 1500 + i * 37];
        vol.write(&format!("/usr/lib/lib{i}.so"), &body).await.unwrap();
    }
    let big: Vec<u8> = (0..(5 * MIB) as usize).map(|i| (i % 253) as u8).collect();
    vol.write("/usr/lib/big.so", &big).await.unwrap();
    vol.flush().await.unwrap();

    assert_eq!(vol.read("/usr/lib/lib7.so").await.unwrap(), vec![7u8; 1500 + 7 * 37]);
    assert_eq!(vol.read("/usr/lib/big.so").await.unwrap(), big);
    drop(vol);

    // Everything the cache held has reached the strict device, in whole
    // sectors, or the check below cannot read it.
    let vol = Volume::open(&dev).await.unwrap();
    // 64 small libraries and the big one; "." and ".." are not listed.
    assert_eq!(vol.read_dir("/usr/lib").await.unwrap().len(), 65);
    drop(vol);

    assert_clean(&dev, "after cached writes on 4 KiB sectors").await;
}
