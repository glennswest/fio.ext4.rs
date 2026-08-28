//! Write amplification stays gone (issue #3).
//!
//! Unpacking a 55 MB file once cost ~14.3 million device operations — the
//! streaming path rewrote the whole file per 64 KiB chunk, the allocator
//! rescanned the bitmap from bit 0 per block, and every metadata touch went to
//! the device. The fix is three parts (streamed unpack, goal-aware allocation,
//! the write-back `CachedDevice`), and this test pins the outcome: placing a
//! multi-megabyte file must cost on the order of its size in device I/O, not
//! the square of it.

use std::sync::atomic::{AtomicU64, Ordering};

use fio_ext4::tar::{self, EntryKind, Header};
use fio_ext4::Volume;
use mkfs_ext4::device::{BlockDevice, MemDevice};
use mkfs_ext4::error::Result as FsResult;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

/// A device that counts what reaches it.
struct CountingDevice {
    inner: MemDevice,
    reads: AtomicU64,
    writes: AtomicU64,
}

impl CountingDevice {
    fn new(inner: MemDevice) -> Self {
        Self {
            inner,
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    fn ops(&self) -> (u64, u64) {
        (
            self.reads.load(Ordering::Relaxed),
            self.writes.load(Ordering::Relaxed),
        )
    }
}

#[async_trait::async_trait]
impl BlockDevice for CountingDevice {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> FsResult<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, buf).await
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> FsResult<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write_at(offset, buf).await
    }

    async fn flush(&self) -> FsResult<()> {
        self.inner.flush().await
    }
}

#[tokio::test]
async fn a_large_file_costs_its_size_not_its_square() {
    // Format on the bare device; the counters watch only the unpack.
    let mem = MemDevice::new(32 * MIB);
    let params = Params::new(Profile::Ext4)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&mem, &params).await.unwrap();
    let dev = CountingDevice::new(mem);

    // One 4 MiB file — comfortably past the 1 MiB inline limit, so this takes
    // the streaming path that once rewrote the whole file per chunk.
    let payload: Vec<u8> = (0..4 * MIB).map(|i| (i % 251) as u8).collect();
    let mut writer = tar::Writer::new(Vec::new());
    writer
        .append(
            &Header {
                path: "big.bin".into(),
                kind: EntryKind::File,
                mode: 0o644,
                mtime: 1_700_000_000,
                size: payload.len() as u64,
                ..Default::default()
            },
            &payload,
        )
        .await
        .unwrap();
    writer.finish().await.unwrap();
    let archive = writer.into_inner();

    {
        let mut vol = Volume::open_cached(&dev).await.unwrap();
        vol.set_time(1_700_000_000);
        let report = vol.unpack_tar(&archive).await.unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(report.bytes, 4 * MIB);
        vol.flush().await.unwrap();
    }

    let (reads, writes) = dev.ops();
    // The file is 1024 blocks. Uncached and rewritten per 64 KiB chunk this
    // was tens of thousands of operations each way; streamed, allocated
    // sequentially and flushed through the cache it is a handful of coalesced
    // writes and one read per distinct metadata block. The bound is loose on
    // purpose — it fails on a regression to any of the three causes, not on
    // noise.
    assert!(
        reads + writes < 500,
        "unpacking 4 MiB cost {reads} reads + {writes} writes — amplification is back"
    );

    // Cheap is worthless unless the result is right — and clean.
    let vol = Volume::open(&dev).await.unwrap();
    let back = vol.read("/big.bin").await.unwrap();
    assert_eq!(back.len(), payload.len());
    assert!(back == payload, "contents differ after streamed unpack");
    drop(vol);

    let report = fsck::check(&dev.inner, &FsckOptions::check_only())
        .await
        .unwrap();
    assert!(
        report.is_clean(),
        "filesystem not clean after streamed unpack:\n{}",
        report
            .problems
            .iter()
            .map(|p| format!("  [pass {} {}] {}", p.pass, p.code, p.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
