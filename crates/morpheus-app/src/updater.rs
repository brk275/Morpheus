//! Signed update discovery and staging. Run blocking methods on spawn_blocking.
//!
//! Release endpoints and the public verification key are compiled into a build,
//! never read from user-editable TOML. No feed or private signing keys ship in
//! development builds. Linux has an AppImage transaction and startup recovery;
//! public activation still requires signed installed-package acceptance.
//! macOS/Windows installation remains disabled pending platform transactions.
//!
//! UI integration must share one UpdateActivity with the call controller and
//! retain its CallLease from the start of joining through final call cleanup.
//! Merely checking a call-status snapshot cannot prevent a concurrent join.

#![allow(dead_code)] // Platform APIs remain compiled while unsupported installs fail closed.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use cargo_packager_updater::{Config, Update, UpdateFormat, UpdaterBuilder, semver::Version};
use tempfile::NamedTempFile;

const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;

#[cfg(target_os = "linux")]
mod linux_transaction;
#[cfg(target_os = "linux")]
pub use linux_transaction::{AppImageSession, PreparedRestart};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateError {
    NotConfigured,
    PackageManager,
    UnsupportedPackage,
    NotWritable,
    InvalidConfiguration,
    CheckFailed,
    UnsupportedFormat,
    NotNewer,
    DownloadFailed,
    StageFailed,
    CorruptStage,
    CallActive,
    UpdateActive,
    InstallNotValidated,
    IncompatibleUpdate,
    RecoveryRequired,
    RestartFailed,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotConfigured => "Signed updates are not configured for this build.",
            Self::PackageManager => "Update this installation using your Linux package manager.",
            Self::UnsupportedPackage => "This installation does not support in-app updates.",
            Self::NotWritable => "The installed application is not writable.",
            Self::InvalidConfiguration => "The release update configuration is invalid.",
            Self::CheckFailed => "The signed update feed could not be checked.",
            Self::UnsupportedFormat => "The update format does not match this installation.",
            Self::NotNewer => "The update is not newer than the installed version.",
            Self::DownloadFailed => "The update could not be downloaded and verified.",
            Self::StageFailed => "The verified update could not be staged.",
            Self::CorruptStage => {
                "The staged update is incomplete or has changed. Download it again."
            }
            Self::CallActive => "Leave the current call before installing an update.",
            Self::UpdateActive => "An update is being installed. Wait before starting a call.",
            Self::InstallNotValidated => {
                "Update installation is disabled until platform recovery checks pass."
            }
            Self::IncompatibleUpdate => "This update requires a separately verified data migration. Install it manually after reviewing the release instructions.",
            Self::RecoveryRequired => "Application update recovery could not finish. The retained previous application has not been deleted.",
            Self::RestartFailed => "The updated application could not start. Recovery keeps the previous application when safe.",
        })
    }
}

impl std::error::Error for UpdateError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PackageKind {
    MacApp,
    WindowsNsis,
    LinuxAppImage,
}

impl PackageKind {
    fn supports(self, format: UpdateFormat) -> bool {
        matches!(
            (self, format),
            (Self::MacApp, UpdateFormat::App)
                | (Self::WindowsNsis, UpdateFormat::Nsis)
                | (Self::LinuxAppImage, UpdateFormat::AppImage)
        )
    }
}

struct InstallLocation {
    kind: PackageKind,
    path: PathBuf,
}

impl InstallLocation {
    fn detect() -> Result<Self, UpdateError> {
        #[cfg(target_os = "macos")]
        {
            let executable =
                std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
            let macos = executable.parent().ok_or(UpdateError::UnsupportedPackage)?;
            let contents = macos.parent().ok_or(UpdateError::UnsupportedPackage)?;
            let app = contents.parent().ok_or(UpdateError::UnsupportedPackage)?;
            if macos.file_name().is_none_or(|part| part != "MacOS")
                || contents.file_name().is_none_or(|part| part != "Contents")
                || app.extension().is_none_or(|ext| ext != "app")
            {
                return Err(UpdateError::UnsupportedPackage);
            }
            Self::writable(PackageKind::MacApp, app.to_path_buf())
        }
        #[cfg(target_os = "linux")]
        {
            let path = std::env::var_os("APPIMAGE")
                .map(PathBuf::from)
                .ok_or(UpdateError::PackageManager)?;
            let mut file = fs::File::open(&path).map_err(|_| UpdateError::UnsupportedPackage)?;
            let mut header = [0; 11];
            file.read_exact(&mut header)
                .map_err(|_| UpdateError::UnsupportedPackage)?;
            if &header[..4] != b"\x7fELF" || &header[8..11] != b"AI\x02" {
                return Err(UpdateError::UnsupportedPackage);
            }
            Self::writable(PackageKind::LinuxAppImage, path)
        }
        #[cfg(target_os = "windows")]
        {
            // A loose EXE is indistinguishable from an NSIS installation.
            // Packaging must explicitly mark the installer build before the
            // runtime can offer self-update. WiX is outside Morpheus v1.
            if option_env!("MORPHEUS_PACKAGE_FORMAT") != Some("nsis") {
                return Err(UpdateError::UnsupportedPackage);
            }
            Self::writable(
                PackageKind::WindowsNsis,
                std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?,
            )
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        Err(UpdateError::UnsupportedPackage)
    }

    fn writable(kind: PackageKind, path: PathBuf) -> Result<Self, UpdateError> {
        let metadata = fs::symlink_metadata(&path).map_err(|_| UpdateError::NotWritable)?;
        if metadata.file_type().is_symlink() || metadata.permissions().readonly() {
            return Err(UpdateError::NotWritable);
        }
        if kind != PackageKind::MacApp {
            if !metadata.is_file() {
                return Err(UpdateError::UnsupportedPackage);
            }
            if kind == PackageKind::LinuxAppImage {
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|_| UpdateError::NotWritable)?;
            }
        } else if !metadata.is_dir() {
            return Err(UpdateError::UnsupportedPackage);
        }
        let parent = path.parent().ok_or(UpdateError::UnsupportedPackage)?;
        // Creating and dropping our own empty file checks actual directory
        // access; mode bits alone don't reflect ACLs or read-only mounts.
        tempfile::Builder::new()
            .prefix(".morpheus-update-probe-")
            .tempfile_in(parent)
            .map_err(|_| UpdateError::NotWritable)?;
        Ok(Self { kind, path })
    }
}

pub struct UpdateService {
    config: Config,
    current: Version,
    location: InstallLocation,
}

impl UpdateService {
    pub fn embedded() -> Result<Self, UpdateError> {
        let endpoint = option_env!("MORPHEUS_UPDATE_FEED")
            .filter(|s| !s.is_empty())
            .ok_or(UpdateError::NotConfigured)?;
        let public_key = option_env!("MORPHEUS_UPDATE_PUBLIC_KEY")
            .filter(|s| !s.is_empty())
            .ok_or(UpdateError::NotConfigured)?;
        let config = Config {
            endpoints: vec![
                endpoint
                    .parse()
                    .map_err(|_| UpdateError::InvalidConfiguration)?,
            ],
            pubkey: public_key.into(),
            ..Default::default()
        };
        let current = Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| UpdateError::InvalidConfiguration)?;
        let location = InstallLocation::detect()?;
        // The patched official builder validates HTTPS and the public key
        // before any network request can be started.
        UpdaterBuilder::new(current.clone(), config.clone())
            .build()
            .map_err(|_| UpdateError::InvalidConfiguration)?;
        Ok(Self {
            config,
            current,
            location,
        })
    }

    pub fn check(&self) -> Result<Option<AvailableUpdate>, UpdateError> {
        let mut updater = UpdaterBuilder::new(self.current.clone(), self.config.clone())
            .timeout(Duration::from_secs(120));
        if self.location.kind == PackageKind::LinuxAppImage {
            updater = updater.executable_path(&self.location.path);
        }
        let Some(update) = updater
            .build()
            .and_then(|updater| updater.check())
            .map_err(|_| UpdateError::CheckFailed)?
        else {
            return Ok(None);
        };
        validate_candidate(&update, &self.current, self.location.kind)?;
        Ok(Some(AvailableUpdate {
            update,
            kind: self.location.kind,
        }))
    }
}

fn validate_candidate(
    update: &Update,
    current: &Version,
    kind: PackageKind,
) -> Result<(), UpdateError> {
    if !kind.supports(update.format) {
        return Err(UpdateError::UnsupportedFormat);
    }
    let next = Version::parse(&update.version).map_err(|_| UpdateError::NotNewer)?;
    if next.cmp_precedence(current) != std::cmp::Ordering::Greater {
        return Err(UpdateError::NotNewer);
    }
    Ok(())
}

pub struct AvailableUpdate {
    update: Update,
    kind: PackageKind,
}

impl AvailableUpdate {
    pub fn version(&self) -> &str {
        &self.update.version
    }

    pub fn stage(self) -> Result<StagedUpdate, UpdateError> {
        let bytes = self
            .update
            .download()
            .map_err(|_| UpdateError::DownloadFailed)?;
        stage_bytes(self.update, self.kind, &bytes)
    }
}

fn stage_bytes(
    update: Update,
    kind: PackageKind,
    bytes: &[u8],
) -> Result<StagedUpdate, UpdateError> {
    if !kind.supports(update.format) {
        return Err(UpdateError::UnsupportedFormat);
    }
    update
        .verify(bytes)
        .map_err(|_| UpdateError::CorruptStage)?;
    let mut file = tempfile::Builder::new()
        .prefix("morpheus-verified-update-")
        .tempfile()
        .map_err(|_| UpdateError::StageFailed)?;
    file.write_all(bytes)
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| UpdateError::StageFailed)?;
    Ok(StagedUpdate { update, file })
}

/// Owns only a staged artifact. Dropping it removes the file; account databases,
/// credentials, drafts and the installed application are never touched.
pub struct StagedUpdate {
    update: Update,
    file: NamedTempFile,
}

impl StagedUpdate {
    pub fn version(&self) -> &str {
        &self.update.version
    }

    pub fn verify(&mut self) -> Result<(), UpdateError> {
        self.verified_bytes().map(|_| ())
    }

    fn verified_bytes(&mut self) -> Result<Vec<u8>, UpdateError> {
        let file = self.file.as_file_mut();
        if file
            .metadata()
            .map_err(|_| UpdateError::CorruptStage)?
            .len()
            > MAX_UPDATE_BYTES
        {
            return Err(UpdateError::CorruptStage);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| UpdateError::CorruptStage)?;
        let mut bytes = Vec::new();
        file.take(MAX_UPDATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| UpdateError::CorruptStage)?;
        self.update
            .verify(&bytes)
            .map_err(|_| UpdateError::CorruptStage)?;
        Ok(bytes)
    }

    pub fn install(&mut self, activity: &UpdateActivity) -> Result<(), UpdateError> {
        let _lease = activity.try_install()?;
        let bytes = self.verified_bytes()?;
        self.update.install(bytes).map_err(|error| match error {
            cargo_packager_updater::Error::InstallationNotValidated => {
                UpdateError::InstallNotValidated
            }
            _ => UpdateError::CorruptStage,
        })
    }

    /// Prepare a signed, compatible Linux update while the current process
    /// retains its lifetime lock. The caller must explicitly hand off and quit.
    #[cfg(target_os = "linux")]
    pub fn prepare_restart<'a>(
        &mut self,
        activity: &UpdateActivity,
        session: &'a mut AppImageSession,
    ) -> Result<PreparedRestart<'a>, UpdateError> {
        let lease = activity.try_install()?;
        if !matches!(self.update.format, UpdateFormat::AppImage) {
            return Err(UpdateError::UnsupportedFormat);
        }
        let bytes = self.verified_bytes()?;
        linux_transaction::prepare_restart(session, &bytes, &self.update.version, lease)
    }
}

/// Must be called before constructing UI, runtimes, or account stores.
pub fn early_dispatch() -> Result<bool, UpdateError> {
    #[cfg(target_os = "linux")]
    return linux_transaction::early_dispatch();
    #[cfg(not(target_os = "linux"))]
    Ok(false)
}

pub struct StartupSession {
    #[cfg(target_os = "linux")]
    pub appimage: Option<AppImageSession>,
    pub restarted: bool,
}

impl StartupSession {
    pub fn begin() -> Result<Self, UpdateError> {
        #[cfg(target_os = "linux")]
        {
            let (appimage, restarted) = linux_transaction::begin_startup()?;
            Ok(Self {
                appimage,
                restarted,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self { restarted: false })
    }

    pub fn acknowledge_before_store(&mut self) -> Result<(), UpdateError> {
        #[cfg(target_os = "linux")]
        if let Some(session) = self.appimage.as_mut() {
            session.acknowledge_before_store()?;
        }
        Ok(())
    }
}

/// One shared exclusion gate for call joins and updater installation.
#[derive(Clone, Default)]
pub struct UpdateActivity(Arc<AtomicU8>);

pub struct CallLease(Arc<AtomicU8>);
pub(crate) struct InstallLease(Arc<AtomicU8>);

impl UpdateActivity {
    /// Lock-free read for output-only incoming sound suppression.
    pub(crate) fn idle(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }

    pub fn try_start_call(&self) -> Result<CallLease, UpdateError> {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|state| {
                if state == 1 {
                    UpdateError::CallActive
                } else {
                    UpdateError::UpdateActive
                }
            })?;
        Ok(CallLease(self.0.clone()))
    }

    pub(crate) fn try_install(&self) -> Result<InstallLease, UpdateError> {
        self.0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|state| {
                if state == 1 {
                    UpdateError::CallActive
                } else {
                    UpdateError::UpdateActive
                }
            })?;
        Ok(InstallLease(self.0.clone()))
    }
}

impl Drop for CallLease {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

impl Drop for InstallLease {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

#[cfg(test)]
#[path = "updater_tests.rs"]
mod tests;
