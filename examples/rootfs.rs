//! Build a small root filesystem — the permissions, owners, device nodes and
//! symlinks a system actually needs to boot, or a container needs to run.

use fio_ext4::{Attrs, Special, Volume};
use mkfs_ext4::device::FileDevice;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: rootfs <image>");
    let dev = FileDevice::open(&path).await?;
    let mut vol = Volume::open(dev).await?;
    vol.set_time(1_700_000_000);

    for dir in ["/etc", "/usr", "/usr/bin", "/usr/lib", "/var", "/dev", "/proc", "/sys"] {
        vol.mkdir_all(dir).await?;
    }
    vol.mkdir_with("/tmp", &Attrs::mode(0o1777)).await?;
    vol.mkdir_with("/root", &Attrs::mode(0o700)).await?;
    vol.mkdir_with("/home", &Attrs::dir()).await?;
    vol.mkdir_with("/home/gw", &Attrs::mode(0o750).owner(1000, 1000)).await?;

    vol.write_with("/etc/hostname", b"router\n", &Attrs::mode(0o644)).await?;
    vol.write_with("/etc/shadow", b"root:!:20000::::::\n", &Attrs::mode(0o600)).await?;
    vol.write_with("/usr/bin/hello", b"#!/bin/sh\necho hi\n", &Attrs::mode(0o755)).await?;
    vol.write_with("/usr/bin/su", b"#!/bin/sh\nexit 1\n", &Attrs::mode(0o4755)).await?;
    vol.write_with(
        "/home/gw/.profile",
        b"export PATH=/usr/bin\n",
        &Attrs::mode(0o640).owner(1000, 1000),
    )
    .await?;

    // The device nodes without which nothing boots.
    let dev_attrs = Attrs::mode(0o666);
    vol.mknod("/dev/null", Special::CharDevice { major: 1, minor: 3 }, &dev_attrs).await?;
    vol.mknod("/dev/zero", Special::CharDevice { major: 1, minor: 5 }, &dev_attrs).await?;
    vol.mknod("/dev/random", Special::CharDevice { major: 1, minor: 8 }, &dev_attrs).await?;
    vol.mknod("/dev/urandom", Special::CharDevice { major: 1, minor: 9 }, &dev_attrs).await?;
    vol.mknod(
        "/dev/console",
        Special::CharDevice { major: 5, minor: 1 },
        &Attrs::mode(0o600),
    )
    .await?;
    vol.mknod(
        "/dev/sda",
        Special::BlockDevice { major: 8, minor: 0 },
        &Attrs::mode(0o660).owner(0, 6),
    )
    .await?;
    vol.mknod("/dev/initctl", Special::Fifo, &Attrs::mode(0o600)).await?;
    // A large device number, to exercise the wider encoding.
    vol.mknod(
        "/dev/big",
        Special::CharDevice { major: 511, minor: 4095 },
        &dev_attrs,
    )
    .await?;

    // The symlinks a merged-/usr layout depends on.
    vol.symlink("/bin", "usr/bin").await?;
    vol.symlink("/lib", "usr/lib").await?;
    vol.symlink("/etc/localtime", "/usr/share/zoneinfo/Etc/UTC").await?;

    vol.flush().await?;
    println!("built a root filesystem in {path}");
    Ok(())
}
