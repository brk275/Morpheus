//! Provenance for the separately named, opt-in private-client diagnostic.
//! The ordinary capture test never calls this verifier.

use std::{
    ffi::{CStr, c_void},
    fs::{self, File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_LIBRARY_BYTES: usize = 64 * 1024 * 1024;
const MAX_MAP_BYTES: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    schema: u32,
    variant: String,
    library_path: PathBuf,
    sha256: String,
    build_id: String,
}

fn lower_hex(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value.len().is_multiple_of(2)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn private_metadata(path: &Path, directory: bool) -> Result<Metadata, &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "artifact metadata unavailable")?;
    // This diagnostic runs as the original unprivileged runner identity.
    let uid = unsafe { libc::geteuid() };
    if uid == 0
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err("artifact ownership or permissions rejected");
    }
    Ok(metadata)
}

fn read_bounded(path: &Path, limit: usize) -> Result<(Vec<u8>, Metadata), &'static str> {
    let expected = private_metadata(path, false)?;
    if expected.len() > limit as u64 {
        return Err("artifact size rejected");
    }
    let file = File::open(path).map_err(|_| "artifact unavailable")?;
    let actual = file
        .metadata()
        .map_err(|_| "artifact metadata unavailable")?;
    if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err("artifact identity changed");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "artifact read failed")?;
    if bytes.len() > limit || bytes.len() as u64 != actual.len() {
        return Err("artifact size changed");
    }
    Ok((bytes, actual))
}

fn word<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

fn align4(value: usize) -> Option<usize> {
    Some(value.checked_add(3)? & !3)
}

/// Read only bounded ELF64 little-endian x86_64 PT_NOTE records. This does not
/// load code, follow section offsets, or dereference an untrusted address.
fn elf_build_id(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() > MAX_LIBRARY_BYTES
        || bytes.get(..7)? != b"\x7fELF\x02\x01\x01"
        || u16::from_le_bytes(word(bytes, 16)?) != 3
        || u16::from_le_bytes(word(bytes, 18)?) != 62
        || u32::from_le_bytes(word(bytes, 20)?) != 1
        || u16::from_le_bytes(word(bytes, 52)?) != 64
        || u16::from_le_bytes(word(bytes, 54)?) != 56
    {
        return None;
    }
    let table = usize::try_from(u64::from_le_bytes(word(bytes, 32)?)).ok()?;
    let count = usize::from(u16::from_le_bytes(word(bytes, 56)?));
    if !(1..=64).contains(&count) {
        return None;
    }
    bytes.get(table..table.checked_add(count.checked_mul(56)?)?)?;
    let mut found = None;
    let mut notes = 0_usize;
    for index in 0..count {
        let header = table.checked_add(index.checked_mul(56)?)?;
        if u32::from_le_bytes(word(bytes, header)?) != 4 {
            continue;
        }
        let start = usize::try_from(u64::from_le_bytes(word(bytes, header + 8)?)).ok()?;
        let length = usize::try_from(u64::from_le_bytes(word(bytes, header + 32)?)).ok()?;
        let end = start.checked_add(length)?;
        bytes.get(start..end)?;
        let mut offset = start;
        while offset < end {
            notes += 1;
            if notes > 128 || offset.checked_add(12)? > end {
                return None;
            }
            let name_len = usize::try_from(u32::from_le_bytes(word(bytes, offset)?)).ok()?;
            let data_len = usize::try_from(u32::from_le_bytes(word(bytes, offset + 4)?)).ok()?;
            let kind = u32::from_le_bytes(word(bytes, offset + 8)?);
            let name_start = offset.checked_add(12)?;
            let data_start = name_start.checked_add(align4(name_len)?)?;
            let next = data_start.checked_add(align4(data_len)?)?;
            if next > end {
                return None;
            }
            if kind == 3 && bytes.get(name_start..name_start.checked_add(name_len)?)? == b"GNU\0" {
                if found.is_some() || !(8..=64).contains(&data_len) {
                    return None;
                }
                found = Some(
                    bytes
                        .get(data_start..data_start.checked_add(data_len)?)?
                        .to_vec(),
                );
            }
            offset = next;
        }
    }
    found
}

fn mapped_function(maps: &str, address: usize, expected_device: u64, expected_inode: u64) -> bool {
    maps.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some((range, permissions, _offset, device, inode)) = fields
            .next()
            .zip(fields.next())
            .zip(fields.next())
            .zip(fields.next())
            .zip(fields.next())
            .map(|((((a, b), c), d), e)| (a, b, c, d, e))
        else {
            return false;
        };
        let Some((start, end)) = range.split_once('-') else {
            return false;
        };
        let Some((major, minor)) = device.split_once(':') else {
            return false;
        };
        permissions.as_bytes().get(2) == Some(&b'x')
            && usize::from_str_radix(start, 16).is_ok_and(|start| address >= start)
            && usize::from_str_radix(end, 16).is_ok_and(|end| address < end)
            && inode.parse::<u64>().ok() == Some(expected_inode)
            && u32::from_str_radix(major, 16).ok() == Some(libc::major(expected_device))
            && u32::from_str_radix(minor, 16).ok() == Some(libc::minor(expected_device))
    })
}

pub(super) fn verify(addresses: &[*const c_void]) -> Result<(), &'static str> {
    if addresses.len() != 33
        || std::env::var("MORPHEUS_WAYLAND_STARTUP_PROBE").as_deref() != Ok("1")
    {
        return Err("private-client diagnostic was not selected");
    }
    let manifest = PathBuf::from(
        std::env::var_os("MORPHEUS_PIPEWIRE_PROBE_ARTIFACT").ok_or("artifact manifest missing")?,
    );
    let parent = manifest.parent().ok_or("artifact directory missing")?;
    private_metadata(parent, true)?;
    if manifest.canonicalize().ok().as_ref() != Some(&manifest) {
        return Err("artifact manifest is not canonical");
    }
    let (bytes, _) = read_bounded(&manifest, 4096)?;
    let artifact: Artifact =
        serde_json::from_slice(&bytes).map_err(|_| "artifact manifest rejected")?;
    if artifact.schema != 1
        || !matches!(artifact.variant.as_str(), "control" | "patched")
        || !lower_hex(&artifact.sha256, 64, 64)
        || !lower_hex(&artifact.build_id, 16, 128)
        || artifact.library_path.parent() != Some(parent)
        || artifact
            .library_path
            .file_name()
            .and_then(|value| value.to_str())
            != Some("libpipewire-0.3.so.0.1005.0")
        || artifact.library_path.canonicalize().ok().as_ref() != Some(&artifact.library_path)
    {
        return Err("artifact manifest contract rejected");
    }
    let (library, metadata) = read_bounded(&artifact.library_path, MAX_LIBRARY_BYTES)?;
    if format!("{:x}", Sha256::digest(&library)) != artifact.sha256 {
        return Err("artifact digest rejected");
    }
    let build_id = elf_build_id(&library).ok_or("artifact ELF identity rejected")?;
    let build_id: String = build_id.iter().map(|byte| format!("{byte:02x}")).collect();
    if build_id != artifact.build_id {
        return Err("artifact build identity rejected");
    }
    let mut maps = String::new();
    File::open("/proc/self/maps")
        .map_err(|_| "loaded mappings unavailable")?
        .take(MAX_MAP_BYTES as u64 + 1)
        .read_to_string(&mut maps)
        .map_err(|_| "loaded mappings unavailable")?;
    if maps.len() > MAX_MAP_BYTES {
        return Err("loaded mappings exceed bound");
    }
    for &address in addresses {
        let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
        if unsafe { libc::dladdr(address, info.as_mut_ptr()) } == 0 {
            return Err("function origin unavailable");
        }
        let info = unsafe { info.assume_init() };
        if info.dli_fname.is_null() {
            return Err("function origin unavailable");
        }
        let name = unsafe { CStr::from_ptr(info.dli_fname) }
            .to_str()
            .map_err(|_| "function origin unavailable")?;
        if fs::canonicalize(name).ok().as_ref() != Some(&artifact.library_path)
            || !mapped_function(&maps, address as usize, metadata.dev(), metadata.ino())
        {
            return Err("function origin differs from reviewed artifact");
        }
    }
    let facts = serde_json::json!({
        "schema": 1, "variant": artifact.variant, "checked": 33,
        "private_probe": 33, "digest_verified": true, "build_id_verified": true,
        "mapped_file_verified": true,
    });
    let path = std::env::var_os("MORPHEUS_WAYLAND_BINDING_PATH")
        .ok_or("private binding facts path missing")?;
    fs::write(
        path,
        serde_json::to_vec(&facts).map_err(|_| "binding facts rejected")?,
    )
    .map_err(|_| "binding facts could not be recorded")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf() -> Vec<u8> {
        let mut bytes = vec![0; 156];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&3_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&4_u32.to_le_bytes());
        bytes[72..80].copy_from_slice(&120_u64.to_le_bytes());
        bytes[96..104].copy_from_slice(&36_u64.to_le_bytes());
        bytes[120..124].copy_from_slice(&4_u32.to_le_bytes());
        bytes[124..128].copy_from_slice(&20_u32.to_le_bytes());
        bytes[128..132].copy_from_slice(&3_u32.to_le_bytes());
        bytes[132..136].copy_from_slice(b"GNU\0");
        bytes[136..].fill(7);
        bytes
    }

    #[test]
    fn bounded_elf_identity_rejects_truncation_and_wrong_binary_shapes() {
        let good = elf();
        assert_eq!(elf_build_id(&good), Some(vec![7; 20]));
        for end in 0..good.len() {
            assert_eq!(elf_build_id(&good[..end]), None);
        }
        for offset in [0, 4, 5, 6, 16, 18, 20, 52, 54, 56, 64, 120, 124, 128, 132] {
            let mut bad = good.clone();
            bad[offset] = 0;
            assert_eq!(elf_build_id(&bad), None, "shape offset {offset}");
        }
        for offset in [32, 72, 96] {
            let mut bad = good.clone();
            bad[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert_eq!(elf_build_id(&bad), None);
        }
    }

    #[test]
    fn duplicate_build_ids_and_unbounded_note_lengths_are_rejected() {
        let good = elf();
        let mut duplicate = good.clone();
        duplicate.extend_from_slice(&good[120..]);
        duplicate[96..104].copy_from_slice(&72_u64.to_le_bytes());
        assert_eq!(elf_build_id(&duplicate), None);
        for offset in [120, 124] {
            let mut bad = good.clone();
            bad[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert_eq!(elf_build_id(&bad), None);
        }
    }

    #[test]
    fn function_mapping_requires_executable_range_and_matching_device_inode() {
        let maps = "1000-2000 r-xp 00000000 08:01 42 /private/library\n";
        let device = libc::makedev(8, 1);
        assert!(mapped_function(maps, 0x1000, device, 42));
        assert!(mapped_function(maps, 0x1fff, device, 42));
        assert!(!mapped_function(maps, 0x2000, device, 42));
        assert!(!mapped_function(maps, 0x0fff, device, 42));
        assert!(!mapped_function(maps, 0x1500, device, 43));
        assert!(!mapped_function(maps, 0x1500, libc::makedev(8, 2), 42));
        assert!(!mapped_function(
            &maps.replace("r-xp", "r--p"),
            0x1500,
            device,
            42
        ));
        for malformed in [
            "",
            "private text",
            "1000-2000 r-xp 0 bad 42",
            "bad r-xp 0 08:01 42",
        ] {
            assert!(!mapped_function(malformed, 0x1500, device, 42));
        }
    }

    #[test]
    fn manifest_accepts_only_complete_lowercase_digest_fields() {
        assert!(lower_hex(&"a7".repeat(32), 64, 64));
        for value in [
            "A".repeat(64),
            "z".repeat(64),
            "a".repeat(63),
            "a".repeat(66),
        ] {
            assert!(!lower_hex(&value, 64, 64));
        }
        assert!(serde_json::from_str::<Artifact>(
            r#"{"schema":1,"variant":"control","library_path":"/private/lib","sha256":"a","build_id":"b","extra":true}"#
        ).is_err());
    }
}
