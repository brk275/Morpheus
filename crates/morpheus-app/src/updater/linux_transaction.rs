//! Same-filesystem AppImage replacement. Only application bytes and this owned
//! sidecar directory are ever touched; account databases are not arguments.
//!
//! A committed health acknowledgement is deliberately BEFORE opening stores.
//! Once committed, even a later crash must not roll back to old database code.

use super::{InstallLease, MAX_UPDATE_BYTES, UpdateError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub(super) const FOOTER_MAGIC: &[u8] = b"\nMORPHEUS-APPIMAGE-METADATA-V1\n";
const JOURNAL_LIMIT: u64 = 16 * 1024;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct PackageMetadata {
    protocol: u8,
    product: String,
    version: String,
    arch: String,
    persistence: String,
}

impl PackageMetadata {
    pub(super) fn embedded() -> Self {
        Self {
            protocol: 1,
            product: "io.github.brk275.morpheus".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            arch: std::env::consts::ARCH.into(),
            persistence: env!("MORPHEUS_PERSISTENCE_COHORT").into(),
        }
    }

    fn compatible(&self, installed: &Self) -> bool {
        self.protocol == 1
            && self.product == installed.product
            && self.arch == installed.arch
            && self.persistence == installed.persistence
    }
}

fn package_metadata(bytes: &[u8]) -> Result<PackageMetadata, UpdateError> {
    // Type 2 AppImage ELF runtime, little-endian x86_64. Other Linux targets
    // require their own package/installed-device acceptance before enabling.
    if bytes.len() < 64
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || &bytes[8..11] != b"AI\x02"
        || u16::from_le_bytes([bytes[18], bytes[19]]) != 62
        || !bytes.ends_with(FOOTER_MAGIC)
    {
        return Err(UpdateError::UnsupportedPackage);
    }
    let end = bytes
        .len()
        .checked_sub(FOOTER_MAGIC.len() + 4)
        .ok_or(UpdateError::CorruptStage)?;
    let len = u32::from_le_bytes(bytes[end..end + 4].try_into().unwrap()) as usize;
    if len == 0 || len > 4096 || end < len {
        return Err(UpdateError::CorruptStage);
    }
    serde_json::from_slice(&bytes[end - len..end]).map_err(|_| UpdateError::CorruptStage)
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

/// Held for the entire app lifetime, including shutdown of Matrix/call tasks.
/// A second process cannot open stores through this installed AppImage.
pub struct AppImageSession {
    installed: PathBuf,
    state: PathBuf,
    _lock: File,
    trial: Option<String>,
    install_lease: Option<InstallLease>,
}

fn private_directory(path: &Path) -> Result<(), UpdateError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_directory(path.parent().ok_or(UpdateError::RecoveryRequired)?)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(UpdateError::NotWritable),
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| UpdateError::RecoveryRequired)?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

fn open_file(path: &Path) -> Result<File, UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| UpdateError::RecoveryRequired)?;
    let metadata = file.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
    // No symlinks, devices, foreign-owned files, or writable shared payloads.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
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

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_file(path: &Path) -> Result<String, UpdateError> {
    let mut reader = open_file(path)?.take(MAX_UPDATE_BYTES + 1);
    let mut digest = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > MAX_UPDATE_BYTES {
            return Err(UpdateError::RecoveryRequired);
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sync_directory(path: &Path) -> Result<(), UpdateError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| UpdateError::RecoveryRequired)
}

impl AppImageSession {
    fn paths(installed: &Path) -> Result<(PathBuf, PathBuf), UpdateError> {
        if !installed.is_absolute() {
            return Err(UpdateError::UnsupportedPackage);
        }
        let parent = installed
            .parent()
            .ok_or(UpdateError::UnsupportedPackage)?
            .canonicalize()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let metadata = fs::metadata(&parent).map_err(|_| UpdateError::RecoveryRequired)?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(UpdateError::NotWritable);
        }
        let name = installed
            .file_name()
            .ok_or(UpdateError::UnsupportedPackage)?;
        let installed = parent.join(name);
        open_file(&installed)?;
        let mut sidecar = std::ffi::OsString::from(".");
        sidecar.push(name);
        sidecar.push(".morpheus-update");
        let state = parent.join(sidecar);
        private_directory(&state)?;
        Ok((installed, state))
    }

    fn lock(installed: &Path, wait: Duration) -> Result<Self, UpdateError> {
        let (installed, state) = Self::paths(installed)?;
        let lock_path = state.join("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let metadata = lock.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let deadline = Instant::now() + wait;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
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
        let valid_hash =
            |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
        if journal.protocol != 1
            || journal.id.is_empty()
            || journal.id.len() > 64
            || !journal.id.bytes().all(|b| b.is_ascii_alphanumeric())
            || !valid_hash(&journal.old_hash)
            || !valid_hash(&journal.new_hash)
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
        temp.persist(self.state.join("journal.json"))
            .map_err(|_| UpdateError::RecoveryRequired)?;
        sync_directory(&self.state)
    }

    fn clean(&self) -> Result<(), UpdateError> {
        // Never recursively delete a path; these are the only owned entries.
        for name in [
            "supervisor-ready",
            "next.AppImage",
            "previous.AppImage",
            "journal.json",
        ] {
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
        let candidate = package_metadata(bytes)?;
        let embedded = PackageMetadata::embedded();
        if !candidate.compatible(&embedded) || candidate.version != version {
            return Err(UpdateError::IncompatibleUpdate);
        }
        let current = cargo_packager_updater::semver::Version::parse(&embedded.version).unwrap();
        let next = cargo_packager_updater::semver::Version::parse(version)
            .map_err(|_| UpdateError::NotNewer)?;
        if next.cmp_precedence(&current) != std::cmp::Ordering::Greater {
            return Err(UpdateError::NotNewer);
        }
        // Copy rather than hardlink: a later in-place modification of the
        // installed inode must not silently modify the rollback image too.
        let old = bounded_read(&self.installed, MAX_UPDATE_BYTES)?;
        // Old image must support this protocol; never execute a legacy app as
        // supervisor. Packaging places metadata after the ELF/SquashFS image.
        if package_metadata(&old)? != embedded {
            return Err(UpdateError::IncompatibleUpdate);
        }
        self.clean()?;
        self.write_payload("previous.AppImage", &old)?;
        self.write_payload("next.AppImage", bytes)?;
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
            old_hash: hash(&old),
            new_hash: hash(bytes),
            candidate,
        })
    }

    fn write_payload(&self, name: &str, bytes: &[u8]) -> Result<(), UpdateError> {
        let path = self.state.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| UpdateError::StageFailed)?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| UpdateError::StageFailed)?;
        sync_directory(&self.state)
    }

    fn apply(&self, journal: &mut Journal) -> Result<(), UpdateError> {
        if journal.phase != Phase::Prepared
            || hash_file(&self.installed)? != journal.old_hash
            || hash_file(&self.state.join("previous.AppImage"))? != journal.old_hash
            || hash_file(&self.state.join("next.AppImage"))? != journal.new_hash
        {
            return Err(UpdateError::RecoveryRequired);
        }
        journal.phase = Phase::ApplyIntent;
        self.save(journal)?;
        // Atomic replacement: the public path is never absent. Both directories
        // are on the same filesystem, and both are fsynced after the rename.
        fs::rename(self.state.join("next.AppImage"), &self.installed)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        sync_directory(&self.state)?;
        sync_directory(self.installed.parent().unwrap())?;
        journal.phase = Phase::AwaitingHealth;
        self.save(journal)
    }

    fn rollback(&self, journal: &mut Journal) -> Result<(), UpdateError> {
        if journal.phase == Phase::Committed {
            return Err(UpdateError::RecoveryRequired);
        }
        let actual = hash_file(&self.installed)?;
        if actual == journal.old_hash {
            return self.clean();
        }
        // An unrelated replacement is never overwritten, even if recovery is
        // requested. Keep the known old image for explicit manual recovery.
        if actual != journal.new_hash
            || hash_file(&self.state.join("previous.AppImage"))? != journal.old_hash
        {
            return Err(UpdateError::RecoveryRequired);
        }
        journal.phase = Phase::RollbackIntent;
        self.save(journal)?;
        fs::rename(self.state.join("previous.AppImage"), &self.installed)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        sync_directory(&self.state)?;
        sync_directory(self.installed.parent().unwrap())?;
        self.clean()
    }

    /// Commit BEFORE any account store, configuration migration or vault open.
    pub fn acknowledge_before_store(&mut self) -> Result<(), UpdateError> {
        self.acknowledge(&PackageMetadata::embedded())
    }

    fn acknowledge(&mut self, running: &PackageMetadata) -> Result<(), UpdateError> {
        let Some(id) = self.trial.take() else {
            return Ok(());
        };
        let mut journal = self.journal()?.ok_or(UpdateError::RecoveryRequired)?;
        if journal.phase != Phase::AwaitingHealth
            || journal.id != id
            || &journal.candidate != running
            || hash_file(&self.installed)? != journal.new_hash
        {
            return Err(UpdateError::RecoveryRequired);
        }
        journal.phase = Phase::Committed;
        self.save(&journal)?;
        // Keep marker and backup until the supervisor observes the commit.
        // The marker also prevents rollback after a power loss at this point.
        Ok(())
    }
}

#[must_use = "Explicitly hand off the restart; dropping this handle cancels preparation."]
pub struct PreparedRestart<'a> {
    session: &'a mut AppImageSession,
    id: String,
    lease: Option<InstallLease>,
    handed_off: bool,
}

impl PreparedRestart<'_> {
    /// On success, close the UI and finish all workers before dropping the
    /// startup session. Failure leaves the current app running and untouched.
    pub fn launch_supervisor(mut self) -> Result<(), UpdateError> {
        let mut child = OwnedChild::spawn(
            image_command(&self.session.state.join("previous.AppImage"))
                .arg("--morpheus-update-supervise")
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
                // The helper is now live and waiting for the old app's lock.
                // Retain call exclusion through all subsequent UI shutdown.
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

    pub fn cancel(mut self) -> Result<(), UpdateError> {
        self.cancel_inner()?;
        self.handed_off = true; // Cancellation complete; Drop need not repeat it.
        Ok(())
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

impl Drop for PreparedRestart<'_> {
    fn drop(&mut self) {
        if !self.handed_off {
            // Preparation has never replaced the installation. A cleanup I/O
            // failure leaves a recoverable Prepared journal for next startup.
            let _ = self.cancel_inner();
        }
    }
}

pub(super) fn prepare_restart<'a>(
    session: &'a mut AppImageSession,
    bytes: &[u8],
    version: &str,
    lease: InstallLease,
) -> Result<PreparedRestart<'a>, UpdateError> {
    session.prepare(bytes, version)?;
    let journal = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    Ok(PreparedRestart {
        session,
        id: journal.id,
        lease: Some(lease),
        handed_off: false,
    })
}

fn image_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // Do not carry the old AppImage's mount/loader paths into the new runtime.
    for name in [
        "APPIMAGE",
        "APPDIR",
        "ARGV0",
        "OWD",
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
    ] {
        command.env_remove(name);
    }
    command
}

/// Retain the unreaped leader until group cleanup or an explicit handoff.
/// A zombie leader still reserves its PID, preventing a recycled process group
/// from receiving our cleanup signal. Child::try_wait must not be used here.
struct OwnedChild {
    child: Option<Child>,
}

impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self, UpdateError> {
        let child = command
            .process_group(0)
            .spawn()
            .map_err(|_| UpdateError::RestartFailed)?;
        Ok(Self { child: Some(child) })
    }

    fn exited(&self) -> Result<bool, UpdateError> {
        let child = self.child.as_ref().ok_or(UpdateError::RecoveryRequired)?;
        let pid = i32::try_from(child.id()).map_err(|_| UpdateError::RecoveryRequired)?;
        loop {
            // SAFETY: zero is a valid initial siginfo_t representation; waitid
            // receives a writable correctly aligned object and this owned PID.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                // SAFETY: successful waitid initializes siginfo_t. With
                // WNOHANG, si_pid stays zero if there is no exit to report.
                return match unsafe { info.si_pid() } {
                    0 => Ok(false),
                    actual if actual == pid => Ok(true),
                    _ => Err(UpdateError::RecoveryRequired),
                };
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                // In particular, ECHILD means ownership was lost. Do not
                // signal that PID/PGID: it may already have been recycled.
                return Err(UpdateError::RecoveryRequired);
            }
        }
    }

    fn stop(&mut self) -> Result<(), UpdateError> {
        let Some(child) = self.child.as_ref() else {
            return Ok(());
        };
        let pid = i32::try_from(child.id()).map_err(|_| UpdateError::RecoveryRequired)?;
        self.exited()?; // Prove the leader is still our unreaped child.
        loop {
            // SAFETY: spawn created a group with this unreaped leader's PID.
            // Its PID cannot be reused until our later wait reaps the leader.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(UpdateError::RecoveryRequired);
            }
        }
        // This is the only reap. Removing ownership first prevents Drop from
        // signaling a possibly recycled group if wait itself reports an error.
        self.child
            .take()
            .unwrap()
            .wait()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        Ok(())
    }

    /// Transfer a ready helper or committed app to independent execution.
    /// Dropping std::process::Child does not signal or reap that process.
    fn handoff(mut self) -> Child {
        self.child.take().unwrap()
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Covers every post-spawn error, including malformed/unreadable
        // journals and failed status observation. Lost ownership fails closed.
        let _ = self.stop();
    }
}

fn supervise(installed: &Path, id: &str) -> Result<(), UpdateError> {
    let (installed, state) = AppImageSession::paths(installed)?;
    let journal = AppImageSession::read_journal(&state)?.ok_or(UpdateError::RecoveryRequired)?;
    // A CLI flag cannot turn an arbitrary binary/path into an update helper.
    let running = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .ok_or(UpdateError::UnsupportedPackage)?;
    if running
        .canonicalize()
        .map_err(|_| UpdateError::RecoveryRequired)?
        != state
            .join("previous.AppImage")
            .canonicalize()
            .map_err(|_| UpdateError::RecoveryRequired)?
        || journal.id != id
        || journal.phase != Phase::Prepared
        || !journal.candidate.compatible(&PackageMetadata::embedded())
    {
        return Err(UpdateError::RecoveryRequired);
    }
    let mut ready =
        tempfile::NamedTempFile::new_in(&state).map_err(|_| UpdateError::RecoveryRequired)?;
    ready
        .write_all(id.as_bytes())
        .and_then(|_| ready.as_file().sync_all())
        .map_err(|_| UpdateError::RecoveryRequired)?;
    // The old app must never see "ready" until helper setup and the write have
    // both succeeded. A partially written readiness file is not a handshake.
    ready
        .persist_noclobber(state.join("supervisor-ready"))
        .map_err(|_| UpdateError::RecoveryRequired)?;
    let session = AppImageSession::lock(&installed, Duration::from_secs(120))?;
    let mut journal = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    if journal.id != id {
        return Err(UpdateError::RecoveryRequired);
    }
    session.apply(&mut journal)?;
    let installed = session.installed.clone();
    let state = session.state.clone();
    // Trial process needs the same lifetime lock; the supervisor must not hold
    // it while the child initializes or while its stores are open.
    drop(session);
    let mut child = match OwnedChild::spawn(
        image_command(&installed)
            .arg("--morpheus-update-trial")
            .arg(id),
    ) {
        Ok(child) => child,
        Err(_) => {
            let session = AppImageSession::lock(&installed, Duration::ZERO)?;
            session.rollback(&mut journal)?;
            drop(session);
            image_command(&installed)
                .spawn()
                .map_err(|_| UpdateError::RestartFailed)?;
            return Err(UpdateError::RestartFailed);
        }
    };
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        // Read-only observation; the child writes by atomic rename. Cleanup is
        // deferred until a later startup because the healthy child holds lock.
        let observed: Journal =
            serde_json::from_slice(&bounded_read(&state.join("journal.json"), JOURNAL_LIMIT)?)
                .map_err(|_| UpdateError::RecoveryRequired)?;
        if observed.id != id {
            return Err(UpdateError::RecoveryRequired);
        }
        if observed.phase == Phase::Committed {
            drop(child.handoff());
            return Ok(());
        }
        let exited = child.exited()?;
        if exited || Instant::now() >= deadline {
            // A final commit can race the timeout. Stop and obtain the lifetime
            // lock, then reread the durable marker before deciding to rollback.
            // Even an exited wrapper can leave a descendant with the lifetime
            // lock. Kill the group before reaping that wrapper's reserved PID.
            child.stop()?;
            let session = AppImageSession::lock(&installed, Duration::from_secs(5))?;
            let mut latest = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
            if latest.phase == Phase::Committed {
                drop(session);
                // We may have stopped a just-acknowledged child at the exact
                // deadline. Restart the committed version, never the backup.
                image_command(&installed)
                    .spawn()
                    .map_err(|_| UpdateError::RestartFailed)?;
                return Ok(());
            }
            session.rollback(&mut latest)?;
            drop(session);
            image_command(&installed)
                .spawn()
                .map_err(|_| UpdateError::RestartFailed)?;
            return Err(UpdateError::RestartFailed);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs before Slint and all storage. Returns true for a handled helper mode.
pub(super) fn early_dispatch() -> Result<bool, UpdateError> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--morpheus-update-metadata") if args.len() == 2 => {
            println!(
                "{}",
                serde_json::to_string(&PackageMetadata::embedded())
                    .map_err(|_| UpdateError::InvalidConfiguration)?
            );
            Ok(true)
        }
        Some("--morpheus-update-supervise") if args.len() == 4 => {
            supervise(
                Path::new(&args[2]),
                args[3].to_str().ok_or(UpdateError::RecoveryRequired)?,
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Begin a packaged app's lifetime. Unmanaged/package-manager binaries have no
/// automatic updater session. A recovery relaunch returns `restarted = true`.
pub(super) fn begin_startup() -> Result<(Option<AppImageSession>, bool), UpdateError> {
    let Some(installed) = std::env::var_os("APPIMAGE").map(PathBuf::from) else {
        return Ok((None, false));
    };
    let mut session = match AppImageSession::lock(&installed, Duration::ZERO) {
        Ok(session) => session,
        Err(error) => {
            // Root-owned/read-only/shared AppImages can still run. If a journal
            // exists, however, its interrupted transaction must be resolved
            // before opening stores, so never silently bypass that failure.
            let sidecar = installed.file_name().and_then(|name| {
                installed.parent().map(|parent| {
                    let mut path = std::ffi::OsString::from(".");
                    path.push(name);
                    path.push(".morpheus-update");
                    parent.join(path).join("journal.json")
                })
            });
            if !matches!(error, UpdateError::UpdateActive)
                && sidecar.is_some_and(|path| {
                    fs::symlink_metadata(path)
                        .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
                })
            {
                return Ok((None, false));
            }
            return Err(error);
        }
    };
    let args: Vec<_> = std::env::args_os().collect();
    let Some(mut journal) = session.journal()? else {
        if args
            .get(1)
            .is_some_and(|arg| arg == "--morpheus-update-trial")
        {
            return Err(UpdateError::RecoveryRequired);
        }
        return Ok((Some(session), false));
    };
    let current_hash = hash_file(&session.installed)?;
    let trial = args.len() == 3
        && args[1] == "--morpheus-update-trial"
        && args[2].to_str() == Some(journal.id.as_str());
    if journal.phase == Phase::Committed {
        if current_hash != journal.new_hash {
            return Err(UpdateError::RecoveryRequired);
        }
        session.clean()?;
    } else if trial
        && journal.phase == Phase::AwaitingHealth
        && current_hash == journal.new_hash
        && journal.candidate == PackageMetadata::embedded()
    {
        session.trial = Some(journal.id);
    } else {
        let replaced = current_hash != journal.old_hash;
        session.rollback(&mut journal)?;
        if replaced {
            let installed = session.installed.clone();
            drop(session);
            image_command(&installed)
                .spawn()
                .map_err(|_| UpdateError::RestartFailed)?;
            return Ok((None, true));
        }
    }
    Ok((Some(session), false))
}

#[cfg(test)]
#[path = "linux_transaction_tests.rs"]
mod tests;
