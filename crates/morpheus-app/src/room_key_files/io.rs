//! Bounded encrypted bytes only. No passphrase, account, UI, SDK or arbitrary
//! path deletion lives in the platform helper. Unknown publication is retained.

use std::path::Path;
use zeroize::Zeroizing;

const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IoError {
    InvalidPath,
    Unsupported,
    NotRegular,
    Limit,
    Read,
    Exists,
    Write,
    CleanupUncertain,
    CommitUncertain,
    NativeUncertain,
}
impl IoError {
    pub(super) fn ownership_uncertain(self) -> bool {
        matches!(
            self,
            Self::CleanupUncertain | Self::CommitUncertain | Self::NativeUncertain
        )
    }
}
impl std::fmt::Display for IoError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::InvalidPath => "Choose an ordinary absolute local file path without links or special components.",
            Self::Unsupported => "This folder or filesystem cannot support the required private key-file operation. Choose another local folder.",
            Self::NotRegular => "Choose one regular encrypted file, without a link or hard-link alias.",
            Self::Limit => "The encrypted room-key file exceeds the supported size limit.",
            Self::Read => "The encrypted key file could not be read consistently. Nothing was imported.",
            Self::Exists => "The selected destination already exists. Choose a new filename; the existing file was preserved.",
            Self::Write => "The encrypted export could not be prepared. Nothing was published.",
            Self::CleanupUncertain => "Private staging cleanup could not be confirmed. No cleanup success was assumed.",
            Self::CommitUncertain => "Encrypted-file publication could not be confirmed. Check the chosen folder before exporting again; it was not retried.",
            Self::NativeUncertain => "Native file-operation completion could not be confirmed. Account cleanup remains blocked.",
        })
    }
}
impl std::error::Error for IoError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Publication {
    Published,
    PublishedCleanupUncertain,
}

#[cfg(windows)]
#[path = "io/windows.rs"]
mod platform;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "io/unix.rs"]
mod platform;

pub(super) fn read_encrypted(path: &Path) -> Result<Zeroizing<Vec<u8>>, IoError> {
    platform::read_encrypted(path)
}
pub(super) struct ExportStage(platform::ExportStage);
impl ExportStage {
    pub(super) fn prepare(path: &Path, encrypted: &[u8]) -> Result<Self, IoError> {
        if encrypted.len() > MAX_FILE_BYTES {
            return Err(IoError::Limit);
        }
        platform::ExportStage::prepare(path, encrypted).map(Self)
    }
    pub(super) fn publish(&mut self) -> Result<Publication, IoError> {
        self.0.publish()
    }
    pub(super) fn discard(self) -> Result<(), IoError> {
        self.0.discard()
    }
}
