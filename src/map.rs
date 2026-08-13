//! Block maps.
//!
//! An inode's block map is read out into a plain list of physical blocks in
//! logical order, modified, and written back. Rebuilding the whole map rather
//! than patching it in place costs a little work per write and buys something
//! worth more: one code path, and no partially-updated tree if a write fails
//! halfway.
//!
//! Both shapes are handled — extent trees for ext4, indirect blocks for ext2
//! and ext3 — because the crate is expected to work on whatever `mkfs.ext4`
//! wrote, and that includes filesystems without extents.

use mkfs_ext4::bytes::put_u32;
use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::fs::{BlockKind, Filesystem};
use mkfs_ext4::structs::extent::{self, Extent, ExtentHeader, ExtentIdx};
use mkfs_ext4::structs::inode::{iflags, Inode, N_BLOCKS, NDIR_BLOCKS};

use crate::alloc::Allocator;
use crate::error::{Error, Result};

/// A hole in a sparse file.
pub(crate) const HOLE: u64 = 0;

/// Read an inode's block map as a list of physical blocks in logical order.
///
/// Holes read as [`HOLE`].
pub(crate) async fn read_block_list<D: BlockDevice>(
    fs: &Filesystem<D>,
    inode: &Inode,
) -> Result<Vec<u64>> {
    let mut list: Vec<u64> = Vec::new();
    fs.walk_blocks(inode, |b| {
        if b.kind == BlockKind::Data {
            if let Some(logical) = b.logical {
                let idx = logical as usize;
                if list.len() <= idx {
                    list.resize(idx + 1, HOLE);
                }
                list[idx] = b.physical;
            }
        }
    })
    .await?;
    Ok(list)
}

/// Every block the inode's map structure itself occupies.
pub(crate) async fn map_metadata_blocks<D: BlockDevice>(
    fs: &Filesystem<D>,
    inode: &Inode,
) -> Result<Vec<u64>> {
    let mut meta = Vec::new();
    fs.walk_blocks(inode, |b| {
        if b.kind == BlockKind::Metadata {
            meta.push(b.physical);
        }
    })
    .await?;
    Ok(meta)
}

/// Coalesce a block list into extents, skipping holes.
fn to_extents(list: &[u64]) -> Vec<Extent> {
    let mut out: Vec<Extent> = Vec::new();
    for (logical, &physical) in list.iter().enumerate() {
        if physical == HOLE {
            continue;
        }
        // Extend the previous extent when this block continues it, both
        // logically and physically.
        if let Some(last) = out.last_mut() {
            let ends_at = last.block as u64 + last.len as u64;
            if ends_at == logical as u64
                && last.start + last.len as u64 == physical
                && (last.len as u32) < extent::INIT_MAX_LEN - 1
            {
                last.len += 1;
                continue;
            }
        }
        out.push(Extent {
            block: logical as u32,
            len: 1,
            start: physical,
        });
    }
    out
}

/// Write a block list back into an inode, allocating whatever map structure it
/// needs and freeing whatever the old map used.
///
/// Returns the number of blocks the map structure itself occupies, which the
/// caller folds into `i_blocks`.
pub(crate) async fn write_block_list<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    alloc: &mut Allocator,
    inum: u32,
    inode: &mut Inode,
    list: &[u64],
) -> Result<u64> {
    // Whatever the old map used is about to be replaced.
    let old_meta = map_metadata_blocks(fs, inode).await?;
    for block in old_meta {
        alloc.free_block(fs, block).await?;
    }

    if inode.uses_extents() {
        write_extent_map(fs, alloc, inum, inode, list).await
    } else {
        write_indirect_map(fs, alloc, inode, list).await
    }
}

/// Build an extent tree for `list`.
async fn write_extent_map<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    alloc: &mut Allocator,
    inum: u32,
    inode: &mut Inode,
    list: &[u64],
) -> Result<u64> {
    let block_size = fs.block_size();
    let extents = to_extents(list);
    let inline_max = ExtentHeader::max_entries(extent::INLINE_LEN, false) as usize;

    inode.flags |= iflags::EXTENTS;

    if extents.len() <= inline_max {
        inode.block = extent::build_inline(&extents)?;
        return Ok(0);
    }

    // Too many for the inode: put them in a leaf and point at it.
    let per_leaf = ExtentHeader::max_entries(block_size as usize, false) as usize;
    let leaves = extents.len().div_ceil(per_leaf);
    if leaves > inline_max {
        return Err(Error::Unsupported(format!(
            "a file this fragmented needs {leaves} extent leaves, more than the \
             {inline_max} indices that fit in an inode; deeper trees are not built yet"
        )));
    }

    let goal = list.iter().copied().find(|&b| b != HOLE).unwrap_or(0);
    let leaf_blocks = alloc.alloc_blocks(fs, goal, leaves as u64).await?;

    let mut indices = Vec::with_capacity(leaves);
    for (i, chunk) in extents.chunks(per_leaf).enumerate() {
        let leaf = leaf_blocks[i];
        let mut buf = vec![0u8; block_size as usize];
        let header = ExtentHeader {
            entries: chunk.len() as u16,
            max: per_leaf as u16,
            depth: 0,
            generation: 0,
        };
        header.encode_into(&mut buf);
        for (j, ext) in chunk.iter().enumerate() {
            let at = extent::HEADER_LEN + j * extent::ENTRY_LEN;
            ext.encode_into(&mut buf[at..at + extent::ENTRY_LEN]);
        }
        stamp_extent_block(fs, &mut buf, inum, inode.generation);
        fs.write_block(leaf, &buf).await?;
        indices.push(ExtentIdx {
            block: chunk[0].block,
            leaf,
        });
    }

    // The inode becomes a one-level tree over those leaves.
    let mut root = [0u8; extent::INLINE_LEN];
    let header = ExtentHeader {
        entries: indices.len() as u16,
        max: inline_max as u16,
        depth: 1,
        generation: 0,
    };
    header.encode_into(&mut root);
    for (i, idx) in indices.iter().enumerate() {
        let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
        idx.encode_into(&mut root[at..at + extent::ENTRY_LEN]);
    }
    inode.block = root;

    Ok(leaves as u64)
}

/// Stamp an extent block's checksum tail, when the filesystem carries one.
fn stamp_extent_block<D: BlockDevice>(
    fs: &Filesystem<D>,
    buf: &mut [u8],
    inum: u32,
    generation: u32,
) {
    if !fs.has_metadata_csum() {
        return;
    }
    let at = buf.len() - extent::TAIL_LEN;
    let crc = mkfs_ext4::csum::extent_block_csum(fs.csum_seed(), inum, generation, &buf[..at]);
    put_u32(buf, at, crc);
}

/// Build direct and indirect pointers for `list`.
async fn write_indirect_map<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    alloc: &mut Allocator,
    inode: &mut Inode,
    list: &[u64],
) -> Result<u64> {
    let block_size = fs.block_size();
    let per_block = (block_size / 4) as usize;
    let mut pointers = [0u32; N_BLOCKS];
    let mut meta_used = 0u64;

    // Direct blocks.
    for (i, slot) in pointers.iter_mut().take(NDIR_BLOCKS).enumerate() {
        *slot = list.get(i).copied().unwrap_or(HOLE) as u32;
    }
    if list.len() <= NDIR_BLOCKS {
        inode.set_block_pointers(&pointers);
        return Ok(0);
    }

    let goal = list.iter().copied().find(|&b| b != HOLE).unwrap_or(0);

    // Single indirect.
    let ind_start = NDIR_BLOCKS;
    let ind_end = (ind_start + per_block).min(list.len());
    let ind = alloc.alloc_block(fs, goal).await?;
    meta_used += 1;
    let mut buf = vec![0u8; block_size as usize];
    for (i, logical) in (ind_start..ind_end).enumerate() {
        put_u32(&mut buf, i * 4, list[logical] as u32);
    }
    fs.write_block(ind, &buf).await?;
    pointers[NDIR_BLOCKS] = ind as u32;

    if list.len() <= ind_end {
        inode.set_block_pointers(&pointers);
        return Ok(meta_used);
    }

    // Double indirect.
    let dind_start = ind_end;
    let dind_capacity = per_block * per_block;
    let dind = alloc.alloc_block(fs, goal).await?;
    meta_used += 1;
    let mut dind_buf = vec![0u8; block_size as usize];

    let mut logical = dind_start;
    let mut child = 0usize;
    // A double indirect block holds `per_block` pointers and no more; past
    // that the file needs the third level.
    while logical < list.len() && child < per_block {
        let end = (logical + per_block).min(list.len());
        let leaf = alloc.alloc_block(fs, goal).await?;
        meta_used += 1;
        let mut leaf_buf = vec![0u8; block_size as usize];
        for (i, l) in (logical..end).enumerate() {
            put_u32(&mut leaf_buf, i * 4, list[l] as u32);
        }
        fs.write_block(leaf, &leaf_buf).await?;
        put_u32(&mut dind_buf, child * 4, leaf as u32);
        child += 1;
        logical = end;
    }
    fs.write_block(dind, &dind_buf).await?;
    pointers[NDIR_BLOCKS + 1] = dind as u32;

    if list.len() <= dind_start + dind_capacity {
        inode.set_block_pointers(&pointers);
        return Ok(meta_used);
    }

    // Triple indirect: a block of double-indirect blocks, each a block of
    // indirect blocks. At 4 KiB blocks this reaches 4 TiB, which is the
    // ceiling of the block-mapped format itself.
    let tind_start = dind_start + dind_capacity;
    let tind = alloc.alloc_block(fs, goal).await?;
    meta_used += 1;
    let mut tind_buf = vec![0u8; block_size as usize];

    let mut logical = tind_start;
    let mut grandchild = 0usize;
    while logical < list.len() && grandchild < per_block {
        let child_dind = alloc.alloc_block(fs, goal).await?;
        meta_used += 1;
        let mut child_dind_buf = vec![0u8; block_size as usize];

        let mut child = 0usize;
        while logical < list.len() && child < per_block {
            let end = (logical + per_block).min(list.len());
            let leaf = alloc.alloc_block(fs, goal).await?;
            meta_used += 1;
            let mut leaf_buf = vec![0u8; block_size as usize];
            for (i, l) in (logical..end).enumerate() {
                put_u32(&mut leaf_buf, i * 4, list[l] as u32);
            }
            fs.write_block(leaf, &leaf_buf).await?;
            put_u32(&mut child_dind_buf, child * 4, leaf as u32);
            child += 1;
            logical = end;
        }

        fs.write_block(child_dind, &child_dind_buf).await?;
        put_u32(&mut tind_buf, grandchild * 4, child_dind as u32);
        grandchild += 1;
    }
    fs.write_block(tind, &tind_buf).await?;
    pointers[NDIR_BLOCKS + 2] = tind as u32;

    if logical < list.len() {
        return Err(Error::Unsupported(format!(
            "a {}-block file exceeds what triple indirection addresses at \
             {block_size}-byte blocks",
            list.len()
        )));
    }

    inode.set_block_pointers(&pointers);
    Ok(meta_used)
}
