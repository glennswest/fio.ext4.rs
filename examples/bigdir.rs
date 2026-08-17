//! Build a directory large enough to need a hash index.
use fio_ext4::Volume;
use mkfs_ext4::device::FileDevice;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let image = args.next().expect("usage: bigdir <image> <count>");
    let count: usize = args.next().unwrap_or_else(|| "5000".into()).parse()?;

    let mut vol = Volume::open(FileDevice::open(&image).await?).await?;
    vol.set_time(1_700_000_000);
    vol.mkdir("/many").await?;
    for i in 0..count {
        vol.write(&format!("/many/file-{i:06}"), format!("{i}\n").as_bytes())
            .await?;
    }
    // A second directory of awkward names, to push runs of equal hashes.
    vol.mkdir("/wide").await?;
    for i in 0..count / 2 {
        vol.write(&format!("/wide/{}-{i:04}", "n".repeat(60)), b"x").await?;
    }
    vol.flush().await?;
    println!("wrote {count} names in /many and {} in /wide", count / 2);
    Ok(())
}
