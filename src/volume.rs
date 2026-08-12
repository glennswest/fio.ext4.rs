//! The filesystem, open for business.
//!
//! [`Volume`] is the whole public surface: open a device, read and write files,
//! make and remove directories, list what is there. No kernel, no mount, no
//! loop device — just positional I/O against whatever implements
//! [`BlockDevice`].

use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::structs::dirent::{self, DirEntry};
use mkfs_ext4::structs::inode::{mode, Inode};
use mkfs_ext4::structs::superblock::ino;

use crate::alloc::Allocator;
use crate::dir;
use crate::error::{Error, Result};
use crate::map;

/// What a directory listing tells you about one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name.
    pub name: String,
    /// The inode it refers to.
    pub inode: u32,
    /// Whether it is a directory.
    pub is_dir: bool,
}

/// What `stat` tells you about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Inode number.
    pub inode: u32,
    /// Size in bytes.
    pub size: u64,
    /// Mode, including the file type bits.
    pub mode: u16,
    /// Hard links.
    pub links: u16,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Blocks of 512 bytes.
    pub blocks: u64,
    /// Modification time, seconds since the epoch.
    pub mtime: u32,
}

impl Stat {
    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.mode & mode::IFMT == mode::IFDIR
    }

    /// Whether this is a regular file.
    pub fn is_file(&self) -> bool {
        self.mode & mode::IFMT == mode::IFREG
    }
}

/// An open filesystem that can be read and written.
pub struct Volume<D: BlockDevice> {
    fs: Filesystem<D>,
    alloc: Allocator,
    now: u32,
}

impl<D: BlockDevice> Volume<D> {
    /// Open a filesystem on `device`.
    pub async fn open(device: D) -> Result<Self> {
        let fs = Filesystem::open(device).await?;
        Ok(Self {
            fs,
            alloc: Allocator::new(),
            now: now_secs(),
        })
    }

    /// Fix the timestamp stamped onto new and modified files.
    ///
    /// Makes an image built by this crate reproducible, the same role
    /// `SOURCE_DATE_EPOCH` plays for `mke2fs`.
    pub fn set_time(&mut self, secs: u32) {
        self.now = secs;
    }

    /// The underlying filesystem, for callers that want to inspect it.
    pub fn filesystem(&self) -> &Filesystem<D> {
        &self.fs
    }

    /// Write back everything still held in memory.
    ///
    /// Call before dropping the volume, or the bitmaps, group descriptors and
    /// superblock counters will not match what was written.
    pub async fn flush(&mut self) -> Result<()> {
        self.alloc.flush(&mut self.fs).await?;
        self.fs.flush_group_descs().await?;
        self.fs.flush_superblock().await?;
        self.fs.device().flush().await?;
        Ok(())
    }

    // ---- reading ----

    /// Resolve a path to its inode number.
    pub async fn lookup(&self, path: &str) -> Result<u32> {
        self.fs
            .resolve_path(path)
            .await?
            .ok_or_else(|| Error::NotFound(path.into()))
    }

    /// Whether a path exists.
    pub async fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.fs.resolve_path(path).await?.is_some())
    }

    /// Stat a path.
    pub async fn stat(&self, path: &str) -> Result<Stat> {
        let inum = self.lookup(path).await?;
        let inode = self.fs.read_inode(inum).await?;
        Ok(Stat {
            inode: inum,
            size: inode.size,
            mode: inode.mode,
            links: inode.links_count,
            uid: inode.uid,
            gid: inode.gid,
            blocks: inode.blocks,
            mtime: inode.mtime,
        })
    }

    /// Read a whole file.
    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let inum = self.lookup(path).await?;
        let inode = self.fs.read_inode(inum).await?;
        if inode.is_dir() {
            return Err(Error::IsADirectory(path.into()));
        }
        Ok(self.fs.read_file(&inode).await?)
    }

    /// List a directory, without "." and "..".
    pub async fn read_dir(&self, path: &str) -> Result<Vec<Entry>> {
        let inum = self.lookup(path).await?;
        let inode = self.fs.read_inode(inum).await?;
        if !inode.is_dir() {
            return Err(Error::NotADirectory(path.into()));
        }

        let mut out = Vec::new();
        for entry in self.fs.read_dir(&inode).await? {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let target = self.fs.read_inode(entry.inode).await?;
            out.push(Entry {
                name: entry.name_string(),
                inode: entry.inode,
                is_dir: target.is_dir(),
            });
        }
        Ok(out)
    }

    // ---- writing ----

    /// Create or replace a file, writing `data` into it.
    pub async fn write(&mut self, path: &str, data: &[u8]) -> Result<u32> {
        let (parent_path, name) = split_path(path)?;
        let parent_ino = self.lookup(&parent_path).await?;

        // Replace in place when the name is already taken, so the inode
        // number — and anything holding it — survives.
        let existing = {
            let parent = self.fs.read_inode(parent_ino).await?;
            self.fs.lookup(&parent, name.as_bytes()).await?
        };

        let inum = match existing {
            Some(inum) => {
                let inode = self.fs.read_inode(inum).await?;
                if inode.is_dir() {
                    return Err(Error::IsADirectory(path.into()));
                }
                self.truncate_inode(inum).await?;
                inum
            }
            None => {
                let inum = self.alloc.alloc_inode(&mut self.fs, parent_ino, false).await?;
                let mut inode = self.new_inode(mode::IFREG | 0o644);
                inode.links_count = 1;
                self.fs.write_inode(inum, &inode).await?;
                self.link_into(parent_ino, name.as_bytes(), inum).await?;
                inum
            }
        };

        self.write_data(inum, data).await?;
        Ok(inum)
    }

    /// Append to a file, creating it if needed.
    pub async fn append(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let existing = self.fs.resolve_path(path).await?;
        match existing {
            None => {
                self.write(path, data).await?;
                Ok(())
            }
            Some(inum) => {
                let inode = self.fs.read_inode(inum).await?;
                if inode.is_dir() {
                    return Err(Error::IsADirectory(path.into()));
                }
                let mut whole = self.fs.read_file(&inode).await?;
                whole.extend_from_slice(data);
                self.write_data(inum, &whole).await
            }
        }
    }

    /// Create a directory.
    pub async fn mkdir(&mut self, path: &str) -> Result<u32> {
        let (parent_path, name) = split_path(path)?;
        let parent_ino = self.lookup(&parent_path).await?;

        {
            let parent = self.fs.read_inode(parent_ino).await?;
            if self.fs.lookup(&parent, name.as_bytes()).await?.is_some() {
                return Err(Error::AlreadyExists(path.into()));
            }
        }

        let inum = self.alloc.alloc_inode(&mut self.fs, parent_ino, true).await?;
        let block_size = self.fs.block_size() as u64;
        let block = self.alloc.alloc_block(&mut self.fs, 0).await?;

        let with_tail = self.fs.has_metadata_csum();
        let filetype = self
            .fs
            .superblock()
            .feature_incompat
            .contains(mkfs_ext4::IncompatFeatures::FILETYPE);
        let mut buf = dir::dot_entries(
            block_size as usize,
            with_tail,
            inum,
            parent_ino,
            filetype,
        )?;

        let mut inode = self.new_inode(mode::IFDIR | 0o755);
        // "." points at itself, and the parent's entry is the second link.
        inode.links_count = 2;
        inode.size = block_size;
        inode.blocks = block_size / 512;
        if self.fs.uses_extents() {
            inode.flags |= mkfs_ext4::structs::inode::iflags::EXTENTS;
        }
        map::write_block_list(&mut self.fs, &mut self.alloc, inum, &mut inode, &[block]).await?;

        self.fs.stamp_dir_block(&mut buf, inum, inode.generation);
        self.fs.write_block(block, &buf).await?;
        self.fs.write_inode(inum, &inode).await?;

        self.link_into(parent_ino, name.as_bytes(), inum).await?;

        // A new subdirectory's ".." is another link to the parent.
        let mut parent = self.fs.read_inode(parent_ino).await?;
        parent.links_count += 1;
        parent.mtime = self.now;
        parent.ctime = self.now;
        self.fs.write_inode(parent_ino, &parent).await?;

        Ok(inum)
    }

    /// Remove a file.
    pub async fn unlink(&mut self, path: &str) -> Result<()> {
        let (parent_path, name) = split_path(path)?;
        let parent_ino = self.lookup(&parent_path).await?;
        let inum = self.lookup(path).await?;

        let inode = self.fs.read_inode(inum).await?;
        if inode.is_dir() {
            return Err(Error::IsADirectory(path.into()));
        }

        self.unlink_from(parent_ino, name.as_bytes()).await?;

        let mut inode = self.fs.read_inode(inum).await?;
        inode.links_count = inode.links_count.saturating_sub(1);
        if inode.links_count == 0 {
            // The last name is gone, so the file's blocks go back.
            self.truncate_inode(inum).await?;
            inode = self.fs.read_inode(inum).await?;
            inode.links_count = 0;
            inode.dtime = self.now;
            self.fs.write_inode(inum, &inode).await?;
            self.alloc.free_inode(&mut self.fs, inum, false).await?;
        } else {
            self.fs.write_inode(inum, &inode).await?;
        }
        Ok(())
    }

    /// Remove an empty directory.
    pub async fn rmdir(&mut self, path: &str) -> Result<()> {
        let (parent_path, name) = split_path(path)?;
        let parent_ino = self.lookup(&parent_path).await?;
        let inum = self.lookup(path).await?;
        if inum == ino::ROOT {
            return Err(Error::InvalidPath("cannot remove the root directory".into()));
        }

        let inode = self.fs.read_inode(inum).await?;
        if !inode.is_dir() {
            return Err(Error::NotADirectory(path.into()));
        }
        let entries = self.fs.read_dir(&inode).await?;
        if !dir::is_empty(&entries) {
            return Err(Error::NotEmpty(path.into()));
        }

        self.unlink_from(parent_ino, name.as_bytes()).await?;
        self.truncate_inode(inum).await?;

        let mut inode = self.fs.read_inode(inum).await?;
        inode.links_count = 0;
        inode.dtime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        self.alloc.free_inode(&mut self.fs, inum, true).await?;

        // Its ".." was a link to the parent.
        let mut parent = self.fs.read_inode(parent_ino).await?;
        parent.links_count = parent.links_count.saturating_sub(1);
        parent.mtime = self.now;
        parent.ctime = self.now;
        self.fs.write_inode(parent_ino, &parent).await?;
        Ok(())
    }

    /// Create every missing directory along a path.
    pub async fn mkdir_all(&mut self, path: &str) -> Result<()> {
        let mut so_far = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            so_far.push('/');
            so_far.push_str(part);
            if !self.exists(&so_far).await? {
                self.mkdir(&so_far).await?;
            }
        }
        Ok(())
    }

    // ---- the machinery ----

    /// A fresh inode with the times and sizing a new file starts with.
    fn new_inode(&self, mode_bits: u16) -> Inode {
        let inode_size = self.fs.superblock().inode_size as usize;
        let extra = if inode_size > 128 { 32 } else { 0 };
        let mut inode = Inode::new(inode_size, extra);
        inode.mode = mode_bits;
        inode.atime = self.now;
        inode.ctime = self.now;
        inode.mtime = self.now;
        inode.crtime = self.now;
        if self.fs.uses_extents() {
            inode.flags |= mkfs_ext4::structs::inode::iflags::EXTENTS;
            // An extent-mapped inode needs a valid, empty tree from the moment
            // it exists. A zeroed `i_block` has no magic in it, and anything
            // that walks the inode before its first write finds a corrupt
            // extent header rather than an empty file.
            inode.block = mkfs_ext4::structs::extent::build_inline(&[])
                .expect("an empty extent list always fits inline");
        }
        inode
    }

    /// Replace a file's contents.
    async fn write_data(&mut self, inum: u32, data: &[u8]) -> Result<()> {
        let block_size = self.fs.block_size() as u64;
        let needed = (data.len() as u64).div_ceil(block_size).max(0);

        let mut inode = self.fs.read_inode(inum).await?;
        let mut list = map::read_block_list(&self.fs, &inode).await?;

        // Give back anything past the new end.
        while list.len() as u64 > needed {
            if let Some(block) = list.pop() {
                if block != map::HOLE {
                    self.alloc.free_block(&mut self.fs, block).await?;
                }
            }
        }
        // And take what is missing.
        let goal = list.last().copied().unwrap_or(0);
        while (list.len() as u64) < needed {
            let block = self.alloc.alloc_block(&mut self.fs, goal).await?;
            list.push(block);
        }

        // Write the contents, padding the final partial block with zeroes.
        for (i, &block) in list.iter().enumerate() {
            let from = i * block_size as usize;
            let to = ((i + 1) * block_size as usize).min(data.len());
            let mut buf = vec![0u8; block_size as usize];
            if from < data.len() {
                buf[..to - from].copy_from_slice(&data[from..to]);
            }
            self.fs.write_block(block, &buf).await?;
        }

        let meta = map::write_block_list(
            &mut self.fs,
            &mut self.alloc,
            inum,
            &mut inode,
            &list,
        )
        .await?;

        inode.size = data.len() as u64;
        inode.blocks = (list.len() as u64 + meta) * (block_size / 512);
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }

    /// Free every block an inode owns and leave it empty.
    async fn truncate_inode(&mut self, inum: u32) -> Result<()> {
        let mut inode = self.fs.read_inode(inum).await?;

        let data = map::read_block_list(&self.fs, &inode).await?;
        let meta = map::map_metadata_blocks(&self.fs, &inode).await?;
        for block in data.into_iter().chain(meta) {
            if block != map::HOLE {
                self.alloc.free_block(&mut self.fs, block).await?;
            }
        }

        inode.block = [0u8; mkfs_ext4::structs::inode::I_BLOCK_LEN];
        if inode.uses_extents() {
            // An empty extent tree is still a tree, and must still say so.
            inode.block = mkfs_ext4::structs::extent::build_inline(&[])?;
        }
        inode.size = 0;
        inode.blocks = 0;
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }

    /// Add a name to a directory.
    async fn link_into(&mut self, dir_ino: u32, name: &[u8], target: u32) -> Result<()> {
        if name.is_empty() || name.len() > dirent::NAME_LEN_MAX || name.contains(&b'/') {
            return Err(Error::InvalidName(String::from_utf8_lossy(name).into()));
        }

        let target_inode = self.fs.read_inode(target).await?;
        let ft = dir::file_type_of(&self.fs, &target_inode);
        let with_tail = self.fs.has_metadata_csum();
        let block_size = self.fs.block_size() as u64;

        let mut dir_inode = self.fs.read_inode(dir_ino).await?;
        let mut list = map::read_block_list(&self.fs, &dir_inode).await?;

        // Try each existing block before growing the directory.
        for &block in &list {
            if block == map::HOLE {
                continue;
            }
            let mut buf = self.fs.read_block(block).await?;
            if dir::insert_into_block(&mut buf, with_tail, target, name, ft)? {
                self.fs
                    .stamp_dir_block(&mut buf, dir_ino, dir_inode.generation);
                self.fs.write_block(block, &buf).await?;
                return Ok(());
            }
        }

        // No room: add a block.
        let goal = list.last().copied().unwrap_or(0);
        let block = self.alloc.alloc_block(&mut self.fs, goal).await?;
        let mut buf = dir::empty_block(block_size as usize, with_tail);
        if !dir::insert_into_block(&mut buf, with_tail, target, name, ft)? {
            return Err(Error::InvalidName(
                "name does not fit in an empty directory block".into(),
            ));
        }
        self.fs
            .stamp_dir_block(&mut buf, dir_ino, dir_inode.generation);
        self.fs.write_block(block, &buf).await?;

        list.push(block);
        let meta = map::write_block_list(
            &mut self.fs,
            &mut self.alloc,
            dir_ino,
            &mut dir_inode,
            &list,
        )
        .await?;
        dir_inode.size = list.len() as u64 * block_size;
        dir_inode.blocks = (list.len() as u64 + meta) * (block_size / 512);
        dir_inode.mtime = self.now;
        dir_inode.ctime = self.now;
        self.fs.write_inode(dir_ino, &dir_inode).await?;
        Ok(())
    }

    /// Take a name out of a directory.
    async fn unlink_from(&mut self, dir_ino: u32, name: &[u8]) -> Result<u32> {
        let with_tail = self.fs.has_metadata_csum();
        let dir_inode = self.fs.read_inode(dir_ino).await?;
        let list = map::read_block_list(&self.fs, &dir_inode).await?;

        for &block in &list {
            if block == map::HOLE {
                continue;
            }
            let mut buf = self.fs.read_block(block).await?;
            if let Some(removed) = dir::remove_from_block(&mut buf, with_tail, name)? {
                self.fs
                    .stamp_dir_block(&mut buf, dir_ino, dir_inode.generation);
                self.fs.write_block(block, &buf).await?;

                let mut dir_inode = self.fs.read_inode(dir_ino).await?;
                dir_inode.mtime = self.now;
                dir_inode.ctime = self.now;
                self.fs.write_inode(dir_ino, &dir_inode).await?;
                return Ok(removed);
            }
        }

        Err(Error::NotFound(String::from_utf8_lossy(name).into()))
    }
}

/// Split a path into its parent and its final component.
fn split_path(path: &str) -> Result<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => ("/", trimmed),
    };
    if name.is_empty() {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok((parent.to_string(), name.to_string()))
}

/// Seconds since the epoch, or zero if the clock is before it.
fn now_secs() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Re-exported so callers can match on entry kinds without depending on
/// `mkfs-ext4` directly.
pub use mkfs_ext4::structs::dirent::file_type as FileType;

/// The raw directory entry type, for callers that want the on-disk view.
pub type RawDirEntry = DirEntry;
