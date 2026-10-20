use super::{archive, pins::Pin};
use std::{
    fs,
    io::{Cursor, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "morpheus-archive-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str, mode: u32) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            name,
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    writer.write_all(b"fixture").unwrap();
    let mut bytes = writer.finish().unwrap().into_inner();
    // Set the central directory's Unix metadata independently of the writer,
    // whose public permission setter already masks special bits.
    let central = bytes.windows(4).position(|s| s == b"PK\x01\x02").unwrap();
    bytes[central + 5] = 3;
    bytes[central + 38..central + 42].copy_from_slice(&(mode << 16).to_le_bytes());
    bytes
}

const ABC: Pin = Pin {
    bytes: 3,
    sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
};

#[test]
fn downloaded_bytes_require_the_pinned_size_and_digest() {
    let mut output = Vec::new();
    archive::copy_verified(&b"abc"[..], &mut output, &ABC).unwrap();
    assert_eq!(output, b"abc");
    for bytes in [&b"ab"[..], &b"abd"[..], &b"abcd"[..]] {
        assert!(archive::copy_verified(bytes, Vec::new(), &ABC).is_err());
    }
}

#[test]
fn cache_receipts_are_bounded_and_match_the_reviewed_identity() {
    let directory = Directory::new();
    assert!(archive::check_receipt(&directory.0, &ABC).is_err());
    archive::write_receipt(&directory.0, &ABC).unwrap();
    archive::check_receipt(&directory.0, &ABC).unwrap();
    assert!(archive::write_receipt(&directory.0, &ABC).is_err());
    // A cache hit must match the pinned identity, not merely the 64-byte shape.
    fs::write(directory.0.join(archive::RECEIPT), "0".repeat(64)).unwrap();
    assert!(archive::check_receipt(&directory.0, &ABC).is_err());
    fs::write(directory.0.join(archive::RECEIPT), ABC.sha256).unwrap();
    archive::check_receipt(&directory.0, &ABC).unwrap();
    fs::write(directory.0.join(archive::RECEIPT), "x".repeat(1024)).unwrap();
    assert!(archive::check_receipt(&directory.0, &ABC).is_err());
}

#[test]
fn unsafe_paths_and_link_entries_are_rejected_before_creation() {
    for name in [
        "../outside",
        "/outside",
        "C:/outside",
        "bundle/file:ads",
        "bundle/NUL",
        "bundle/dir\\file",
        "bundle/file.",
    ] {
        let directory = Directory::new();
        let mut zip = zip::ZipArchive::new(Cursor::new(fixture(name, 0o100644))).unwrap();
        assert!(archive::extract(&mut zip, &directory.0).is_err());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }
    let directory = Directory::new();
    let mut zip = zip::ZipArchive::new(Cursor::new(fixture("link", 0o120777))).unwrap();
    assert!(archive::extract(&mut zip, &directory.0).is_err());
    assert!(!directory.0.join("link").exists());
}

#[test]
fn oversized_entries_and_existing_files_are_not_unpacked_or_overwritten() {
    let directory = Directory::new();
    let mut bytes = fixture("oversized", 0o100644);
    let central = bytes.windows(4).position(|s| s == b"PK\x01\x02").unwrap();
    bytes[central + 24..central + 28]
        .copy_from_slice(&(2_u32 * 1024 * 1024 * 1024 + 1).to_le_bytes());
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    assert!(archive::extract(&mut zip, &directory.0).is_err());
    assert!(!directory.0.join("oversized").exists());
    fs::write(directory.0.join("kept"), b"existing").unwrap();
    let mut zip = zip::ZipArchive::new(Cursor::new(fixture("kept", 0o100644))).unwrap();
    assert!(archive::extract(&mut zip, &directory.0).is_err());
    assert_eq!(fs::read(directory.0.join("kept")).unwrap(), b"existing");
}

#[cfg(unix)]
#[test]
fn regular_files_drop_special_bits_and_directory_links_are_not_followed() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let directory = Directory::new();
    let mut zip = zip::ZipArchive::new(Cursor::new(fixture("bundle/file", 0o107777))).unwrap();
    archive::extract(&mut zip, &directory.0).unwrap();
    assert_eq!(
        fs::read(directory.0.join("bundle/file")).unwrap(),
        b"fixture"
    );
    assert_eq!(
        fs::metadata(directory.0.join("bundle/file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    let outside = Directory::new();
    fs::write(outside.0.join("file"), b"kept").unwrap();
    symlink(&outside.0, directory.0.join("redirect")).unwrap();
    let mut zip = zip::ZipArchive::new(Cursor::new(fixture("redirect/file", 0o100644))).unwrap();
    assert!(archive::extract(&mut zip, &directory.0).is_err());
    assert_eq!(fs::read(outside.0.join("file")).unwrap(), b"kept");
}

#[cfg(unix)]
#[test]
fn cached_receipts_and_roots_cannot_be_links() {
    use std::os::unix::fs::symlink;
    let directory = Directory::new();
    let outside = Directory::new();
    fs::write(outside.0.join("identity"), ABC.sha256).unwrap();
    symlink(
        outside.0.join("identity"),
        directory.0.join(archive::RECEIPT),
    )
    .unwrap();
    assert!(archive::check_receipt(&directory.0, &ABC).is_err());
    symlink(&outside.0, directory.0.join("cache")).unwrap();
    assert!(archive::write_receipt(&directory.0.join("cache"), &ABC).is_err());
    assert!(!outside.0.join(archive::RECEIPT).exists());
    assert_eq!(
        fs::read(outside.0.join("identity")).unwrap(),
        ABC.sha256.as_bytes()
    );
}
