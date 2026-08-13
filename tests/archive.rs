//! Tar archives in and out of a filesystem.
//!
//! The assertions that matter here are about what survives: a file's mode, its
//! owner, its labels, and whether a later layer inherits any of them from the
//! file it replaced. It should not — that is the whole difficulty of stacking
//! layers, and the easiest thing to get quietly wrong, because an image whose
//! `/etc/shadow` kept the previous layer's mode still boots.
//!
//! And, as everywhere else in this crate: the filesystem checks clean
//! afterwards, or the write did not happen, it did damage.

use fio_ext4::tar::{self, EntryKind, Header};
use fio_ext4::Volume;
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fsck::{self, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

async fn fresh(size: u64) -> MemDevice {
    let dev = MemDevice::new(size);
    let params = Params::new(Profile::Ext4)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&dev, &params).await.unwrap();
    dev
}

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

/// Build an archive from entries, the way a layer would arrive.
async fn archive(entries: &[(Header, &[u8])]) -> Vec<u8> {
    let mut writer = tar::Writer::new(Vec::new());
    for (header, data) in entries {
        writer.append(header, data).await.unwrap();
    }
    writer.finish().await.unwrap();
    writer.into_inner()
}

fn file(path: &str, mode: u16) -> Header {
    Header {
        path: path.into(),
        kind: EntryKind::File,
        mode,
        mtime: 1_700_000_000,
        ..Default::default()
    }
}

fn dir(path: &str) -> Header {
    Header {
        path: path.into(),
        kind: EntryKind::Directory,
        mode: 0o755,
        mtime: 1_700_000_000,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_tree_survives_the_round_trip() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    let mut suid = file("usr/bin/hello", 0o4755);
    suid.uid = 0;
    let mut owned = file("etc/hostname", 0o644);
    owned.uid = 1000;
    owned.gid = 1000;
    owned.xattrs = vec![("security.selinux".into(), b"system_u:object_r:etc_t:s0".to_vec())];
    let link = Header {
        path: "bin".into(),
        kind: EntryKind::Symlink,
        link: "usr/bin".into(),
        mode: 0o777,
        ..Default::default()
    };
    let node = Header {
        path: "dev/null".into(),
        kind: EntryKind::CharDevice,
        mode: 0o666,
        major: 1,
        minor: 3,
        mtime: 1_700_000_000,
        ..Default::default()
    };

    let source = archive(&[
        (dir("etc"), &b""[..]),
        (owned, b"router\n"),
        (dir("usr"), b""),
        (dir("usr/bin"), b""),
        (suid, b"#!/bin/sh\n"),
        (dir("dev"), b""),
        (node, b""),
        (link, b""),
    ])
    .await;

    let report = vol.unpack_tar(&source).await.unwrap();
    assert_eq!(report.files, 2);
    assert_eq!(report.directories, 4);
    assert_eq!(report.symlinks, 1);
    assert_eq!(report.devices, 1);
    assert_eq!(report.bytes, 17);

    // Out again, and everything that was set is still set.
    let out = vol.pack_tar("/").await.unwrap();
    let back: Vec<_> = tar::read(&out)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.header.path.clone(), e.header))
        .collect();
    let find = |path: &str| {
        back.iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("{path} is missing from the archive"))
            .1
            .clone()
    };

    assert_eq!(find("usr/bin/hello").mode, 0o4755, "the setuid bit");
    assert_eq!(find("etc/hostname").uid, 1000);
    assert_eq!(find("etc/hostname").gid, 1000);
    assert_eq!(find("etc/hostname").mtime, 1_700_000_000);
    assert_eq!(
        find("etc/hostname").xattrs,
        vec![(
            "security.selinux".to_string(),
            b"system_u:object_r:etc_t:s0".to_vec()
        )]
    );
    assert_eq!(find("bin").kind, EntryKind::Symlink);
    assert_eq!(find("bin").link, "usr/bin");
    assert_eq!(find("dev/null").kind, EntryKind::CharDevice);
    assert_eq!((find("dev/null").major, find("dev/null").minor), (1, 3));

    vol.flush().await.unwrap();
    assert_clean(&dev, "after a round trip").await;
}

#[tokio::test]
async fn names_come_out_depth_first_and_sorted() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    let source = archive(&[
        (dir("b"), &b""[..]),
        (dir("a"), b""),
        (dir("a/z"), b""),
        (file("a/z/deep", 0o644), b"x"),
        (file("a/m", 0o644), b"x"),
        (file("b/only", 0o644), b"x"),
    ])
    .await;
    vol.unpack_tar(&source).await.unwrap();

    let out = vol.pack_tar("/").await.unwrap();
    let names: Vec<String> = tar::read(&out)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.header.path)
        .filter(|p| p != "lost+found")
        .collect();

    // Depth-first in sorted order — a directory is immediately followed by
    // what is inside it, which is what every other tar produces and what makes
    // two runs over the same tree byte-identical.
    assert_eq!(names, ["a", "a/m", "a/z", "a/z/deep", "b", "b/only"]);
}

#[tokio::test]
async fn a_replacing_file_inherits_nothing_from_the_one_below() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    // The lower layer: locked down, labelled, owned by root.
    let mut lower = file("etc/shadow", 0o600);
    lower.xattrs = vec![
        (
            "security.selinux".into(),
            b"system_u:object_r:shadow_t:s0".to_vec(),
        ),
        ("user.layer".into(), b"one".to_vec()),
    ];
    vol.unpack_tar(&archive(&[(dir("etc"), &b""[..]), (lower, b"root:!:\n")]).await)
        .await
        .unwrap();

    // The upper layer replaces it, saying nothing about labels.
    let mut upper = file("etc/shadow", 0o644);
    upper.uid = 1234;
    upper.gid = 1234;
    vol.unpack_tar(&archive(&[(upper, &b"replaced\n"[..])]).await)
        .await
        .unwrap();

    let stat = vol.stat("/etc/shadow").await.unwrap();
    assert_eq!(stat.mode & 0o7777, 0o644, "mode came from the lower layer");
    assert_eq!(stat.uid, 1234, "owner came from the lower layer");
    assert_eq!(
        vol.list_xattrs("/etc/shadow").await.unwrap(),
        vec![],
        "the replaced file kept the label of the file underneath it"
    );
    assert_eq!(vol.read("/etc/shadow").await.unwrap(), b"replaced\n");

    vol.flush().await.unwrap();
    assert_clean(&dev, "after replacing a labelled file").await;
}

#[tokio::test]
async fn a_replacing_directory_loses_the_labels_below_it() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    let mut lower = dir("etc");
    lower.mode = 0o700;
    lower.xattrs = vec![("user.layer".into(), b"one".to_vec())];
    vol.unpack_tar(&archive(&[(lower, &b""[..])]).await)
        .await
        .unwrap();
    assert_eq!(vol.list_xattrs("/etc").await.unwrap().len(), 1);

    // The upper layer names the same directory and says nothing about labels.
    vol.unpack_tar(&archive(&[(dir("etc"), &b""[..])]).await)
        .await
        .unwrap();

    assert_eq!(vol.stat("/etc").await.unwrap().mode & 0o7777, 0o755);
    assert_eq!(
        vol.list_xattrs("/etc").await.unwrap(),
        vec![],
        "the directory kept the label from the layer below"
    );

    vol.flush().await.unwrap();
    assert_clean(&dev, "after replacing a labelled directory").await;
}

#[tokio::test]
async fn whiteouts_delete_and_opaque_markers_empty() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.unpack_tar(
        &archive(&[
            (dir("etc"), &b""[..]),
            (file("etc/keep", 0o644), b"k"),
            (file("etc/drop", 0o644), b"d"),
            (dir("var"), b""),
            (dir("var/lib"), b""),
            (file("var/lib/state", 0o644), b"s"),
            (file("var/log", 0o644), b"l"),
        ])
        .await,
    )
    .await
    .unwrap();

    let layer = archive(&[
        (file("etc/.wh.drop", 0o644), &b""[..]),
        (file("var/.wh..wh..opq", 0o644), b""),
        (file("var/fresh", 0o644), b"f"),
        // aufs bookkeeping, which is dropped rather than written.
        (file("etc/.wh..wh.plnk", 0o644), b""),
    ])
    .await;
    let report = vol
        .unpack_tar_layer(tar::Bytes::new(&layer), "/")
        .await
        .unwrap();
    assert_eq!(report.removed, 3, "drop, plus var's two children");

    assert!(vol.exists("/etc/keep").await.unwrap());
    assert!(!vol.exists("/etc/drop").await.unwrap(), "whiteout ignored");
    assert!(!vol.exists("/etc/.wh.drop").await.unwrap(), "marker written");
    assert!(!vol.exists("/etc/.wh..wh.plnk").await.unwrap());
    assert!(
        !vol.exists("/var/lib").await.unwrap(),
        "the opaque marker left a subtree behind"
    );
    assert!(!vol.exists("/var/log").await.unwrap());
    assert!(vol.exists("/var/fresh").await.unwrap(), "the layer's own");

    vol.flush().await.unwrap();
    assert_clean(&dev, "after applying whiteouts").await;
}

#[tokio::test]
async fn without_layer_semantics_a_whiteout_is_just_a_name() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    vol.unpack_tar(
        &archive(&[
            (dir("etc"), &b""[..]),
            (file("etc/drop", 0o644), b"d"),
            (file("etc/.wh.drop", 0o644), b"w"),
        ])
        .await,
    )
    .await
    .unwrap();

    assert!(vol.exists("/etc/drop").await.unwrap());
    assert_eq!(vol.read("/etc/.wh.drop").await.unwrap(), b"w");
    vol.flush().await.unwrap();
    assert_clean(&dev, "after an ordinary tarball").await;
}

#[tokio::test]
async fn deleting_a_file_gives_back_its_attribute_block() {
    let dev = fresh(32 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    // Enough attributes that they cannot fit in the inode and need a block of
    // their own — which is reachable only from the inode, and so is leaked by
    // a delete that does not free it. e2fsck calls that "block bitmap
    // differences".
    let mut heavy = file("etc/labelled", 0o644);
    heavy.xattrs = (0..6)
        .map(|i| (format!("user.attribute{i}"), vec![b'v'; 60]))
        .collect();
    vol.unpack_tar(&archive(&[(dir("etc"), &b""[..]), (heavy, b"x")]).await)
        .await
        .unwrap();
    assert_eq!(vol.list_xattrs("/etc/labelled").await.unwrap().len(), 6);

    vol.unlink("/etc/labelled").await.unwrap();
    vol.flush().await.unwrap();
    assert_clean(&dev, "after deleting a file with an attribute block").await;
}

#[tokio::test]
async fn a_file_larger_than_the_streaming_threshold_arrives_whole() {
    let dev = fresh(64 * MIB).await;
    let mut vol = Volume::open(&dev).await.unwrap();
    vol.set_time(1_700_000_000);

    // Past the point where the unpacker stops gathering and starts streaming,
    // so both halves of that path are exercised.
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    vol.unpack_tar(&archive(&[(file("big.bin", 0o644), &big)]).await)
        .await
        .unwrap();

    assert_eq!(vol.read("/big.bin").await.unwrap(), big);
    vol.flush().await.unwrap();
    assert_clean(&dev, "after a large file").await;
}
