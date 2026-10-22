//! Bounded archive staging for the pinned native SDK download.

use std::{
    fs,
    io::{self, Read, Seek, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use super::pins::Pin;

const MAX_ENTRIES: usize = 100_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(super) const RECEIPT: &str = ".morpheus-archive-sha256";

pub(super) fn copy_verified(
    mut source: impl Read,
    mut output: impl Write,
    pin: &Pin,
) -> Result<()> {
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .context("Cannot read native SDK archive")?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("Native SDK archive is too large"))?;
        if count > pin.bytes {
            return Err(anyhow!("Native SDK archive exceeds its pinned size"));
        }
        hash.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .context("Cannot stage native SDK archive")?;
    }
    if count != pin.bytes || format!("{:x}", hash.finalize()) != pin.sha256 {
        return Err(anyhow!(
            "Native SDK archive identity does not match the reviewed release"
        ));
    }
    output.flush().context("Cannot flush native SDK archive")?;
    Ok(())
}

pub(super) fn check_receipt(directory: &Path, pin: &Pin) -> Result<()> {
    if !regular_directory(&fs::symlink_metadata(directory)?) {
        return Err(anyhow!("Unsafe native SDK cache directory"));
    }
    let metadata = fs::symlink_metadata(directory.join(RECEIPT)).context(
        "Native SDK cache has no verified-download receipt; remove that cache before rebuilding",
    )?;
    if !metadata.is_file() || !plain_metadata(&metadata) || metadata.len() != 64 {
        return Err(anyhow!("Invalid native SDK cache receipt"));
    }
    let mut bytes = Vec::with_capacity(65);
    fs::File::open(directory.join(RECEIPT))
        .context("Native SDK cache has no verified-download receipt; remove that cache before rebuilding")?
        .take(65).read_to_end(&mut bytes)?;
    if bytes != pin.sha256.as_bytes() {
        return Err(anyhow!(
            "Native SDK cache receipt does not match the reviewed release"
        ));
    }
    Ok(())
}

pub(super) fn write_receipt(directory: &Path, pin: &Pin) -> Result<()> {
    if !regular_directory(&fs::symlink_metadata(directory)?) {
        return Err(anyhow!("Unsafe native SDK receipt directory"));
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(RECEIPT))
        .context("Cannot record verified native SDK download")?;
    file.write_all(pin.sha256.as_bytes())?;
    Ok(())
}

fn regular_directory(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && plain_metadata(metadata)
}

fn plain_metadata(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        } // reparse point
    }
    true
}

fn entry_path(file: &zip::read::ZipFile<'_>) -> Result<PathBuf> {
    let name = file.name();
    if name.len() > 4096 || name.contains('\\') || name.contains('\0') {
        return Err(anyhow!("Unsupported native SDK archive path"));
    }
    let relative = file
        .enclosed_name()
        .ok_or_else(|| anyhow!("Unsafe native SDK archive path"))?;
    let mut count = 0;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(anyhow!("Unsafe native SDK archive path"));
        };
        let text = component
            .to_str()
            .ok_or_else(|| anyhow!("Unsupported native SDK archive path"))?;
        if text
            .chars()
            .any(|c| c.is_ascii_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            || text.ends_with('.')
            || text.ends_with(' ')
        {
            return Err(anyhow!("Unsupported native SDK archive path"));
        }
        let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|tail| {
                matches!(
                    tail,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        }) {
            return Err(anyhow!("Unsupported native SDK archive path"));
        }
        count += 1;
    }
    if count == 0 || count > 128 {
        return Err(anyhow!("Unsupported native SDK archive path depth"));
    }
    Ok(relative.to_owned())
}

fn ensure_directory(base: &Path, relative: &Path) -> Result<PathBuf> {
    if !regular_directory(&fs::symlink_metadata(base)?) {
        return Err(anyhow!("Unsafe native SDK staging directory"));
    }
    let mut current = base.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(anyhow!("Unsafe native SDK archive directory"));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if regular_directory(&metadata) => {}
            Ok(_) => {
                return Err(anyhow!(
                    "Native SDK archive directory is not a regular directory"
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

pub(super) fn extract<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, base: &Path) -> Result<()> {
    if archive.len() > MAX_ENTRIES {
        return Err(anyhow!("Native SDK archive has too many entries"));
    }
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .context("Cannot read native SDK archive entry")?;
        let relative = entry_path(&file)?;
        let directory = file.is_dir();
        let mode = file.unix_mode().unwrap_or(0);
        let kind = mode & 0o170000;
        if kind != 0 && kind != if directory { 0o040000 } else { 0o100000 } {
            return Err(anyhow!(
                "Native SDK archive contains an unsupported entry type"
            ));
        }
        let size = file.size();
        expanded = expanded
            .checked_add(size)
            .ok_or_else(|| anyhow!("Native SDK archive is too large"))?;
        if size > MAX_FILE_BYTES || expanded > MAX_EXPANDED_BYTES {
            return Err(anyhow!("Native SDK archive expands beyond its limit"));
        }
        if directory {
            if size != 0 {
                return Err(anyhow!("Native SDK archive directory has file contents"));
            }
            ensure_directory(base, &relative)?;
            continue;
        }
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = ensure_directory(base, parent)?;
        let name = relative
            .file_name()
            .ok_or_else(|| anyhow!("Empty native SDK archive filename"))?;
        // Never overwrite or open a pre-existing link/file, including aliases.
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(parent.join(name))?;
        let copied = io::copy(&mut file.by_ref().take(size + 1), &mut output)
            .context("Cannot unpack native SDK archive entry")?;
        if copied != size {
            return Err(anyhow!("Native SDK archive entry length does not match"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Preserve owner execution only; discard special/group/other bits.
            output.set_permissions(fs::Permissions::from_mode(0o600 | (mode & 0o100)))?;
        }
    }
    Ok(())
}
