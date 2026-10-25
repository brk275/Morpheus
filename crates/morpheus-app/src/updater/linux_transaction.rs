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
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub(super) const FOOTER_MAGIC: &[u8] = b"\nMORPHEUS-APPIMAGE-METADATA-V1\n";
const JOURNAL_LIMIT: u64 = 16 * 1024;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(45);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(120);
const REQUEST_PREFIX: &[u8] = b"MORPHEUS-LINUX-HANDOFF-V2\n";
const SUCCESS_PREFIX: &[u8] = b"MORPHEUS-LINUX-CLEAN-V2\n";

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
            protocol: 2,
            product: "io.github.brk275.morpheus".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            arch: std::env::consts::ARCH.into(),
            persistence: env!("MORPHEUS_PERSISTENCE_COHORT").into(),
        }
    }

    fn compatible(&self, installed: &Self) -> bool {
        self.protocol == 2
            && installed.protocol == 2
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Journal {
    protocol: u8,
    id: String,
    phase: Phase,
    old_hash: String,
    new_hash: String,
    candidate: PackageMetadata,
    // Legacy journals remain readable for rollback/committed recovery only.
    // This numeric identity is checked solely against kernel socket credentials
    // before acquiring the nonreusable process capability; never poll/signal it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<Origin>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Origin {
    pid: u32,
    uid: u32,
}

fn transaction_digest(journal: &Journal) -> Result<String, UpdateError> {
    if journal.protocol != 2
        || journal.phase != Phase::Prepared
        || journal.candidate.protocol != 2
        || journal.origin.is_none()
    {
        return Err(UpdateError::RecoveryRequired);
    }
    serde_json::to_vec(journal)
        .map(|bytes| hash(&bytes))
        .map_err(|_| UpdateError::RecoveryRequired)
}

fn authority_frame(prefix: &[u8], digest: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(prefix.len() + digest.len());
    frame.extend_from_slice(prefix);
    frame.extend_from_slice(digest.as_bytes());
    frame
}

/// A private, single-use authority. Dropping it never authorizes installation.
/// It is retained with the original lifetime lock through ordinary shutdown.
struct ShutdownAuthority {
    channel: UnixStream,
    prepared: Journal,
}

/// A kernel process capability, not a reusable numeric PID or exit-status
/// receipt. With flags zero this becomes readable only after the entire
/// original thread group has exited. The original process is not our child.
struct ParentExitMonitor(OwnedFd);

fn close_on_exec(fd: libc::c_int) -> Result<(), UpdateError> {
    // SAFETY: the caller retains this live descriptor; fcntl has no pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

impl ParentExitMonitor {
    fn capture(channel: &UnixStream, origin: &Origin) -> Result<Self, UpdateError> {
        if !cfg!(target_arch = "x86_64") {
            return Err(UpdateError::UnsupportedPackage);
        }
        let fd = channel.as_raw_fd();
        for (option, expected) in [
            (libc::SO_TYPE, libc::SOCK_STREAM),
            (libc::SO_DOMAIN, libc::AF_UNIX),
        ] {
            let mut value: libc::c_int = 0;
            let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
            // SAFETY: each output has its exact initialized allocation size.
            if unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    (&mut value as *mut libc::c_int).cast(),
                    &mut len,
                )
            } != 0
                || len as usize != std::mem::size_of_val(&value)
                || value != expected
            {
                return Err(UpdateError::RecoveryRequired);
            }
        }
        // SAFETY: ucred is initialized and getsockopt receives the exact size.
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut len,
            )
        } != 0
            || len as usize != std::mem::size_of_val(&peer)
            || peer.pid <= 1
            || peer.uid != unsafe { libc::geteuid() }
            || peer.pid as u32 != origin.pid
            || peer.uid != origin.uid
        {
            return Err(UpdateError::RecoveryRequired);
        }
        // Linux 6.5 SO_PEERPIDFD: the kernel retains task_tgid of the process
        // that made socketpair, even through the AppImage runtime's exec/fork.
        // The package protocol admits only Linux x86_64, whose UAPI value is77.
        let mut pidfd: libc::c_int = -1;
        let mut len = std::mem::size_of_val(&pidfd) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                77,
                (&mut pidfd as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        let pidfd = if result == 0 {
            if pidfd < 0 {
                return Err(UpdateError::RecoveryRequired);
            }
            // SAFETY: successful SO_PEERPIDFD transfers a newly allocated fd.
            let owned = unsafe { OwnedFd::from_raw_fd(pidfd) };
            if len as usize != std::mem::size_of::<libc::c_int>() {
                return Err(UpdateError::RecoveryRequired);
            }
            owned
        } else {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOPROTOOPT) {
                return Err(UpdateError::RecoveryRequired);
            }
            // Older kernels may be used only when the socket creator remains
            // our exact direct parent across acquisition. Runtime wrappers or
            // reparenting fail closed; there is no numeric-PID polling fallback.
            if unsafe { libc::getppid() } != peer.pid {
                return Err(UpdateError::RecoveryRequired);
            }
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, peer.pid, 0_u32) };
            if raw < 0 {
                return Err(UpdateError::RecoveryRequired);
            }
            // SAFETY: successful pidfd_open transfers a new owned descriptor.
            let owned = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
            if unsafe { libc::getppid() } != peer.pid {
                return Err(UpdateError::RecoveryRequired);
            }
            owned
        };
        for fd in [fd, pidfd.as_raw_fd()] {
            close_on_exec(fd)?;
        }
        let monitor = Self(pidfd);
        if monitor.exited(Duration::ZERO)? {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(monitor)
    }

    fn exited(&self, wait: Duration) -> Result<bool, UpdateError> {
        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut descriptor = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let milliseconds = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
            // SAFETY: poll receives one live, writable descriptor structure.
            let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
            if result >= 0 {
                if descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    return Err(UpdateError::RecoveryRequired);
                }
                return Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0);
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(UpdateError::RecoveryRequired);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}

fn receive_exact_frame(
    channel: &mut UnixStream,
    frame: &[u8],
    eof: bool,
    deadline: Instant,
) -> Result<(), UpdateError> {
    let mut actual = vec![0; frame.len()];
    let mut offset = 0;
    while offset < actual.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(UpdateError::RecoveryRequired)?;
        channel
            .set_read_timeout(Some(remaining))
            .map_err(|_| UpdateError::RecoveryRequired)?;
        match channel.read(&mut actual[offset..]) {
            Ok(0) => return Err(UpdateError::RecoveryRequired),
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(UpdateError::RecoveryRequired),
        }
    }
    if actual != frame {
        return Err(UpdateError::RecoveryRequired);
    }
    if eof {
        let mut extra = [0_u8; 1];
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(UpdateError::RecoveryRequired)?;
            channel
                .set_read_timeout(Some(remaining))
                .map_err(|_| UpdateError::RecoveryRequired)?;
            match channel.read(&mut extra) {
                Ok(0) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                _ => return Err(UpdateError::RecoveryRequired),
            }
        }
    }
    Ok(())
}

fn await_original_shutdown(
    channel: &mut UnixStream,
    parent: &ParentExitMonitor,
    digest: &str,
    deadline: Instant,
) -> Result<(), UpdateError> {
    receive_exact_frame(
        channel,
        &authority_frame(SUCCESS_PREFIX, digest),
        true,
        deadline,
    )?;
    if !parent.exited(deadline.saturating_duration_since(Instant::now()))? {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

/// Held for the entire app lifetime, including shutdown of Matrix/call tasks.
/// A second process cannot open stores through this installed AppImage.
pub struct AppImageSession {
    installed: PathBuf,
    state: PathBuf,
    _lock: File,
    trial: Option<String>,
    install_lease: Option<InstallLease>,
    shutdown_authority: Option<ShutdownAuthority>,
}

impl Drop for AppImageSession {
    fn drop(&mut self) {
        // CLOEXEC acts at exec, so an unrelated concurrent spawn can still
        // inherit this locked open-file description between fork and exec.
        // Retire the original session's lock at its existing Drop boundary,
        // rather than waiting for every such descriptor alias to close.
        // Failure leaves File's ordinary close as the fail-closed fallback;
        // releasing a lock is never a shutdown or update-success receipt.
        let _ = self._lock.unlock();
    }
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
            shutdown_authority: None,
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
        if !matches!(journal.protocol, 1 | 2)
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
            protocol: 2,
            id,
            phase: Phase::Prepared,
            old_hash: hash(&old),
            new_hash: hash(bytes),
            candidate,
            origin: Some(Origin {
                pid: std::process::id(),
                uid: unsafe { libc::geteuid() },
            }),
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

    fn validate_prepared(&self, prepared: &Journal) -> Result<(), UpdateError> {
        if self.journal()?.as_ref() != Some(prepared) {
            return Err(UpdateError::RecoveryRequired);
        }
        validate_prepared_files(&self.installed, &self.state, prepared)
    }

    pub(super) fn shutdown_pending(&self) -> bool {
        self.shutdown_authority.is_some()
    }

    /// Called only after the complete ordinary app scope returned success and
    /// its UI/runtime/owner locals have dropped. Never called by Drop. The
    /// caller must have recovered unique ownership of StartupSession first.
    pub(super) fn complete_successful_shutdown(mut self) -> Result<(), UpdateError> {
        let Some(mut authority) = self.shutdown_authority.take() else {
            return Ok(());
        };
        self.validate_prepared(&authority.prepared)?;
        if authority.prepared.origin.as_ref()
            != Some(&Origin {
                pid: std::process::id(),
                uid: unsafe { libc::geteuid() },
            })
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let digest = transaction_digest(&authority.prepared)?;
        // Last fallible operation. A failed/partial write cannot produce the
        // complete terminal frame. Drop merely closes the socket, including on
        // error; EOF alone is never authority. There is no success-on-Drop.
        authority
            .channel
            .write_all(&authority_frame(SUCCESS_PREFIX, &digest))
            .map_err(|_| UpdateError::RecoveryRequired)
    }

    fn apply(&self, journal: &mut Journal) -> Result<(), UpdateError> {
        transaction_digest(journal)?;
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

fn validate_prepared_files(
    installed: &Path,
    state: &Path,
    prepared: &Journal,
) -> Result<(), UpdateError> {
    transaction_digest(prepared)?;
    if hash_file(installed)? != prepared.old_hash
        || hash_file(&state.join("previous.AppImage"))? != prepared.old_hash
        || hash_file(&state.join("next.AppImage"))? != prepared.new_hash
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
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
        let prepared = self
            .session
            .journal()?
            .ok_or(UpdateError::RecoveryRequired)?;
        self.session.validate_prepared(&prepared)?;
        let digest = transaction_digest(&prepared)?;
        let (mut parent, helper) = UnixStream::pair().map_err(|_| UpdateError::RestartFailed)?;
        // Explicitly enforce inheritance rather than depending on std's socket
        // defaults. Stdio installs only the child endpoint at fd0 for exec.
        close_on_exec(parent.as_raw_fd())?;
        close_on_exec(helper.as_raw_fd())?;
        parent
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(|_| UpdateError::RestartFailed)?;
        parent
            .write_all(&authority_frame(REQUEST_PREFIX, &digest))
            .map_err(|_| UpdateError::RestartFailed)?;
        let mut command = image_command(&self.session.state.join("previous.AppImage"));
        command
            .arg("--morpheus-update-supervise")
            .arg(&self.session.installed)
            .arg(&self.id)
            .stdin(Stdio::from(OwnedFd::from(helper)));
        let mut child = OwnedChild::spawn(&mut command)?;
        // Command retains its stdio configuration after spawn. Close that
        // child-end alias in the parent before any readiness observation.
        drop(command);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child.exited()? {
                child.stop()?;
                return Err(UpdateError::RestartFailed);
            }
            if bounded_read(&self.session.state.join("supervisor-ready"), 64)
                .is_ok_and(|bytes| bytes == digest.as_bytes())
            {
                // The helper owns the original process capability and awaits
                // our one-shot successful-scope authority, then actual death.
                // Retain call exclusion through all subsequent UI shutdown.
                self.session.install_lease = self.lease.take();
                self.session.shutdown_authority = Some(ShutdownAuthority {
                    channel: parent,
                    prepared,
                });
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
    let digest = transaction_digest(&journal)?;
    let origin = journal
        .origin
        .as_ref()
        .ok_or(UpdateError::RecoveryRequired)?;
    // Duplicating stdin keeps RAII ownership explicit. Only an inherited
    // connected private AF_UNIX stream with the original creator is admitted.
    let mut channel = UnixStream::from(
        std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| UpdateError::RecoveryRequired)?,
    );
    let parent = ParentExitMonitor::capture(&channel, origin)?;
    // The cloned RAII channel and original inherited stdin are distinct fd
    // aliases. Neither may leak through any later trial/recovery exec.
    close_on_exec(libc::STDIN_FILENO)?;
    receive_exact_frame(
        &mut channel,
        &authority_frame(REQUEST_PREFIX, &digest),
        false,
        Instant::now() + Duration::from_secs(15),
    )?;
    validate_prepared_files(&installed, &state, &journal)?;
    let mut ready =
        tempfile::NamedTempFile::new_in(&state).map_err(|_| UpdateError::RecoveryRequired)?;
    ready
        .write_all(digest.as_bytes())
        .and_then(|_| ready.as_file().sync_all())
        .map_err(|_| UpdateError::RecoveryRequired)?;
    // The old app must never see "ready" until helper setup and the write have
    // both succeeded. A partially written readiness file is not a handshake.
    ready
        .persist_noclobber(state.join("supervisor-ready"))
        .map_err(|_| UpdateError::RecoveryRequired)?;
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    await_original_shutdown(&mut channel, &parent, &digest, deadline)?;
    // These descriptors never reach a trial/recovery process. CLOEXEC also
    // covers the original stdin alias retained by the AppImage runtime.
    drop((channel, parent));
    let session = AppImageSession::lock(&installed, Duration::from_secs(120))?;
    session.validate_prepared(&journal)?;
    let mut journal = journal;
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
    dispatch_args(&args)
}

fn dispatch_args(args: &[std::ffi::OsString]) -> Result<bool, UpdateError> {
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--morpheus-update-metadata") => {
            if args.len() != 2 {
                return Err(UpdateError::RecoveryRequired);
            }
            println!(
                "{}",
                serde_json::to_string(&PackageMetadata::embedded())
                    .map_err(|_| UpdateError::InvalidConfiguration)?
            );
            Ok(true)
        }
        Some("--morpheus-update-supervise") => {
            if args.len() != 4 {
                return Err(UpdateError::RecoveryRequired);
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
