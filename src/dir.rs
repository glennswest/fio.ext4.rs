//! Directory entries.
//!
//! A directory block is a singly-linked run of entries, each carrying the
//! distance to the next. Adding a name means finding an entry with slack in it
//! and splitting that slack off; removing one means giving its space back to
//! the entry before it. Nothing is ever moved, which is what keeps the block's
//! links intact.

use mkfs_ext4::bytes::{put_u16, put_u32, put_u8};
use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::structs::dirent::{self, file_type, DirEntry};
use mkfs_ext4::structs::inode::{mode, Inode};

use crate::error::{Error, Result};

/// The file type byte for an inode, or `UNKNOWN` when the filesystem does not
/// carry them.
pub(crate) fn file_type_of<D: BlockDevice>(fs: &Filesystem<D>, inode: &Inode) -> u8 {
    if !fs
        .superblock()
        .feature_incompat
        .contains(mkfs_ext4::IncompatFeatures::FILETYPE)
    {
        return file_type::UNKNOWN;
    }
    match inode.mode & mode::IFMT {
        mode::IFREG => file_type::REG_FILE,
        mode::IFDIR => file_type::DIR,
        mode::IFCHR => file_type::CHRDEV,
        mode::IFBLK => file_type::BLKDEV,
        mode::IFIFO => file_type::FIFO,
        mode::IFSOCK => file_type::SOCK,
        mode::IFLNK => file_type::SYMLINK,
        _ => file_type::UNKNOWN,
    }
}

/// Try to place a new entry in one directory block.
///
/// Returns true if it fitted. An entry fits where some existing entry has more
/// room than its own name needs — including a deleted entry, whose whole span
/// is free.
pub(crate) fn insert_into_block(
    block: &mut [u8],
    with_tail: bool,
    inum: u32,
    name: &[u8],
    file_type: u8,
) -> Result<bool> {
    let limit = block.len() - if with_tail { dirent::TAIL_LEN } else { 0 };
    let need = dirent::rec_len(name.len());

    let mut at = 0usize;
    while at + dirent::ENTRY_HEADER_LEN <= limit {
        let entry = DirEntry::decode(&block[at..limit])
            .map_err(|e| Error::Fs(e))?;
        let rec_len = entry.rec_len as usize;
        if rec_len == 0 {
            break;
        }

        // Space actually used by this entry, versus what it is holding.
        let used = if entry.inode == 0 {
            0
        } else {
            dirent::rec_len(entry.name.len())
        };
        let slack = rec_len.saturating_sub(used);

        if slack >= need {
            if used == 0 {
                // A dead entry: take the whole span.
                write_entry(&mut block[at..], inum, name, file_type, rec_len as u16);
            } else {
                // Shrink the existing entry to what it needs, and take the rest.
                put_u16(block, at + 4, used as u16);
                let new_at = at + used;
                write_entry(
                    &mut block[new_at..],
                    inum,
                    name,
                    file_type,
                    slack as u16,
                );
            }
            return Ok(true);
        }

        at += rec_len;
    }

    Ok(false)
}

/// Write one entry at the start of `buf`, spanning `rec_len` bytes.
fn write_entry(buf: &mut [u8], inum: u32, name: &[u8], file_type: u8, rec_len: u16) {
    put_u32(buf, 0, inum);
    put_u16(buf, 4, rec_len);
    put_u8(buf, 6, name.len() as u8);
    put_u8(buf, 7, file_type);
    buf[dirent::ENTRY_HEADER_LEN..dirent::ENTRY_HEADER_LEN + name.len()].copy_from_slice(name);
    for b in &mut buf[dirent::ENTRY_HEADER_LEN + name.len()..rec_len as usize] {
        *b = 0;
    }
}

/// Remove a name from one directory block.
///
/// The entry's span is folded into the one before it, so the block's chain
/// stays unbroken. When it is the first entry there is nothing before it, so
/// its inode is zeroed instead and the span is left for the next insert.
pub(crate) fn remove_from_block(
    block: &mut [u8],
    with_tail: bool,
    name: &[u8],
) -> Result<Option<u32>> {
    let limit = block.len() - if with_tail { dirent::TAIL_LEN } else { 0 };

    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    while at + dirent::ENTRY_HEADER_LEN <= limit {
        let entry = DirEntry::decode(&block[at..limit]).map_err(Error::Fs)?;
        let rec_len = entry.rec_len as usize;
        if rec_len == 0 {
            break;
        }

        if entry.inode != 0 && entry.name == name {
            let removed = entry.inode;
            match previous {
                Some(prev_at) => {
                    let prev = DirEntry::decode(&block[prev_at..limit]).map_err(Error::Fs)?;
                    put_u16(block, prev_at + 4, prev.rec_len + entry.rec_len);
                }
                None => put_u32(block, at, 0),
            }
            return Ok(Some(removed));
        }

        previous = Some(at);
        at += rec_len;
    }

    Ok(None)
}

/// Build a fresh, empty directory block that one entry can be added to.
pub(crate) fn empty_block(block_size: usize, with_tail: bool) -> Vec<u8> {
    let mut buf = vec![0u8; block_size];
    let limit = block_size - if with_tail { dirent::TAIL_LEN } else { 0 };
    // One dead entry spanning the whole usable block.
    put_u32(&mut buf, 0, 0);
    put_u16(&mut buf, 4, limit as u16);
    if with_tail {
        dirent::write_tail_header(&mut buf[limit..limit + dirent::TAIL_LEN]);
    }
    buf
}

/// The first two entries every directory starts with.
pub(crate) fn dot_entries(
    block_size: usize,
    with_tail: bool,
    self_ino: u32,
    parent_ino: u32,
    filetype: bool,
) -> Result<Vec<u8>> {
    let ft = |t: u8| if filetype { t } else { file_type::UNKNOWN };
    let entries = vec![
        DirEntry::new(self_ino, b".", ft(file_type::DIR)).map_err(Error::Fs)?,
        DirEntry::new(parent_ino, b"..", ft(file_type::DIR)).map_err(Error::Fs)?,
    ];
    dirent::build_block(&entries, block_size, with_tail).map_err(Error::Fs)
}

/// Whether a directory holds anything beyond "." and "..".
pub(crate) fn is_empty(entries: &[DirEntry]) -> bool {
    entries
        .iter()
        .all(|e| e.name == b"." || e.name == b".." || e.inode == 0)
}
