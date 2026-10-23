//! Bounded `.app.tar.gz` extraction for Morpheus's separate transaction layer.
//! This does not verify a release signature, install, execute, or delete a bundle.
//! The caller must verify the exact input bytes first and supply an empty private
//! directory. The retained upstream installer is deliberately not involved.

use crate::{Error, Result};
use flate2::read::MultiGzDecoder;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{c_char, c_int, CString},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 32_768;
const ROOT: &str = "Morpheus.app";

fn invalid() -> Error {
    Error::Security("invalid macOS update archive")
}

unsafe extern "C" {
    // Darwin's public sys/stat.h declares mode_t as uint16_t. Its lchmod(3)
    // changes the link itself, never the target; unsupported filesystems fail.
    fn lchmod(path: *const c_char, mode: u16) -> c_int;
    fn geteuid() -> u32;
}

/// Restore an owned link's exact ordinary permission bits without following it.
/// Used by both archive extraction and the application's backup copy. Directory
/// and regular-file write restrictions are separate and remain unchanged.
pub fn restore_owned_symlink_mode(path: &Path, mode: u32) -> Result<()> {
    if mode & !0o777 != 0 {
        return Err(invalid());
    }
    let before = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no arguments and returns the process's effective UID.
    let owner = unsafe { geteuid() };
    if !before.file_type().is_symlink()
        || before.uid() != owner
        || before.nlink() != 1
        || before.mode() & 0o7000 != 0
    {
        return Err(invalid());
    }
    let path_c = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid())?;
    // SAFETY: the CString is live and NUL-terminated, the validated mode fits
    // Darwin mode_t, and lchmod explicitly acts on the link rather than target.
    if unsafe { lchmod(path_c.as_ptr(), mode as u16) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let after = fs::symlink_metadata(path)?;
    if !after.file_type().is_symlink()
        || after.uid() != owner
        || after.nlink() != 1
        || after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.mode() & 0o7777 != mode
    {
        return Err(invalid());
    }
    Ok(())
}

fn relative(path: &Path) -> Result<PathBuf> {
    let text = path.to_str().ok_or_else(invalid)?;
    // Shipping resources use ASCII names. Reject Unicode/case aliases and
    // platform-specific separators rather than relying on filesystem folding.
    if text.len() > 1024
        || !text.is_ascii()
        || text
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b == b'\\' || b == b':')
    {
        return Err(invalid());
    }
    let parts: Vec<_> = path.components().collect();
    if parts.is_empty()
        || parts.len() > 32
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
        || parts[0].as_os_str() != ROOT
    {
        return Err(invalid());
    }
    Ok(parts.iter().map(|part| part.as_os_str()).collect())
}

fn internal_link(path: &Path, target: &Path) -> Result<()> {
    if target.as_os_str().is_empty() || target.is_absolute() {
        return Err(invalid());
    }
    let text = target.to_str().ok_or_else(invalid)?;
    if text.len() > 1024
        || !text.is_ascii()
        || text
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b == b'\\' || b == b':')
    {
        return Err(invalid());
    }
    let mut depth = path.components().count() - 1;
    for part in target.components() {
        match part {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 1 => depth -= 1,
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

fn register_path(
    path: &Path,
    directory: bool,
    registered: &mut BTreeMap<String, (PathBuf, bool)>,
) -> Result<()> {
    let mut prefix = PathBuf::new();
    let count = path.components().count();
    let mut pending = Vec::new();
    for (index, part) in path.components().enumerate() {
        if !matches!(part, Component::Normal(_)) {
            return Err(invalid());
        }
        prefix.push(part);
        let directory = index + 1 != count || directory;
        let key = prefix.to_str().ok_or_else(invalid)?.to_ascii_lowercase();
        if let Some((existing, existing_directory)) = registered.get(&key) {
            // Case-insensitive APFS must not combine two spellings of an
            // implicit directory, nor turn a file/link prefix into a directory.
            if existing != &prefix || *existing_directory != directory {
                return Err(invalid());
            }
        } else {
            pending.push((key, (prefix.clone(), directory)));
        }
    }
    if registered
        .len()
        .checked_add(pending.len())
        .is_none_or(|len| len > MAX_ENTRIES)
    {
        return Err(invalid());
    }
    // Register every concrete prefix before any mkdir. The bound includes
    // implicit parent directories, regular files and deferred links together.
    registered.extend(pending);
    Ok(())
}

fn parents(destination: &Path, relative: &Path) -> Result<()> {
    let mut path = destination.to_path_buf();
    for part in relative.parent().ok_or_else(invalid)?.components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(invalid()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            }
            Err(_) => return Err(invalid()),
        }
    }
    Ok(())
}

/// Extract a previously signature-verified archive to an empty private
/// directory, returning its single `.app` path. All paths, types, file sizes,
/// links and gzip output are bounded. No tar unpack/preserve APIs are used.
/// Failed extraction is left in the caller-owned staging directory for its
/// transaction's explicit bounded cleanup; no external path is removed.
pub fn unpack_verified_bundle(bytes: &[u8], destination: &Path) -> Result<PathBuf> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(invalid());
    }
    let metadata = fs::symlink_metadata(destination)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
        || fs::read_dir(destination)?.next().is_some()
    {
        return Err(invalid());
    }
    let decoder = MultiGzDecoder::new(bytes).take(MAX_EXPANDED_BYTES + 1);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = BTreeSet::new();
    let mut registered = BTreeMap::new();
    let mut links = Vec::new();
    let mut directories = Vec::new();
    let mut count = 0usize;
    for entry in archive.entries()?.raw(true) {
        count += 1;
        if count > MAX_ENTRIES {
            return Err(invalid());
        }
        let mut entry = entry?;
        let path = relative(&entry.path()?)?;
        let key = path.to_str().ok_or_else(invalid)?.to_ascii_lowercase();
        if !seen.insert(key) {
            return Err(invalid());
        }
        let header = entry.header();
        let kind = header.entry_type();
        let size = header.size()?;
        let mode = header.mode()?;
        if size > MAX_ARCHIVE_BYTES
            || if kind.is_symlink() {
                mode & !0o777 != 0
            } else {
                mode & 0o7022 != 0
            }
        {
            return Err(invalid());
        }
        register_path(&path, kind.is_dir(), &mut registered)?;
        parents(destination, &path)?;
        let output = destination.join(&path);
        if kind.is_dir() {
            if size != 0 {
                return Err(invalid());
            }
            match fs::create_dir(&output) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = fs::symlink_metadata(&output)?;
                    if !existing.is_dir() || existing.file_type().is_symlink() {
                        return Err(invalid());
                    }
                }
                Err(error) => return Err(error.into()),
            }
            fs::set_permissions(&output, fs::Permissions::from_mode(0o700))?;
            directories.push((output, mode & 0o777));
        } else if kind.is_file() {
            if path.components().count() < 3 {
                return Err(invalid());
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600 | (mode & 0o111))
                .open(&output)?;
            let copied = std::io::copy(&mut entry, &mut file)?;
            if copied != size {
                return Err(invalid());
            }
            file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
            file.flush()?;
            file.sync_all()?;
        } else if kind.is_symlink() {
            if size != 0 || path.components().count() < 3 {
                return Err(invalid());
            }
            let target = entry.link_name()?.ok_or_else(invalid)?.into_owned();
            internal_link(&path, &target)?;
            // Create links last: no archive entry can traverse one while writing.
            links.push((output, target, mode));
        } else {
            // Hard links, sparse files, devices, FIFOs and opaque metadata are
            // not application resources. PAX/GNU path handling stays in tar's
            // bounded reader. Raw mode rejects their extension records rather
            // than allocating attacker-sized GNU/PAX metadata.
            return Err(invalid());
        }
    }
    let mut decoder = archive.into_inner();
    let mut tail = [0; 64 * 1024];
    loop {
        let n = decoder.read(&mut tail)?;
        if n == 0 {
            break;
        }
        if tail[..n].iter().any(|b| *b != 0) {
            return Err(invalid());
        }
    }
    if decoder.limit() == 0 || count == 0 {
        return Err(invalid());
    }
    for (path, target, mode) in &links {
        std::os::unix::fs::symlink(target, path)?;
        restore_owned_symlink_mode(path, *mode)?;
    }
    let bundle = destination.join(ROOT);
    let canonical = bundle.canonicalize()?;
    for (path, _, _) in links {
        // Also reject dangling links, cycles and link chains that escape after
        // resolution. Lexical checks alone are insufficient for link chains.
        if !path.canonicalize()?.starts_with(&canonical) {
            return Err(invalid());
        }
    }
    // Apply exact allowed directory modes last, so a legitimate readonly
    // resource directory does not prevent filling its children. All bytes and
    // concrete paths were validated before a directory can become less private.
    for (path, mode) in directories.into_iter().rev() {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};

    fn private_destination() -> tempfile::TempDir {
        // tempfile3.27 requests0777 for directories by default. Supply the
        // parser's actual private staging contract explicitly; never change
        // process-global umask or let a destination rejection mask a test of
        // archive traversal, aliases, links, modes or bounded decoding.
        let directory = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        assert_eq!(
            fs::symlink_metadata(directory.path()).unwrap().mode() & 0o777,
            0o700
        );
        directory
    }

    fn archive(entries: &[(&str, tar::EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        archive_with_link_mode(entries, 0o777)
    }

    fn archive_with_link_mode(
        entries: &[(&str, tar::EntryType, &[u8], Option<&str>)],
        link_mode: u32,
    ) -> Vec<u8> {
        let mut tar = Vec::new();
        for (path, kind, bytes, link) in entries {
            let mut header = tar::Header::new_ustar();
            // Write raw header paths so the fixture generator does not reject
            // hostile names on the parser's behalf. Nothing is executed.
            header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
            header.set_entry_type(*kind);
            header.set_mode(if kind.is_symlink() { link_mode } else { 0o700 });
            header.set_size(bytes.len() as u64);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            header.set_cksum();
            tar.extend_from_slice(header.as_bytes());
            tar.extend_from_slice(bytes);
            tar.resize(tar.len().next_multiple_of(512), 0);
        }
        tar.extend_from_slice(&[0; 1024]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        gzip.write_all(&tar).unwrap();
        gzip.finish().unwrap()
    }

    #[test]
    fn extracts_only_owned_bundle_files_and_safe_internal_links() {
        let destination = private_destination();
        let bytes = archive(&[
            (
                "Morpheus.app/Contents/Resources/theme",
                tar::EntryType::Regular,
                b"theme",
                None,
            ),
            (
                "Morpheus.app/Contents/Resources/link",
                tar::EntryType::Symlink,
                b"",
                Some("theme"),
            ),
        ]);
        let bundle = unpack_verified_bundle(&bytes, destination.path()).unwrap();
        assert_eq!(
            fs::read(bundle.join("Contents/Resources/link")).unwrap(),
            b"theme"
        );
        assert!(fs::symlink_metadata(bundle.join("Contents/Resources/link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn valid_archive_requires_a_private_real_empty_destination() {
        let bytes = archive(&[(
            "Morpheus.app/Contents/file",
            tar::EntryType::Regular,
            b"data",
            None,
        )]);
        let destination = private_destination();
        fs::set_permissions(destination.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
        assert!(fs::read_dir(destination.path()).unwrap().next().is_none());
        fs::set_permissions(destination.path(), fs::Permissions::from_mode(0o700)).unwrap();

        let owner = private_destination();
        let alias = owner.path().join("destination-link");
        std::os::unix::fs::symlink(destination.path(), &alias).unwrap();
        assert!(unpack_verified_bundle(&bytes, &alias).is_err());
        assert!(fs::read_dir(destination.path()).unwrap().next().is_none());

        let sentinel = destination.path().join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 1);
        fs::remove_file(sentinel).unwrap();

        // Identical valid bytes must reach the archive reader after only the
        // destination precondition is satisfied; rejection cases above cannot
        // accidentally pass because their archive input was malformed.
        let app = unpack_verified_bundle(&bytes, destination.path()).unwrap();
        assert_eq!(fs::read(app.join("Contents/file")).unwrap(), b"data");
    }

    #[test]
    fn preserves_exact_link_modes_without_changing_the_target_or_accepting_special_bits() {
        let entries = [
            (
                "Morpheus.app/Contents/Resources/theme",
                tar::EntryType::Regular,
                &b"theme"[..],
                None,
            ),
            (
                "Morpheus.app/Contents/Resources/link",
                tar::EntryType::Symlink,
                &b""[..],
                Some("theme"),
            ),
        ];
        for mode in [0o777, 0o700, 0o640, 0] {
            let destination = private_destination();
            let bytes = archive_with_link_mode(&entries, mode);
            let app = unpack_verified_bundle(&bytes, destination.path()).unwrap();
            assert_eq!(
                fs::symlink_metadata(app.join("Contents/Resources/link"))
                    .unwrap()
                    .mode()
                    & 0o7777,
                mode
            );
            assert_eq!(
                fs::metadata(app.join("Contents/Resources/theme"))
                    .unwrap()
                    .mode()
                    & 0o7777,
                0o700
            );
            assert_eq!(
                fs::read(app.join("Contents/Resources/link")).unwrap(),
                b"theme"
            );
        }
        for mode in [0o1777, 0o2777, 0o4777, 0o100777] {
            let destination = private_destination();
            assert!(unpack_verified_bundle(
                &archive_with_link_mode(&entries, mode),
                destination.path()
            )
            .is_err());
        }
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("ordinary");
        fs::write(&file, b"keep").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(restore_owned_symlink_mode(&file, 0o777).is_err());
        assert!(restore_owned_symlink_mode(directory.path(), 0o777).is_err());
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o600);
    }

    #[test]
    fn rejects_traversal_foreign_roots_special_entries_and_case_aliases() {
        for path in [
            "../outside",
            "/Morpheus.app/Contents/file",
            "Other.app/Contents/file",
            "Morpheus.app/Contents/../outside",
            "Morpheus.app/Contents/a:b",
        ] {
            let destination = private_destination();
            let bytes = archive(&[(path, tar::EntryType::Regular, b"no", None)]);
            assert!(
                unpack_verified_bundle(&bytes, destination.path()).is_err(),
                "{path}"
            );
        }
        for kind in [
            tar::EntryType::Link,
            tar::EntryType::Fifo,
            tar::EntryType::GNUSparse,
            tar::EntryType::GNULongName,
            tar::EntryType::XHeader,
        ] {
            let destination = private_destination();
            let bytes = archive(&[(
                "Morpheus.app/Contents/file",
                kind,
                b"",
                Some("Contents/other"),
            )]);
            assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
        }
        let destination = private_destination();
        let bytes = archive(&[
            (
                "Morpheus.app/Contents/Theme",
                tar::EntryType::Regular,
                b"one",
                None,
            ),
            (
                "Morpheus.app/Contents/theme",
                tar::EntryType::Regular,
                b"two",
                None,
            ),
        ]);
        assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
    }

    #[test]
    fn archive_rejects_case_aliases_in_implicit_parent_directories() {
        let destination = private_destination();
        let bytes = archive(&[
            (
                "Morpheus.app/Contents/Foo/a",
                tar::EntryType::Regular,
                b"one",
                None,
            ),
            (
                "Morpheus.app/Contents/foo/b",
                tar::EntryType::Regular,
                b"two",
                None,
            ),
        ]);
        assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
    }

    #[test]
    fn rejects_external_dangling_and_parent_link_chains_without_touching_targets() {
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        for target in ["../../../../outside", "/tmp/outside", "missing"] {
            let destination = private_destination();
            let bytes = archive(&[(
                "Morpheus.app/Contents/Resources/link",
                tar::EntryType::Symlink,
                b"",
                Some(target),
            )]);
            assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
            assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
        }
        let destination = private_destination();
        let bytes = archive(&[
            (
                "Morpheus.app/Contents/link",
                tar::EntryType::Symlink,
                b"",
                Some("Resources"),
            ),
            (
                "Morpheus.app/Contents/link/file",
                tar::EntryType::Regular,
                b"no traversal",
                None,
            ),
        ]);
        assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
    }

    #[test]
    fn rejects_gzip_corruption_nonempty_destinations_and_large_declared_files() {
        let destination = private_destination();
        let mut bytes = archive(&[(
            "Morpheus.app/Contents/file",
            tar::EntryType::Regular,
            b"data",
            None,
        )]);
        bytes.truncate(bytes.len() - 3);
        assert!(unpack_verified_bundle(&bytes, destination.path()).is_err());
        let destination = private_destination();
        fs::write(destination.path().join("sentinel"), b"keep").unwrap();
        assert!(unpack_verified_bundle(&archive(&[]), destination.path()).is_err());
        assert_eq!(
            fs::read(destination.path().join("sentinel")).unwrap(),
            b"keep"
        );
        let mut header = tar::Header::new_ustar();
        header.set_path("Morpheus.app/Contents/huge").unwrap();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o700);
        header.set_size(MAX_ARCHIVE_BYTES + 1);
        header.set_cksum();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        gzip.write_all(header.as_bytes()).unwrap();
        let destination = private_destination();
        assert!(unpack_verified_bundle(&gzip.finish().unwrap(), destination.path()).is_err());
    }

    #[test]
    fn counts_implicit_directories_and_rejects_growth_before_any_creation() {
        let mut registered = BTreeMap::new();
        register_path(Path::new("Morpheus.app"), true, &mut registered).unwrap();
        for index in 0..(MAX_ENTRIES - 1) / 2 {
            register_path(
                &PathBuf::from(format!("Morpheus.app/d{index}/file")),
                false,
                &mut registered,
            )
            .unwrap();
        }
        register_path(Path::new("Morpheus.app/final"), false, &mut registered).unwrap();
        assert_eq!(registered.len(), MAX_ENTRIES);
        assert!(
            register_path(Path::new("Morpheus.app/extra/leaf"), false, &mut registered).is_err()
        );
        assert_eq!(registered.len(), MAX_ENTRIES);
        assert!(!registered.contains_key("morpheus.app/extra"));
    }

    #[test]
    fn case_aliases_and_file_or_link_parent_prefixes_are_rejected() {
        let mut registered = BTreeMap::new();
        register_path(
            Path::new("Morpheus.app/Contents/Foo/a"),
            false,
            &mut registered,
        )
        .unwrap();
        let original = registered.clone();
        assert!(register_path(
            Path::new("Morpheus.app/Contents/foo/b"),
            false,
            &mut registered
        )
        .is_err());
        assert_eq!(registered, original);
        assert!(register_path(
            Path::new("Morpheus.app/Contents/Foo/a/child"),
            false,
            &mut registered
        )
        .is_err());
        assert_eq!(registered, original);
        // The same concrete prefix spelling may be shared by ordinary files.
        register_path(
            Path::new("Morpheus.app/Contents/Foo/b"),
            false,
            &mut registered,
        )
        .unwrap();
        assert_eq!(registered.len(), original.len() + 1);
    }
}
