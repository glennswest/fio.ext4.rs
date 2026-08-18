//! Extent trees, and where their checksum lives.
//!
//! A file that needs more extents than the four an inode holds gets a leaf
//! block of its own, and that block carries a checksum tail. The tail does not
//! sit at the end of the block: it sits after the space `eh_max` entries would
//! occupy, which is what `EXT4_EXTENT_TAIL_OFFSET` computes. At 1 KiB and
//! 4 KiB blocks the two happen to coincide, so only 2 KiB (and 8 KiB, 32 KiB)
//! show the difference — and there it made every large file unreadable to the
//! kernel while our own reader, computing the offset the same wrong way, saw
//! nothing amiss.

use fio_ext4::Volume;
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::structs::extent::{self, ExtentHeader, ExtentIdx};

/// Write a file fragmented enough to need a leaf block, and return the volume.
async fn with_a_fragmented_file(block_size: u32) -> Volume<MemDevice> {
    let dev = MemDevice::new(256 * 1024 * 1024);
    let params = Params::new(Profile::Ext4)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000)
        .block_size(block_size);
    format(&dev, &params).await.unwrap();

    let mut vol = Volume::open(dev).await.unwrap();
    for i in 0..400 {
        vol.write(&format!("/f{i}"), &vec![b'x'; 40_000]).await.unwrap();
    }
    for i in (0..400).step_by(2) {
        vol.unlink(&format!("/f{i}")).await.unwrap();
    }
    vol.write("/big", &vec![b'b'; 1_716_616]).await.unwrap();
    vol.flush().await.unwrap();
    vol
}

#[tokio::test]
async fn an_extent_leaf_carries_its_checksum_where_the_kernel_reads_it() {
    for block_size in [1024u32, 2048, 4096] {
        let vol = with_a_fragmented_file(block_size).await;
        let inum = vol.lookup("/big").await.unwrap();
        let inode = vol.filesystem().read_inode(inum).await.unwrap();
        let root = ExtentHeader::decode(&inode.block).unwrap();
        assert!(
            root.depth > 0,
            "{block_size}: the file was meant to be fragmented past four extents"
        );

        for i in 0..root.entries as usize {
            let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
            let idx = ExtentIdx::decode(&inode.block[at..]);
            let buf = vol.filesystem().read_block(idx.leaf).await.unwrap();
            let leaf = ExtentHeader::decode(&buf).unwrap();

            // EXT4_EXTENT_TAIL_OFFSET: after eh_max entries, not at the end.
            let tail = extent::HEADER_LEN + leaf.max as usize * extent::ENTRY_LEN;
            assert!(tail + extent::TAIL_LEN <= buf.len(), "{block_size}: tail overruns");
            let stored = u32::from_le_bytes(buf[tail..tail + 4].try_into().unwrap());
            let want = mkfs_ext4::csum::extent_block_csum(
                vol.filesystem().csum_seed(),
                inum,
                inode.generation,
                &buf[..tail],
            );
            assert_eq!(
                stored, want,
                "{block_size}-byte blocks: leaf checksum is not what a reader computes"
            );
        }
    }
}
