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
