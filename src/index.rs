//! Maintaining a directory's hash index (`dir_index`).
//!
//! Without an index, finding a name means reading the directory until it turns
//! up, and adding one means reading it until a gap turns up. Both are linear,
//! so filling a directory with *n* names costs *n²* block reads — which is
//! bearable at a hundred names and not at ten thousand. An index makes both a
//! walk of two or three blocks.
//!
//! # How the tree is grown
//!
//! Not the way the kernel does it. The kernel splits one leaf at a time,
//! because it is servicing a live filesystem and cannot stop the world. This
//! crate builds images, so it does what `e2fsck -D` does instead: when a leaf
//! will not take another name, the whole index is rebuilt from the directory's
//! current contents — sorted by hash, packed into fresh leaves, and re-indexed.
//!
//! That is one code path for the first conversion and for every growth after
//! it, rather than a leaf split, a node split, and a root promotion that each
//! have to be right on their own. Each rebuild is linear in the directory's
//! size, and leaves are packed with a fifth of each block left free, so a
//! rebuild happens roughly once per two hundred names rather than once per
//! name.

use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::structs::dirent::{self, DirEntry};
use mkfs_ext4::structs::htree;
use mkfs_ext4::structs::inode::{iflags, Inode};

use crate::dir;
use crate::error::{Error, Result};
use crate::map;
use crate::volume::Volume;

/// How much of each leaf block to leave free when rebuilding.
///
/// e2fsck calls this `indexed_dir_slack_percentage` and defaults it to 20. It
/// is the difference between a rebuild every couple of hundred names and a
/// rebuild every time a name happens to land in a full leaf.
const SLACK_PERCENT: usize = 20;

/// A name waiting to be placed, with the hash that decides where it goes.
struct Sorted {
    hash: u32,
    minor: u32,
    entry: DirEntry,
}

impl<D: BlockDevice> Volume<D> {
    /// Whether a directory carries a hash index.
    pub(crate) fn is_indexed(&self, inode: &Inode) -> bool {
        inode.flags & iflags::INDEX != 0
    }

    /// Whether this filesystem allows directories to be indexed at all.
    pub(crate) fn indexing_available(&self) -> bool {
        self.fs.superblock().has_dir_index()
    }

    /// Add a name to an indexed directory.
    ///
    /// Walks the index to the one leaf the name belongs in. If that leaf is
    /// full, the whole index is rebuilt with the new name included — which is
    /// also what guarantees this terminates, since the rebuild does not have to
    /// find room in an existing block.
    pub(crate) async fn indexed_link(
        &mut self,
        dir_ino: u32,
        name: &[u8],
        target: u32,
        file_type: u8,
    ) -> Result<()> {
        let dir_inode = self.fs.read_inode(dir_ino).await?;
        let (hash, _) = self.hash_name(name);

        let leaf = self.find_leaf(&dir_inode, hash).await?;
        let physical = match self.fs.resolve_block(&dir_inode, leaf).await? {
            Some(block) => block,
            None => return self.rebuild_index(dir_ino, Some((name, target, file_type))).await,
        };

        let with_tail = self.fs.has_metadata_csum();
        let mut buf = self.fs.read_block(physical).await?;
        if dir::insert_into_block(&mut buf, with_tail, target, name, file_type)? {
            self.fs
                .stamp_dir_block(&mut buf, dir_ino, dir_inode.generation);
            self.fs.write_block(physical, &buf).await?;
            self.touch_dir(dir_ino).await?;
            return Ok(());
        }

        // The leaf is full. Redistribute everything, this name included.
        self.rebuild_index(dir_ino, Some((name, target, file_type))).await
    }

    /// Remove a name from an indexed directory.
    ///
    /// The index says which leaf to look in, so this touches one block rather
    /// than reading the directory until the name turns up. It also keeps the
    /// removal away from the root and interior blocks, which are not directory
    /// blocks and must never be edited as if they were: the root's `..` runs to
    /// the very end of its block, past where an ordinary block stops to leave
    /// room for a checksum, so walking it as one fails on the first entry.
    pub(crate) async fn indexed_unlink(&mut self, dir_ino: u32, name: &[u8]) -> Result<u32> {
        let dir_inode = self.fs.read_inode(dir_ino).await?;
        let with_tail = self.fs.has_metadata_csum();
        let (hash, _) = self.hash_name(name);

        // Names sharing a hash can spill into the following leaves, so a miss
        // in the first one is not an answer on its own.
        for leaf in self.leaf_run(&dir_inode, hash).await? {
            let Some(physical) = self.fs.resolve_block(&dir_inode, leaf).await? else {
                continue;
            };
            let mut buf = self.fs.read_block(physical).await?;
            if let Some(removed) = dir::remove_from_block(&mut buf, with_tail, name)? {
                self.fs
                    .stamp_dir_block(&mut buf, dir_ino, dir_inode.generation);
                self.fs.write_block(physical, &buf).await?;
                self.touch_dir(dir_ino).await?;
                return Ok(removed);
            }
        }

        Err(Error::NotFound(String::from_utf8_lossy(name).into()))
    }

    /// Which logical block of the directory holds names with this hash.
    async fn find_leaf(&self, dir_inode: &Inode, hash: u32) -> Result<u64> {
        Ok(self.leaf_run(dir_inode, hash).await?[0])
    }

    /// Every logical block a name with this hash could be in.
    ///
    /// Usually one. More when a run of equal hashes spilled past a leaf's end,
    /// which the following leaves declare by setting the low bit of their own
    /// hash — the one bit a hash never uses.
    async fn leaf_run(&self, dir_inode: &Inode, hash: u32) -> Result<Vec<u64>> {
        let block_size = self.fs.block_size() as usize;
        let root_block = self.fs
            .resolve_block(dir_inode, 0)
            .await?
            .ok_or_else(|| Error::Unsupported("an indexed directory with no root block".into()))?;
        let root = self.fs.read_block(root_block).await?;

        let Some(mut offset) = htree::count_offset(&root, block_size) else {
            return Err(Error::Unsupported(
                "a directory claims an index but its first block holds none".into(),
            ));
        };
        if offset != htree::ROOT_COUNT_OFFSET {
            return Err(Error::Unsupported(
                "a directory's first block is an interior node, not a root".into(),
            ));
        }

        // One level of interior nodes is as deep as this crate builds. Two is
        // hundreds of millions of names, and the level below already spans
        // more than any directory anyone should have.
        let mut deepest = root;
        if htree::indirect_levels(&deepest) > 0 {
            let chosen = htree::find(&deepest, offset, hash)?;
            let (_, target) = htree::entry(&deepest, offset, chosen);
            let physical = self.fs
                .resolve_block(dir_inode, target as u64)
                .await?
                .ok_or_else(|| Error::Unsupported("an index points at a hole".into()))?;
            let node = self.fs.read_block(physical).await?;
            let node_offset = htree::count_offset(&node, block_size).ok_or_else(|| {
                Error::Unsupported("an index points at a block holding no index".into())
            })?;
            deepest = node;
            offset = node_offset;
        }

        let count = htree::count(&deepest, offset) as usize;
        let mut at = htree::find(&deepest, offset, hash)?;
        let mut run = Vec::new();
        loop {
            let (_, leaf) = htree::entry(&deepest, offset, at);
            run.push(leaf as u64);
            at += 1;
            if at >= count {
                break;
            }
            let (next, _) = htree::entry(&deepest, offset, at);
            if next & 1 == 0 {
                break;
            }
        }
        Ok(run)
    }

    /// The hash of a name, under this filesystem's algorithm and seed.
    fn hash_name(&self, name: &[u8]) -> (u32, u32) {
        let sb = self.fs.superblock();
        let version = htree::version_for(sb.def_hash_version, sb.flags);
        htree::dirhash(version, name, &sb.hash_seed)
    }

    /// Rebuild a directory's index from scratch, optionally adding a name.
    ///
    /// This is both the conversion of a linear directory into an indexed one
    /// and the way an indexed one grows.
    pub(crate) async fn rebuild_index(
        &mut self,
        dir_ino: u32,
        extra: Option<(&[u8], u32, u8)>,
    ) -> Result<()> {
        let block_size = self.fs.block_size() as usize;
        let with_tail = self.fs.has_metadata_csum();
        let mut dir_inode = self.fs.read_inode(dir_ino).await?;

        // "." and ".." are not indexed; they stay in the root block, where a
        // reader that knows nothing about the index still finds them.
        let existing = self.fs.read_dir(&dir_inode).await?;
        let parent = self.parent_of(dir_ino, &dir_inode).await?;

        let mut names: Vec<Sorted> = Vec::with_capacity(existing.len() + 1);
        for entry in existing {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let (hash, minor) = self.hash_name(&entry.name);
            names.push(Sorted { hash, minor, entry });
        }
        if let Some((name, target, file_type)) = extra {
            if names.iter().any(|s| s.entry.name == name) {
                return Err(Error::AlreadyExists(
                    String::from_utf8_lossy(name).into_owned(),
                ));
            }
            let (hash, minor) = self.hash_name(name);
            names.push(Sorted {
                hash,
                minor,
                entry: DirEntry::new(target, name, file_type)?,
            });
        }

        // Hash order is the order the tree is searched in. Ties are broken by
        // the minor hash and then the name, so a rebuild of the same directory
        // always lays out the same way.
        names.sort_by(|a, b| {
            (a.hash, a.minor, &a.entry.name).cmp(&(b.hash, b.minor, &b.entry.name))
        });

        let (leaves, hashes) = pack_leaves(&names, block_size, with_tail)?;
        let plan = plan_index(leaves.len(), block_size, with_tail)?;

        // Lay the blocks out: the root, then the leaves, then any interior
        // nodes. That is the order e2fsck writes them in, and the order the
        // index entries below assume.
        let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(plan.total);
        blocks.push(htree::build_root(
            block_size,
            dir_ino,
            parent,
            htree::version_for(
                self.fs.superblock().def_hash_version,
                self.fs.superblock().flags,
            ),
            plan.levels,
            self.fs.superblock().has_filetype(),
            with_tail,
        ));
        for leaf in &leaves {
            blocks.push(dirent::build_block(leaf, block_size, with_tail)?);
        }
        for _ in 0..plan.nodes {
            blocks.push(htree::build_node(block_size, with_tail));
        }

        // The first entry of any index block carries no hash: everything below
        // the second entry's hash belongs to it.
        if plan.levels == 0 {
            for i in 0..leaves.len() {
                let hash = if i == 0 { 0 } else { hashes[i] };
                htree::set_entry(&mut blocks[0], htree::ROOT_COUNT_OFFSET, i, hash, 1 + i as u32);
            }
            htree::set_count(&mut blocks[0], htree::ROOT_COUNT_OFFSET, leaves.len() as u16);
        } else {
            for (node, chunk) in (0..plan.nodes).zip(0..) {
                let first = chunk * plan.per_node;
                let last = (first + plan.per_node).min(leaves.len());
                let node_block = 1 + leaves.len() + node;

                for (k, leaf) in (first..last).enumerate() {
                    let hash = if k == 0 { 0 } else { hashes[leaf] };
                    htree::set_entry(
                        &mut blocks[node_block],
                        htree::NODE_COUNT_OFFSET,
                        k,
                        hash,
                        1 + leaf as u32,
                    );
                }
                htree::set_count(
                    &mut blocks[node_block],
                    htree::NODE_COUNT_OFFSET,
                    (last - first) as u16,
                );

                let hash = if node == 0 { 0 } else { hashes[first] };
                htree::set_entry(
                    &mut blocks[0],
                    htree::ROOT_COUNT_OFFSET,
                    node,
                    hash,
                    node_block as u32,
                );
            }
            htree::set_count(&mut blocks[0], htree::ROOT_COUNT_OFFSET, plan.nodes as u16);
        }

        self.write_directory(dir_ino, &mut dir_inode, blocks).await
    }

    /// Replace a directory's blocks with the ones given, and mark it indexed.
    async fn write_directory(
        &mut self,
        dir_ino: u32,
        dir_inode: &mut Inode,
        blocks: Vec<Vec<u8>>,
    ) -> Result<()> {
        let block_size = self.fs.block_size() as u64;
        let mut list = map::read_block_list(&self.fs, dir_inode).await?;

        // Grow or shrink the directory to the length the tree needs. Blocks
        // already in place keep their position, so a rebuild rewrites contents
        // rather than moving the directory.
        while list.len() < blocks.len() {
            let goal = list.last().copied().unwrap_or(0);
            let block = self.alloc.alloc_block(&mut self.fs, goal).await?;
            list.push(block);
        }
        for block in list.drain(blocks.len()..).collect::<Vec<_>>() {
            if block != map::HOLE {
                self.alloc.free_block(&mut self.fs, block).await?;
            }
        }

        let generation = dir_inode.generation;
        for (logical, mut buf) in blocks.into_iter().enumerate() {
            let physical = list[logical];
            // A leaf is an ordinary directory block and takes the dirent tail
            // checksum; the root and the interior nodes take the index one.
            match htree::count_offset(&buf, block_size as usize) {
                Some(offset) if self.fs.has_metadata_csum() => htree::stamp_csum(
                    &mut buf,
                    block_size as usize,
                    self.fs.csum_seed(),
                    dir_ino,
                    generation,
                    offset,
                ),
                _ => self.fs.stamp_dir_block(&mut buf, dir_ino, generation),
            }
            self.fs.write_block(physical, &buf).await?;
        }

        let meta = map::write_block_list(&mut self.fs, &mut self.alloc, dir_ino, dir_inode, &list)
            .await?;
        dir_inode.flags |= iflags::INDEX;
        dir_inode.size = list.len() as u64 * block_size;
        dir_inode.blocks = (list.len() as u64 + meta) * (block_size / 512);
        dir_inode.mtime = self.now;
        dir_inode.ctime = self.now;
        self.fs.write_inode(dir_ino, dir_inode).await?;
        Ok(())
    }

    /// A directory's parent, from its own `..`.
    async fn parent_of(&self, dir_ino: u32, dir_inode: &Inode) -> Result<u32> {
        for entry in self.fs.read_dir(dir_inode).await? {
            if entry.name == b".." {
                return Ok(entry.inode);
            }
        }
        // The root directory is its own parent, and is the only directory that
        // could plausibly be missing one.
        Ok(dir_ino)
    }

    /// Note that a directory changed.
    async fn touch_dir(&mut self, dir_ino: u32) -> Result<()> {
        let mut inode = self.fs.read_inode(dir_ino).await?;
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.fs.write_inode(dir_ino, &inode).await?;
        Ok(())
    }
}

/// Pack sorted names into leaf blocks, and say what hash each leaf starts at.
///
/// Mirrors e2fsck's `copy_dir_entries`, including the slack it leaves and the
/// low bit it sets on a leaf's hash when that leaf continues a run of equal
/// hashes from the one before — without which a lookup would stop at the first
/// leaf and miss names that spilled into the next.
fn pack_leaves(
    names: &[Sorted],
    block_size: usize,
    with_tail: bool,
) -> Result<(Vec<Vec<DirEntry>>, Vec<u32>)> {
    let usable = block_size - if with_tail { dirent::TAIL_LEN } else { 0 };
    let smallest = dirent::rec_len(1);
    let slack = ((usable * SLACK_PERCENT) / 100).max(smallest);

    let mut leaves: Vec<Vec<DirEntry>> = vec![Vec::new()];
    let mut hashes: Vec<u32> = vec![0];
    let mut used = 0usize;
    // No real hash can equal this: every hash has its low bit cleared.
    let mut previous = 1u32;

    for name in names {
        let need = dirent::rec_len(name.entry.name.len());
        if need > usable {
            return Err(Error::InvalidName(
                "a name too long to fit in a directory block".into(),
            ));
        }
        if used + need > usable && !leaves.last().unwrap().is_empty() {
            leaves.push(Vec::new());
            hashes.push(0);
            used = 0;
        }
        if leaves.last().unwrap().is_empty() {
            let at = leaves.len() - 1;
            hashes[at] = if name.hash == previous {
                name.hash | 1
            } else {
                name.hash
            };
        }

        leaves.last_mut().unwrap().push(name.entry.clone());
        used += need;
        previous = name.hash;

        // Stop short of filling the block, so the next name that hashes here
        // has somewhere to go without forcing a rebuild.
        if usable - used < slack {
            leaves.push(Vec::new());
            hashes.push(0);
            used = 0;
        }
    }

    // The loop can leave an empty block on the end; an index entry pointing at
    // a leaf holding nothing is not wrong, but there is no reason to write one.
    if leaves.len() > 1 && leaves.last().unwrap().is_empty() {
        leaves.pop();
        hashes.pop();
    }

    Ok((leaves, hashes))
}

/// How the index over a given number of leaves is shaped.
struct Plan {
    /// Levels of interior nodes: 0 if the root points straight at the leaves.
    levels: u8,
    /// How many interior nodes there are.
    nodes: usize,
    /// How many leaves each interior node covers.
    per_node: usize,
    /// Blocks in the finished directory, the root included.
    total: usize,
}

fn plan_index(leaves: usize, block_size: usize, with_tail: bool) -> Result<Plan> {
    let root_limit = htree::limit(block_size, htree::ROOT_COUNT_OFFSET, with_tail) as usize;
    let node_limit = htree::limit(block_size, htree::NODE_COUNT_OFFSET, with_tail) as usize;

    if leaves <= root_limit {
        return Ok(Plan {
            levels: 0,
            nodes: 0,
            per_node: 0,
            total: 1 + leaves,
        });
    }

    let nodes = leaves.div_ceil(node_limit);
    if nodes > root_limit {
        // Two levels of interior nodes would be needed. At 4 KiB blocks one
        // level already spans some fifty million names, so this is a directory
        // no filesystem should be asked to hold.
        return Err(Error::Unsupported(format!(
            "a directory of {leaves} blocks needs a three-level index"
        )));
    }

    Ok(Plan {
        levels: 1,
        nodes,
        per_node: node_limit,
        total: 1 + leaves + nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[(&str, u32)]) -> Vec<Sorted> {
        names
            .iter()
            .map(|&(name, hash)| Sorted {
                hash,
                minor: 0,
                entry: DirEntry::new(12, name.as_bytes(), dirent::file_type::REG_FILE).unwrap(),
            })
            .collect()
    }

    #[test]
    fn a_leaf_that_continues_a_run_of_equal_hashes_is_marked() {
        // Enough names on one hash to spill past a block, which is the case the
        // low bit exists for.
        let mut names = Vec::new();
        for i in 0..80 {
            names.push((format!("name-that-is-fairly-long-{i:03}"), 0x1000u32));
        }
        let refs: Vec<(&str, u32)> = names.iter().map(|(n, h)| (n.as_str(), *h)).collect();
        let (leaves, hashes) = pack_leaves(&sorted(&refs), 1024, false).unwrap();

        assert!(leaves.len() > 1, "the names should not have fitted in one");
        assert_eq!(hashes[0] & 1, 0, "the first leaf continues nothing");
        for (i, &hash) in hashes.iter().enumerate().skip(1) {
            assert_eq!(
                hash & 1,
                1,
                "leaf {i} continues the same hash and must say so"
            );
        }
    }

    #[test]
    fn distinct_hashes_do_not_set_the_continuation_bit() {
        let names: Vec<(String, u32)> = (0..80)
            .map(|i| (format!("name-that-is-fairly-long-{i:03}"), (i as u32 + 1) << 8))
            .collect();
        let refs: Vec<(&str, u32)> = names.iter().map(|(n, h)| (n.as_str(), *h)).collect();
        let (_, hashes) = pack_leaves(&sorted(&refs), 1024, false).unwrap();
        for (i, &hash) in hashes.iter().enumerate() {
            assert_eq!(hash & 1, 0, "leaf {i} does not continue anything");
        }
    }

    #[test]
    fn leaves_are_left_with_room_to_grow() {
        let names: Vec<(String, u32)> = (0..200)
            .map(|i| (format!("file-{i:04}"), (i as u32 + 1) << 8))
            .collect();
        let refs: Vec<(&str, u32)> = names.iter().map(|(n, h)| (n.as_str(), *h)).collect();
        let (leaves, _) = pack_leaves(&sorted(&refs), 1024, true).unwrap();

        let usable = 1024 - dirent::TAIL_LEN;
        let slack = (usable * SLACK_PERCENT) / 100;
        // A leaf is closed once its free space drops below the slack, so what
        // is left is the slack less the entry that crossed the line — never
        // nothing, which is the point. A directory whose leaves were packed
        // full would rebuild itself on every single insert.
        let widest = dirent::rec_len("file-0000".len());
        for (i, leaf) in leaves.iter().enumerate().take(leaves.len() - 1) {
            let used: usize = leaf.iter().map(|e| dirent::rec_len(e.name.len())).sum();
            let free = usable - used;
            assert!(
                free >= slack - widest,
                "leaf {i} was packed to {used} of {usable}, leaving only {free}"
            );
            assert!(free >= dirent::rec_len(1), "leaf {i} has no usable room left");
        }
    }

    #[test]
    fn an_empty_directory_still_gets_one_leaf() {
        let (leaves, hashes) = pack_leaves(&[], 1024, false).unwrap();
        assert_eq!(leaves.len(), 1);
        assert_eq!(hashes.len(), 1);
        assert!(leaves[0].is_empty());
    }

    #[test]
    fn the_index_grows_a_level_only_when_the_root_runs_out() {
        // 4 KiB blocks with checksums: (4096 - 32 - 8) / 8 = 507 root entries.
        let flat = plan_index(507, 4096, true).unwrap();
        assert_eq!(flat.levels, 0);
        assert_eq!(flat.total, 508);

        let deep = plan_index(508, 4096, true).unwrap();
        assert_eq!(deep.levels, 1);
        // (4096 - 8 - 8) / 8 = 510 leaves per interior node.
        assert_eq!(deep.per_node, 510);
        assert_eq!(deep.nodes, 1);
        assert_eq!(deep.total, 1 + 508 + 1);

        // And a directory too large for one level is refused rather than
        // written wrong.
        assert!(plan_index(507 * 510 + 1, 4096, true).is_err());
    }
}
