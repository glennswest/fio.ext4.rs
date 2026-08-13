//! Tar archives, streamed in and out of a filesystem.
//!
//! This is the operation an image build actually performs: take a container
//! layer or a rootfs tarball and lay it down with its ownership, permissions,
//! symlinks, device nodes and extended attributes intact — or read a tree back
//! out as an archive. Doing either through a kernel needs root and a mount;
//! doing it here needs neither.
//!
//! Everything is streamed. Nothing here requires the archive to be in memory,
//! which is what makes a pipe, a `docker save`, or a registry pull as valid a
//! source as a file on disk — and what makes an archive larger than RAM
//! possible on a device that does not have much.
//!
//! ```no_run
//! use fio_ext4::{tar, Volume};
//! use mkfs_ext4::device::FileDevice;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut vol = Volume::open(FileDevice::open("disk.img").await?).await?;
//!
//! // From a pipe — a layer arriving from a registry, say.
//! let report = vol.unpack_tar_from(tar::Io::new(tokio::io::stdin())).await?;
//! println!("{} files, {} bytes", report.files, report.bytes);
//!
//! // And back out again.
//! vol.pack_tar_to(tar::Io::new(tokio::io::stdout()), "/").await?;
//! vol.flush().await?;
//! # Ok(())
//! # }
//! ```
//!
//! The reader and writer are written out rather than taken from a crate
//! because the format is a few hundred lines of 512-byte headers, and because
//! the interesting part — GNU long names, PAX records, `SCHILY.xattr.*` — is
//! exactly the part a general-purpose reader hands back in a shape this crate
//! would have to rearrange anyway. It also keeps the dependency list short
//! enough to build for a microcontroller.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// One block of a tar archive. Everything in the format is a multiple of this.
pub const BLOCK: usize = 512;

/// A byte stream an archive can be read from.
///
/// Deliberately smaller than [`tokio::io::AsyncRead`]: implementing it needs no
/// runtime, no pinning and no allocation, so a target with neither an operating
/// system nor an executor can still feed an archive in from a UART or an SD
/// card. Use [`Io`] to wrap anything that already implements tokio's trait.
#[allow(async_fn_in_trait)]
pub trait Source {
    /// Read into `buf`, returning how many bytes were placed there.
    ///
    /// Returning `Ok(0)` means end of stream, and must keep meaning that on
    /// every later call.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
}

/// A byte stream an archive can be written to.
#[allow(async_fn_in_trait)]
pub trait Sink {
    /// Write all of `buf`.
    async fn write_all(&mut self, buf: &[u8]) -> Result<()>;

    /// Flush anything buffered. Called once, when the archive is complete.
    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A [`Source`] over an archive already in memory.
pub struct Bytes<'a> {
    rest: &'a [u8],
}

impl<'a> Bytes<'a> {
    /// Read an archive from a slice.
    pub fn new(archive: &'a [u8]) -> Self {
        Self { rest: archive }
    }
}

impl Source for Bytes<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = buf.len().min(self.rest.len());
        buf[..n].copy_from_slice(&self.rest[..n]);
        self.rest = &self.rest[n..];
        Ok(n)
    }
}

impl Sink for Vec<u8> {
    async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.extend_from_slice(buf);
        Ok(())
    }
}

/// Adapts anything implementing tokio's `AsyncRead` or `AsyncWrite`.
///
/// This is the bridge to files, sockets, pipes and child-process stdio — a
/// `tokio::fs::File`, `tokio::io::stdin()`, or the body of an HTTP response
/// from a registry.
pub struct Io<T> {
    inner: T,
}

impl<T> Io<T> {
    /// Wrap a tokio stream.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Give the stream back.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: tokio::io::AsyncRead + Unpin> Source for Io<T> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        Ok(self.inner.read(buf).await?)
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> Sink for Io<T> {
    async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        Ok(self.inner.write_all(buf).await?)
    }

    async fn flush(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        Ok(self.inner.flush().await?)
    }
}

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryKind {
    /// A regular file.
    #[default]
    File,
    /// A hard link to an earlier entry.
    HardLink,
    /// A symbolic link.
    Symlink,
    /// A character device.
    CharDevice,
    /// A block device.
    BlockDevice,
    /// A directory.
    Directory,
    /// A named pipe.
    Fifo,
}

impl EntryKind {
    /// The tar type flag that denotes this kind.
    fn typeflag(self) -> u8 {
        match self {
            EntryKind::File => b'0',
            EntryKind::HardLink => b'1',
            EntryKind::Symlink => b'2',
            EntryKind::CharDevice => b'3',
            EntryKind::BlockDevice => b'4',
            EntryKind::Directory => b'5',
            EntryKind::Fifo => b'6',
        }
    }
}

/// One entry's metadata — everything about it except its contents.
#[derive(Debug, Clone, Default)]
pub struct Header {
    /// Path within the archive, with any leading `./` and `/` removed.
    pub path: String,
    /// What it is.
    pub kind: EntryKind,
    /// Permission bits.
    pub mode: u16,
    /// Owning user.
    pub uid: u32,
    /// Owning group.
    pub gid: u32,
    /// Modification time, seconds since the epoch.
    pub mtime: u32,
    /// Length of the entry's contents. Zero for anything but a regular file.
    pub size: u64,
    /// Target, for a symlink or hard link.
    pub link: String,
    /// Device major, for a device node.
    pub major: u32,
    /// Device minor, for a device node.
    pub minor: u32,
    /// Extended attributes, carried in `SCHILY.xattr.*` PAX records.
    pub xattrs: Vec<(String, Vec<u8>)>,
}

impl Header {
    /// A header for a regular file.
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o644,
            size,
            ..Default::default()
        }
    }

    /// A header for a directory.
    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::Directory,
            mode: 0o755,
            ..Default::default()
        }
    }
}

/// One entry read whole, for callers that would rather not stream.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The entry's metadata.
    pub header: Header,
    /// The entry's contents.
    pub data: Vec<u8>,
}

/// Read every entry from an archive held in memory.
///
/// A convenience over [`Reader`] for small archives; it holds the whole of
/// every file at once, which the streaming interface does not.
pub async fn read(archive: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = Reader::new(Bytes::new(archive));
    let mut out = Vec::new();
    while let Some(header) = reader.next().await? {
        let data = reader.read_to_end().await?;
        out.push(Entry { header, data });
    }
    Ok(out)
}

/// Reads entries from a tar stream, one at a time.
///
/// Call [`next`](Reader::next) for the following entry's metadata, then
/// [`read`](Reader::read) or [`read_to_end`](Reader::read_to_end) for its
/// contents. Contents left unread are skipped when the next entry is asked
/// for, so a caller that only wants the listing pays nothing for the data.
pub struct Reader<S: Source> {
    src: S,
    /// Bytes of the current entry's contents not yet handed to the caller.
    left: u64,
    /// Padding still to discard once those contents run out.
    pad: usize,
    /// Set once the end-of-archive marker has been seen.
    done: bool,
    /// PAX records that apply to every entry from here on.
    global: BTreeMap<String, Vec<u8>>,
}

impl<S: Source> Reader<S> {
    /// Start reading an archive from a stream.
    pub fn new(src: S) -> Self {
        Self {
            src,
            left: 0,
            pad: 0,
            done: false,
            global: BTreeMap::new(),
        }
    }

    /// Give the underlying stream back — positioned after the archive.
    pub fn into_inner(self) -> S {
        self.src
    }

    /// Advance to the next entry, or `None` at the end of the archive.
    pub async fn next(&mut self) -> Result<Option<Header>> {
        if self.done {
            return Ok(None);
        }
        self.skip_rest().await?;

        // Carried between headers by the GNU and PAX extension mechanisms.
        let mut long_name: Option<String> = None;
        let mut long_link: Option<String> = None;
        let mut pax: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut empty_blocks = 0;
        let mut block = [0u8; BLOCK];

        loop {
            if !self.read_block(&mut block).await? {
                // A stream that simply stops is treated as ended: plenty of
                // producers omit the two-zero-block marker.
                self.done = true;
                return Ok(None);
            }

            // Two consecutive zero blocks end the archive.
            if block.iter().all(|&b| b == 0) {
                empty_blocks += 1;
                if empty_blocks >= 2 {
                    self.done = true;
                    return Ok(None);
                }
                continue;
            }
            empty_blocks = 0;

            let size = octal(&block[124..136])?;
            let typeflag = block[156];

            match typeflag {
                // GNU long name and long link target: they name the entry whose
                // header comes next.
                b'L' => {
                    long_name = Some(cstr(&self.read_body(size).await?));
                    continue;
                }
                b'K' => {
                    long_link = Some(cstr(&self.read_body(size).await?));
                    continue;
                }
                // PAX extended header, for the next entry or for all of them.
                b'x' => {
                    pax = parse_pax(&self.read_body(size).await?)?;
                    continue;
                }
                b'g' => {
                    self.global = parse_pax(&self.read_body(size).await?)?;
                    continue;
                }
                _ => {}
            }

            let kind = match typeflag {
                b'0' | 0 | b'7' => EntryKind::File,
                b'1' => EntryKind::HardLink,
                b'2' => EntryKind::Symlink,
                b'3' => EntryKind::CharDevice,
                b'4' => EntryKind::BlockDevice,
                b'5' => EntryKind::Directory,
                b'6' => EntryKind::Fifo,
                // Anything else — sparse files, volume labels — is skipped
                // rather than guessed at.
                _ => {
                    self.left = size;
                    self.pad = pad_for(size);
                    self.skip_rest().await?;
                    continue;
                }
            };

            let global = &self.global;
            let get = |key: &str| pax.get(key).or_else(|| global.get(key));

            // Name: PAX beats a GNU long name, which beats the header fields.
            let mut path = match get("path") {
                Some(v) => String::from_utf8_lossy(v).into_owned(),
                None => long_name.take().unwrap_or_else(|| {
                    let name = cstr(&block[0..100]);
                    let prefix = cstr(&block[345..500]);
                    if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    }
                }),
            };
            path = normalise(&path);

            let link = match get("linkpath") {
                Some(v) => String::from_utf8_lossy(v).into_owned(),
                None => long_link.take().unwrap_or_else(|| cstr(&block[157..257])),
            };

            let number = |key: &str, fallback: u64| -> u64 {
                get(key)
                    .and_then(|v| String::from_utf8_lossy(v).trim().parse::<f64>().ok())
                    .map(|n| n as u64)
                    .unwrap_or(fallback)
            };

            let xattrs = pax
                .iter()
                .chain(global.iter())
                .filter_map(|(k, v)| {
                    k.strip_prefix("SCHILY.xattr.")
                        .map(|name| (name.to_string(), v.clone()))
                })
                .collect();

            let size = number("size", size);
            // Only a regular file has contents; a device node's header carries
            // its major and minor where a file would carry its length.
            let size = if kind == EntryKind::File { size } else { 0 };
            self.left = size;
            self.pad = pad_for(size);

            return Ok(Some(Header {
                path,
                kind,
                mode: (octal(&block[100..108])? & 0o7777) as u16,
                uid: number("uid", octal(&block[108..116])?) as u32,
                gid: number("gid", octal(&block[116..124])?) as u32,
                mtime: number("mtime", octal(&block[136..148])?) as u32,
                size,
                link,
                major: octal(&block[329..337]).unwrap_or(0) as u32,
                minor: octal(&block[337..345]).unwrap_or(0) as u32,
                xattrs,
            }));
        }
    }

    /// Read part of the current entry's contents, returning `0` at its end.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.left as usize);
        let n = self.src.read(&mut buf[..want]).await?;
        if n == 0 {
            return Err(truncated(self.left));
        }
        self.left -= n as u64;
        Ok(n)
    }

    /// Read the rest of the current entry's contents into memory.
    pub async fn read_to_end(&mut self) -> Result<Vec<u8>> {
        let mut out = vec![0u8; self.left as usize];
        let mut at = 0;
        while at < out.len() {
            let n = self.read(&mut out[at..]).await?;
            if n == 0 {
                return Err(truncated((out.len() - at) as u64));
            }
            at += n;
        }
        Ok(out)
    }

    /// Discard whatever is left of the current entry, and its padding.
    async fn skip_rest(&mut self) -> Result<()> {
        let mut scratch = [0u8; BLOCK];
        while self.left > 0 {
            let want = scratch.len().min(self.left as usize);
            let n = self.src.read(&mut scratch[..want]).await?;
            if n == 0 {
                return Err(truncated(self.left));
            }
            self.left -= n as u64;
        }
        let pad = std::mem::take(&mut self.pad);
        if pad > 0 {
            self.exact(&mut scratch[..pad]).await?;
        }
        Ok(())
    }

    /// Read one header block; `false` if the stream ended cleanly first.
    async fn read_block(&mut self, block: &mut [u8; BLOCK]) -> Result<bool> {
        let n = self.fill(block).await?;
        if n == 0 {
            return Ok(false);
        }
        if n < BLOCK {
            return Err(Error::InvalidPath(format!(
                "archive ends mid-header, {n} bytes into a {BLOCK}-byte block"
            )));
        }
        Ok(true)
    }

    /// Read an extension entry's body, padding included.
    async fn read_body(&mut self, size: u64) -> Result<Vec<u8>> {
        let mut body = vec![0u8; size as usize];
        self.exact(&mut body).await?;
        let pad = pad_for(size);
        if pad > 0 {
            let mut scratch = [0u8; BLOCK];
            self.exact(&mut scratch[..pad]).await?;
        }
        Ok(body)
    }

    /// Fill `buf` completely, or fail.
    async fn exact(&mut self, buf: &mut [u8]) -> Result<()> {
        let want = buf.len();
        let n = self.fill(buf).await?;
        if n < want {
            return Err(truncated((want - n) as u64));
        }
        Ok(())
    }

    /// Read until `buf` is full or the stream ends, returning the byte count.
    ///
    /// A stream is not obliged to hand over as much as was asked for — a pipe
    /// routinely gives back whatever has arrived — so every read here loops.
    async fn fill(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut at = 0;
        while at < buf.len() {
            let n = self.src.read(&mut buf[at..]).await?;
            if n == 0 {
                break;
            }
            at += n;
        }
        Ok(at)
    }
}

/// Writes entries to a tar stream, one at a time.
///
/// Call [`start`](Writer::start) with an entry's metadata, then
/// [`write`](Writer::write) its contents, then [`start`] the next one.
/// [`finish`](Writer::finish) writes the end-of-archive marker; an archive
/// without it is usually readable but is not well formed.
pub struct Writer<K: Sink> {
    sink: K,
    /// Contents still owed for the entry that was started.
    left: u64,
    /// Padding owed once those contents are written.
    pad: usize,
}

impl<K: Sink> Writer<K> {
    /// Start writing an archive to a stream.
    pub fn new(sink: K) -> Self {
        Self {
            sink,
            left: 0,
            pad: 0,
        }
    }

    /// Begin an entry. Its contents, if any, follow via [`write`](Writer::write).
    pub async fn start(&mut self, header: &Header) -> Result<()> {
        self.end_entry().await?;

        // Anything that will not fit the fixed-width fields — a long path, a
        // long link target, an extended attribute, a uid past 2097151 — goes
        // in a PAX header ahead of the entry. One mechanism covers them all,
        // which is why this does not also emit GNU 'L' and 'K' records.
        let mut records: Vec<(String, Vec<u8>)> = Vec::new();
        if header.path.len() > 100 {
            records.push(("path".into(), header.path.clone().into_bytes()));
        }
        if header.link.len() > 100 {
            records.push(("linkpath".into(), header.link.clone().into_bytes()));
        }
        if header.uid > 0o7777777 {
            records.push(("uid".into(), header.uid.to_string().into_bytes()));
        }
        if header.gid > 0o7777777 {
            records.push(("gid".into(), header.gid.to_string().into_bytes()));
        }
        for (name, value) in &header.xattrs {
            records.push((format!("SCHILY.xattr.{name}"), value.clone()));
        }
        if !records.is_empty() {
            let body = build_pax(&records);
            let mut pax = Header::file(format!("PaxHeaders/{}", basename(&header.path)), body.len() as u64);
            pax.mode = 0o644;
            pax.mtime = header.mtime;
            self.sink.write_all(&encode(&pax, b'x')).await?;
            self.sink.write_all(&body).await?;
            let pad = pad_for(body.len() as u64);
            if pad > 0 {
                self.sink.write_all(&[0u8; BLOCK][..pad]).await?;
            }
        }

        self.sink
            .write_all(&encode(header, header.kind.typeflag()))
            .await?;
        self.left = if header.kind == EntryKind::File {
            header.size
        } else {
            0
        };
        self.pad = pad_for(self.left);
        Ok(())
    }

    /// Write part of the current entry's contents.
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        if data.len() as u64 > self.left {
            return Err(Error::InvalidPath(format!(
                "entry was declared as {} more bytes but {} were written",
                self.left,
                data.len()
            )));
        }
        self.sink.write_all(data).await?;
        self.left -= data.len() as u64;
        Ok(())
    }

    /// Write a whole entry — metadata and contents together.
    pub async fn append(&mut self, header: &Header, data: &[u8]) -> Result<()> {
        let mut header = header.clone();
        if header.kind == EntryKind::File {
            header.size = data.len() as u64;
        }
        self.start(&header).await?;
        if header.kind == EntryKind::File {
            self.write(data).await?;
        }
        Ok(())
    }

    /// Close the archive: pad the last entry, then the end-of-archive marker.
    pub async fn finish(&mut self) -> Result<()> {
        self.end_entry().await?;
        self.sink.write_all(&[0u8; BLOCK]).await?;
        self.sink.write_all(&[0u8; BLOCK]).await?;
        self.sink.flush().await
    }

    /// Give the underlying stream back.
    pub fn into_inner(self) -> K {
        self.sink
    }

    /// Pad the entry in progress out to a block boundary.
    async fn end_entry(&mut self) -> Result<()> {
        if self.left > 0 {
            return Err(Error::InvalidPath(format!(
                "entry was declared as {} bytes longer than what was written",
                self.left
            )));
        }
        let pad = std::mem::take(&mut self.pad);
        if pad > 0 {
            self.sink.write_all(&[0u8; BLOCK][..pad]).await?;
        }
        Ok(())
    }
}

/// Lay a header out as its 512-byte block.
fn encode(header: &Header, typeflag: u8) -> [u8; BLOCK] {
    let mut block = [0u8; BLOCK];

    // A path too long for the fixed fields is truncated here, having already
    // been written out in full as a PAX record.
    let (name, prefix) = split_name(&header.path);
    let put = |block: &mut [u8; BLOCK], at: usize, text: &[u8]| {
        let n = text.len().min(block.len() - at);
        block[at..at + n].copy_from_slice(&text[..n]);
    };
    let put_octal = |block: &mut [u8; BLOCK], at: usize, width: usize, value: u64| {
        let text = format!("{:0>width$o}", value, width = width - 1);
        let text = &text[text.len().saturating_sub(width - 1)..];
        block[at..at + width - 1].copy_from_slice(text.as_bytes());
    };

    put(&mut block, 0, name.as_bytes());
    put_octal(&mut block, 100, 8, header.mode as u64);
    put_octal(&mut block, 108, 8, header.uid.min(0o7777777) as u64);
    put_octal(&mut block, 116, 8, header.gid.min(0o7777777) as u64);
    put_octal(
        &mut block,
        124,
        12,
        if typeflag == b'0' || typeflag == b'x' {
            header.size
        } else {
            0
        },
    );
    put_octal(&mut block, 136, 12, header.mtime as u64);
    block[156] = typeflag;
    put(&mut block, 157, header.link.as_bytes());
    put(&mut block, 257, b"ustar\0");
    put(&mut block, 263, b"00");
    put_octal(&mut block, 329, 8, header.major as u64);
    put_octal(&mut block, 337, 8, header.minor as u64);
    put(&mut block, 345, prefix.as_bytes());

    // The checksum is computed with its own field read as spaces, then written
    // back into it as six octal digits, a NUL and a space.
    block[148..156].fill(b' ');
    let sum: u32 = block.iter().map(|&b| b as u32).sum();
    let text = format!("{sum:06o}\0 ");
    block[148..156].copy_from_slice(text.as_bytes());
    block
}

/// Split a path into ustar's `name` and `prefix` fields.
fn split_name(path: &str) -> (&str, &str) {
    if path.len() <= 100 {
        return (path, "");
    }
    // The split has to fall on a separator, and both halves have to fit.
    for (i, _) in path.match_indices('/') {
        if i <= 155 && path.len() - i - 1 <= 100 {
            return (&path[i + 1..], &path[..i]);
        }
    }
    (&path[path.len() - 100..], "")
}

/// The last component of a path, for naming a PAX header after its entry.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build a PAX extended header body from its records.
fn build_pax(records: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in records {
        // Each record opens with its own total length, which includes the
        // digits of that length — so the width has to be solved for.
        let rest = key.len() + 1 + value.len() + 2; // "key" "=" value " " "\n"
        let mut len = rest + 1;
        while len.to_string().len() + rest != len {
            len += 1;
        }
        out.extend_from_slice(len.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(key.as_bytes());
        out.push(b'=');
        out.extend_from_slice(value);
        out.push(b'\n');
    }
    out
}

/// Bytes of padding needed to reach the next block boundary.
fn pad_for(size: u64) -> usize {
    let rem = (size % BLOCK as u64) as usize;
    if rem == 0 {
        0
    } else {
        BLOCK - rem
    }
}

/// Archives carry `./etc/passwd` and `/etc/passwd` alike; pick one.
pub(crate) fn normalise(path: &str) -> String {
    let mut path = path;
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    path.trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

/// The error for a stream that stopped in the middle of something.
fn truncated(missing: u64) -> Error {
    Error::InvalidPath(format!("archive is truncated: {missing} bytes short"))
}

/// Read a NUL-padded field as a string.
fn cstr(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Read one of tar's octal number fields.
///
/// GNU marks values too large for octal by setting the top bit and storing
/// them big-endian binary — which is how a file over 8 GiB records its size.
fn octal(field: &[u8]) -> Result<u64> {
    if field.first().is_some_and(|&b| b & 0x80 != 0) {
        let mut value: u64 = 0;
        for &b in &field[field.len().saturating_sub(8)..] {
            value = (value << 8) | b as u64;
        }
        return Ok(value & 0x7fff_ffff_ffff_ffff);
    }
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0 && b != b' ')
        .map(|&b| b as char)
        .collect();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(trimmed, 8)
        .map_err(|_| Error::InvalidPath(format!("malformed octal field '{trimmed}' in tar header")))
}

/// Parse a PAX extended header: repeated `LEN key=value\n` records.
fn parse_pax(body: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut at = 0usize;

    while at < body.len() {
        // The record opens with its own total length in decimal.
        let space = match body[at..].iter().position(|&b| b == b' ') {
            Some(i) => at + i,
            None => break,
        };
        let len: usize = String::from_utf8_lossy(&body[at..space])
            .trim()
            .parse()
            .map_err(|_| Error::InvalidPath("malformed PAX record length".into()))?;
        if len == 0 || at + len > body.len() {
            break;
        }

        let record = &body[space + 1..at + len];
        if let Some(eq) = record.iter().position(|&b| b == b'=') {
            let key = String::from_utf8_lossy(&record[..eq]).into_owned();
            let mut value = record[eq + 1..].to_vec();
            // Each record ends with a newline that is not part of the value.
            if value.last() == Some(&b'\n') {
                value.pop();
            }
            out.insert(key, value);
        }
        at += len;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a stream a few bytes at a time, the way a pipe does — so a reader
    /// that assumes a read returns everything asked for fails here.
    struct Trickle<'a> {
        rest: &'a [u8],
        at_a_time: usize,
    }

    impl Source for Trickle<'_> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let n = buf.len().min(self.rest.len()).min(self.at_a_time);
            buf[..n].copy_from_slice(&self.rest[..n]);
            self.rest = &self.rest[n..];
            Ok(n)
        }
    }

    async fn build(entries: &[(Header, &[u8])]) -> Vec<u8> {
        let mut writer = Writer::new(Vec::new());
        for (header, data) in entries {
            writer.append(header, data).await.unwrap();
        }
        writer.finish().await.unwrap();
        writer.into_inner()
    }

    #[tokio::test]
    async fn round_trips_a_file_and_a_directory() {
        let mut dir = Header::directory("etc");
        dir.mode = 0o750;
        dir.uid = 1000;
        let mut file = Header::file("etc/hostname", 0);
        file.mtime = 1_700_000_000;

        let archive = build(&[(dir, b""), (file, b"router\n")]).await;
        let entries = read(&archive).await.unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].header.path, "etc");
        assert_eq!(entries[0].header.kind, EntryKind::Directory);
        assert_eq!(entries[0].header.mode, 0o750);
        assert_eq!(entries[0].header.uid, 1000);
        assert_eq!(entries[1].header.path, "etc/hostname");
        assert_eq!(entries[1].header.mtime, 1_700_000_000);
        assert_eq!(entries[1].data, b"router\n");
    }

    #[tokio::test]
    async fn round_trips_links_devices_and_xattrs() {
        let mut link = Header {
            path: "bin".into(),
            kind: EntryKind::Symlink,
            mode: 0o777,
            link: "usr/bin".into(),
            ..Default::default()
        };
        link.xattrs.clear();
        let dev = Header {
            path: "dev/null".into(),
            kind: EntryKind::CharDevice,
            mode: 0o666,
            major: 1,
            minor: 3,
            ..Default::default()
        };
        let labelled = Header {
            path: "etc/shadow".into(),
            kind: EntryKind::File,
            mode: 0o600,
            xattrs: vec![(
                "security.selinux".into(),
                b"system_u:object_r:shadow_t:s0".to_vec(),
            )],
            ..Default::default()
        };

        let archive = build(&[(link, b""), (dev, b""), (labelled, b"root:!:\n")]).await;
        let entries = read(&archive).await.unwrap();

        assert_eq!(entries[0].header.kind, EntryKind::Symlink);
        assert_eq!(entries[0].header.link, "usr/bin");
        assert_eq!(entries[1].header.kind, EntryKind::CharDevice);
        assert_eq!((entries[1].header.major, entries[1].header.minor), (1, 3));
        assert_eq!(
            entries[2].header.xattrs,
            vec![(
                "security.selinux".to_string(),
                b"system_u:object_r:shadow_t:s0".to_vec()
            )]
        );
        assert_eq!(entries[2].data, b"root:!:\n");
    }

    #[tokio::test]
    async fn round_trips_a_path_too_long_for_the_header() {
        let long = format!("var/lib/{}/deep/file.conf", "component-name/".repeat(12));
        assert!(long.len() > 100);
        let archive = build(&[(Header::file(long.clone(), 0), b"x")]).await;
        let entries = read(&archive).await.unwrap();
        assert_eq!(entries[0].header.path, long);
        assert_eq!(entries[0].data, b"x");
    }

    #[tokio::test]
    async fn reads_correctly_from_a_stream_that_dribbles() {
        let archive = build(&[
            (Header::file("a", 0), &b"a".repeat(1000)[..]),
            (Header::file("b", 0), b"bb"),
        ])
        .await;

        for at_a_time in [1, 7, 100, 512] {
            let mut reader = Reader::new(Trickle {
                rest: &archive,
                at_a_time,
            });
            let mut seen = Vec::new();
            while let Some(header) = reader.next().await.unwrap() {
                seen.push((header.path.clone(), reader.read_to_end().await.unwrap()));
            }
            assert_eq!(seen.len(), 2, "at {at_a_time} bytes a time");
            assert_eq!(seen[0].1.len(), 1000);
            assert_eq!(seen[1].1, b"bb");
        }
    }

    #[tokio::test]
    async fn skips_the_contents_of_entries_that_are_not_read() {
        let archive = build(&[
            (Header::file("big", 0), &b"x".repeat(4096)[..]),
            (Header::file("small", 0), b"y"),
        ])
        .await;
        let mut reader = Reader::new(Bytes::new(&archive));
        assert_eq!(reader.next().await.unwrap().unwrap().path, "big");
        // Deliberately not reading "big" — the next call has to step over it.
        let second = reader.next().await.unwrap().unwrap();
        assert_eq!(second.path, "small");
        assert_eq!(reader.read_to_end().await.unwrap(), b"y");
    }

    #[tokio::test]
    async fn a_truncated_archive_is_an_error_not_a_short_read() {
        let archive = build(&[(Header::file("a", 0), &b"x".repeat(2000)[..])]).await;
        let mut reader = Reader::new(Bytes::new(&archive[..BLOCK + 100]));
        assert_eq!(reader.next().await.unwrap().unwrap().path, "a");
        assert!(reader.read_to_end().await.is_err());
    }

    #[tokio::test]
    async fn strips_leading_dot_slash() {
        assert_eq!(normalise("./usr/bin/sh"), "usr/bin/sh");
        assert_eq!(normalise("/etc/"), "etc");
        assert_eq!(normalise("etc"), "etc");
    }

    #[test]
    fn reads_octal_and_gnu_binary_sizes() {
        assert_eq!(octal(b"0000755\0").unwrap(), 0o755);
        assert_eq!(octal(b"        ").unwrap(), 0);
        // Top bit set: big-endian binary, for sizes octal cannot express.
        let mut big = vec![0x80u8, 0, 0, 0];
        big.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x40, 0]);
        assert_eq!(octal(&big).unwrap(), 0x4000);
    }

    #[test]
    fn pax_records_carry_their_own_length() {
        let body = build_pax(&[
            ("uid".into(), b"1000".to_vec()),
            ("SCHILY.xattr.user.a".into(), b"hello".to_vec()),
        ]);
        let parsed = parse_pax(&body).unwrap();
        assert_eq!(parsed.get("uid").map(|v| v.as_slice()), Some(&b"1000"[..]));
        assert_eq!(
            parsed.get("SCHILY.xattr.user.a").map(|v| v.as_slice()),
            Some(&b"hello"[..])
        );
        // The declared length must equal the record's real length, including
        // the digits that declare it — the classic way to get PAX wrong.
        let first_space = body.iter().position(|&b| b == b' ').unwrap();
        let declared: usize = String::from_utf8_lossy(&body[..first_space])
            .parse()
            .unwrap();
        assert_eq!(&body[declared - 1..declared], b"\n");
    }

    #[test]
    fn an_empty_archive_yields_nothing() {
        let empty = vec![0u8; BLOCK * 2];
        let entries = futures_lite_block_on(read(&empty)).unwrap();
        assert!(entries.is_empty());
        assert!(futures_lite_block_on(read(&[])).unwrap().is_empty());
    }

    /// A one-line executor, so the two simplest tests need no runtime.
    fn futures_lite_block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_long_name_splits_across_name_and_prefix() {
        let path = format!("{}/file", "d".repeat(120));
        let (name, prefix) = split_name(&path);
        assert_eq!(name, "file");
        assert_eq!(prefix, "d".repeat(120));
        assert!(prefix.len() <= 155 && name.len() <= 100);
    }
}
