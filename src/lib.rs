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
//! # Archives
//!
//! A tar archive can be streamed straight into a filesystem, or read back out
//! of one, with ownership, permissions, symlinks, hard links, device nodes and
//! extended attributes intact — see [`archive`] for the path-level interface
//! and [`tar`] for the streaming one. The stream can be a file, a pipe, or a
//! container layer arriving from a registry.
//!
//! # Large directories
//!
//! A directory that outgrows one block becomes hash-indexed (`dir_index`) and
//! stays indexed as it changes, so names are added, found and removed through
//! the tree rather than by reading the whole directory.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod alloc;
pub mod tar;
mod dir;
mod index;
mod map;

pub mod archive;
pub mod error;
pub mod volume;

pub use error::{Error, Result};
pub use mkfs_ext4::structs::xattr::Xattr;
pub use volume::{Attrs, Entry, PackReport, Special, Stat, UnpackReport, Volume};

/// The filesystem crate underneath, re-exported so a caller needs one
/// dependency rather than two.
pub use mkfs_ext4;
