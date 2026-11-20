//! The review findings of #23, one test or more each, named by finding id.
//!
//! Inside the crate rather than under `tests/`, because several cases need
//! a state no public writer makes — an attribute block shared by three
//! inodes, a journal holding a committed transaction — and build it through
//! the crate's own handles on the filesystem. Each case ends with a forced
//! fsck, as every test here does.

use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, CheckScope, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

use crate::error::Error;
use crate::volume::{Special, Volume};
use crate::Attrs;

const MIB: u64 = 1024 * 1024;

async fn fresh(profile: Profile, block_size: u32, size: u64) -> MemDevice {
    let dev = MemDevice::new(size);
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000)
        .block_size(block_size);
    format(&dev, &params).await.unwrap();
    dev
}

async fn assert_clean(dev: &MemDevice, what: &str) {
    let forced = FsckOptions::check_only().force(true);
    let report = fsck::check(dev, &forced).await.unwrap();
    assert_eq!(report.scope, CheckScope::Forced, "fsck skipped the filesystem");
    assert!(report.directories > 0, "fsck read no directories");
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

async fn open(dev: &MemDevice) -> Volume<&MemDevice> {
    let mut vol = Volume::open(dev).await.unwrap();
    vol.set_time(1_700_000_000);
    vol
}

/// Free blocks and inodes, as the superblock counts them right now.
fn free(vol: &Volume<&MemDevice>) -> (u64, u32) {
    let sb = vol.fs.superblock();
    (sb.free_blocks_count, sb.free_inodes_count)
}

// ---- names (3235dae01a21, 48f2ceb27e92) ----

#[tokio::test]
async fn dot_and_dotdot_are_never_a_name_to_change() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.mkdir("/d").await.unwrap();
        vol.mkdir("/d/empty").await.unwrap();
        vol.write("/f", b"file").await.unwrap();
        let before = free(&vol);

        for bad in ["/d/empty/.", "/d/empty/..", "/d/."] {
            let e = vol.rmdir(bad).await.unwrap_err();
            assert!(matches!(e, Error::InvalidName(_)), "rmdir {bad}: {e}");
        }
        for (from, to) in [("/d/empty/.", "/moved"), ("/f", "/d/.."), ("/f", "/d/.")] {
            let e = vol.rename(from, to).await.unwrap_err();
            assert!(matches!(e, Error::InvalidName(_)), "rename {from} {to}: {e}");
        }
        assert!(vol.write("/d/.", b"x").await.is_err());
        assert!(vol.mkdir("/d/..").await.is_err());
        assert!(vol.symlink("/d/.", "x").await.is_err());
        assert!(vol.unlink("/d/..").await.is_err());

        // Refused before an inode is taken, not after.
        let long = format!("/{}", "n".repeat(256));
        assert!(matches!(vol.write(&long, b"x").await.unwrap_err(), Error::InvalidName(_)));
        assert!(matches!(vol.mkdir("/a\0b").await.unwrap_err(), Error::InvalidName(_)));
        assert_eq!(free(&vol), before, "a refused name allocated something");

        assert!(vol.stat("/d/empty").await.unwrap().is_dir());
        assert_eq!(vol.read("/f").await.unwrap(), b"file");
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after refused dot names").await;
}

// ---- rename across types (ed975422b071) ----

#[tokio::test]
async fn rename_will_not_swap_a_directory_for_a_file() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.write("/f", b"keep me").await.unwrap();
        vol.mkdir("/d").await.unwrap();

        let e = vol.rename("/d", "/f").await.unwrap_err();
        assert!(matches!(e, Error::NotADirectory(_)), "dir onto file: {e}");
        let e = vol.rename("/f", "/d").await.unwrap_err();
        assert!(matches!(e, Error::IsADirectory(_)), "file onto dir: {e}");

        assert_eq!(vol.read("/f").await.unwrap(), b"keep me");
        assert!(vol.stat("/d").await.unwrap().is_dir());

        // Like onto like still replaces.
        vol.write("/g", b"new").await.unwrap();
        vol.rename("/g", "/f").await.unwrap();
        assert_eq!(vol.read("/f").await.unwrap(), b"new");
        vol.mkdir("/e").await.unwrap();
        vol.rename("/e", "/d").await.unwrap();
        assert!(!vol.exists("/e").await.unwrap());
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after typed renames").await;
}

// ---- data writes to special inodes (9534db6f345d, 978776e2fe03) ----

#[tokio::test]
async fn file_writes_leave_symlinks_and_devices_alone() {
    let dev = fresh(Profile::Ext4, 4096, 32 * MIB).await;
    {
        let mut vol = open(&dev).await;
        vol.symlink("/s", "target").await.unwrap();
        vol.mknod("/null", Special::CharDevice { major: 1, minor: 3 }, &Attrs::mode(0o666))
            .await
            .unwrap();
        vol.mknod("/pipe", Special::Fifo, &Attrs::mode(0o600)).await.unwrap();
        let before = free(&vol);

        for path in ["/s", "/null", "/pipe"] {
            assert!(vol.write(path, b"x").await.is_err(), "write {path}");
            assert!(vol.append(path, b"x").await.is_err(), "append {path}");
            assert!(vol.write_at(path, 3, b"x").await.is_err(), "write_at {path}");
        }
        assert_eq!(free(&vol), before);
        assert_eq!(vol.read_link("/s").await.unwrap(), "target");
        assert_eq!(vol.device_numbers("/null").await.unwrap(), (1, 3));
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, "after refused special writes").await;
}

// ---- symlink target length (f737f3d6e9e4) ----

async fn symlink_limits(block_size: u32) {
    let dev = fresh(Profile::Ext4, block_size, 32 * MIB).await;
    let max = (block_size as usize).min(4096) - 1;
    {
        let mut vol = open(&dev).await;
        let longest = "t".repeat(max);
        vol.symlink("/longest", &longest).await.unwrap();
        assert_eq!(vol.read_link("/longest").await.unwrap(), longest);

        let before = free(&vol);
        let e = vol.symlink("/too-long", &"t".repeat(max + 1)).await.unwrap_err();
        assert!(matches!(e, Error::InvalidPath(_)), "{block_size}: {e}");
        assert!(vol.symlink("/empty", "").await.is_err());
        assert_eq!(free(&vol), before, "{block_size}: a refused symlink allocated");
        assert!(!vol.exists("/too-long").await.unwrap());
        vol.flush().await.unwrap();
    }
    assert_clean(&dev, &format!("symlinks at {block_size}")).await;
}

#[tokio::test]
async fn symlink_targets_are_bounded_before_allocating_1k() {
    symlink_limits(1024).await;
}

#[tokio::test]
async fn symlink_targets_are_bounded_before_allocating_4k() {
    symlink_limits(4096).await;
}
