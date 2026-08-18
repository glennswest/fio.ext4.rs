//! Build an image whose large file needs an extent tree, for a real reader.
//!
//! Our own checker cannot catch a leaf written where no other reader looks,
//! because it looks in the same place. This writes a fragmented large file and
//! a small one to an image file so `e2fsck` and a kernel mount can have their
//! say — which is how the tail-offset bug in #2 was found and confirmed.
//!
//!     cargo run --example extent_image -- /tmp/img.raw 2048
//!     scp /tmp/img.raw linux: && ssh linux e2fsck -fn img.raw
use fio_ext4::Volume;
use mkfs_ext4::device::FileDevice;
use mkfs_ext4::params::{Params, Profile};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: extent_image <image> [block-size]");
    let bs: u32 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(4096);
    let size = 256u64 * 1024 * 1024;
    std::fs::File::create(&path)?.set_len(size)?;

    let dev = FileDevice::open(&path).await?;
    mkfs_ext4::format::format(&dev, &Params::new(Profile::Ext4).block_size(bs)).await?;

    let mut vol = Volume::open(dev).await?;
    vol.mkdir("/etc").await?;
    vol.write("/etc/group", &vec![b'a'; 306]).await?;
    for i in 0..400 {
        vol.write(&format!("/f{i}"), &vec![b'x'; 40_000]).await?;
    }
    for i in (0..400).step_by(2) {
        vol.unlink(&format!("/f{i}")).await?;
    }
    vol.mkdir("/lib").await?;
    let big: Vec<u8> = (0..1_716_616u32).map(|i| (i % 251) as u8).collect();
    vol.write("/lib/libc.so.6", &big).await?;
    vol.flush().await?;

    // What our own reader thinks, for the record.
    let back = vol.read("/lib/libc.so.6").await?;
    println!("ours: /lib/libc.so.6 {} bytes, identical {}", back.len(), back == big);
    let sum: u32 = big.iter().map(|&b| b as u32).sum();
    println!("checksum of contents: {sum}");
    Ok(())
}
