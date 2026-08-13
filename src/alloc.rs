//! Allocating and freeing blocks and inodes.
//!
//! Every mutation goes through here, because every mutation has to keep four
//! things in step: the bitmap, the group descriptor's counters, the
//! superblock's totals, and the bitmap's checksum. Doing that in one place is
//! what keeps the filesystem passing `fsck` after a write.

use std::collections::BTreeMap;

use mkfs_ext4::csum;
use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::structs::group_desc::bg_flags;

use crate::error::{Error, Result};

/// Bitmaps held in memory while a volume is open, written back on flush.
///
/// A write that touches one block would otherwise read, modify and write a
/// whole bitmap block for every allocation.
#[derive(Default)]
pub(crate) struct BitmapCache {
    block: BTreeMap<u32, Vec<u8>>,
    inode: BTreeMap<u32, Vec<u8>>,
    dirty_block: BTreeMap<u32, bool>,
    dirty_inode: BTreeMap<u32, bool>,
}

impl BitmapCache {
    /// Whether anything is still waiting to be written back.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty_block.values().any(|d| *d) || self.dirty_inode.values().any(|d| *d)
    }
}

/// Allocation over a live filesystem.
pub(crate) struct Allocator {
    pub(crate) cache: BitmapCache,
}

impl Allocator {
    pub(crate) fn new() -> Self {
        Self {
            cache: BitmapCache::default(),
        }
    }

    /// The block bitmap for a group, read once and kept.
    async fn block_bitmap<D: BlockDevice>(
        &mut self,
        fs: &Filesystem<D>,
        group: u32,
    ) -> Result<&mut Vec<u8>> {
        if !self.cache.block.contains_key(&group) {
            let bitmap = fs.read_block_bitmap(group).await?;
            self.cache.block.insert(group, bitmap);
        }
        Ok(self.cache.block.get_mut(&group).expect("just inserted"))
    }

    /// The inode bitmap for a group, read once and kept.
    async fn inode_bitmap<D: BlockDevice>(
        &mut self,
        fs: &Filesystem<D>,
        group: u32,
    ) -> Result<&mut Vec<u8>> {
        if !self.cache.inode.contains_key(&group) {
            let bitmap = fs.read_inode_bitmap(group).await?;
            self.cache.inode.insert(group, bitmap);
        }
        Ok(self.cache.inode.get_mut(&group).expect("just inserted"))
    }

    /// Allocate one block, preferring the group `goal` is in.
    pub(crate) async fn alloc_block<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem<D>,
        goal: u64,
    ) -> Result<u64> {
        let sb = fs.superblock().clone();
        let groups = sb.group_count();
        let start_group = if goal >= sb.first_data_block as u64 && goal < sb.blocks_count {
            fs.group_of_block(goal)
        } else {
            0
        };

        // Search from the goal's group outwards, wrapping — the same shape of
        // search the kernel does, and it keeps a file's blocks near each other.
        for step in 0..groups {
            let group = (start_group + step) % groups;
            let in_group = fs.group_block_count(group) as u64;
            let first = fs.group_first_block(group);

            let bitmap = self.block_bitmap(fs, group).await?;
            let mut found = None;
            for i in 0..in_group {
                if !Filesystem::<D>::test_bit(bitmap, i) {
                    Filesystem::<D>::set_bit(bitmap, i, true);
                    found = Some(first + i);
                    break;
                }
            }

            if let Some(block) = found {
                self.cache.dirty_block.insert(group, true);
                // The group descriptor and the superblock both count free
                // blocks, and both have to move.
                let desc = &mut fs.group_descs_mut()[group as usize];
                desc.free_blocks_count = desc.free_blocks_count.saturating_sub(1);
                // Once a block is written into a group, its bitmap is real.
                desc.flags &= !bg_flags::BLOCK_UNINIT;
                let sb = fs.superblock_mut();
                sb.free_blocks_count = sb.free_blocks_count.saturating_sub(1);
                return Ok(block);
            }
        }

        Err(Error::NoSpace)
    }

    /// Allocate `count` blocks, near `goal` where possible.
    pub(crate) async fn alloc_blocks<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem<D>,
        goal: u64,
        count: u64,
    ) -> Result<Vec<u64>> {
        let mut out = Vec::with_capacity(count as usize);
        let mut next_goal = goal;
        for _ in 0..count {
            let block = self.alloc_block(fs, next_goal).await?;
            next_goal = block + 1;
            out.push(block);
        }
        Ok(out)
    }

    /// Give a block back.
    pub(crate) async fn free_block<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem<D>,
        block: u64,
    ) -> Result<()> {
        let sb = fs.superblock().clone();
        if block < sb.first_data_block as u64 || block >= sb.blocks_count {
            return Ok(());
        }
        let group = fs.group_of_block(block);
        let index = block - fs.group_first_block(group);

        let bitmap = self.block_bitmap(fs, group).await?;
        if !Filesystem::<D>::test_bit(bitmap, index) {
            // Already free; freeing twice would corrupt the counters.
            return Ok(());
        }
        Filesystem::<D>::set_bit(bitmap, index, false);
        self.cache.dirty_block.insert(group, true);

        let desc = &mut fs.group_descs_mut()[group as usize];
        desc.free_blocks_count += 1;
        let sb = fs.superblock_mut();
        sb.free_blocks_count += 1;
        Ok(())
    }

    /// Allocate an inode. `is_dir` keeps the directory tally straight.
    pub(crate) async fn alloc_inode<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem<D>,
        near: u32,
        is_dir: bool,
    ) -> Result<u32> {
        let sb = fs.superblock().clone();
        let groups = sb.group_count();
        let start_group = if near > 0 && near <= sb.inodes_count {
            (near - 1) / sb.inodes_per_group
        } else {
            0
        };

        for step in 0..groups {
            let group = (start_group + step) % groups;
            let bitmap = self.inode_bitmap(fs, group).await?;

            let mut found = None;
            for i in 0..sb.inodes_per_group as u64 {
                let inum = group * sb.inodes_per_group + i as u32 + 1;
                if inum > sb.inodes_count {
                    break;
                }
                // Never hand out a reserved inode.
                if inum < sb.first_ino {
                    continue;
                }
                if !Filesystem::<D>::test_bit(bitmap, i) {
                    Filesystem::<D>::set_bit(bitmap, i, true);
                    found = Some((inum, i));
                    break;
                }
            }

            if let Some((inum, index)) = found {
                self.cache.dirty_inode.insert(group, true);
                let ipg = sb.inodes_per_group;
                let desc = &mut fs.group_descs_mut()[group as usize];
                desc.free_inodes_count = desc.free_inodes_count.saturating_sub(1);
                if is_dir {
                    desc.used_dirs_count += 1;
                }
                desc.flags &= !bg_flags::INODE_UNINIT;
                // itable_unused counts never-used inodes at the tail of the
                // table. Handing one out from within that tail shortens it.
                let used_upto = index as u32 + 1;
                if desc.itable_unused > ipg - used_upto {
                    desc.itable_unused = ipg - used_upto;
                }
                let sb = fs.superblock_mut();
                sb.free_inodes_count = sb.free_inodes_count.saturating_sub(1);
                return Ok(inum);
            }
        }

        Err(Error::NoInodes)
    }

    /// Give an inode back.
    pub(crate) async fn free_inode<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem<D>,
        inum: u32,
        was_dir: bool,
    ) -> Result<()> {
        let sb = fs.superblock().clone();
        if inum == 0 || inum > sb.inodes_count {
            return Ok(());
        }
        let group = (inum - 1) / sb.inodes_per_group;
        let index = ((inum - 1) % sb.inodes_per_group) as u64;

        let bitmap = self.inode_bitmap(fs, group).await?;
        if !Filesystem::<D>::test_bit(bitmap, index) {
            return Ok(());
        }
        Filesystem::<D>::set_bit(bitmap, index, false);
        self.cache.dirty_inode.insert(group, true);

        let desc = &mut fs.group_descs_mut()[group as usize];
        desc.free_inodes_count += 1;
        if was_dir {
            desc.used_dirs_count = desc.used_dirs_count.saturating_sub(1);
        }
        let sb = fs.superblock_mut();
        sb.free_inodes_count += 1;
        Ok(())
    }

    /// Write back every bitmap that changed, restamping its checksum.
    pub(crate) async fn flush<D: BlockDevice>(&mut self, fs: &mut Filesystem<D>) -> Result<()> {
        let sb = fs.superblock().clone();
        let has_csum = fs.has_metadata_csum();
        let seed = fs.csum_seed();
        let bb_len = (sb.blocks_per_group as usize).div_ceil(8);
        let ib_len = (sb.inodes_per_group as usize).div_ceil(8);

        for (&group, bitmap) in &self.cache.block {
            if !self.cache.dirty_block.get(&group).copied().unwrap_or(false) {
                continue;
            }
            let desc = fs.group_descs()[group as usize];
            fs.write_block(desc.block_bitmap, bitmap).await?;
            if has_csum {
                let c = csum::bitmap_csum(seed, &bitmap[..bb_len]);
                fs.group_descs_mut()[group as usize].block_bitmap_csum = c;
            }
        }
        for (&group, bitmap) in &self.cache.inode {
            if !self.cache.dirty_inode.get(&group).copied().unwrap_or(false) {
                continue;
            }
            let desc = fs.group_descs()[group as usize];
            fs.write_block(desc.inode_bitmap, bitmap).await?;
            if has_csum {
                let c = csum::bitmap_csum(seed, &bitmap[..ib_len]);
                fs.group_descs_mut()[group as usize].inode_bitmap_csum = c;
            }
        }

        self.cache.dirty_block.clear();
        self.cache.dirty_inode.clear();
        Ok(())
    }
}
