//! Writing files into a filesystem, and getting them back.
//!
//! The last assertion in most of these is the same one: after the writes, the
//! filesystem still checks clean. A file writer that leaves `fsck` complaining
//! has not written the file, it has damaged the filesystem.

use fio_ext4::{Error, Volume};
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

async fn fresh(profile: Profile, size: u64) -> MemDevice {
    let dev = MemDevice::new(size);
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&dev, &params).await.unwrap();
    dev
}

/// Check the filesystem and panic with the findings if it is not clean.
async fn assert_clean(dev: &MemDevice, what: &str) {
    let report = fsck::check(dev, &FsckOptions::check_only()).await.unwrap();
    assert!(
        report.is_clean(),
        "{what}: filesystem is not clean:\n{}",
        report
            .problems
            .iter()
            .map(|p| format!("  [pass {} {}] {}", p.pass, p.code, p.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[tokio::test]
async fn writes_and_reads_a_file() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.write("/hello.txt", b"hello, filesystem\n").await.unwrap();
    vol.flush().await.unwrap();

    assert_eq!(vol.read("/hello.txt").await.unwrap(), b"hello, filesystem\n");
    let st = vol.stat("/hello.txt").await.unwrap();
    assert_eq!(st.size, 18);
    assert!(st.is_file());
    assert_eq!(st.links, 1);

    drop(vol);
    assert_clean(&dev, "after one small file").await;
}

#[tokio::test]
async fn writes_a_file_spanning_many_blocks() {
    let dev = fresh(Profile::Ext4, 32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    // Well past one block, and not a whole number of them.
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    vol.write("/big.bin", &data).await.unwrap();
    vol.flush().await.unwrap();

    assert_eq!(vol.read("/big.bin").await.unwrap(), data);
    assert_eq!(vol.stat("/big.bin").await.unwrap().size, 100_000);

    drop(vol);
    assert_clean(&dev, "after a 100 KB file").await;
}

#[tokio::test]
async fn makes_directories_and_lists_them() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir("/etc").await.unwrap();
    vol.write("/etc/hostname", b"router\n").await.unwrap();
    vol.write("/etc/motd", b"welcome\n").await.unwrap();
    vol.mkdir("/etc/conf.d").await.unwrap();
    vol.flush().await.unwrap();

    let mut names: Vec<String> = vol
        .read_dir("/etc")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["conf.d", "hostname", "motd"]);

    let root: Vec<String> = vol
        .read_dir("/")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(root.contains(&"etc".to_string()));
    assert!(root.contains(&"lost+found".to_string()));

    // A directory with a subdirectory has three links: its own ".", its
    // parent's entry, and the child's "..".
    assert_eq!(vol.stat("/etc").await.unwrap().links, 3);

    drop(vol);
    assert_clean(&dev, "after making directories").await;
}

#[tokio::test]
async fn mkdir_all_creates_the_whole_path() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir_all("/usr/local/share/doc").await.unwrap();
    vol.write("/usr/local/share/doc/readme", b"hi").await.unwrap();
    vol.flush().await.unwrap();

    assert!(vol.exists("/usr/local/share").await.unwrap());
    assert_eq!(vol.read("/usr/local/share/doc/readme").await.unwrap(), b"hi");

    drop(vol);
    assert_clean(&dev, "after mkdir_all").await;
}

#[tokio::test]
async fn overwrites_a_file_in_place() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    let inum = vol.write("/f", &vec![b'a'; 50_000]).await.unwrap();
    // Shorter than before, so blocks have to be given back.
    let again = vol.write("/f", b"short").await.unwrap();
    vol.flush().await.unwrap();

    assert_eq!(inum, again, "the inode should be reused, not replaced");
    assert_eq!(vol.read("/f").await.unwrap(), b"short");
    assert_eq!(vol.stat("/f").await.unwrap().size, 5);

    drop(vol);
    assert_clean(&dev, "after shrinking a file").await;
}

#[tokio::test]
async fn appends_to_a_file() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.append("/log", b"one\n").await.unwrap();
    vol.append("/log", b"two\n").await.unwrap();
    vol.append("/log", b"three\n").await.unwrap();
    vol.flush().await.unwrap();

    assert_eq!(vol.read("/log").await.unwrap(), b"one\ntwo\nthree\n");

    drop(vol);
    assert_clean(&dev, "after appending").await;
}

#[tokio::test]
async fn unlinks_a_file_and_reclaims_its_space() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    let before = vol.filesystem().superblock().free_blocks_count;

    vol.write("/temp.bin", &vec![7u8; 200_000]).await.unwrap();
    vol.flush().await.unwrap();
    let during = vol.filesystem().superblock().free_blocks_count;
    assert!(during < before, "writing should consume blocks");

    vol.unlink("/temp.bin").await.unwrap();
    vol.flush().await.unwrap();
    let after = vol.filesystem().superblock().free_blocks_count;

    assert_eq!(after, before, "unlink should give every block back");
    assert!(!vol.exists("/temp.bin").await.unwrap());

    drop(vol);
    assert_clean(&dev, "after unlink").await;
}

#[tokio::test]
async fn removes_an_empty_directory_but_not_a_full_one() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir("/full").await.unwrap();
    vol.write("/full/f", b"x").await.unwrap();
    vol.mkdir("/empty").await.unwrap();
    vol.flush().await.unwrap();

    assert!(matches!(
        vol.rmdir("/full").await,
        Err(Error::NotEmpty(_))
    ));

    vol.rmdir("/empty").await.unwrap();
    vol.flush().await.unwrap();
    assert!(!vol.exists("/empty").await.unwrap());
    assert!(vol.exists("/full").await.unwrap());

    drop(vol);
    assert_clean(&dev, "after rmdir").await;
}

#[tokio::test]
async fn a_directory_grows_past_one_block() {
    let dev = fresh(Profile::Ext4, 32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    // A 1 KiB block holds roughly 40 short names, so this forces the
    // directory to take a second and third block.
    vol.mkdir("/many").await.unwrap();
    for i in 0..150 {
        vol.write(&format!("/many/file-{i:04}"), b"x").await.unwrap();
    }
    vol.flush().await.unwrap();

    let entries = vol.read_dir("/many").await.unwrap();
    assert_eq!(entries.len(), 150);
    assert!(vol.stat("/many").await.unwrap().size > vol.filesystem().block_size() as u64);

    // And every one of them reads back.
    for i in 0..150 {
        assert_eq!(vol.read(&format!("/many/file-{i:04}")).await.unwrap(), b"x");
    }

    drop(vol);
    assert_clean(&dev, "after a multi-block directory").await;
}

#[tokio::test]
async fn works_on_ext2_and_ext3_without_extents() {
    for profile in [Profile::Ext2, Profile::Ext3] {
        let dev = fresh(profile, 32 * MIB).await;
        let mut vol = Volume::open(&dev).await.unwrap();
        vol.set_time(1_700_000_000);

        vol.mkdir("/dir").await.unwrap();
        // Past the twelve direct blocks, so indirect blocks are exercised.
        let data: Vec<u8> = (0..80_000u32).map(|i| (i % 199) as u8).collect();
        vol.write("/dir/indirect.bin", &data).await.unwrap();
        vol.write("/dir/small", b"tiny").await.unwrap();
        vol.flush().await.unwrap();

        assert_eq!(
            vol.read("/dir/indirect.bin").await.unwrap(),
            data,
            "{}",
            profile.name()
        );
        assert_eq!(vol.read("/dir/small").await.unwrap(), b"tiny");

        drop(vol);
        assert_clean(&dev, profile.name()).await;
    }
}

#[tokio::test]
async fn reports_the_obvious_mistakes() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    assert!(matches!(
        vol.read("/nope").await,
        Err(Error::NotFound(_))
    ));
    vol.mkdir("/d").await.unwrap();
    assert!(matches!(
        vol.read("/d").await,
        Err(Error::IsADirectory(_))
    ));
    assert!(matches!(
        vol.mkdir("/d").await,
        Err(Error::AlreadyExists(_))
    ));
    assert!(matches!(
        vol.write("/d", b"x").await,
        Err(Error::IsADirectory(_))
    ));
    assert!(matches!(
        vol.mkdir("/missing/child").await,
        Err(Error::NotFound(_))
    ));
}

/// Writes have to survive being closed and reopened, which means the bitmaps,
/// counters and superblock all landed on the device rather than staying in
/// memory.
#[tokio::test]
async fn everything_survives_a_reopen() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;

    {
        let mut vol = Volume::open(&dev).await.unwrap();
        vol.set_time(1_700_000_000);
        vol.mkdir_all("/a/b").await.unwrap();
        vol.write("/a/b/c.txt", b"persisted").await.unwrap();
        vol.flush().await.unwrap();
    }

    let vol = Volume::open(&dev).await.unwrap();
    assert_eq!(vol.read("/a/b/c.txt").await.unwrap(), b"persisted");
    assert_clean(&dev, "after reopen").await;
}

/// Permissions and ownership are not decoration: a root filesystem where
/// everything is `0644 root:root` boots into a broken system.
#[tokio::test]
async fn permissions_and_ownership_are_kept() {
    use fio_ext4::Attrs;

    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir_all("/etc").await.unwrap();
    vol.write_with("/etc/shadow", b"root:!:20000::::::\n", &Attrs::mode(0o600))
        .await
        .unwrap();

    vol.mkdir_all("/usr/bin").await.unwrap();
    vol.write_with(
        "/usr/bin/true",
        b"#!/bin/sh\nexit 0\n",
        &Attrs::mode(0o755).owner(0, 0),
    )
    .await
    .unwrap();

    // The sticky bit is a permission bit, so /tmp works the usual way.
    vol.mkdir_with("/tmp", &Attrs::mode(0o1777)).await.unwrap();

    // A user's home, owned by that user.
    vol.mkdir_with("/home", &Attrs::dir()).await.unwrap();
    vol.mkdir_with("/home/gw", &Attrs::mode(0o700).owner(1000, 1000))
        .await
        .unwrap();
    vol.write_with(
        "/home/gw/.profile",
        b"export PATH=/usr/bin\n",
        &Attrs::mode(0o644).owner(1000, 1000),
    )
    .await
    .unwrap();

    // And setuid, which is the bit that most obviously matters.
    vol.write_with("/usr/bin/su", b"\x7fELF", &Attrs::mode(0o4755))
        .await
        .unwrap();

    vol.flush().await.unwrap();

    let shadow = vol.stat("/etc/shadow").await.unwrap();
    assert_eq!(shadow.mode & 0o7777, 0o600);
    let exe = vol.stat("/usr/bin/true").await.unwrap();
    assert_eq!(exe.mode & 0o7777, 0o755);
    let tmp = vol.stat("/tmp").await.unwrap();
    assert_eq!(tmp.mode & 0o7777, 0o1777);
    assert!(tmp.is_dir());
    let home = vol.stat("/home/gw").await.unwrap();
    assert_eq!((home.uid, home.gid), (1000, 1000));
    assert_eq!(home.mode & 0o7777, 0o700);
    let su = vol.stat("/usr/bin/su").await.unwrap();
    assert_eq!(su.mode & 0o7777, 0o4755, "setuid bit must survive");

    drop(vol);
    assert_clean(&dev, "after setting modes and owners").await;
}

#[tokio::test]
async fn chmod_and_chown_change_what_they_should_and_nothing_else() {
    use fio_ext4::Attrs;

    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.write_with("/f", b"contents", &Attrs::mode(0o644)).await.unwrap();
    vol.chmod("/f", 0o600).await.unwrap();
    vol.chown("/f", 1000, 100).await.unwrap();
    vol.flush().await.unwrap();

    let st = vol.stat("/f").await.unwrap();
    assert_eq!(st.mode & 0o7777, 0o600);
    assert_eq!((st.uid, st.gid), (1000, 100));
    // chmod must not turn a file into something else.
    assert!(st.is_file());
    assert_eq!(vol.read("/f").await.unwrap(), b"contents");

    drop(vol);
    assert_clean(&dev, "after chmod and chown").await;
}

/// A root filesystem needs device nodes. Without `/dev/null` and
/// `/dev/console` a system does not boot and a container does not run.
#[tokio::test]
async fn device_nodes_fifos_and_sockets() {
    use fio_ext4::{Attrs, Special};

    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir("/dev").await.unwrap();
    vol.mknod("/dev/null", Special::CharDevice { major: 1, minor: 3 }, &Attrs::mode(0o666))
        .await
        .unwrap();
    vol.mknod("/dev/sda", Special::BlockDevice { major: 8, minor: 0 }, &Attrs::mode(0o660).owner(0, 6))
        .await
        .unwrap();
    vol.mknod("/dev/initctl", Special::Fifo, &Attrs::mode(0o600)).await.unwrap();
    vol.mknod("/dev/log", Special::Socket, &Attrs::mode(0o666)).await.unwrap();
    // Past 255, so the wider device-number encoding is used.
    vol.mknod("/dev/big", Special::CharDevice { major: 511, minor: 4095 }, &Attrs::mode(0o600))
        .await
        .unwrap();
    vol.flush().await.unwrap();

    async fn inode_at<D: mkfs_ext4::device::BlockDevice>(
        vol: &Volume<D>,
        path: &str,
    ) -> mkfs_ext4::structs::inode::Inode {
        let inum = vol.lookup(path).await.unwrap();
        vol.filesystem().read_inode(inum).await.unwrap()
    }

    let null = inode_at(&vol, "/dev/null").await;
    assert_eq!(null.file_type(), 0x2000, "character device");
    assert_eq!(null.device_numbers(), (1, 3));
    assert_eq!(null.blocks, 0, "a device owns no blocks");
    assert_eq!(null.size, 0);

    let sda = inode_at(&vol, "/dev/sda").await;
    assert_eq!(sda.file_type(), 0x6000, "block device");
    assert_eq!(sda.device_numbers(), (8, 0));
    assert_eq!((sda.uid, sda.gid), (0, 6));

    assert_eq!(inode_at(&vol, "/dev/initctl").await.file_type(), 0x1000, "fifo");
    assert_eq!(inode_at(&vol, "/dev/log").await.file_type(), 0xc000, "socket");

    // The wide encoding has to survive a round trip too.
    assert_eq!(inode_at(&vol, "/dev/big").await.device_numbers(), (511, 4095));

    drop(vol);
    assert_clean(&dev, "after creating device nodes").await;
}

#[tokio::test]
async fn symlinks_short_and_long() {
    let dev = fresh(Profile::Ext4, 16 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.mkdir_all("/usr/bin").await.unwrap();
    vol.symlink("/bin", "usr/bin").await.unwrap();

    // Longer than the 60 bytes that fit inside an inode, so it takes a block.
    let long = "/a/very/long/path/".repeat(6);
    vol.symlink("/far", &long).await.unwrap();
    vol.flush().await.unwrap();

    assert_eq!(vol.read_link("/bin").await.unwrap(), "usr/bin");
    assert_eq!(vol.read_link("/far").await.unwrap(), long);

    // A short target costs no blocks at all — that is the point of a fast
    // symlink.
    let short = vol.stat("/bin").await.unwrap();
    assert_eq!(short.blocks, 0);
    assert_eq!(short.size, 7);
    let far = vol.stat("/far").await.unwrap();
    assert!(far.blocks > 0);

    drop(vol);
    assert_clean(&dev, "after creating symlinks").await;
}
