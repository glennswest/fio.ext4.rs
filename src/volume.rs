//! The filesystem, open for business.
//!
//! [`Volume`] is the whole public surface: open a device, read and write files,
//! make and remove directories, list what is there. No kernel, no mount, no
//! loop device — just positional I/O against whatever implements
//! [`BlockDevice`].

use std::collections::BTreeMap;

use mkfs_ext4::cache::CachedDevice;
use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::structs::dirent::{self, DirEntry};
use mkfs_ext4::structs::inode::{mode, Inode};
use mkfs_ext4::structs::xattr::{self, Xattr};
use mkfs_ext4::structs::superblock::ino;

use crate::alloc::Allocator;
use crate::dir;
use crate::error::{Error, Result};
use crate::map;
use crate::tar::{self, normalise};


/// Permissions and ownership for something being created.
///
/// A root filesystem is not just a tree of bytes: `/etc/shadow` has to be
/// `0600`, `/usr/bin/*` has to be executable, `/tmp` has to be `1777` with the
/// sticky bit, and files have to belong to somebody. Creating everything
/// `0644 root:root` produces an image that boots into a broken system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attrs {
    /// Permission bits, including setuid, setgid and sticky. The file-type
    /// bits are supplied by the operation, not by the caller.
    pub mode: u16,
    /// Owning user.
    pub uid: u32,
    /// Owning group.
    pub gid: u32,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }
}

impl Attrs {
    /// Permissions, owned by root.
    pub fn mode(mode: u16) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    /// The defaults a directory wants rather than a file.
    pub fn dir() -> Self {
        Self {
            mode: 0o755,
            ..Default::default()
        }
    }

    /// Set the owner.
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    /// Permission bits only, with any file-type bits masked off.
    fn perms(&self) -> u16 {
        self.mode & 0o7777
    }
}


/// What kind of special file to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// A character device, such as `/dev/null` or `/dev/console`.
    CharDevice {
        /// Major number.
        major: u32,
        /// Minor number.
        minor: u32,
    },
    /// A block device, such as `/dev/sda`.
    BlockDevice {
        /// Major number.
        major: u32,
        /// Minor number.
        minor: u32,
    },
    /// A named pipe.
    Fifo,
    /// A unix socket.
    Socket,
}

impl Special {
    fn mode_bits(&self) -> u16 {
        match self {
            Special::CharDevice { .. } => mode::IFCHR,
            Special::BlockDevice { .. } => mode::IFBLK,
            Special::Fifo => mode::IFIFO,
            Special::Socket => mode::IFSOCK,
        }
    }

    fn device(&self) -> Option<(u32, u32)> {
        match *self {
            Special::CharDevice { major, minor } | Special::BlockDevice { major, minor } => {
                Some((major, minor))
            }
            _ => None,
        }
    }
}

/// What an unpack did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnpackReport {
    /// Regular files written.
    pub files: u64,
    /// Directories created or adjusted.
    pub directories: u64,
    /// Symbolic links created.
    pub symlinks: u64,
    /// Hard links created.
    pub hard_links: u64,
    /// Device nodes and FIFOs created.
    pub devices: u64,
    /// Extended attributes set.
    pub xattrs: u64,
    /// Bytes of file content written.
    pub bytes: u64,
    /// Names removed by whiteout markers.
    pub removed: u64,
}

/// Join a directory and a name, without doubling the separator.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Turn an archive's name/value pairs into attributes.
fn to_xattrs(pairs: &[(String, Vec<u8>)]) -> Vec<Xattr> {
    pairs
        .iter()
        .map(|(name, value)| Xattr::new(name.clone(), value.clone()))
        .collect()
}

/// How much of a large file is moved between the archive and the filesystem
/// at once. Big enough that the per-write path lookup does not dominate, small
/// enough to sit on a modest device's heap.
const CHUNK: usize = 64 * 1024;

/// What a pack did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackReport {
    /// Regular files read out.
    pub files: u64,
    /// Directories.
    pub directories: u64,
    /// Symbolic links.
    pub symlinks: u64,
    /// Hard links — names beyond the first for one inode.
    pub hard_links: u64,
    /// Device nodes and FIFOs.
    pub devices: u64,
    /// Extended attributes carried across.
    pub xattrs: u64,
    /// Bytes of file content read.
    pub bytes: u64,
}

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
    pub(crate) fs: Filesystem<D>,
    pub(crate) alloc: Allocator,
    pub(crate) now: u32,
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

    /// Open a filesystem on `device`, wrapped in a write-back block cache.
    ///
    /// The cache is [`mkfs_ext4::CachedDevice`]: hot metadata blocks stay
    /// resident and reach the device once per [`Volume::flush`] instead of
    /// once per operation, and streamed file data goes down as large coalesced
    /// writes. Measured against a volume over NVMe/TCP this is the difference
    /// between ~288 device operations per 4 KiB of payload and a handful per
    /// megabyte (issue #3).
    ///
    /// The trade is durability between sync points: nothing is on the device
    /// until [`Volume::flush`] — which this crate already requires before the
    /// filesystem is consistent, so a consumer that flushes at its sync points
    /// and treats a torn build as discard-and-rebuild loses nothing.
    pub async fn open_cached(device: D) -> Result<Volume<CachedDevice<D>>> {
        Volume::open(CachedDevice::new(device)).await
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
    /// **Call this before dropping the volume.** Directory blocks and inodes
    /// are written as they change, but the bitmaps, group descriptor counts and
    /// superblock totals are buffered — so a volume dropped without flushing
    /// leaves names pointing at inodes the bitmap never marked used. That is
    /// corruption, not merely unsaved work.
    ///
    /// Dropping a dirty volume logs a warning through `tracing`, which is the
    /// most an infallible `Drop` can do about an operation that can fail.
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
    ///
    /// New files are `0644 root:root`. Use [`Volume::write_with`] to say
    /// otherwise — an image whose every file is `0644` is not a working root
    /// filesystem.
    pub async fn write(&mut self, path: &str, data: &[u8]) -> Result<u32> {
        self.write_with(path, data, &Attrs::default()).await
    }

    /// Create or replace a file with explicit permissions and ownership.
    ///
    /// Replacing an existing file leaves its permissions and owner alone, the
    /// way writing to a file through a filesystem does — `attrs` applies to
    /// files this call creates.
    pub async fn write_with(
        &mut self,
        path: &str,
        data: &[u8],
        attrs: &Attrs,
    ) -> Result<u32> {
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
                let mut inode = self.new_inode(mode::IFREG | attrs.perms());
                inode.uid = attrs.uid;
                inode.gid = attrs.gid;
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

    /// Create a directory, `0755 root:root`.
    pub async fn mkdir(&mut self, path: &str) -> Result<u32> {
        self.mkdir_with(path, &Attrs::dir()).await
    }

    /// Create a directory with explicit permissions and ownership.
    ///
    /// The sticky bit is just a permission bit, so `0o1777` gives the `/tmp`
    /// semantics a booting system expects.
    pub async fn mkdir_with(&mut self, path: &str, attrs: &Attrs) -> Result<u32> {
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

        let mut inode = self.new_inode(mode::IFDIR | attrs.perms());
        inode.uid = attrs.uid;
        inode.gid = attrs.gid;
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
            self.free_xattr_block(inum).await?;
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
        self.free_xattr_block(inum).await?;

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

    /// Create every missing directory along a path, `0755 root:root`.
    pub async fn mkdir_all(&mut self, path: &str) -> Result<()> {
        self.mkdir_all_with(path, &Attrs::dir()).await
    }

    /// Create every missing directory along a path, with given attributes.
    pub async fn mkdir_all_with(&mut self, path: &str, attrs: &Attrs) -> Result<()> {
        let mut so_far = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            so_far.push('/');
            so_far.push_str(part);
            if !self.exists(&so_far).await? {
                self.mkdir_with(&so_far, attrs).await?;
            }
        }
        Ok(())
    }



    /// Create a hard link: a second name for the same inode.
    pub async fn link(&mut self, existing: &str, new_path: &str) -> Result<u32> {
        let inum = self.lookup(existing).await?;
        let inode = self.fs.read_inode(inum).await?;
        if inode.is_dir() {
            // Linking a directory would make the tree a graph, and every
            // checker in existence treats that as corruption.
            return Err(Error::IsADirectory(existing.into()));
        }

        let (parent_path, name) = split_path(new_path)?;
        let parent_ino = self.lookup(&parent_path).await?;
        {
            let parent = self.fs.read_inode(parent_ino).await?;
            if self.fs.lookup(&parent, name.as_bytes()).await?.is_some() {
                return Err(Error::AlreadyExists(new_path.into()));
            }
        }

        self.link_into(parent_ino, name.as_bytes(), inum).await?;

        let mut inode = self.fs.read_inode(inum).await?;
        inode.links_count += 1;
        inode.ctime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        Ok(inum)
    }

    /// Move or rename a file or directory.
    ///
    /// Replacing an existing name is allowed for files, as it is on any unix
    /// filesystem; replacing a non-empty directory is not.
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let inum = self.lookup(from).await?;
        let (from_parent_path, from_name) = split_path(from)?;
        let (to_parent_path, to_name) = split_path(to)?;
        let from_parent = self.lookup(&from_parent_path).await?;
        let to_parent = self.lookup(&to_parent_path).await?;

        let moving = self.fs.read_inode(inum).await?;
        let is_dir = moving.is_dir();

        // A directory cannot be moved inside itself; the subtree would be
        // unreachable and its link counts unrecoverable.
        if is_dir && self.is_ancestor(inum, to_parent).await? {
            return Err(Error::InvalidPath(format!(
                "cannot move {from} inside itself"
            )));
        }

        // Clear the destination if something is already there.
        if let Some(existing) = self.fs.resolve_path(to).await? {
            if existing != inum {
                let target = self.fs.read_inode(existing).await?;
                if target.is_dir() {
                    let entries = self.fs.read_dir(&target).await?;
                    if !dir::is_empty(&entries) {
                        return Err(Error::NotEmpty(to.into()));
                    }
                    self.rmdir(to).await?;
                } else {
                    self.unlink(to).await?;
                }
            } else {
                return Ok(());
            }
        }

        self.link_into(to_parent, to_name.as_bytes(), inum).await?;
        self.unlink_from(from_parent, from_name.as_bytes()).await?;

        // A directory carries a link to its parent through "..", so moving one
        // between directories moves that link too.
        if is_dir && from_parent != to_parent {
            self.repoint_dotdot(inum, to_parent).await?;

            let mut old = self.fs.read_inode(from_parent).await?;
            old.links_count = old.links_count.saturating_sub(1);
            old.ctime = self.now;
            self.fs.write_inode(from_parent, &old).await?;

            let mut new = self.fs.read_inode(to_parent).await?;
            new.links_count += 1;
            new.ctime = self.now;
            self.fs.write_inode(to_parent, &new).await?;
        }

        let mut moved = self.fs.read_inode(inum).await?;
        moved.ctime = self.now;
        self.fs.write_inode(inum, &moved).await?;
        Ok(())
    }

    /// Write `data` at `offset`, leaving the rest of the file alone.
    ///
    /// Extends the file if the write runs past the end, and leaves a hole if
    /// the offset is past it.
    pub async fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let inum = match self.fs.resolve_path(path).await? {
            Some(inum) => inum,
            None => self.write_with(path, &[], &Attrs::default()).await?,
        };
        let inode = self.fs.read_inode(inum).await?;
        if inode.is_dir() {
            return Err(Error::IsADirectory(path.into()));
        }

        // Read, splice, write back. Whole-file rewrite is the honest
        // implementation until a block-granular path is worth the complexity;
        // an image builder writes files once.
        let mut whole = self.fs.read_file(&inode).await?;
        let end = (offset + data.len() as u64) as usize;
        if whole.len() < end {
            whole.resize(end, 0);
        }
        whole[offset as usize..end].copy_from_slice(data);
        self.write_data(inum, &whole).await
    }

    /// Whether `ancestor` is at or above `of` in the tree.
    async fn is_ancestor(&self, ancestor: u32, of: u32) -> Result<bool> {
        let mut at = of;
        for _ in 0..4096 {
            if at == ancestor {
                return Ok(true);
            }
            if at == ino::ROOT {
                return Ok(false);
            }
            let inode = self.fs.read_inode(at).await?;
            match self.fs.lookup(&inode, b"..").await? {
                Some(parent) if parent != at => at = parent,
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    /// Point a directory's ".." at a new parent.
    async fn repoint_dotdot(&mut self, dir_ino: u32, new_parent: u32) -> Result<()> {
        let with_tail = self.fs.has_metadata_csum();
        let dir_inode = self.fs.read_inode(dir_ino).await?;
        let list = map::read_block_list(&self.fs, &dir_inode).await?;

        for &block in &list {
            if block == map::HOLE {
                continue;
            }
            let mut buf = self.fs.read_block(block).await?;
            let limit = buf.len() - if with_tail { dirent::TAIL_LEN } else { 0 };
            let mut at = 0usize;
            while at + dirent::ENTRY_HEADER_LEN <= limit {
                let entry = dirent::DirEntry::decode(&buf[at..limit]).map_err(Error::Fs)?;
                if entry.rec_len == 0 {
                    break;
                }
                if entry.name == b".." {
                    mkfs_ext4::bytes::put_u32(&mut buf, at, new_parent);
                    self.fs
                        .stamp_dir_block(&mut buf, dir_ino, dir_inode.generation);
                    self.fs.write_block(block, &buf).await?;
                    return Ok(());
                }
                at += entry.rec_len as usize;
            }
        }
        Ok(())
    }


    /// Read one extended attribute.
    pub async fn get_xattr(&self, path: &str, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .list_xattrs(path)
            .await?
            .into_iter()
            .find(|a| a.name == name)
            .map(|a| a.value))
    }

    /// List every extended attribute on a path.
    pub async fn list_xattrs(&self, path: &str) -> Result<Vec<Xattr>> {
        let inum = self.lookup(path).await?;
        let raw = self.fs.read_inode_raw(inum).await?;
        let sb = self.fs.superblock();
        let extra = if sb.inode_size > 128 {
            mkfs_ext4::bytes::get_u16(&raw, 0x80)
        } else {
            0
        };

        let mut out = xattr::read_inode_xattrs(&raw, sb.inode_size as usize, extra)?;

        // Anything that did not fit inside the inode lives in a block of its
        // own, which i_file_acl points at.
        let inode = self.fs.read_inode(inum).await?;
        if inode.file_acl != 0 {
            let block = self.fs.read_block(inode.file_acl).await?;
            out.extend(xattr::read_block_xattrs(&block)?);
        }
        Ok(out)
    }

    /// Set an extended attribute, replacing any existing value.
    ///
    /// This is how a SELinux label (`security.selinux`) or a POSIX ACL
    /// (`system.posix_acl_access`) gets onto a file in an image built without a
    /// kernel to do it.
    pub async fn set_xattr(&mut self, path: &str, name: &str, value: &[u8]) -> Result<()> {
        let mut attrs = self.list_xattrs(path).await?;
        match attrs.iter_mut().find(|a| a.name == name) {
            Some(existing) => existing.value = value.to_vec(),
            None => attrs.push(Xattr::new(name, value.to_vec())),
        }
        self.write_xattrs(path, &attrs).await
    }

    /// Remove an extended attribute.
    pub async fn remove_xattr(&mut self, path: &str, name: &str) -> Result<()> {
        let mut attrs = self.list_xattrs(path).await?;
        attrs.retain(|a| a.name != name);
        self.write_xattrs(path, &attrs).await
    }

    /// Replace a path's whole attribute set.
    ///
    /// Attributes are kept inside the inode where they fit, and spill into a
    /// block of their own when they do not — a realistic SELinux label plus one
    /// other attribute already exceeds the ~92 bytes a 256-byte inode has
    /// spare, so the block is the normal case rather than the exception.
    pub async fn write_xattrs(&mut self, path: &str, attrs: &[Xattr]) -> Result<()> {
        let inum = self.lookup(path).await?;
        let sb = self.fs.superblock().clone();
        let inode_size = sb.inode_size as usize;

        let mut raw = self.fs.read_inode_raw(inum).await?;
        let extra = if sb.inode_size > 128 {
            mkfs_ext4::bytes::get_u16(&raw, 0x80)
        } else {
            0
        };

        // Try the inode first; fall back to a block for the whole set rather
        // than splitting it, which keeps reading simple and ordering stable.
        let inline_ok = xattr::write_inode_xattrs(&mut raw, inode_size, extra, attrs).is_ok();
        if !inline_ok {
            xattr::write_inode_xattrs(&mut raw, inode_size, extra, &[])?;
        }

        let mut inode = self.fs.read_inode(inum).await?;
        let old_block = inode.file_acl;

        if inline_ok {
            if old_block != 0 {
                self.alloc.free_block(&mut self.fs, old_block).await?;
                inode.file_acl = 0;
                inode.blocks = inode.blocks.saturating_sub(sb.block_size() as u64 / 512);
            }
        } else {
            let block = if old_block != 0 {
                old_block
            } else {
                let b = self.alloc.alloc_block(&mut self.fs, 0).await?;
                inode.file_acl = b;
                inode.blocks += sb.block_size() as u64 / 512;
                b
            };
            let mut buf = xattr::write_block_xattrs(sb.block_size() as usize, attrs)?;
            if self.fs.has_metadata_csum() {
                xattr::stamp_block_csum(&mut buf, self.fs.csum_seed(), block);
            }
            self.fs.write_block(block, &buf).await?;
        }

        // The inode's checksum covers its attribute area, so the modified bytes
        // have to travel with it rather than being recomputed from the fields.
        inode.ctime = self.now;
        inode.tail = raw[128 + extra as usize..inode_size].to_vec();
        while inode.tail.last() == Some(&0) {
            inode.tail.pop();
        }
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }


    // ---- archives ----

    /// Unpack a tar archive held in memory.
    ///
    /// A convenience over [`Volume::unpack_tar_from`]; prefer that one for
    /// anything large, since this holds the whole archive at once.
    pub async fn unpack_tar(&mut self, archive: &[u8]) -> Result<UnpackReport> {
        self.unpack_tar_from(tar::Bytes::new(archive)).await
    }

    /// Unpack a tar archive from a stream.
    ///
    /// This is the image-build operation: a container layer or rootfs tarball
    /// laid down with its ownership, permissions, symlinks, hard links, device
    /// nodes and extended attributes intact — with no kernel, no mount and no
    /// root.
    ///
    /// The archive is never held in memory. The source can be a file, a pipe,
    /// a socket or a registry response; see [`tar::Source`].
    ///
    /// Parent directories are created as needed, and entries that appear later
    /// win — which is how stacked layers are meant to resolve.
    pub async fn unpack_tar_from<S: tar::Source>(&mut self, src: S) -> Result<UnpackReport> {
        self.unpack_tar_into(src, "/").await
    }

    /// Unpack a tar archive from a stream, rooted at `dest` rather than `/`.
    ///
    /// `dest` is created if it does not exist.
    pub async fn unpack_tar_into<S: tar::Source>(
        &mut self,
        src: S,
        dest: &str,
    ) -> Result<UnpackReport> {
        self.unpack_stream(src, dest, false).await
    }

    /// Unpack an OCI container layer, honouring whiteout markers.
    ///
    /// A layer does not only add; it deletes. `.wh.<name>` removes `<name>`
    /// from the layers underneath, and `.wh..wh..opq` hides everything already
    /// in its directory. Neither marker is itself created. Unpack layers in
    /// order and the result is the image's filesystem.
    ///
    /// The markers are obeyed as they arrive, which is the point in the stream
    /// where every layer format places them: after the directory's own entry
    /// and before its new contents.
    pub async fn unpack_tar_layer<S: tar::Source>(
        &mut self,
        src: S,
        dest: &str,
    ) -> Result<UnpackReport> {
        self.unpack_stream(src, dest, true).await
    }

    /// The unpack loop, with or without layer semantics.
    async fn unpack_stream<S: tar::Source>(
        &mut self,
        src: S,
        dest: &str,
        whiteouts: bool,
    ) -> Result<UnpackReport> {
        let base = normalise(dest);
        if !base.is_empty() {
            self.mkdir_all(&format!("/{base}")).await?;
        }
        let at = |path: &str| {
            if base.is_empty() {
                format!("/{path}")
            } else {
                format!("/{base}/{path}")
            }
        };

        let mut reader = tar::Reader::new(src);
        let mut report = UnpackReport::default();

        // A directory's mode and timestamp are applied at the end, because
        // writing its children changes both. GNU tar defers them for the same
        // reason. Its extended attributes ride along, since a directory that
        // already existed in a lower layer must end up with this layer's
        // labels and not the ones underneath.
        let mut deferred: Vec<(String, Attrs, u32, Vec<(String, Vec<u8>)>)> = Vec::new();

        while let Some(header) = reader.next().await? {
            if header.path.is_empty() {
                continue;
            }
            let (parent, name) = match header.path.rfind('/') {
                Some(i) => (&header.path[..i], &header.path[i + 1..]),
                None => ("", header.path.as_str()),
            };

            if whiteouts && name.starts_with(".wh.") {
                report.removed += self.apply_whiteout(&at(parent), name).await?;
                continue;
            }

            let path = at(&header.path);
            let attrs = Attrs {
                mode: header.mode,
                uid: header.uid,
                gid: header.gid,
            };

            // Whatever is being unpacked needs somewhere to go.
            if !parent.is_empty() {
                self.mkdir_all(&at(parent)).await?;
            }

            match header.kind {
                tar::EntryKind::Directory => {
                    // A name that was a file in a lower layer and is a
                    // directory in this one has to change kind, not merge.
                    if self.exists(&path).await? && !self.stat(&path).await?.is_dir() {
                        self.remove_all(&path).await?;
                    }
                    if !self.exists(&path).await? {
                        self.mkdir_with(&path, &attrs).await?;
                    }
                    deferred.push((path.clone(), attrs, header.mtime, header.xattrs.clone()));
                    report.directories += 1;
                }
                tar::EntryKind::File => {
                    // Always a fresh inode. Writing over the old one would
                    // inherit its mode, its ownership and its extended
                    // attributes — so a layer that replaces a file would keep
                    // the label of the file it replaced.
                    self.replace(&path).await?;
                    self.unpack_file(&path, &attrs, &mut reader).await?;
                    report.files += 1;
                    report.bytes += header.size;
                }
                tar::EntryKind::Symlink => {
                    self.replace(&path).await?;
                    self.symlink(&path, &header.link).await?;
                    report.symlinks += 1;
                }
                tar::EntryKind::HardLink => {
                    // A link target is a path within the archive, written the
                    // same way the entry names are.
                    let target = at(&normalise(&header.link));
                    self.replace(&path).await?;
                    self.link(&target, &path).await?;
                    report.hard_links += 1;
                }
                tar::EntryKind::CharDevice
                | tar::EntryKind::BlockDevice
                | tar::EntryKind::Fifo => {
                    let special = match header.kind {
                        tar::EntryKind::CharDevice => Special::CharDevice {
                            major: header.major,
                            minor: header.minor,
                        },
                        tar::EntryKind::BlockDevice => Special::BlockDevice {
                            major: header.major,
                            minor: header.minor,
                        },
                        _ => Special::Fifo,
                    };
                    self.replace(&path).await?;
                    self.mknod(&path, special, &attrs).await?;
                    report.devices += 1;
                }
            }

            // A symlink's own attributes are neither settable nor meaningful.
            // A hard link shares the target's inode, so setting them again
            // would only overwrite what the target already said. A directory
            // waits for the deferred pass.
            if !matches!(
                header.kind,
                tar::EntryKind::Symlink | tar::EntryKind::HardLink | tar::EntryKind::Directory
            ) {
                if !header.xattrs.is_empty() {
                    report.xattrs += header.xattrs.len() as u64;
                    self.write_xattrs(&path, &to_xattrs(&header.xattrs)).await?;
                }
                if header.mtime != 0 {
                    self.set_times(&path, header.mtime).await?;
                }
            }
        }

        // Deepest first, so a parent's timestamp is not disturbed after it has
        // been set by work done inside it.
        deferred.sort_by_key(|(path, _, _, _)| std::cmp::Reverse(path.matches('/').count()));
        for (path, attrs, mtime, xattrs) in deferred {
            self.chmod(&path, attrs.mode).await?;
            self.chown(&path, attrs.uid, attrs.gid).await?;
            // Unconditionally, including when empty: this is what clears the
            // labels a lower layer put on the same directory.
            report.xattrs += xattrs.len() as u64;
            self.write_xattrs(&path, &to_xattrs(&xattrs)).await?;
            if mtime != 0 {
                self.set_times(&path, mtime).await?;
            }
        }

        Ok(report)
    }

    /// Act on a whiteout marker, returning how many names it removed.
    ///
    /// `.wh..wh..opq` empties its directory of everything the lower layers put
    /// there. `.wh.<name>` removes that one name. Anything else beginning
    /// `.wh..wh.` is an aufs bookkeeping file and is dropped rather than
    /// written, which is what every other implementation does with them.
    async fn apply_whiteout(&mut self, dir: &str, name: &str) -> Result<u64> {
        if name == ".wh..wh..opq" {
            if !self.exists(dir).await? {
                return Ok(0);
            }
            let mut removed = 0;
            for entry in self.read_dir(dir).await? {
                let child = join(dir, &entry.name);
                self.remove_all(&child).await?;
                removed += 1;
            }
            return Ok(removed);
        }
        if name.starts_with(".wh..wh.") {
            return Ok(0);
        }

        let victim = join(dir, &name[".wh.".len()..]);
        if self.exists(&victim).await? {
            self.remove_all(&victim).await?;
            return Ok(1);
        }
        Ok(0)
    }

    /// Remove a name, and everything under it if it is a directory.
    pub async fn remove_all(&mut self, path: &str) -> Result<()> {
        if !self.stat(path).await?.is_dir() {
            return self.unlink(path).await;
        }

        // Gathered first, deepest last, then removed in reverse — an async
        // function that recurses needs boxing, and a deep tree would recurse
        // as deep as it goes.
        let mut order = vec![path.to_string()];
        let mut at = 0;
        while at < order.len() {
            let dir = order[at].clone();
            at += 1;
            for entry in self.read_dir(&dir).await? {
                let child = join(&dir, &entry.name);
                if entry.is_dir {
                    order.push(child);
                } else {
                    self.unlink(&child).await?;
                }
            }
        }
        for dir in order.into_iter().rev() {
            self.rmdir(&dir).await?;
        }
        Ok(())
    }

    /// Write one file's contents out of an archive stream.
    ///
    /// Small files go down in a single write. Anything larger is streamed a
    /// chunk at a time, so unpacking a multi-gigabyte layer costs a buffer,
    /// not the file — and each of its blocks is allocated and written exactly
    /// once, with the block map built once at the end. The path this replaced
    /// pushed every chunk through [`Volume::write_at`], which reads the whole
    /// file back and rewrites all of it per call: placing a 55 MB file that
    /// way cost ~56 GB of device writes and as much again in reads (issue #3).
    async fn unpack_file<S: tar::Source>(
        &mut self,
        path: &str,
        attrs: &Attrs,
        reader: &mut tar::Reader<S>,
    ) -> Result<()> {
        /// Above this, contents are streamed rather than gathered.
        const INLINE_LIMIT: usize = 1 << 20;

        let mut buf = vec![0u8; CHUNK];
        let mut pending = Vec::new();
        while pending.len() < INLINE_LIMIT {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                // The whole file fitted; one write does it.
                self.write_with(path, &pending, attrs).await?;
                return Ok(());
            }
            pending.extend_from_slice(&buf[..n]);
        }

        // Too big to gather. Create the file empty, then stream: every full
        // block in hand is allocated next to the previous one and written,
        // and only the not-yet-full tail waits in memory.
        let inum = self.write_with(path, &[], attrs).await?;
        let block_size = self.fs.block_size() as usize;
        let mut list: Vec<u64> = Vec::new();
        let mut size = 0u64;
        loop {
            let full = pending.len() / block_size;
            if full > 0 {
                let goal = list.last().copied().unwrap_or(0);
                let blocks = self
                    .alloc
                    .alloc_blocks(&mut self.fs, goal, full as u64)
                    .await?;
                for (i, &block) in blocks.iter().enumerate() {
                    self.fs
                        .write_block(block, &pending[i * block_size..(i + 1) * block_size])
                        .await?;
                }
                list.extend_from_slice(&blocks);
                size += (full * block_size) as u64;
                pending.drain(..full * block_size);
            }
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&buf[..n]);
        }
        if !pending.is_empty() {
            let goal = list.last().copied().unwrap_or(0);
            let block = self.alloc.alloc_block(&mut self.fs, goal).await?;
            let mut padded = vec![0u8; block_size];
            padded[..pending.len()].copy_from_slice(&pending);
            self.fs.write_block(block, &padded).await?;
            list.push(block);
            size += pending.len() as u64;
        }

        let mut inode = self.fs.read_inode(inum).await?;
        let meta =
            map::write_block_list(&mut self.fs, &mut self.alloc, inum, &mut inode, &list).await?;
        inode.size = size;
        inode.blocks = (list.len() as u64 + meta) * (block_size as u64 / 512);
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }

    /// Clear the way for an entry that is replacing an existing name.
    async fn replace(&mut self, path: &str) -> Result<()> {
        if self.exists(path).await? {
            self.remove_all(path).await?;
        }
        Ok(())
    }

    /// Pack a tree into a tar archive held in memory.
    ///
    /// A convenience over [`Volume::pack_tar_to`] for small trees.
    pub async fn pack_tar(&self, root: &str) -> Result<Vec<u8>> {
        let mut writer = tar::Writer::new(Vec::new());
        self.pack_into(&mut writer, root).await?;
        writer.finish().await?;
        Ok(writer.into_inner())
    }

    /// Pack a tree into a tar archive, written to a stream.
    ///
    /// The inverse of [`Volume::unpack_tar_from`], and the way to get a
    /// container layer back out of an image without mounting it. Modes,
    /// ownership, timestamps, symlinks, hard links, device nodes and extended
    /// attributes all survive the round trip.
    ///
    /// Names are emitted in sorted order, so the same tree produces the same
    /// archive — which is what makes a build reproducible.
    pub async fn pack_tar_to<K: tar::Sink>(&self, sink: K, root: &str) -> Result<PackReport> {
        let mut writer = tar::Writer::new(sink);
        let report = self.pack_into(&mut writer, root).await?;
        writer.finish().await?;
        Ok(report)
    }

    /// Walk a tree, writing each name into an archive.
    async fn pack_into<K: tar::Sink>(
        &self,
        writer: &mut tar::Writer<K>,
        root: &str,
    ) -> Result<PackReport> {
        let root = root.trim_end_matches('/');
        let mut report = PackReport::default();

        // Inodes already emitted, so the second name for one becomes a hard
        // link rather than a second copy of the contents.
        let mut seen: BTreeMap<u32, String> = BTreeMap::new();

        // An explicit stack rather than recursion: an async fn that calls
        // itself needs boxing, and a deep tree would recurse as deep. Each
        // level holds its remaining names in reverse, so popping walks the
        // tree depth-first in sorted order — the order every other tar
        // produces, and the reason two runs over the same tree agree.
        let mut stack = vec![(root.to_string(), self.sorted_children(root).await?)];

        loop {
            let next = match stack.last_mut() {
                Some((dir, rest)) => rest.pop().map(|entry| (dir.clone(), entry)),
                None => break,
            };
            let (dir, entry) = match next {
                Some(found) => found,
                None => {
                    stack.pop();
                    continue;
                }
            };

            {
                let path = join(&dir, &entry.name);
                let stat = self.stat(&path).await?;
                // Relative to the root being packed, and never absolute — an
                // archive of absolute paths is a hazard to whoever unpacks it.
                let name = path[root.len()..].trim_start_matches('/').to_string();

                if let Some(target) = seen.get(&stat.inode) {
                    writer
                        .append(
                            &tar::Header {
                                path: name,
                                kind: tar::EntryKind::HardLink,
                                link: target.clone(),
                                mode: stat.mode & 0o7777,
                                uid: stat.uid,
                                gid: stat.gid,
                                mtime: stat.mtime,
                                ..Default::default()
                            },
                            &[],
                        )
                        .await?;
                    report.hard_links += 1;
                    continue;
                }
                if stat.links > 1 && !stat.is_dir() {
                    seen.insert(stat.inode, name.clone());
                }

                let mut header = tar::Header {
                    path: name,
                    kind: tar::EntryKind::File,
                    mode: stat.mode & 0o7777,
                    uid: stat.uid,
                    gid: stat.gid,
                    mtime: stat.mtime,
                    ..Default::default()
                };
                header.xattrs = self
                    .list_xattrs(&path)
                    .await?
                    .into_iter()
                    .map(|x| (x.name, x.value))
                    .collect();
                report.xattrs += header.xattrs.len() as u64;

                let mut data = Vec::new();
                match stat.mode & mode::IFMT {
                    mode::IFDIR => {
                        header.kind = tar::EntryKind::Directory;
                        writer.append(&header, &[]).await?;
                        report.directories += 1;
                        stack.push((path.clone(), self.sorted_children(&path).await?));
                        continue;
                    }
                    mode::IFLNK => {
                        header.kind = tar::EntryKind::Symlink;
                        header.link = self.read_link(&path).await?;
                        header.xattrs.clear();
                        report.symlinks += 1;
                    }
                    mode::IFCHR | mode::IFBLK | mode::IFIFO | mode::IFSOCK => {
                        let (major, minor) = self.device_numbers(&path).await?;
                        header.kind = match stat.mode & mode::IFMT {
                            mode::IFCHR => tar::EntryKind::CharDevice,
                            mode::IFBLK => tar::EntryKind::BlockDevice,
                            _ => tar::EntryKind::Fifo,
                        };
                        header.major = major;
                        header.minor = minor;
                        report.devices += 1;
                    }
                    _ => {
                        data = self.read(&path).await?;
                        header.size = data.len() as u64;
                        report.files += 1;
                        report.bytes += data.len() as u64;
                    }
                }

                writer.append(&header, &data).await?;
            }
        }

        Ok(report)
    }

    /// A directory's names, sorted and reversed, ready to be popped in order.
    async fn sorted_children(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut names = self.read_dir(if dir.is_empty() { "/" } else { dir }).await?;
        names.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(names)
    }

    /// The major and minor numbers of a device node; `(0, 0)` for anything else.
    pub async fn device_numbers(&self, path: &str) -> Result<(u32, u32)> {
        let inode = self.fs.read_inode(self.lookup(path).await?).await?;
        Ok(inode.device_numbers())
    }

    /// Create a device node, FIFO or socket.
    ///
    /// A root filesystem without `/dev/null` and `/dev/console` does not boot,
    /// and a container image without them does not run. The device numbers go
    /// where the kernel looks for them: small ones in the classic 16-bit slot,
    /// larger ones in the wider encoding.
    pub async fn mknod(
        &mut self,
        path: &str,
        special: Special,
        attrs: &Attrs,
    ) -> Result<u32> {
        let (parent_path, name) = split_path(path)?;
        let parent_ino = self.lookup(&parent_path).await?;
        {
            let parent = self.fs.read_inode(parent_ino).await?;
            if self.fs.lookup(&parent, name.as_bytes()).await?.is_some() {
                return Err(Error::AlreadyExists(path.into()));
            }
        }

        let inum = self.alloc.alloc_inode(&mut self.fs, parent_ino, false).await?;
        let mut inode = self.new_inode(special.mode_bits() | attrs.perms());
        // i_block holds the device number, not a block map, so the extent flag
        // an ordinary file would carry must not be set here.
        inode.flags &= !mkfs_ext4::structs::inode::iflags::EXTENTS;
        inode.block = [0u8; mkfs_ext4::structs::inode::I_BLOCK_LEN];
        inode.uid = attrs.uid;
        inode.gid = attrs.gid;
        inode.links_count = 1;
        inode.size = 0;
        inode.blocks = 0;
        if let Some((major, minor)) = special.device() {
            inode.set_device_numbers(major, minor);
        }
        self.fs.write_inode(inum, &inode).await?;
        self.link_into(parent_ino, name.as_bytes(), inum).await?;
        Ok(inum)
    }

    /// Create a symbolic link.
    ///
    /// A target short enough lives inside the inode itself — a "fast" symlink,
    /// costing no blocks at all. Longer targets take a block.
    pub async fn symlink(&mut self, path: &str, target: &str) -> Result<u32> {
        let (parent_path, name) = split_path(path)?;
        let parent_ino = self.lookup(&parent_path).await?;
        {
            let parent = self.fs.read_inode(parent_ino).await?;
            if self.fs.lookup(&parent, name.as_bytes()).await?.is_some() {
                return Err(Error::AlreadyExists(path.into()));
            }
        }

        let inum = self.alloc.alloc_inode(&mut self.fs, parent_ino, false).await?;
        // A symlink's permissions are conventionally 0777; the target's are
        // what actually apply.
        let mut inode = self.new_inode(mode::IFLNK | 0o777);
        inode.links_count = 1;
        inode.size = target.len() as u64;

        let bytes = target.as_bytes();
        if bytes.len() < mkfs_ext4::structs::inode::I_BLOCK_LEN {
            inode.flags &= !mkfs_ext4::structs::inode::iflags::EXTENTS;
            inode.block = [0u8; mkfs_ext4::structs::inode::I_BLOCK_LEN];
            inode.block[..bytes.len()].copy_from_slice(bytes);
            inode.blocks = 0;
            self.fs.write_inode(inum, &inode).await?;
        } else {
            let block = self.alloc.alloc_block(&mut self.fs, 0).await?;
            let mut buf = vec![0u8; self.fs.block_size() as usize];
            buf[..bytes.len()].copy_from_slice(bytes);
            self.fs.write_block(block, &buf).await?;
            map::write_block_list(&mut self.fs, &mut self.alloc, inum, &mut inode, &[block])
                .await?;
            inode.blocks = self.fs.block_size() as u64 / 512;
            self.fs.write_inode(inum, &inode).await?;
        }

        self.link_into(parent_ino, name.as_bytes(), inum).await?;
        Ok(inum)
    }

    /// Read a symbolic link's target.
    pub async fn read_link(&self, path: &str) -> Result<String> {
        let inum = self.lookup(path).await?;
        let inode = self.fs.read_inode(inum).await?;
        if !inode.is_symlink() {
            return Err(Error::InvalidPath(format!("{path} is not a symlink")));
        }
        let bytes = if inode.size < mkfs_ext4::structs::inode::I_BLOCK_LEN as u64 {
            inode.block[..inode.size as usize].to_vec()
        } else {
            let mut whole = self.fs.read_file(&inode).await?;
            whole.truncate(inode.size as usize);
            whole
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Change a path's permission bits, leaving its type alone.
    ///
    /// Takes the full 12 bits, so setuid (`0o4000`), setgid (`0o2000`) and the
    /// sticky bit (`0o1000`) are all settable.
    pub async fn chmod(&mut self, path: &str, mode: u16) -> Result<()> {
        let inum = self.lookup(path).await?;
        let mut inode = self.fs.read_inode(inum).await?;
        inode.mode = (inode.mode & mode::IFMT) | (mode & 0o7777);
        inode.ctime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }

    /// Change a path's owner and group.
    pub async fn chown(&mut self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let inum = self.lookup(path).await?;
        let mut inode = self.fs.read_inode(inum).await?;
        inode.uid = uid;
        inode.gid = gid;
        inode.ctime = self.now;
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }

    /// Set a path's modification and access times.
    ///
    /// Image builds want a fixed timestamp so the same inputs produce the same
    /// bytes.
    pub async fn set_times(&mut self, path: &str, secs: u32) -> Result<()> {
        let inum = self.lookup(path).await?;
        let mut inode = self.fs.read_inode(inum).await?;
        inode.atime = secs;
        inode.mtime = secs;
        inode.ctime = secs;
        self.fs.write_inode(inum, &inode).await?;
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
    /// Give back the block holding an inode's extended attributes.
    ///
    /// Attributes that did not fit in the inode live in a block of their own,
    /// and that block is reachable only from `i_file_acl` — so deleting the
    /// inode without freeing it leaks it, and `e2fsck` reports the block as in
    /// use but owned by nothing.
    async fn free_xattr_block(&mut self, inum: u32) -> Result<()> {
        let mut inode = self.fs.read_inode(inum).await?;
        if inode.file_acl == 0 {
            return Ok(());
        }
        self.alloc.free_block(&mut self.fs, inode.file_acl).await?;
        inode.blocks = inode
            .blocks
            .saturating_sub(self.fs.superblock().block_size() as u64 / 512);
        inode.file_acl = 0;
        self.fs.write_inode(inum, &inode).await?;
        Ok(())
    }

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

        // An indexed directory is not searched for a gap; the name's hash says
        // which one block it can go in.
        if self.is_indexed(&dir_inode) {
            return self.indexed_link(dir_ino, name, target, ft).await;
        }

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

        // No room anywhere. On a filesystem that allows it, this is the point
        // at which the directory stops being a list and becomes a tree — the
        // same point the kernel picks.
        if self.indexing_available() {
            return self.rebuild_index(dir_ino, Some((name, target, ft))).await;
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

        // An indexed directory holds the name in exactly one leaf, and its
        // root and interior blocks are not directory blocks at all.
        if self.is_indexed(&dir_inode) {
            return self.indexed_unlink(dir_ino, name).await;
        }

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

impl<D: BlockDevice> Drop for Volume<D> {
    fn drop(&mut self) {
        if self.alloc.cache.is_dirty() {
            tracing::warn!(
                "fio-ext4: Volume dropped with unflushed changes — the block and \
                 inode bitmaps were not written, so the filesystem on this device \
                 is inconsistent. Call Volume::flush().await before dropping."
            );
        }
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
