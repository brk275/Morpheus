//! The Windows update is a signed data envelope, never an executable installer.
//! Its one resource ledger is also the install marker and the uninstall contract.

use super::super::{MAX_UPDATE_BYTES, UpdateError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const MAGIC: &[u8] = b"MORPHEUS-WINDOWS-PAYLOAD-V1\n";
pub(super) const MARKER: &str = "morpheus-install.json";
pub(super) const UNINSTALLER: &str = "uninstall.exe";
pub(super) const FILES: [&str; 5] = [
    "LICENSE",
    "licenses/Symphonia-MPL-2.0.txt",
    "licenses/Symphonia-NOTICE.txt",
    "morpheus-app.exe",
    "themes/aurora.toml",
];
pub(super) const MARKER_LIMIT: usize = 16 * 1024;
// Generated from the reviewed template and resource contract, not user input.
pub(super) const UNINSTALL_CONTRACT: &str =
    "3f45a065d910b1b8aa889bb4d74dfec33682c23bcfee2dc69d66ed5ce9382a9d";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub protocol: u8,
    pub product: String,
    pub version: String,
    pub arch: String,
    pub persistence: String,
}

impl Identity {
    pub fn valid(&self) -> bool {
        self.protocol == 1
            && self.product == "io.github.brk275.morpheus"
            && self.arch == "x86_64"
            && cargo_packager_updater::semver::Version::parse(&self.version).is_ok()
            && valid_hash(&self.persistence)
    }

    pub fn compatible(&self, old: &Self) -> bool {
        self.valid()
            && old.valid()
            && self.protocol == old.protocol
            && self.product == old.product
            && self.arch == old.arch
            && self.persistence == old.persistence
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub schema: u8,
    pub identity: Identity,
    pub uninstaller_contract: String,
    pub files: Vec<Entry>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        if bytes.is_empty() || bytes.len() > MARKER_LIMIT {
            return Err(UpdateError::CorruptStage);
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| UpdateError::CorruptStage)?;
        if value.schema != 1
            || !value.identity.valid()
            || value.uninstaller_contract != UNINSTALL_CONTRACT
            || value.files.len() != FILES.len()
        {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let mut total = 0_u64;
        for (entry, expected) in value.files.iter().zip(FILES) {
            // Exact paths prevent traversal, aliases, streams, reserved names,
            // case collisions, extra DLLs and future uninstaller drift at once.
            if entry.path != expected || entry.bytes == 0 || !valid_hash(&entry.sha256) {
                return Err(UpdateError::CorruptStage);
            }
            total = total
                .checked_add(entry.bytes)
                .ok_or(UpdateError::CorruptStage)?;
            if total > MAX_UPDATE_BYTES {
                return Err(UpdateError::CorruptStage);
            }
        }
        Ok(value)
    }
}

pub(super) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) struct Payload<'a> {
    pub manifest: Manifest,
    pub marker: &'a [u8],
    pub contents: Vec<&'a [u8]>,
}

impl<'a> Payload<'a> {
    pub fn parse(
        bytes: &'a [u8],
        installed: &Identity,
        version: &str,
    ) -> Result<Self, UpdateError> {
        if bytes.len() as u64 > MAX_UPDATE_BYTES || !bytes.starts_with(MAGIC) {
            return Err(UpdateError::UnsupportedFormat);
        }
        let raw_len = bytes
            .get(MAGIC.len()..MAGIC.len() + 4)
            .ok_or(UpdateError::CorruptStage)?;
        let len = u32::from_le_bytes(raw_len.try_into().unwrap()) as usize;
        let start = MAGIC.len() + 4;
        let end = start.checked_add(len).ok_or(UpdateError::CorruptStage)?;
        let marker = bytes.get(start..end).ok_or(UpdateError::CorruptStage)?;
        let manifest = Manifest::parse(marker)?;
        if !manifest.identity.compatible(installed) || manifest.identity.version != version {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let next = cargo_packager_updater::semver::Version::parse(version)
            .map_err(|_| UpdateError::NotNewer)?;
        let old = cargo_packager_updater::semver::Version::parse(&installed.version)
            .map_err(|_| UpdateError::NotNewer)?;
        if next.cmp_precedence(&old) != std::cmp::Ordering::Greater {
            return Err(UpdateError::NotNewer);
        }
        let mut offset = end;
        let mut contents = Vec::with_capacity(FILES.len());
        for entry in &manifest.files {
            let count = usize::try_from(entry.bytes).map_err(|_| UpdateError::CorruptStage)?;
            let end = offset.checked_add(count).ok_or(UpdateError::CorruptStage)?;
            let file = bytes.get(offset..end).ok_or(UpdateError::CorruptStage)?;
            if hash(file) != entry.sha256 {
                return Err(UpdateError::CorruptStage);
            }
            contents.push(file);
            offset = end;
        }
        if offset != bytes.len() || pe_identity(contents[3])? != manifest.identity {
            return Err(UpdateError::CorruptStage);
        }
        Ok(Self {
            manifest,
            marker,
            contents,
        })
    }
}

/// Parse only. No loader, DLL resolution, executable memory or resource execution.
pub(super) fn pe_identity(bytes: &[u8]) -> Result<Identity, UpdateError> {
    let invalid = || UpdateError::UnsupportedPackage;
    let u16_at = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(invalid)
    };
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(invalid)
    };
    if bytes.len() < 256 || &bytes[..2] != b"MZ" {
        return Err(invalid());
    }
    let pe = u32_at(60)? as usize;
    if !(64..=1024 * 1024).contains(&pe) || bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err(invalid());
    }
    let count = u16_at(pe + 6)? as usize;
    let optional = u16_at(pe + 20)? as usize;
    if u16_at(pe + 4)? != 0x8664
        || !(1..=96).contains(&count)
        || optional < 112
        || u16_at(pe + 24)? != 0x20b
        || u16_at(pe + 24 + 68)? != 2
        || u16_at(pe + 22)? & 0x2002 != 0x0002
    {
        return Err(invalid());
    }
    let table = pe.checked_add(24 + optional).ok_or_else(invalid)?;
    let table_end = table.checked_add(count * 40).ok_or_else(invalid)?;
    if table_end > bytes.len() {
        return Err(invalid());
    }
    let mut ranges = Vec::new();
    let mut identity = None;
    for section in 0..count {
        let at = table + section * 40;
        let raw = u32_at(at + 20)? as usize;
        let size = u32_at(at + 16)? as usize;
        if size > 0 {
            let end = raw.checked_add(size).ok_or_else(invalid)?;
            if raw < table_end
                || end > bytes.len()
                || ranges.iter().any(|&(a, b)| raw < b && a < end)
            {
                return Err(invalid());
            }
            ranges.push((raw, end));
        }
        if bytes.get(at..at + 8) == Some(b".mrphupd") {
            let used = u32_at(at + 8)? as usize;
            let flags = u32_at(at + 36)?;
            if identity.is_some()
                || !(1..=4096).contains(&used)
                || used > size
                || flags & 0xe200_0040 != 0x4000_0040
                || bytes[raw + used..raw + size].iter().any(|&b| b != 0)
            {
                return Err(invalid());
            }
            let value: Identity =
                serde_json::from_slice(&bytes[raw..raw + used]).map_err(|_| invalid())?;
            if !value.valid() {
                return Err(invalid());
            }
            identity = Some(value);
        }
    }
    identity.ok_or_else(invalid)
}
