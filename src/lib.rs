//! Async userspace file I/O into an **ext2 / ext3 / ext4** filesystem.
//!
//! Read and write files inside a filesystem image or volume with no kernel, no
//! mount and no loop device — which means it works on a Mac, in a container
//! without privileges, and against storage that is not a block device at all.
//!
//! ```no_run
//! use fio_ext4::Volume;
//! use mkfs_ext4::device::FileDevice;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let device = FileDevice::open("disk.img").await?;
//! let mut vol = Volume::open(device).await?;
//!
//! vol.mkdir_all("/etc/conf.d").await?;
//! vol.write("/etc/hostname", b"router\n").await?;
//!
//! let back = vol.read("/etc/hostname").await?;
//! assert_eq!(back, b"router\n");
//!
//! // Nothing is durable until the bitmaps and counters are written back.
//! vol.flush().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # What it is for
//!
//! Building the contents of a filesystem image, and inspecting one, from a
//! program rather than from a shell with root. The companion
//! [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs) crate creates the
//! filesystem and checks it; this one fills it in.
//!
//! # What it maintains
//!
//! Every write keeps the things `fsck` checks in step — block and inode
//! bitmaps, the group descriptors' free counts and directory tallies, the
//! superblock's totals, and every metadata checksum. A filesystem written
//! through this crate passes `e2fsck` afterwards; that is the test the crate is
//! held to.
//!
//! # What it does not do yet
//!
//! Hard links, symlinks, rename, extended attributes, and files large enough to
//! need triple indirection. Directory indexing (`dir_index`) is read but not
//! maintained, so very large directories are appended linearly.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod alloc;
mod dir;
mod map;

pub mod error;
pub mod volume;

pub use error::{Error, Result};
pub use volume::{Entry, Stat, Volume};

/// The filesystem crate underneath, re-exported so a caller needs one
/// dependency rather than two.
pub use mkfs_ext4;
