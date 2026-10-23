//! Owned, same-filesystem macOS bundle transaction. Installation remains gated
//! until installed-package acceptance; the upstream installer is never called.
//! No account/config paths are accepted. Durable health is committed before any
//! store opens, after which recovery must never run older persistence code.

use super::{InstallLease, MAX_UPDATE_BYTES, UpdateError};
use cargo_packager_updater::semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

// Deliberate release gate, not a user-editable setting or environment override.
// Enabling requires reviewed installed upgrade/rollback/signing acceptance.
pub(super) const INSTALLATION_VALIDATED: bool = false;
const JOURNAL_LIMIT: u64 = 16 * 1024;
const MAX_BUNDLE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 32_768;
const EXECUTABLE: &str = "Contents/MacOS/morpheus-app";

#[used]
#[unsafe(link_section = "__TEXT,__morpheus_upd")]
static UPDATE_IDENTITY: [u8; include_bytes!(concat!(
    env!("OUT_DIR"),
    "/morpheus-update-identity.json"
))
.len()] = *include_bytes!(concat!(env!("OUT_DIR"), "/morpheus-update-identity.json"));

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PackageMetadata {
    protocol: u8,
    product: String,
    version: String,
    arch: String,
    persistence: String,
}

impl PackageMetadata {
    fn embedded() -> Result<Self, UpdateError> {
        // Refer to the actual section at runtime, retaining it through native
        // linker dead stripping. Packaging reads the section without executing.
        Self::parse(std::hint::black_box(&UPDATE_IDENTITY))
    }

    fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        if bytes.is_empty() || bytes.len() > 4096 {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let metadata: Self =
            serde_json::from_slice(bytes).map_err(|_| UpdateError::IncompatibleUpdate)?;
        if metadata.protocol != 1
            || metadata.product != "io.github.brk275.morpheus"
            || !matches!(metadata.arch.as_str(), "x86_64" | "aarch64")
            || metadata.version.is_empty()
            || metadata.version.len() > 128
            || Version::parse(&metadata.version).is_err()
            || metadata.persistence.len() != 64
            || !metadata.persistence.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(UpdateError::IncompatibleUpdate);
        }
        Ok(metadata)
    }

    fn compatible(&self, installed: &Self) -> bool {
        self.protocol == installed.protocol
            && self.product == installed.product
            && self.arch == installed.arch
            && self.persistence == installed.persistence
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, UpdateError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(UpdateError::IncompatibleUpdate)?
            .try_into()
            .unwrap(),
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, UpdateError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(UpdateError::IncompatibleUpdate)?
            .try_into()
            .unwrap(),
    ))
}

/// Parse only a thin, native 64-bit MH_EXECUTE and its single immutable section.
/// Fat/cross-architecture/legacy bundles need separate acceptance; never launch
/// an uninstalled executable to discover its version or persistence cohort.
fn executable_metadata(bytes: &[u8]) -> Result<PackageMetadata, UpdateError> {
    if bytes.len() < 32 || u32_at(bytes, 0)? != 0xfeedfacf || u32_at(bytes, 12)? != 2 {
        return Err(UpdateError::IncompatibleUpdate);
    }
    let cpu = match std::env::consts::ARCH {
        "x86_64" => 0x0100_0007,
        "aarch64" => 0x0100_000c,
        _ => return Err(UpdateError::IncompatibleUpdate),
    };
    if u32_at(bytes, 4)? != cpu {
        return Err(UpdateError::IncompatibleUpdate);
    }
    let count = u32_at(bytes, 16)? as usize;
    let commands_len = u32_at(bytes, 20)? as usize;
    if count == 0 || count > 4096 || commands_len > 1024 * 1024 || commands_len > bytes.len() - 32 {
        return Err(UpdateError::IncompatibleUpdate);
    }
    let end = 32 + commands_len;
    let mut position = 32usize;
    let mut identity = None;
    for _ in 0..count {
        let command = u32_at(bytes, position)?;
        let size = u32_at(bytes, position + 4)? as usize;
        if size < 8 || size % 8 != 0 || size > end.saturating_sub(position) {
            return Err(UpdateError::IncompatibleUpdate);
        }
        if command == 0x19 {
            // LC_SEGMENT_64
            if size < 72 {
                return Err(UpdateError::IncompatibleUpdate);
            }
            let sections = u32_at(bytes, position + 64)? as usize;
            if sections > 4096 || 72 + sections * 80 != size {
                return Err(UpdateError::IncompatibleUpdate);
            }
            for index in 0..sections {
                let section = position + 72 + index * 80;
                if &bytes[section..section + 16] == b"__morpheus_upd\0\0" {
                    if identity.is_some()
                        || &bytes[section + 16..section + 32] != b"__TEXT\0\0\0\0\0\0\0\0\0\0"
                        || &bytes[position + 8..position + 24] != b"__TEXT\0\0\0\0\0\0\0\0\0\0"
                        || u32_at(bytes, position + 60)? & 2 != 0
                        || u32_at(bytes, section + 64)? & 0xff != 0
                    {
                        return Err(UpdateError::IncompatibleUpdate);
                    }
                    let len = u64_at(bytes, section + 40)?;
                    let offset = u32_at(bytes, section + 48)? as u64;
                    let segment_offset = u64_at(bytes, position + 40)?;
                    let segment_size = u64_at(bytes, position + 48)?;
                    let stop = offset
                        .checked_add(len)
                        .ok_or(UpdateError::IncompatibleUpdate)?;
                    let segment_end = segment_offset
                        .checked_add(segment_size)
                        .ok_or(UpdateError::IncompatibleUpdate)?;
                    if len == 0
                        || len > 4096
                        || offset < end as u64
                        || stop > bytes.len() as u64
                        || offset < segment_offset
                        || stop > segment_end
                    {
                        return Err(UpdateError::IncompatibleUpdate);
                    }
                    identity = Some(PackageMetadata::parse(
                        &bytes[offset as usize..stop as usize],
                    )?);
                }
            }
        }
        position += size;
    }
    if position != end {
        return Err(UpdateError::IncompatibleUpdate);
    }
    let metadata = identity.ok_or(UpdateError::IncompatibleUpdate)?;
    if metadata.arch != std::env::consts::ARCH {
        return Err(UpdateError::IncompatibleUpdate);
    }
    Ok(metadata)
}

fn owner() -> u32 {
    // SAFETY: geteuid has no pointer arguments or preconditions.
    unsafe { libc::geteuid() }
}

fn directory(path: &Path) -> Result<fs::Metadata, UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| UpdateError::RecoveryRequired)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != owner()
        || metadata.mode() & 0o7022 != 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(metadata)
}

fn open_file(path: &Path) -> Result<File, UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| UpdateError::RecoveryRequired)?;
    let metadata = file.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
    if !metadata.is_file()
        || metadata.uid() != owner()
        || metadata.mode() & 0o7022 != 0
        || metadata.nlink() != 1
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(file)
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let file = open_file(path)?;
    if file
        .metadata()
        .map_err(|_| UpdateError::RecoveryRequired)?
        .len()
        > limit
    {
        return Err(UpdateError::RecoveryRequired);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UpdateError::RecoveryRequired)?;
    if bytes.len() as u64 > limit {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(bytes)
}

fn sync_directory(path: &Path) -> Result<(), UpdateError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| UpdateError::RecoveryRequired)
}

/// Apple SDK fcntl.h defines F_FULLFSYNC as fsync plus a drive-to-media
/// flush request. This is intentionally checked on owned regular descriptors;
/// unsupported filesystems/descriptor modes fail closed, with no barrier-only
/// fallback. Actual hardware/power-loss acceptance is a separate release gate.
fn full_sync_regular(file: &File) -> Result<(), UpdateError> {
    let metadata = file.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
    if !metadata.is_file()
        || metadata.uid() != owner()
        || metadata.nlink() != 1
        || metadata.mode() & 0o7022 != 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    file.sync_all().map_err(|_| UpdateError::RecoveryRequired)?;
    for _ in 0..8 {
        // SAFETY: as_raw_fd remains owned and live throughout fcntl. The
        // F_FULLFSYNC command takes no variadic pointer/integer argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(UpdateError::RecoveryRequired);
        }
    }
    Err(UpdateError::RecoveryRequired)
}

fn private_directory(path: &Path) -> Result<(), UpdateError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_directory(path.parent().ok_or(UpdateError::RecoveryRequired)?)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(UpdateError::NotWritable),
    }
    let metadata = directory(path)?;
    if metadata.mode() & 0o077 != 0 {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

fn valid_name(path: &Path) -> bool {
    path.components().all(|part| match part {
        Component::Normal(name) => {
            name.as_bytes().len() <= 255
                && !name.as_bytes().is_empty()
                && name
                    .as_bytes()
                    .iter()
                    .all(|b| b.is_ascii() && *b >= 0x20 && !b"\\:\x7f".contains(b))
        }
        _ => false,
    })
}

/// A bounded sorted manifest of the complete app tree, including all resources,
/// file modes and internal symbolic-link text. No links are traversed to walk.
fn entries(root: &Path) -> Result<Vec<PathBuf>, UpdateError> {
    walk_entries(root, true)
}

fn walk_entries(root: &Path, check_links: bool) -> Result<Vec<PathBuf>, UpdateError> {
    directory(root)?;
    fn visit(
        root: &Path,
        relative: &Path,
        out: &mut Vec<PathBuf>,
        bytes: &mut u64,
        depth: usize,
        check_links: bool,
    ) -> Result<(), UpdateError> {
        if depth > 32 {
            return Err(UpdateError::RecoveryRequired);
        }
        let mut children = Vec::new();
        for entry in fs::read_dir(root.join(relative)).map_err(|_| UpdateError::RecoveryRequired)? {
            let path = relative.join(
                entry
                    .map_err(|_| UpdateError::RecoveryRequired)?
                    .file_name(),
            );
            if !valid_name(&path) || path.as_os_str().len() > 1024 {
                return Err(UpdateError::RecoveryRequired);
            }
            children.push(path);
            if children.len() + out.len() > MAX_ENTRIES {
                return Err(UpdateError::RecoveryRequired);
            }
        }
        children.sort();
        let mut aliases = std::collections::BTreeSet::new();
        for path in children {
            if !aliases.insert(path.file_name().unwrap().to_ascii_lowercase()) {
                return Err(UpdateError::RecoveryRequired);
            }
            let actual = root.join(&path);
            let metadata =
                fs::symlink_metadata(&actual).map_err(|_| UpdateError::RecoveryRequired)?;
            if metadata.uid() != owner()
                || if metadata.file_type().is_symlink() {
                    metadata.mode() & 0o7000 != 0
                        || metadata.nlink() != 1
                        || (check_links && metadata.mode() & 0o400 == 0)
                } else {
                    metadata.mode() & 0o7022 != 0
                }
            {
                return Err(UpdateError::RecoveryRequired);
            }
            out.push(path.clone());
            if out.len() > MAX_ENTRIES {
                return Err(UpdateError::RecoveryRequired);
            }
            if metadata.is_dir() {
                visit(root, &path, out, bytes, depth + 1, check_links)?;
            } else if metadata.is_file() {
                if metadata.nlink() != 1 {
                    return Err(UpdateError::RecoveryRequired);
                }
                *bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or(UpdateError::RecoveryRequired)?;
                if *bytes > MAX_BUNDLE_BYTES {
                    return Err(UpdateError::RecoveryRequired);
                }
            } else if metadata.file_type().is_symlink() {
                if check_links {
                    let target =
                        fs::read_link(&actual).map_err(|_| UpdateError::RecoveryRequired)?;
                    if target.as_os_str().len() > 1024
                        || target.is_absolute()
                        || !actual
                            .canonicalize()
                            .map_err(|_| UpdateError::RecoveryRequired)?
                            .starts_with(root)
                    {
                        return Err(UpdateError::RecoveryRequired);
                    }
                } else if metadata.len() > 1024 {
                    // Failed-stage cleanup only unlinks the known owned entry.
                    // lstat bounds its text length without reading/following a
                    // target that an unreadable link mode deliberately hides.
                    return Err(UpdateError::RecoveryRequired);
                }
            } else {
                return Err(UpdateError::RecoveryRequired);
            }
        }
        Ok(())
    }
    let canonical = root
        .canonicalize()
        .map_err(|_| UpdateError::RecoveryRequired)?;
    let mut out = Vec::new();
    let mut bytes = 0;
    visit(
        &canonical,
        Path::new(""),
        &mut out,
        &mut bytes,
        0,
        check_links,
    )?;
    out.sort();
    Ok(out)
}

fn tree_hash(root: &Path) -> Result<String, UpdateError> {
    let paths = entries(root)?;
    let mut hash = Sha256::new();
    hash.update(b"morpheus-macos-bundle-tree-v1\0");
    hash.update((directory(root)?.mode() & 0o777).to_le_bytes());
    let mut total = 0u64;
    for path in paths {
        let actual = root.join(&path);
        let metadata = fs::symlink_metadata(&actual).map_err(|_| UpdateError::RecoveryRequired)?;
        let name = path.as_os_str().as_bytes();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update((metadata.mode() & 0o777).to_le_bytes());
        if metadata.is_dir() {
            hash.update(b"d");
        } else if metadata.file_type().is_symlink() {
            hash.update(b"l");
            let target = fs::read_link(&actual).map_err(|_| UpdateError::RecoveryRequired)?;
            hash.update((target.as_os_str().len() as u64).to_le_bytes());
            hash.update(target.as_os_str().as_bytes());
        } else {
            hash.update(b"f");
            let file = open_file(&actual)?;
            let len = file
                .metadata()
                .map_err(|_| UpdateError::RecoveryRequired)?
                .len();
            hash.update(len.to_le_bytes());
            let mut source = file.take(MAX_UPDATE_BYTES + 1);
            let mut read = 0u64;
            let mut buffer = [0; 64 * 1024];
            loop {
                let n = source
                    .read(&mut buffer)
                    .map_err(|_| UpdateError::RecoveryRequired)?;
                if n == 0 {
                    break;
                }
                read += n as u64;
                total += n as u64;
                if read > MAX_UPDATE_BYTES || total > MAX_BUNDLE_BYTES {
                    return Err(UpdateError::RecoveryRequired);
                }
                hash.update(&buffer[..n]);
            }
            if read != len {
                return Err(UpdateError::RecoveryRequired);
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn copy_bundle(source: &Path, destination: &Path) -> Result<(), UpdateError> {
    let paths = entries(source)?;
    let root_mode = directory(source)?.permissions();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .map_err(|_| UpdateError::StageFailed)?;
    for path in paths {
        let input = source.join(&path);
        let output = destination.join(&path);
        let metadata = fs::symlink_metadata(&input).map_err(|_| UpdateError::StageFailed)?;
        if metadata.is_dir() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&output)
                .map_err(|_| UpdateError::StageFailed)?;
        } else if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(
                fs::read_link(input).map_err(|_| UpdateError::StageFailed)?,
                &output,
            )
            .map_err(|_| UpdateError::StageFailed)?;
            cargo_packager_updater::macos_archive::restore_owned_symlink_mode(
                &output,
                metadata.mode() & 0o777,
            )
            .map_err(|_| UpdateError::StageFailed)?;
        } else {
            let mut input = open_file(&input)?.take(MAX_UPDATE_BYTES + 1);
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(metadata.mode() & 0o777)
                .custom_flags(libc::O_NOFOLLOW)
                .open(output)
                .map_err(|_| UpdateError::StageFailed)?;
            let size =
                std::io::copy(&mut input, &mut output).map_err(|_| UpdateError::StageFailed)?;
            if size != metadata.len() || size > MAX_UPDATE_BYTES {
                return Err(UpdateError::StageFailed);
            }
            restore_payload_mode(&output, metadata.permissions())?;
            output.sync_all().map_err(|_| UpdateError::StageFailed)?;
        }
    }
    // Fill private writable directories first; then restore the exact signed
    // tree modes, including readable-but-not-writable resource directories.
    for path in entries(source)?.into_iter().rev() {
        let metadata =
            fs::symlink_metadata(source.join(&path)).map_err(|_| UpdateError::StageFailed)?;
        if metadata.is_dir() {
            fs::set_permissions(destination.join(path), metadata.permissions())
                .map_err(|_| UpdateError::StageFailed)?;
        }
    }
    fs::set_permissions(destination, root_mode).map_err(|_| UpdateError::StageFailed)?;
    sync_tree(destination)
}

fn restore_payload_mode(file: &File, permissions: fs::Permissions) -> Result<(), UpdateError> {
    // OpenOptions creation modes are filtered by umask. Restore the exact
    // already validated source mode through the owned descriptor before sync.
    if permissions.mode() & 0o7022 != 0 {
        return Err(UpdateError::StageFailed);
    }
    file.set_permissions(permissions)
        .map_err(|_| UpdateError::StageFailed)
}

fn sync_tree(root: &Path) -> Result<(), UpdateError> {
    for path in entries(root)?.into_iter().rev() {
        if fs::symlink_metadata(root.join(&path))
            .map_err(|_| UpdateError::RecoveryRequired)?
            .is_dir()
        {
            sync_directory(&root.join(path))?;
        }
    }
    sync_directory(root)
}

fn full_sync_tree(root: &Path) -> Result<(), UpdateError> {
    // File bytes/modes and the complete tree's directory entries were synced
    // first. A checked regular-file media flush follows that ordering; never
    // send F_FULLFSYNC to directory descriptors or silently downgrade it.
    sync_tree(root)?;
    for path in entries(root)? {
        let path = root.join(path);
        if fs::symlink_metadata(&path)
            .map_err(|_| UpdateError::RecoveryRequired)?
            .is_file()
        {
            full_sync_regular(&open_file(&path)?)?;
        }
    }
    Ok(())
}

fn remove_bundle(root: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(UpdateError::RecoveryRequired),
        Ok(_) => {}
    }
    let paths = walk_entries(root, false)?;
    // Cleanup may remove a rejected dangling/external link, but never follows
    // it. Only known owned directories are made writable before unlinking.
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .map_err(|_| UpdateError::RecoveryRequired)?;
    for path in &paths {
        let actual = root.join(path);
        if fs::symlink_metadata(&actual)
            .map_err(|_| UpdateError::RecoveryRequired)?
            .is_dir()
        {
            fs::set_permissions(actual, fs::Permissions::from_mode(0o700))
                .map_err(|_| UpdateError::RecoveryRequired)?;
        }
    }
    for path in paths.into_iter().rev() {
        let actual = root.join(path);
        let metadata = fs::symlink_metadata(&actual).map_err(|_| UpdateError::RecoveryRequired)?;
        if metadata.is_dir() {
            fs::remove_dir(actual)
        } else {
            fs::remove_file(actual)
        }
        .map_err(|_| UpdateError::RecoveryRequired)?;
    }
    fs::remove_dir(root).map_err(|_| UpdateError::RecoveryRequired)
}

fn bundle_metadata(root: &Path) -> Result<PackageMetadata, UpdateError> {
    directory(root)?;
    directory(&root.join("Contents"))?;
    directory(&root.join("Contents/MacOS"))?;
    let executable = root.join(EXECUTABLE);
    if open_file(&executable)?
        .metadata()
        .map_err(|_| UpdateError::RecoveryRequired)?
        .mode()
        & 0o100
        == 0
    {
        return Err(UpdateError::IncompatibleUpdate);
    }
    let metadata = executable_metadata(&bounded_read(&executable, MAX_UPDATE_BYTES)?)?;
    let plist = String::from_utf8(bounded_read(&root.join("Contents/Info.plist"), 64 * 1024)?)
        .map_err(|_| UpdateError::IncompatibleUpdate)?;
    validate_plist(&plist, &metadata)?;
    Ok(metadata)
}

fn validate_plist(plist: &str, metadata: &PackageMetadata) -> Result<(), UpdateError> {
    // The current Cargo Packager producer emits one root dict of strings and
    // booleans. Parse that closed XML subset rather than finding key substrings.
    // Nested dictionaries/arrays, custom entities/DTDs, CDATA and multiple roots
    // require separately reviewed format support, never a permissive fallback.
    struct Xml<'a> {
        rest: &'a str,
    }
    impl Xml<'_> {
        fn space(&mut self) {
            self.rest = self.rest.trim_start_matches([' ', '\t', '\r', '\n']);
        }
        fn consume(&mut self, token: &str) -> bool {
            self.space();
            if let Some(rest) = self.rest.strip_prefix(token) {
                self.rest = rest;
                true
            } else {
                false
            }
        }
        fn require(&mut self, token: &str) -> Result<(), UpdateError> {
            self.consume(token)
                .then_some(())
                .ok_or(UpdateError::IncompatibleUpdate)
        }
        fn text(&mut self, tag: &str) -> Result<String, UpdateError> {
            self.require(&format!("<{tag}>"))?;
            let end = self.rest.find('<').ok_or(UpdateError::IncompatibleUpdate)?;
            if end > 16 * 1024 {
                return Err(UpdateError::IncompatibleUpdate);
            }
            let (encoded, rest) = self.rest.split_at(end);
            self.rest = rest;
            self.require(&format!("</{tag}>"))?;
            let mut decoded = String::new();
            let mut input = encoded;
            while let Some(index) = input.find('&') {
                decoded.push_str(&input[..index]);
                input = &input[index..];
                let (entity, value) = [
                    ("&amp;", '&'),
                    ("&lt;", '<'),
                    ("&gt;", '>'),
                    ("&quot;", '"'),
                    ("&apos;", '\''),
                ]
                .into_iter()
                .find(|(entity, _)| input.starts_with(entity))
                .ok_or(UpdateError::IncompatibleUpdate)?;
                decoded.push(value);
                input = &input[entity.len()..];
            }
            decoded.push_str(input);
            if decoded
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\r' | '\n'))
            {
                return Err(UpdateError::IncompatibleUpdate);
            }
            Ok(decoded)
        }
    }
    if plist.len() > 64 * 1024 {
        return Err(UpdateError::IncompatibleUpdate);
    }
    let mut xml = Xml { rest: plist };
    xml.consume("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    xml.consume("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">");
    xml.require("<plist version=\"1.0\">")?;
    xml.require("<dict>")?;
    let mut values = std::collections::BTreeMap::new();
    while !xml.consume("</dict>") {
        if values.len() >= 128 {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let key = xml.text("key")?;
        if key.is_empty() || key.len() > 128 {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let value = if xml.consume("<true/>") || xml.consume("<false/>") {
            None
        } else {
            Some(xml.text("string")?)
        };
        if values.insert(key, value).is_some() {
            return Err(UpdateError::IncompatibleUpdate);
        }
    }
    xml.require("</plist>")?;
    xml.space();
    if !xml.rest.is_empty() {
        return Err(UpdateError::IncompatibleUpdate);
    }
    for (key, value) in [
        ("CFBundleIdentifier", metadata.product.as_str()),
        ("CFBundleExecutable", "morpheus-app"),
        ("CFBundleShortVersionString", metadata.version.as_str()),
        ("CFBundlePackageType", "APPL"),
    ] {
        if values.get(key).and_then(Option::as_deref) != Some(value) {
            return Err(UpdateError::IncompatibleUpdate);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Phase {
    Prepared,
    ApplyIntent,
    AwaitingHealth,
    Committed,
    RollbackIntent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    protocol: u8,
    id: String,
    phase: Phase,
    old_hash: String,
    new_hash: String,
    candidate: PackageMetadata,
}

/// Retained from startup through UI, call, account and runtime shutdown.
pub struct MacAppSession {
    installed: PathBuf,
    state: PathBuf,
    _lock: File,
    trial: Option<String>,
    install_lease: Option<InstallLease>,
}

impl MacAppSession {
    fn paths(installed: &Path) -> Result<(PathBuf, PathBuf), UpdateError> {
        if !installed.is_absolute()
            || installed
                .file_name()
                .is_none_or(|name| name != "Morpheus.app")
        {
            return Err(UpdateError::UnsupportedPackage);
        }
        let parent = installed
            .parent()
            .ok_or(UpdateError::UnsupportedPackage)?
            .canonicalize()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        directory(&parent).map_err(|_| UpdateError::NotWritable)?;
        let installed = parent.join("Morpheus.app");
        directory(&installed)?;
        let state = parent.join(".Morpheus.app.morpheus-update");
        private_directory(&state)?;
        if directory(&installed)?.dev() != directory(&state)?.dev() {
            return Err(UpdateError::NotWritable);
        }
        Ok((installed, state))
    }

    fn lock(installed: &Path, wait: Duration) -> Result<Self, UpdateError> {
        let (installed, state) = Self::paths(installed)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(state.join("lock"))
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let metadata = lock.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
        if !metadata.is_file()
            || metadata.uid() != owner()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let deadline = Instant::now() + wait;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(UpdateError::UpdateActive),
                Err(_) => return Err(UpdateError::RecoveryRequired),
            }
        }
        Ok(Self {
            installed,
            state,
            _lock: lock,
            trial: None,
            install_lease: None,
        })
    }

    fn journal(&self) -> Result<Option<Journal>, UpdateError> {
        Self::read_journal(&self.state)
    }

    fn read_journal(state: &Path) -> Result<Option<Journal>, UpdateError> {
        let path = state.join("journal.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(UpdateError::RecoveryRequired),
            Ok(_) => {}
        }
        let journal: Journal = serde_json::from_slice(&bounded_read(&path, JOURNAL_LIMIT)?)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let valid_hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        if journal.protocol != 1
            || journal.id.is_empty()
            || journal.id.len() > 64
            || !journal.id.bytes().all(|b| b.is_ascii_alphanumeric())
            || !valid_hash(&journal.old_hash)
            || !valid_hash(&journal.new_hash)
            || PackageMetadata::parse(
                &serde_json::to_vec(&journal.candidate)
                    .map_err(|_| UpdateError::RecoveryRequired)?,
            )
            .is_err()
        {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(Some(journal))
    }

    fn save(&self, journal: &Journal) -> Result<(), UpdateError> {
        let bytes = serde_json::to_vec(journal).map_err(|_| UpdateError::RecoveryRequired)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.state)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        temp.write_all(&bytes)
            .and_then(|_| temp.as_file().sync_all())
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let path = self.state.join("journal.json");
        // Retain persist's actual written descriptor. Reopening a pathname
        // could flush a different inode after an unexpected replacement.
        let retained = temp
            .persist(&path)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let file = retained
            .metadata()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let published = fs::symlink_metadata(&path).map_err(|_| UpdateError::RecoveryRequired)?;
        if !published.is_file()
            || published.file_type().is_symlink()
            || file.dev() != published.dev()
            || file.ino() != published.ino()
        {
            return Err(UpdateError::RecoveryRequired);
        }
        sync_directory(&self.state)?;
        // This final checked flush is after publication + directory fsync.
        // Callers cannot apply or open stores on a merely renamed journal.
        full_sync_regular(&retained)
    }

    fn clean(&self) -> Result<(), UpdateError> {
        // Fixed, owned names only. The bounded walker never follows symlinks;
        // no caller-supplied tree or account directory is recursively removed.
        for name in ["next.app", "previous.app", "extract"] {
            remove_bundle(&self.state.join(name))?;
        }
        for name in ["supervisor-ready", "journal.json"] {
            match fs::remove_file(self.state.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(UpdateError::RecoveryRequired),
            }
        }
        sync_directory(&self.state)
    }

    fn prepare(&mut self, bytes: &[u8], version: &str) -> Result<(), UpdateError> {
        if self.journal()?.is_some() {
            return Err(UpdateError::UpdateActive);
        }
        let embedded = PackageMetadata::embedded()?;
        if bundle_metadata(&self.installed)? != embedded {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let next = Version::parse(version).map_err(|_| UpdateError::NotNewer)?;
        if next.cmp_precedence(&Version::parse(&embedded.version).unwrap())
            != std::cmp::Ordering::Greater
        {
            return Err(UpdateError::NotNewer);
        }
        self.clean()?;
        let extract = self.state.join("extract");
        private_directory(&extract)?;
        let unpacked =
            cargo_packager_updater::macos_archive::unpack_verified_bundle(bytes, &extract)
                .map_err(|_| UpdateError::CorruptStage)?;
        let candidate = bundle_metadata(&unpacked)?;
        if !candidate.compatible(&embedded) || candidate.version != version {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let new_hash = tree_hash(&unpacked)?;
        let old_hash = tree_hash(&self.installed)?;
        copy_bundle(&self.installed, &self.state.join("previous.app"))?;
        if tree_hash(&self.state.join("previous.app"))? != old_hash {
            return Err(UpdateError::RecoveryRequired);
        }
        fs::rename(unpacked, self.state.join("next.app")).map_err(|_| UpdateError::StageFailed)?;
        sync_tree(&self.state.join("next.app"))?;
        sync_directory(&extract)?;
        sync_directory(&self.state)?;
        fs::remove_dir(extract).map_err(|_| UpdateError::StageFailed)?;
        sync_directory(&self.state)?;
        // Complete both payload trees and enclosing entry ordering before
        // publishing the durable Prepared journal. Any unsupported flush
        // leaves the installed application untouched and preparation fails.
        full_sync_tree(&self.state.join("previous.app"))?;
        full_sync_tree(&self.state.join("next.app"))?;
        let random = tempfile::Builder::new()
            .prefix("")
            .tempfile_in(&self.state)
            .map_err(|_| UpdateError::StageFailed)?;
        let id = random
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        drop(random);
        self.save(&Journal {
            protocol: 1,
            id,
            phase: Phase::Prepared,
            old_hash,
            new_hash,
            candidate,
        })
    }

    fn apply(&self, journal: &mut Journal) -> Result<(), UpdateError> {
        if journal.phase != Phase::Prepared
            || tree_hash(&self.installed)? != journal.old_hash
            || tree_hash(&self.state.join("previous.app"))? != journal.old_hash
            || tree_hash(&self.state.join("next.app"))? != journal.new_hash
        {
            return Err(UpdateError::RecoveryRequired);
        }
        journal.phase = Phase::ApplyIntent;
        self.save(journal)?;
        swap_bundles(&self.installed, &self.state.join("next.app"))?;
        sync_directory(self.installed.parent().unwrap())?;
        sync_directory(&self.state)?;
        journal.phase = Phase::AwaitingHealth;
        self.save(journal)
    }

    fn rollback(&self, journal: &mut Journal) -> Result<(), UpdateError> {
        if journal.phase == Phase::Committed {
            return Err(UpdateError::RecoveryRequired);
        }
        let actual = tree_hash(&self.installed)?;
        if actual == journal.old_hash {
            self.sync_restored()?;
            return self.clean();
        }
        if actual != journal.new_hash
            || tree_hash(&self.state.join("previous.app"))? != journal.old_hash
        {
            return Err(UpdateError::RecoveryRequired);
        }
        journal.phase = Phase::RollbackIntent;
        self.save(journal)?;
        // APFS/HFS+ directory exchange is atomic: public .app never disappears.
        // No copy/remove replacement fallback is allowed on other filesystems.
        swap_bundles(&self.installed, &self.state.join("previous.app"))?;
        self.sync_restored()?;
        self.clean()
    }

    fn sync_restored(&self) -> Result<(), UpdateError> {
        sync_directory(self.installed.parent().unwrap())?;
        sync_directory(&self.state)?;
        full_sync_tree(&self.installed)
    }

    fn confirm_commit(&self) -> Result<(), UpdateError> {
        // A previous commit may have become visible just before a failed
        // media flush. Never roll it back; retry the checked barrier before
        // cleanup or opening stores through this committed installation.
        sync_directory(self.installed.parent().unwrap())?;
        sync_directory(&self.state)?;
        full_sync_regular(&open_file(&self.state.join("journal.json"))?)
    }

    pub fn acknowledge_before_store(&mut self) -> Result<(), UpdateError> {
        let Some(id) = self.trial.take() else {
            return Ok(());
        };
        let mut journal = self.journal()?.ok_or(UpdateError::RecoveryRequired)?;
        if journal.phase != Phase::AwaitingHealth
            || journal.id != id
            || journal.candidate != PackageMetadata::embedded()?
            || bundle_metadata(&self.installed)? != journal.candidate
            || tree_hash(&self.installed)? != journal.new_hash
        {
            return Err(UpdateError::RecoveryRequired);
        }
        journal.phase = Phase::Committed;
        self.save(&journal)
    }
}

fn swap_bundles(left: &Path, right: &Path) -> Result<(), UpdateError> {
    directory(left)?;
    directory(right)?;
    let left =
        CString::new(left.as_os_str().as_bytes()).map_err(|_| UpdateError::RecoveryRequired)?;
    let right =
        CString::new(right.as_os_str().as_bytes()).map_err(|_| UpdateError::RecoveryRequired)?;
    // SAFETY: both live C strings are NUL-terminated absolute owned paths.
    // RENAME_SWAP is one atomic same-filesystem exchange, not two renames.
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    } != 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

#[must_use = "Hand off explicitly; dropping cancels preparation without replacing the app."]
pub struct PreparedMacRestart<'a> {
    session: &'a mut MacAppSession,
    id: String,
    lease: Option<InstallLease>,
    handed_off: bool,
}

impl PreparedMacRestart<'_> {
    pub fn launch_supervisor(mut self) -> Result<(), UpdateError> {
        let mut child = OwnedChild::spawn(
            app_command(&self.session.state.join("previous.app"))
                .arg("--morpheus-mac-update-supervise")
                .arg(&self.session.installed)
                .arg(&self.id),
        )?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child.exited()? {
                child.stop()?;
                return Err(UpdateError::RestartFailed);
            }
            if bounded_read(&self.session.state.join("supervisor-ready"), 64)
                .is_ok_and(|bytes| bytes == self.id.as_bytes())
            {
                self.session.install_lease = self.lease.take();
                self.handed_off = true;
                drop(child.handoff());
                return Ok(());
            }
            if Instant::now() >= deadline {
                child.stop()?;
                return Err(UpdateError::RestartFailed);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn cancel_inner(&mut self) -> Result<(), UpdateError> {
        if let Some(journal) = self.session.journal()? {
            if journal.id != self.id || journal.phase != Phase::Prepared {
                return Err(UpdateError::RecoveryRequired);
            }
            self.session.clean()?;
        }
        self.lease.take();
        Ok(())
    }
}

impl Drop for PreparedMacRestart<'_> {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = self.cancel_inner();
        }
    }
}

pub(super) fn prepare_restart<'a>(
    session: &'a mut MacAppSession,
    bytes: &[u8],
    version: &str,
    lease: InstallLease,
) -> Result<PreparedMacRestart<'a>, UpdateError> {
    if !INSTALLATION_VALIDATED {
        return Err(UpdateError::InstallNotValidated);
    }
    prepare_restart_admitted(session, bytes, version, lease)
}

// The shipping caller above always applies the closed activation gate. The
// only other caller is the compile-gated, strictly synthetic acceptance actor;
// archive, identity, tree, durability, nonce and lease checks stay shared here.
fn prepare_restart_admitted<'a>(
    session: &'a mut MacAppSession,
    bytes: &[u8],
    version: &str,
    lease: InstallLease,
) -> Result<PreparedMacRestart<'a>, UpdateError> {
    session.prepare(bytes, version)?;
    let id = session.journal()?.ok_or(UpdateError::RecoveryRequired)?.id;
    Ok(PreparedMacRestart {
        session,
        id,
        lease: Some(lease),
        handed_off: false,
    })
}

fn app_command(bundle: &Path) -> Command {
    let mut command = Command::new(bundle.join(EXECUTABLE));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // Direct native executable, no `open`, shell, privilege prompt or installer.
    // Do not carry injection/old-loader configuration into helper or trial.
    remove_loader_environment(&mut command, std::env::vars_os().map(|(name, _)| name));
    command
}

fn remove_loader_environment(
    command: &mut Command,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) {
    for name in names {
        // Loader variables are an open-ended namespace, including versioned
        // paths and image suffixes. Match raw names, without a UTF-8 bypass.
        if name.as_os_str().as_bytes().starts_with(b"DYLD_") {
            command.env_remove(name);
        }
    }
}

// An unreaped leader reserves its PID/PGID, even after exit. Observe with
// WNOWAIT, attempt the anchored group signal, then perform the only reap.
// Neither observing that leader nor a successful group signal proves that
// every descendant has stopped. In particular, Darwin can return EPERM for
// a zombie-only group and for a group containing live nonsignalable members.
#[derive(Debug)]
enum TrialStopError {
    Failed(UpdateError),
    ExitedLeaderGroupUnproven(std::process::ExitStatus),
}

struct OwnedChild {
    child: Option<Child>,
}
impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self, UpdateError> {
        Ok(Self {
            child: Some(
                command
                    .process_group(0)
                    .spawn()
                    .map_err(|_| UpdateError::RestartFailed)?,
            ),
        })
    }
    fn exited(&self) -> Result<bool, UpdateError> {
        let pid = i32::try_from(
            self.child
                .as_ref()
                .ok_or(UpdateError::RecoveryRequired)?
                .id(),
        )
        .map_err(|_| UpdateError::RecoveryRequired)?;
        loop {
            // SAFETY: waitid receives a valid aligned writable siginfo_t and
            // only this owned unreaped child PID. WNOWAIT preserves ownership.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } == 0
            {
                return match unsafe { info.si_pid() } {
                    0 => Ok(false),
                    actual if actual == pid => Ok(true),
                    _ => Err(UpdateError::RecoveryRequired),
                };
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(UpdateError::RecoveryRequired);
            }
        }
    }
    fn stop(&mut self) -> Result<(), UpdateError> {
        if self.child.is_none() {
            return Ok(());
        }
        self.exited()?;
        self.signal_group()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        self.reap_owned()?;
        Ok(())
    }

    fn stop_before_health(&mut self) -> Result<(), TrialStopError> {
        let exited = self.exited().map_err(TrialStopError::Failed)?;
        match self.signal_group() {
            Ok(()) => self
                .reap_owned()
                .map(|_| ())
                .map_err(TrialStopError::Failed),
            Err(error) if exited && error.raw_os_error() == Some(libc::EPERM) => {
                // This is only an observed direct-child exit. Reap exactly
                // that owned child, never signal its numeric group again,
                // and keep group cleanup an explicit error. The supervisor
                // may attempt serialized pre-store rollback, not claim drain.
                let status = self.reap_owned().map_err(TrialStopError::Failed)?;
                Err(TrialStopError::ExitedLeaderGroupUnproven(status))
            }
            Err(_) => Err(TrialStopError::Failed(UpdateError::RecoveryRequired)),
        }
    }

    fn signal_group(&self) -> Result<(), std::io::Error> {
        let pid = self
            .child
            .as_ref()
            .and_then(|child| i32::try_from(child.id()).ok())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ECHILD))?;
        loop {
            // SAFETY: spawn created this group; its unreaped leader reserves
            // the PID until the single wait below. Never signal after ECHILD.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn reap_owned(&mut self) -> Result<std::process::ExitStatus, UpdateError> {
        // Retain the actual Child even on wait failure. No caller keeps a
        // cached PID/PGID or performs a later signal after this successful reap.
        let status = self
            .child
            .as_mut()
            .ok_or(UpdateError::RecoveryRequired)?
            .wait()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        self.child.take();
        Ok(status)
    }
    #[cfg(not(morpheus_macos_update_acceptance))]
    fn handoff(mut self) -> Child {
        self.child.take().unwrap()
    }
    #[cfg(morpheus_macos_update_acceptance)]
    fn handoff(mut self) {
        // Synthetic monitors retain waitable direct-child ownership, rather
        // than trying to signal orphan numeric PIDs after a simulated shutdown.
        acceptance::retain_child(self.child.take().unwrap());
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TrialRecovery {
    RolledBack,
    Committed,
}

fn same_transaction(left: &Journal, right: &Journal) -> bool {
    left.protocol == right.protocol
        && left.id == right.id
        && left.old_hash == right.old_hash
        && left.new_hash == right.new_hash
        && left.candidate == right.candidate
}

fn recover_failed_trial(
    installed: &Path,
    expected: &Journal,
) -> Result<TrialRecovery, UpdateError> {
    // An exited leader is not a group-drain proof. Recovery is independently
    // serialized with every trial's lifetime app lock. If a surviving holder
    // still owns it, fail rather than replacing a potentially healthy app.
    let session = MacAppSession::lock(installed, Duration::from_secs(5))?;
    let mut latest = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    if expected.phase != Phase::AwaitingHealth
        || !same_transaction(&latest, expected)
        || !matches!(latest.phase, Phase::AwaitingHealth | Phase::Committed)
        || tree_hash(&session.installed)? != latest.new_hash
        || bundle_metadata(&session.installed)? != latest.candidate
    {
        return Err(UpdateError::RecoveryRequired);
    }
    if latest.phase == Phase::Committed {
        // A health/store boundary won the lock first. Preserve the exact new
        // app, including on flush failure; an older persistence writer cannot
        // be restarted by rolling it back.
        session.confirm_commit()?;
        return Ok(TrialRecovery::Committed);
    }
    session.rollback(&mut latest)?;
    // Existing startup and acknowledge_before_store revalidate the nonce,
    // phase, executable identity and complete tree before opening stores. A
    // late trial cannot commit through this now-restored bundle/journal.
    Ok(TrialRecovery::RolledBack)
}

fn supervise(installed: &Path, id: &str) -> Result<(), UpdateError> {
    let (installed, state) = MacAppSession::paths(installed)?;
    let prepared = MacAppSession::read_journal(&state)?.ok_or(UpdateError::RecoveryRequired)?;
    let running = std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
    if running
        .canonicalize()
        .map_err(|_| UpdateError::RecoveryRequired)?
        != state
            .join("previous.app")
            .join(EXECUTABLE)
            .canonicalize()
            .map_err(|_| UpdateError::RecoveryRequired)?
        || prepared.id != id
        || prepared.phase != Phase::Prepared
        || !prepared.candidate.compatible(&PackageMetadata::embedded()?)
        || tree_hash(&state.join("previous.app"))? != prepared.old_hash
    {
        return Err(UpdateError::RecoveryRequired);
    }
    let mut ready =
        tempfile::NamedTempFile::new_in(&state).map_err(|_| UpdateError::RecoveryRequired)?;
    ready
        .write_all(id.as_bytes())
        .and_then(|_| ready.as_file().sync_all())
        .map_err(|_| UpdateError::RecoveryRequired)?;
    ready
        .persist_noclobber(state.join("supervisor-ready"))
        .map_err(|_| UpdateError::RecoveryRequired)?;
    sync_directory(&state)?;
    let session = MacAppSession::lock(&installed, Duration::from_secs(120))?;
    let mut journal = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    if !same_transaction(&journal, &prepared) {
        return Err(UpdateError::RecoveryRequired);
    }
    session.apply(&mut journal)?;
    drop(session);
    #[cfg(morpheus_macos_update_acceptance)]
    acceptance::after_apply_checkpoint()?;
    let mut child = match OwnedChild::spawn(
        app_command(&installed)
            .arg("--morpheus-mac-update-trial")
            .arg(id),
    ) {
        Ok(child) => child,
        Err(_) => {
            recover_failed_trial(&installed, &journal)?;
            spawn_restarted(&installed)?;
            return Err(UpdateError::RestartFailed);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let observed = MacAppSession::read_journal(&state)?.ok_or(UpdateError::RecoveryRequired)?;
        if !same_transaction(&observed, &journal) {
            return Err(UpdateError::RecoveryRequired);
        }
        if observed.phase == Phase::Committed {
            drop(child.handoff());
            return Ok(());
        }
        if child.exited()? || Instant::now() >= deadline {
            let cleanup_unproven = match child.stop_before_health() {
                Ok(()) => false,
                Err(TrialStopError::ExitedLeaderGroupUnproven(_status)) => true,
                Err(TrialStopError::Failed(error)) => return Err(error),
            };
            let recovered = recover_failed_trial(&installed, &journal)?;
            spawn_restarted(&installed)?;
            return if cleanup_unproven {
                // Restoration/relaunch did not establish group quiescence.
                // Keep that production cleanup gate visibly unresolved.
                Err(UpdateError::RecoveryRequired)
            } else if recovered == TrialRecovery::Committed {
                Ok(())
            } else {
                Err(UpdateError::RestartFailed)
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_restarted(installed: &Path) -> Result<(), UpdateError> {
    #[cfg(not(morpheus_macos_update_acceptance))]
    {
        app_command(installed)
            .spawn()
            .map_err(|_| UpdateError::RestartFailed)?;
    }
    #[cfg(morpheus_macos_update_acceptance)]
    {
        let child = OwnedChild::spawn(&mut app_command(installed))?;
        child.handoff();
    }
    Ok(())
}

pub(super) fn early_dispatch() -> Result<bool, UpdateError> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--morpheus-update-metadata") if args.len() == 2 => {
            println!(
                "{}",
                serde_json::to_string(&PackageMetadata::embedded()?)
                    .map_err(|_| UpdateError::InvalidConfiguration)?
            );
            Ok(true)
        }
        Some("--morpheus-mac-update-supervise") if args.len() == 4 => {
            if !INSTALLATION_VALIDATED {
                return Err(UpdateError::InstallNotValidated);
            }
            supervise(
                Path::new(&args[2]),
                args[3].to_str().ok_or(UpdateError::RecoveryRequired)?,
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(super) fn begin_startup() -> Result<(Option<MacAppSession>, bool), UpdateError> {
    begin_startup_admitted(INSTALLATION_VALIDATED)
}

fn begin_startup_admitted(admitted: bool) -> Result<(Option<MacAppSession>, bool), UpdateError> {
    let executable = std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
    let Some(installed) = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
    else {
        return Ok((None, false));
    };
    if installed
        .file_name()
        .is_none_or(|name| name != "Morpheus.app")
    {
        return Ok((None, false));
    }
    if executable
        .canonicalize()
        .map_err(|_| UpdateError::RecoveryRequired)?
        != installed
            .join(EXECUTABLE)
            .canonicalize()
            .map_err(|_| UpdateError::RecoveryRequired)?
    {
        return Err(UpdateError::UnsupportedPackage);
    }
    if !admitted {
        // Revoking future activation must not ignore an existing transaction
        // and open stores through unresolved application replacement.
        let pending = installed
            .parent()
            .unwrap()
            .join(".Morpheus.app.morpheus-update/journal.json");
        match fs::symlink_metadata(pending) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, false)),
            _ => return Err(UpdateError::RecoveryRequired),
        }
    }
    let args: Vec<_> = std::env::args_os().collect();
    let mut session = match MacAppSession::lock(installed, Duration::ZERO) {
        Ok(session) => session,
        Err(error) => {
            let journal = installed
                .parent()
                .unwrap()
                .join(".Morpheus.app.morpheus-update/journal.json");
            if error != UpdateError::UpdateActive
                && fs::symlink_metadata(journal)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok((None, false));
            }
            return Err(error);
        }
    };
    let Some(mut journal) = session.journal()? else {
        if args
            .get(1)
            .is_some_and(|s| s == "--morpheus-mac-update-trial")
        {
            return Err(UpdateError::RecoveryRequired);
        }
        // Legacy or differently packaged apps can still launch manually.
        match bundle_metadata(&session.installed) {
            Ok(identity) if identity == PackageMetadata::embedded()? => {}
            _ => return Ok((None, false)),
        }
        return Ok((Some(session), false));
    };
    let current = tree_hash(&session.installed)?;
    let trial = args.len() == 3
        && args[1] == "--morpheus-mac-update-trial"
        && args[2].to_str() == Some(journal.id.as_str());
    if journal.phase == Phase::Committed {
        if current != journal.new_hash {
            return Err(UpdateError::RecoveryRequired);
        }
        session.confirm_commit()?;
        session.clean()?;
    } else if trial
        && journal.phase == Phase::AwaitingHealth
        && current == journal.new_hash
        && journal.candidate == PackageMetadata::embedded()?
    {
        session.trial = Some(journal.id);
    } else {
        let replaced = current != journal.old_hash;
        session.rollback(&mut journal)?;
        if replaced {
            let installed = session.installed.clone();
            drop(session);
            spawn_restarted(&installed)?;
            return Ok((None, true));
        }
    }
    Ok((Some(session), false))
}

#[cfg(any(test, morpheus_macos_update_acceptance))]
#[path = "macos_transaction_acceptance.rs"]
pub(super) mod acceptance;

#[cfg(test)]
#[path = "macos_transaction_tests.rs"]
mod tests;
