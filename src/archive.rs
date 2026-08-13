//! Whole-archive operations, in terms of paths and streams.
//!
//! [`volume`](crate::volume) works on an open [`Volume`]; this module works on
//! an image file and an archive, which is what a caller usually has. It opens
//! the image, streams the archive in or out, flushes, and reports what it did.
//!
//! A path of `-`, or `None`, means standard input or standard output — so the
//! same call covers a file, a pipe, and an archive arriving from or going to a
//! container registry.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use fio_ext4::archive::{self, UnpackOptions};
//!
//! // From a file.
//! archive::unpack("disk.img", Some("layer.tar"), &UnpackOptions::default()).await?;
//!
//! // From a pipe: `skopeo copy ... | this`.
//! archive::unpack("disk.img", None::<&str>, &UnpackOptions::default()).await?;
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use mkfs_ext4::device::FileDevice;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::Result;
use crate::tar;
use crate::volume::{PackReport, UnpackReport, Volume};

/// How an archive stream is compressed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    /// Decide by looking at the first bytes of the stream. Writing treats this
    /// as [`Compression::None`], since there is nothing to look at.
    #[default]
    Auto,
    /// A plain tar archive.
    None,
    /// gzip, which is how nearly every container layer is shipped.
    Gzip,
}

/// How to unpack.
#[derive(Debug, Clone)]
pub struct UnpackOptions {
    /// Where in the filesystem the archive's root lands. Created if missing.
    pub into: String,
    /// How the archive is compressed.
    pub compression: Compression,
    /// Treat the archive as an OCI layer: obey `.wh.` whiteout markers, which
    /// delete rather than add. Off by default, because in an ordinary tarball
    /// a name beginning `.wh.` is just a name.
    pub whiteouts: bool,
}

impl Default for UnpackOptions {
    fn default() -> Self {
        Self {
            into: "/".into(),
            compression: Compression::Auto,
            whiteouts: false,
        }
    }
}

/// How to pack.
#[derive(Debug, Clone)]
pub struct PackOptions {
    /// Which subtree of the filesystem to archive.
    pub from: String,
    /// How to compress what is written.
    pub compression: Compression,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            from: "/".into(),
            compression: Compression::None,
        }
    }
}

/// Unpack an archive into a filesystem image.
///
/// `archive` of `None` reads standard input.
pub async fn unpack(
    image: impl AsRef<Path>,
    archive: Option<impl AsRef<Path>>,
    options: &UnpackOptions,
) -> Result<UnpackReport> {
    let source = source(archive).await?;
    let mut volume = Volume::open(FileDevice::open(image.as_ref()).await?).await?;
    let report = unpack_into(&mut volume, source, options).await?;
    volume.flush().await?;
    Ok(report)
}

/// Unpack an archive into an already-open filesystem.
///
/// Does not flush; the caller decides when the image is finished with.
pub async fn unpack_into<D, R>(
    volume: &mut Volume<D>,
    source: R,
    options: &UnpackOptions,
) -> Result<UnpackReport>
where
    D: mkfs_ext4::device::BlockDevice,
    R: AsyncRead + Unpin + Send + 'static,
{
    let source = decompress(Box::new(source), options.compression).await?;
    let source = tar::Io::new(source);
    if options.whiteouts {
        volume.unpack_tar_layer(source, &options.into).await
    } else {
        volume.unpack_tar_into(source, &options.into).await
    }
}

/// Pack a filesystem image's contents into an archive.
///
/// `archive` of `None` writes to standard output.
pub async fn pack(
    image: impl AsRef<Path>,
    archive: Option<impl AsRef<Path>>,
    options: &PackOptions,
) -> Result<PackReport> {
    let sink = sink(archive).await?;
    let volume = Volume::open(FileDevice::open(image.as_ref()).await?).await?;
    pack_from(&volume, sink, options).await
}

/// Pack from an already-open filesystem into a stream.
pub async fn pack_from<D, W>(
    volume: &Volume<D>,
    sink: W,
    options: &PackOptions,
) -> Result<PackReport>
where
    D: mkfs_ext4::device::BlockDevice,
    W: AsyncWrite + Unpin + Send + 'static,
{
    match options.compression {
        Compression::Gzip => {
            #[cfg(feature = "gzip")]
            {
                use tokio::io::AsyncWriteExt;
                let mut encoder =
                    async_compression::tokio::write::GzipEncoder::new(Box::new(sink));
                let report = volume
                    .pack_tar_to(tar::Io::new(&mut encoder), &options.from)
                    .await?;
                encoder.shutdown().await?;
                Ok(report)
            }
            #[cfg(not(feature = "gzip"))]
            Err(crate::Error::Unsupported(
                "gzip: rebuild with the 'gzip' feature".into(),
            ))
        }
        _ => volume.pack_tar_to(tar::Io::new(sink), &options.from).await,
    }
}

/// Open an archive for reading — a file, or standard input.
///
/// `None`, or a path of `-`, is standard input.
pub async fn source(
    path: Option<impl AsRef<Path>>,
) -> Result<Box<dyn AsyncRead + Unpin + Send + 'static>> {
    match path.as_ref().map(|p| p.as_ref()) {
        Some(p) if p != Path::new("-") => Ok(Box::new(tokio::fs::File::open(p).await?)),
        _ => Ok(Box::new(tokio::io::stdin())),
    }
}

/// Open an archive for writing — a file, or standard output.
///
/// `None`, or a path of `-`, is standard output.
pub async fn sink(
    path: Option<impl AsRef<Path>>,
) -> Result<Box<dyn AsyncWrite + Unpin + Send + 'static>> {
    match path.as_ref().map(|p| p.as_ref()) {
        Some(p) if p != Path::new("-") => Ok(Box::new(tokio::fs::File::create(p).await?)),
        _ => Ok(Box::new(tokio::io::stdout())),
    }
}

/// Put a decompressor in front of the stream, if the stream needs one.
///
/// Detection reads the two magic bytes and puts them back, rather than seeking
/// — a pipe cannot seek, and a pipe is half the point of this module.
async fn decompress(
    source: Box<dyn AsyncRead + Unpin + Send + 'static>,
    compression: Compression,
) -> Result<Box<dyn AsyncRead + Unpin + Send + 'static>> {
    use tokio::io::AsyncReadExt;

    let (compression, source): (_, Box<dyn AsyncRead + Unpin + Send + 'static>) = match compression
    {
        Compression::Auto => {
            let mut source = source;
            let mut magic = [0u8; 2];
            let mut at = 0;
            while at < magic.len() {
                let n = source.read(&mut magic[at..]).await?;
                if n == 0 {
                    break;
                }
                at += n;
            }
            let found = if magic == [0x1f, 0x8b] {
                Compression::Gzip
            } else {
                Compression::None
            };
            let rewound = std::io::Cursor::new(magic[..at].to_vec()).chain(source);
            (found, Box::new(rewound))
        }
        other => (other, source),
    };

    match compression {
        Compression::Gzip => {
            #[cfg(feature = "gzip")]
            {
                Ok(Box::new(
                    async_compression::tokio::bufread::GzipDecoder::new(
                        tokio::io::BufReader::new(source),
                    ),
                ))
            }
            #[cfg(not(feature = "gzip"))]
            Err(crate::Error::Unsupported(
                "the archive is gzipped: rebuild with the 'gzip' feature".into(),
            ))
        }
        _ => Ok(source),
    }
}
