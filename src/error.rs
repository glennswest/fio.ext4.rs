//! Errors.

/// Anything that can go wrong reading or writing a file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filesystem layer failed.
    #[error(transparent)]
    Fs(#[from] mkfs_ext4::Error),

    /// No such file or directory.
    #[error("no such file or directory: {0}")]
    NotFound(String),

    /// A path component that should have been a directory was not.
    #[error("not a directory: {0}")]
    NotADirectory(String),

    /// The target is a directory and the operation wanted a file.
    #[error("is a directory: {0}")]
    IsADirectory(String),

    /// The name is already taken.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// A directory still has entries in it.
    #[error("directory not empty: {0}")]
    NotEmpty(String),

    /// The filesystem has no free blocks left.
    #[error("no space left on device")]
    NoSpace,

    /// The filesystem has no free inodes left.
    #[error("no inodes left on device")]
    NoInodes,

    /// A name is too long, or otherwise unusable.
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// The filesystem uses something this implementation cannot write.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The path was malformed.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// The stream an archive was being read from or written to failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;
