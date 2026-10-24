//! Fixed-ledger, per-user Windows updates. NSIS is used only for initial/manual
//! installation. A signed data envelope replaces the complete owned app payload;
//! the source-bound original uninstaller and registration are retained/recovered.
//!
//! No account/configuration path is accepted. The old process must exit before
//! replacement; the app commits health before opening stores. Activation stays
//! closed until fresh native installed-package/interrupt/uninstall acceptance.

use super::{InstallLease, MAX_UPDATE_BYTES, UpdateError};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(morpheus_windows_update_acceptance)]
pub(crate) mod acceptance;
mod native;
mod payload;
use payload::{
    Entry, FILES, Identity, MARKER, MARKER_LIMIT, Manifest, Payload, UNINSTALLER, hash,
    pe_identity, valid_hash,
};

pub(super) const INSTALLATION_VALIDATED: bool = false;
const JOURNAL_LIMIT: u64 = 64 * 1024;
const HELPER: &str = "supervisor.exe";
const RECOVERY_REQUEST: &str = "recovery.json";
const RECOVERY_READY: &str = "recovery-ready";
const RECOVERY_LIMIT: u64 = 4096;

#[used]
#[unsafe(link_section = ".mrphupd")]
static EMBEDDED_IDENTITY: [u8; include_bytes!(concat!(
    env!("OUT_DIR"),
    "/morpheus-update-identity.json"
))
.len()] = *include_bytes!(concat!(env!("OUT_DIR"), "/morpheus-update-identity.json"));

fn embedded_identity() -> Result<Identity, UpdateError> {
    let identity: Identity = serde_json::from_slice(&EMBEDDED_IDENTITY)
        .map_err(|_| UpdateError::InvalidConfiguration)?;
    if !identity.valid()
        || identity.version != env!("CARGO_PKG_VERSION")
        || identity.persistence != env!("MORPHEUS_PERSISTENCE_COHORT")
    {
        return Err(UpdateError::InvalidConfiguration);
    }
    Ok(identity)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Phase {
    Prepared,
    ApplyIntent,
    AwaitingHealth,
    Committed,
    RollbackIntent,
    RolledBack,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u8,
    id: String,
    phase: Phase,
    old: Manifest,
    new: Manifest,
    old_marker: String,
    new_marker: String,
    old_marker_bytes: u64,
    new_marker_bytes: u64,
    uninstaller: Entry,
    old_registration: native::Registration,
    new_registration: native::Registration,
    parent_id: u32,
    parent_stamp: u64,
    helper_hash: String,
}

impl Journal {
    fn same_transaction(&self, other: &Self) -> bool {
        let mut normalized = self.clone();
        normalized.phase = other.phase;
        normalized == *other
    }
    fn validate(&self) -> Result<(), UpdateError> {
        if self.schema != 1
            || !valid_id(&self.id)
            || self.parent_id == 0
            || self.parent_stamp == 0
            || !valid_hash(&self.old_marker)
            || !valid_hash(&self.new_marker)
            || !(1..=MARKER_LIMIT as u64).contains(&self.old_marker_bytes)
            || !(1..=MARKER_LIMIT as u64).contains(&self.new_marker_bytes)
            || !valid_hash(&self.helper_hash)
            || self.uninstaller.path != UNINSTALLER
            || self.uninstaller.bytes == 0
            || self.uninstaller.bytes > 16 * 1024 * 1024
            || !valid_hash(&self.uninstaller.sha256)
            || !self.new.identity.compatible(&self.old.identity)
            || self.old_registration.version != self.old.identity.version
            || self.new_registration.version != self.new.identity.version
        {
            return Err(UpdateError::RecoveryRequired);
        }
        for manifest in [&self.old, &self.new] {
            Manifest::parse(
                &serde_json::to_vec(manifest).map_err(|_| UpdateError::RecoveryRequired)?,
            )?;
        }
        let old = cargo_packager_updater::semver::Version::parse(&self.old.identity.version)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        let new = cargo_packager_updater::semver::Version::parse(&self.new.identity.version)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        if new.cmp_precedence(&old) != std::cmp::Ordering::Greater {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RecoveryRequest {
    schema: u8,
    nonce: String,
    phase: Phase,
    journal_hash: String,
    parent_id: u32,
    parent_stamp: u64,
}
impl RecoveryRequest {
    fn parse(bytes: &[u8], journal: &Journal) -> Result<Self, UpdateError> {
        if bytes.len() as u64 > RECOVERY_LIMIT {
            return Err(UpdateError::RecoveryRequired);
        }
        let request: Self =
            serde_json::from_slice(bytes).map_err(|_| UpdateError::RecoveryRequired)?;
        let mut snapshot = journal.clone();
        snapshot.phase = request.phase;
        if request.schema != 1
            || !valid_id(&request.nonce)
            || !needs_copied_recovery(request.phase)
            || request.parent_id == 0
            || request.parent_stamp == 0
            || request.journal_hash
                != hash(&serde_json::to_vec(&snapshot).map_err(|_| UpdateError::RecoveryRequired)?)
        {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(request)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RecoveryReady {
    schema: u8,
    request_hash: String,
    helper_id: u32,
    helper_stamp: u64,
}
impl RecoveryReady {
    fn parse(bytes: &[u8], request: &[u8]) -> Result<Self, UpdateError> {
        if bytes.len() as u64 > RECOVERY_LIMIT {
            return Err(UpdateError::RecoveryRequired);
        }
        let ready: Self =
            serde_json::from_slice(bytes).map_err(|_| UpdateError::RecoveryRequired)?;
        if ready.schema != 1
            || ready.request_hash != hash(request)
            || ready.helper_id == 0
            || ready.helper_stamp == 0
        {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(ready)
    }
}
fn needs_copied_recovery(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::ApplyIntent | Phase::AwaitingHealth | Phase::RollbackIntent
    )
}
fn optional_private(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>, UpdateError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(UpdateError::RecoveryRequired),
        Ok(_) => native::read(path, maximum).map(Some),
    }
}

fn valid_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn all_paths() -> impl Iterator<Item = &'static str> {
    FILES.into_iter().chain([MARKER, UNINSTALLER])
}

fn enumerate(root: &Path, complete: bool) -> Result<Vec<String>, UpdateError> {
    fn visit(
        root: &Path,
        current: &Path,
        depth: usize,
        result: &mut Vec<String>,
    ) -> Result<(), UpdateError> {
        if depth > 2 {
            return Err(UpdateError::UnsupportedPackage);
        }
        let _directory = native::Directory::open(current)?;
        for item in fs::read_dir(current).map_err(|_| UpdateError::RecoveryRequired)? {
            let item = item.map_err(|_| UpdateError::RecoveryRequired)?;
            let path = item.path();
            let meta = fs::symlink_metadata(&path).map_err(|_| UpdateError::RecoveryRequired)?;
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(UpdateError::UnsupportedPackage);
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| UpdateError::RecoveryRequired)?;
            let name = relative
                .to_str()
                .ok_or(UpdateError::UnsupportedPackage)?
                .replace('\\', "/");
            if meta.is_dir() {
                if !["licenses", "themes"].contains(&name.as_str()) {
                    return Err(UpdateError::UnsupportedPackage);
                }
                visit(root, &path, depth + 1, result)?;
            } else if meta.is_file() {
                if !all_paths().any(|expected| expected == name) {
                    return Err(UpdateError::UnsupportedPackage);
                }
                native::open(&path)?;
                result.push(name);
            } else {
                return Err(UpdateError::UnsupportedPackage);
            }
            if result.len() > all_paths().count() {
                return Err(UpdateError::UnsupportedPackage);
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    visit(root, root, 0, &mut found)?;
    found.sort();
    let mut expected = all_paths().map(str::to_owned).collect::<Vec<_>>();
    expected.sort();
    if complete && found != expected {
        return Err(UpdateError::UnsupportedPackage);
    }
    Ok(found)
}

fn check_entry(root: &Path, entry: &Entry) -> Result<(), UpdateError> {
    let bytes = native::read(&root.join(&entry.path), entry.bytes)?;
    if bytes.len() as u64 != entry.bytes || hash(&bytes) != entry.sha256 {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

fn check_tree(
    root: &Path,
    manifest: &Manifest,
    marker_hash: &str,
    uninstaller: &Entry,
) -> Result<(), UpdateError> {
    enumerate(root, true)?;
    let marker = native::read(&root.join(MARKER), MARKER_LIMIT as u64)?;
    if hash(&marker) != marker_hash || Manifest::parse(&marker)? != *manifest {
        return Err(UpdateError::RecoveryRequired);
    }
    for entry in &manifest.files {
        check_entry(root, entry)?;
    }
    check_entry(root, uninstaller)?;
    let executable = native::read(&root.join("morpheus-app.exe"), MAX_UPDATE_BYTES)?;
    if pe_identity(&executable)? != manifest.identity {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}

/// Held from before stores open until every Matrix/call worker has shut down.
pub struct WindowsSession {
    root: native::Directory,
    state: native::Directory,
    _lock: File,
    registry: native::Registry,
    identity: Identity,
    trial: Option<String>,
    install_lease: Option<InstallLease>,
}

impl WindowsSession {
    fn lock(root: &Path, identity: Identity) -> Result<Self, UpdateError> {
        let root = native::Directory::open(root)?;
        let mut state = root.path.as_os_str().to_os_string();
        state.push(".morpheus-update");
        let state = native::create_directory(Path::new(&state))?;
        let lock = native::lock(&state.path.join("lock"))?;
        let registry = native::Registry::open()?;
        Ok(Self {
            root,
            state,
            _lock: lock,
            registry,
            identity,
            trial: None,
            install_lease: None,
        })
    }
    fn journal(&self) -> Result<Option<Journal>, UpdateError> {
        let path = self.state.path.join("journal.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(UpdateError::RecoveryRequired),
            Ok(_) => {
                let journal: Journal = serde_json::from_slice(&native::read(&path, JOURNAL_LIMIT)?)
                    .map_err(|_| UpdateError::RecoveryRequired)?;
                journal.validate()?;
                Ok(Some(journal))
            }
        }
    }
    fn write_journal(&self, journal: &Journal) -> Result<(), UpdateError> {
        journal.validate()?;
        let previous = self.journal()?;
        if let Some(previous) = &previous {
            if !previous.same_transaction(journal)
                || !valid_transition(previous.phase, journal.phase)
            {
                return Err(UpdateError::RecoveryRequired);
            }
        } else if journal.phase != Phase::Prepared {
            return Err(UpdateError::RecoveryRequired);
        }
        let bytes = serde_json::to_vec(journal).map_err(|_| UpdateError::RecoveryRequired)?;
        if bytes.len() as u64 > JOURNAL_LIMIT {
            return Err(UpdateError::RecoveryRequired);
        }
        // Fresh exclusive temp name: a crash before publication cannot block
        // the next recovery or cause an unexplained file to be overwritten.
        let temporary = native::journal_temp(&self.state.path, &bytes)?;
        let destination = self.state.path.join("journal.json");
        if previous.is_some() {
            native::replace(&destination, &temporary)?;
        } else {
            native::publish_new(&destination, &temporary)?;
        }
        native::flush(&destination)
    }
    fn workspace(&self, journal: &Journal) -> Result<native::Directory, UpdateError> {
        native::Directory::open(&self.state.path.join(&journal.id))
    }
    fn current(&self) -> Result<(Manifest, Vec<u8>, Entry, native::Registration), UpdateError> {
        let marker = native::read(&self.root.path.join(MARKER), MARKER_LIMIT as u64)?;
        let manifest = Manifest::parse(&marker)?;
        if manifest.identity != self.identity {
            return Err(UpdateError::UnsupportedPackage);
        }
        let uninstaller = native::read(&self.root.path.join(UNINSTALLER), 16 * 1024 * 1024)?;
        if uninstaller.len() < 64 || &uninstaller[..2] != b"MZ" {
            return Err(UpdateError::UnsupportedPackage);
        }
        let entry = Entry {
            path: UNINSTALLER.into(),
            bytes: uninstaller.len() as u64,
            sha256: hash(&uninstaller),
        };
        check_tree(&self.root.path, &manifest, &hash(&marker), &entry)?;
        let registration = self.registry.snapshot(&self.root.path)?;
        if registration.version != manifest.identity.version {
            return Err(UpdateError::UnsupportedPackage);
        }
        Ok((manifest, marker, entry, registration))
    }
    pub fn acknowledge_before_store(&mut self) -> Result<(), UpdateError> {
        let Some(id) = self.trial.take() else {
            return Ok(());
        };
        let mut journal = self.journal()?.ok_or(UpdateError::RecoveryRequired)?;
        if journal.id != id
            || journal.phase != Phase::AwaitingHealth
            || journal.new.identity != self.identity
        {
            return Err(UpdateError::RecoveryRequired);
        }
        check_tree(
            &self.root.path,
            &journal.new,
            &journal.new_marker,
            &journal.uninstaller,
        )?;
        if self.registry.snapshot(&self.root.path)? != journal.new_registration {
            return Err(UpdateError::RecoveryRequired);
        }
        // Durable commit BEFORE stores: a later crash may never run old data code.
        journal.phase = Phase::Committed;
        self.write_journal(&journal)?;
        native::write_new(
            &self.workspace(&journal)?.path.join("health"),
            journal.new_marker.as_bytes(),
        )?;
        Ok(())
    }
}

fn valid_transition(old: Phase, next: Phase) -> bool {
    old == next
        || matches!(
            (old, next),
            (Phase::Prepared, Phase::ApplyIntent)
                | (
                    Phase::ApplyIntent,
                    Phase::AwaitingHealth | Phase::RollbackIntent
                )
                | (
                    Phase::AwaitingHealth,
                    Phase::Committed | Phase::RollbackIntent
                )
                | (Phase::RollbackIntent, Phase::RolledBack)
        )
}

fn create_payload_tree(
    path: &Path,
    manifest: &Manifest,
    marker: &[u8],
    files: &[&[u8]],
    uninstaller: &[u8],
) -> Result<(), UpdateError> {
    let _root = native::create_directory(path)?;
    let _licenses = native::create_directory(&path.join("licenses"))?;
    let _themes = native::create_directory(&path.join("themes"))?;
    for (name, bytes) in FILES.into_iter().zip(files) {
        native::write_new(&path.join(name), bytes)?;
    }
    native::write_new(&path.join(MARKER), marker)?;
    native::write_new(&path.join(UNINSTALLER), uninstaller)?;
    let uninstall = Entry {
        path: UNINSTALLER.into(),
        bytes: uninstaller.len() as u64,
        sha256: hash(uninstaller),
    };
    check_tree(path, manifest, &hash(marker), &uninstall)
}

pub struct PreparedWindowsRestart<'a> {
    session: &'a mut WindowsSession,
    journal: Journal,
    _lease: InstallLease,
}

pub(super) fn prepare_restart<'a>(
    session: &'a mut WindowsSession,
    bytes: &[u8],
    version: &str,
    lease: InstallLease,
) -> Result<PreparedWindowsRestart<'a>, UpdateError> {
    if !INSTALLATION_VALIDATED {
        return Err(UpdateError::InstallNotValidated);
    }
    prepare_restart_admitted(session, bytes, version, lease)
}

// Shipping callers reach this core only through the unchanged admission above.
// The compile-only actor must validate its immutable private fixture plan first.
fn prepare_restart_admitted<'a>(
    session: &'a mut WindowsSession,
    bytes: &[u8],
    version: &str,
    lease: InstallLease,
) -> Result<PreparedWindowsRestart<'a>, UpdateError> {
    if session.journal()?.is_some() || session.install_lease.is_some() {
        return Err(UpdateError::UpdateActive);
    }
    // One interrupted preparation must not permit another bounded-but-large
    // tree on every retry. Retain unexplained stages for explicit recovery.
    for entry in fs::read_dir(&session.state.path).map_err(|_| UpdateError::RecoveryRequired)? {
        if entry
            .map_err(|_| UpdateError::RecoveryRequired)?
            .file_name()
            != "lock"
        {
            return Err(UpdateError::RecoveryRequired);
        }
    }
    let payload = Payload::parse(bytes, &session.identity, version)?;
    let (old, old_marker, uninstaller, old_registration) = session.current()?;
    let mut temporary = tempfile::Builder::new()
        .prefix("transaction-")
        .tempdir_in(&session.state.path)
        .map_err(|_| UpdateError::StageFailed)?;
    // Keep any interrupted stage for owned pre-store recovery, never recursive
    // Drop cleanup of a tree another process could have changed.
    temporary.disable_cleanup(true);
    let id = temporary
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| valid_id(s))
        .ok_or(UpdateError::StageFailed)?
        .to_owned();
    let work = native::Directory::open(temporary.path())?;
    let uninstall_bytes = native::read(&session.root.path.join(UNINSTALLER), uninstaller.bytes)?;
    let mut old_files = Vec::new();
    for entry in &old.files {
        old_files.push(native::read(
            &session.root.path.join(&entry.path),
            entry.bytes,
        )?);
    }
    let references = old_files.iter().map(Vec::as_slice).collect::<Vec<_>>();
    create_payload_tree(
        &work.path.join("backup"),
        &old,
        &old_marker,
        &references,
        &uninstall_bytes,
    )?;
    drop(references);
    drop(old_files);
    create_payload_tree(
        &work.path.join("candidate"),
        &payload.manifest,
        payload.marker,
        &payload.contents,
        &uninstall_bytes,
    )?;
    let helper = native::read(
        &session.root.path.join("morpheus-app.exe"),
        MAX_UPDATE_BYTES,
    )?;
    if pe_identity(&helper)? != old.identity {
        return Err(UpdateError::StageFailed);
    }
    native::write_new(&work.path.join(HELPER), &helper)?;
    let (parent_id, parent_stamp) = native::ParentProcess::current_stamp()?;
    let total = payload
        .manifest
        .files
        .iter()
        .try_fold(
            uninstaller.bytes + payload.marker.len() as u64,
            |sum, entry| sum.checked_add(entry.bytes),
        )
        .ok_or(UpdateError::StageFailed)?;
    let new_registration = native::Registration {
        version: payload.manifest.identity.version.clone(),
        estimated_size: u32::try_from(total.div_ceil(1024))
            .map_err(|_| UpdateError::StageFailed)?,
    };
    let journal = Journal {
        schema: 1,
        id,
        phase: Phase::Prepared,
        old,
        new: payload.manifest,
        old_marker: hash(&old_marker),
        new_marker: hash(payload.marker),
        old_marker_bytes: old_marker.len() as u64,
        new_marker_bytes: payload.marker.len() as u64,
        uninstaller,
        old_registration,
        new_registration,
        parent_id,
        parent_stamp,
        helper_hash: hash(&helper),
    };
    session.write_journal(&journal)?;
    Ok(PreparedWindowsRestart {
        session,
        journal,
        _lease: lease,
    })
}

impl PreparedWindowsRestart<'_> {
    pub fn launch_supervisor(self) -> Result<(), UpdateError> {
        let Self {
            session,
            journal,
            _lease: lease,
        } = self;
        let work = session.workspace(&journal)?;
        if hash(&native::read(&work.path.join(HELPER), MAX_UPDATE_BYTES)?) != journal.helper_hash {
            return Err(UpdateError::CorruptStage);
        }
        let mut child = Command::new(work.path.join(HELPER))
            .arg("--morpheus-windows-update-supervisor")
            .arg(&session.root.path)
            .arg(&journal.id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| UpdateError::RestartFailed)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(ready) = native::read(&work.path.join("ready"), 64) {
                if ready != journal.helper_hash.as_bytes() {
                    terminate(&mut child)?;
                    return Err(UpdateError::RestartFailed);
                }
                session.install_lease = Some(lease);
                return Ok(());
            }
            if child
                .try_wait()
                .map_err(|_| UpdateError::RestartFailed)?
                .is_some()
            {
                return Err(UpdateError::RestartFailed);
            }
            if Instant::now() >= deadline {
                terminate(&mut child)?;
                return Err(UpdateError::RestartFailed);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn terminate(child: &mut Child) -> Result<(), UpdateError> {
    if child
        .try_wait()
        .map_err(|_| UpdateError::RestartFailed)?
        .is_none()
    {
        child.kill().map_err(|_| UpdateError::RestartFailed)?;
    }
    child.wait().map_err(|_| UpdateError::RestartFailed)?;
    Ok(())
}

fn ordered_paths() -> impl Iterator<Item = &'static str> {
    FILES
        .into_iter()
        .filter(|p| *p != "morpheus-app.exe")
        .chain([MARKER, "morpheus-app.exe"])
}

fn clear_replacement_temp(workspace: &Path, journal: &Journal) -> Result<(), UpdateError> {
    let path = workspace.join("replace.next");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(UpdateError::RecoveryRequired),
        Ok(_) => {}
    }
    let bytes = native::read(&path, MAX_UPDATE_BYTES)?;
    let size = bytes.len() as u64;
    let digest = hash(&bytes);
    // A later file's temp can survive before an earlier entry is rolled back.
    // Bind it to the whole immutable transaction, never only the current entry.
    let known = journal
        .old
        .files
        .iter()
        .chain(&journal.new.files)
        .any(|entry| entry.bytes == size && entry.sha256 == digest)
        || (size == journal.old_marker_bytes && digest == journal.old_marker)
        || (size == journal.new_marker_bytes && digest == journal.new_marker);
    if !known {
        return Err(UpdateError::RecoveryRequired);
    }
    fs::remove_file(path).map_err(|_| UpdateError::RecoveryRequired)
}

/// Hash-inferred recovery admits each exact old/new value and no other one.
/// Every old file is already durable in backup before the first replacement.
fn replace_payload(
    session: &WindowsSession,
    journal: &Journal,
    forward: bool,
) -> Result<(), UpdateError> {
    let workspace = session.workspace(journal)?;
    let backup = workspace.path.join("backup");
    check_tree(
        &backup,
        &journal.old,
        &journal.old_marker,
        &journal.uninstaller,
    )?;
    check_entry(&session.root.path, &journal.uninstaller)?;
    // Unknown files and directories fail without modifying the installation.
    enumerate(&session.root.path, false)?;
    clear_replacement_temp(&workspace.path, journal)?;
    for path in ordered_paths() {
        let old = if path == MARKER {
            journal.old_marker.as_str()
        } else {
            journal
                .old
                .files
                .iter()
                .find(|e| e.path == path)
                .ok_or(UpdateError::RecoveryRequired)?
                .sha256
                .as_str()
        };
        let new = if path == MARKER {
            journal.new_marker.as_str()
        } else {
            journal
                .new
                .files
                .iter()
                .find(|e| e.path == path)
                .ok_or(UpdateError::RecoveryRequired)?
                .sha256
                .as_str()
        };
        let destination = session.root.path.join(path);
        let current = match fs::symlink_metadata(&destination) {
            Ok(_) => Some(hash(&native::read(&destination, MAX_UPDATE_BYTES)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(UpdateError::RecoveryRequired),
        };
        if current
            .as_deref()
            .is_some_and(|value| value != old && value != new)
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let desired = if forward { new } else { old };
        if current.as_deref() == Some(desired) {
            continue;
        }
        let source = if forward {
            workspace.path.join("candidate").join(path)
        } else {
            backup.join(path)
        };
        let bytes = native::read(&source, MAX_UPDATE_BYTES)?;
        if hash(&bytes) != desired {
            return Err(UpdateError::RecoveryRequired);
        }
        let temporary = workspace.path.join("replace.next");
        native::write_new(&temporary, &bytes)?;
        if current.is_some() {
            native::replace(&destination, &temporary)?;
        } else {
            native::publish_new(&destination, &temporary)?;
        }
        // Flush the replaced file through its own handle before the next step.
        native::flush(&session.root.path.join(path))?;
        if hash(&native::read(
            &session.root.path.join(path),
            MAX_UPDATE_BYTES,
        )?) != desired
        {
            return Err(UpdateError::RecoveryRequired);
        }
        #[cfg(morpheus_windows_update_acceptance)]
        if forward && path == MARKER {
            acceptance::after_marker_replace(journal)?;
        }
    }
    let (from, to) = if forward {
        (&journal.old_registration, &journal.new_registration)
    } else {
        (&journal.new_registration, &journal.old_registration)
    };
    session
        .registry
        .set_conditional(&session.root.path, from, to)?;
    check_tree(
        &session.root.path,
        if forward { &journal.new } else { &journal.old },
        if forward {
            &journal.new_marker
        } else {
            &journal.old_marker
        },
        &journal.uninstaller,
    )
}

fn confirm_commit(session: &WindowsSession, journal: &Journal) -> Result<(), UpdateError> {
    confirm_commit_data(
        &session.root.path,
        &session.state.path.join("journal.json"),
        journal,
        &session.registry.snapshot(&session.root.path)?,
    )
}
fn confirm_commit_data(
    root: &Path,
    journal_path: &Path,
    journal: &Journal,
    registration: &native::Registration,
) -> Result<(), UpdateError> {
    if journal.phase != Phase::Committed {
        return Err(UpdateError::RecoveryRequired);
    }
    check_tree(
        root,
        &journal.new,
        &journal.new_marker,
        &journal.uninstaller,
    )?;
    if *registration != journal.new_registration {
        return Err(UpdateError::RecoveryRequired);
    }
    // ReplaceFile may have published the irreversible commit before its final
    // flush failed. Retry that checked barrier before cleanup or opening stores.
    native::flush(journal_path)
}

fn recover(session: &WindowsSession, journal: &mut Journal) -> Result<bool, UpdateError> {
    match journal.phase {
        Phase::Prepared => {
            check_tree(
                &session.root.path,
                &journal.old,
                &journal.old_marker,
                &journal.uninstaller,
            )?;
            Ok(false)
        }
        Phase::Committed => {
            confirm_commit(session, journal)?;
            Ok(false)
        }
        Phase::RolledBack => {
            check_tree(
                &session.root.path,
                &journal.old,
                &journal.old_marker,
                &journal.uninstaller,
            )?;
            Ok(false)
        }
        Phase::ApplyIntent | Phase::AwaitingHealth | Phase::RollbackIntent => {
            journal.phase = Phase::RollbackIntent;
            session.write_journal(journal)?;
            replace_payload(session, journal, false)?;
            journal.phase = Phase::RolledBack;
            session.write_journal(journal)?;
            Ok(true)
        }
    }
}

fn cleanup_finished(session: &WindowsSession, journal: &Journal) -> Result<(), UpdateError> {
    let (manifest, marker, registration) = match journal.phase {
        Phase::Committed => (&journal.new, &journal.new_marker, &journal.new_registration),
        Phase::Prepared | Phase::RolledBack => {
            (&journal.old, &journal.old_marker, &journal.old_registration)
        }
        _ => return Err(UpdateError::RecoveryRequired),
    };
    check_tree(&session.root.path, manifest, marker, &journal.uninstaller)?;
    if session.registry.snapshot(&session.root.path)? != *registration {
        return Err(UpdateError::RecoveryRequired);
    }
    let path = session.state.path.join(&journal.id);
    if path
        .try_exists()
        .map_err(|_| UpdateError::RecoveryRequired)?
    {
        let workspace = native::Directory::open(&path)?;
        let mut allowed = std::collections::BTreeMap::<String, Vec<String>>::new();
        for (folder, manifest, marker) in [
            ("backup", &journal.old, &journal.old_marker),
            ("candidate", &journal.new, &journal.new_marker),
        ] {
            for entry in &manifest.files {
                allowed.insert(
                    format!("{folder}/{}", entry.path),
                    vec![entry.sha256.clone()],
                );
            }
            allowed.insert(format!("{folder}/{MARKER}"), vec![marker.clone()]);
            allowed.insert(
                format!("{folder}/{UNINSTALLER}"),
                vec![journal.uninstaller.sha256.clone()],
            );
        }
        if let Some(request_bytes) =
            optional_private(&workspace.path.join(RECOVERY_REQUEST), RECOVERY_LIMIT)?
        {
            RecoveryRequest::parse(&request_bytes, journal)?;
            allowed.insert(RECOVERY_REQUEST.into(), vec![hash(&request_bytes)]);
            if let Some(ready_bytes) =
                optional_private(&workspace.path.join(RECOVERY_READY), RECOVERY_LIMIT)?
            {
                RecoveryReady::parse(&ready_bytes, &request_bytes)?;
                allowed.insert(RECOVERY_READY.into(), vec![hash(&ready_bytes)]);
            }
        } else if optional_private(&workspace.path.join(RECOVERY_READY), RECOVERY_LIMIT)?.is_some()
        {
            return Err(UpdateError::RecoveryRequired);
        }
        allowed.insert(HELPER.into(), vec![journal.helper_hash.clone()]);
        allowed.insert("ready".into(), vec![hash(journal.helper_hash.as_bytes())]);
        allowed.insert("health".into(), vec![hash(journal.new_marker.as_bytes())]);
        let replacement_hashes = journal
            .old
            .files
            .iter()
            .chain(&journal.new.files)
            .map(|entry| entry.sha256.clone())
            .chain([journal.old_marker.clone(), journal.new_marker.clone()])
            .collect();
        allowed.insert("replace.next".into(), replacement_hashes);
        fn collect(
            root: &Path,
            current: &Path,
            allowed: &std::collections::BTreeMap<String, Vec<String>>,
            files: &mut Vec<PathBuf>,
            directories: &mut Vec<PathBuf>,
        ) -> Result<(), UpdateError> {
            let _directory = native::Directory::open(current)?;
            for entry in fs::read_dir(current).map_err(|_| UpdateError::RecoveryRequired)? {
                let entry = entry.map_err(|_| UpdateError::RecoveryRequired)?;
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| UpdateError::RecoveryRequired)?
                    .to_str()
                    .ok_or(UpdateError::RecoveryRequired)?
                    .replace('\\', "/");
                let meta =
                    fs::symlink_metadata(&path).map_err(|_| UpdateError::RecoveryRequired)?;
                if meta.is_dir() {
                    if ![
                        "backup",
                        "candidate",
                        "backup/licenses",
                        "backup/themes",
                        "candidate/licenses",
                        "candidate/themes",
                    ]
                    .contains(&relative.as_str())
                    {
                        return Err(UpdateError::RecoveryRequired);
                    }
                    collect(root, &path, allowed, files, directories)?;
                    directories.push(path);
                } else {
                    let hashes = allowed
                        .get(&relative)
                        .ok_or(UpdateError::RecoveryRequired)?;
                    if !hashes.contains(&hash(&native::read(&path, MAX_UPDATE_BYTES)?)) {
                        return Err(UpdateError::RecoveryRequired);
                    }
                    files.push(path);
                }
            }
            Ok(())
        }
        let (mut files, mut directories) = (Vec::new(), Vec::new());
        collect(
            &workspace.path,
            &workspace.path,
            &allowed,
            &mut files,
            &mut directories,
        )?;
        // Try the helper first. If it is still mapped, leave the recoverable
        // workspace/journal intact for a later normal startup; never kill it.
        files.sort_by_key(|p| p.file_name().is_none_or(|name| name != HELPER));
        for file in files {
            fs::remove_file(file).map_err(|_| UpdateError::RecoveryRequired)?;
        }
        for directory in directories {
            fs::remove_dir(directory).map_err(|_| UpdateError::RecoveryRequired)?;
        }
        drop(workspace);
        fs::remove_dir(&path).map_err(|_| UpdateError::RecoveryRequired)?;
    }
    // Missing known files are allowed only in this post-terminal cleanup. A
    // crash after any deletion resumes without requiring deleted backups.
    fs::remove_file(session.state.path.join("journal.json"))
        .map_err(|_| UpdateError::RecoveryRequired)
}

fn launch_current(root: &Path) -> Result<(), UpdateError> {
    Command::new(root.join("morpheus-app.exe"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| UpdateError::RestartFailed)?;
    Ok(())
}

fn checked_copied_helper(workspace: &Path, journal: &Journal) -> Result<PathBuf, UpdateError> {
    let path = workspace.join(HELPER);
    let bytes = native::read(&path, MAX_UPDATE_BYTES)?;
    if hash(&bytes) != journal.helper_hash || pe_identity(&bytes)? != journal.old.identity {
        return Err(UpdateError::CorruptStage);
    }
    Ok(path)
}

/// Called only while normal startup owns the installed lifetime lock and has
/// opened no stores or workers. It does not repair its own mapped executable.
fn launch_recovery(session: &WindowsSession, journal: &Journal) -> Result<(), UpdateError> {
    if !INSTALLATION_VALIDATED || !needs_copied_recovery(journal.phase) {
        return Err(UpdateError::InstallNotValidated);
    }
    launch_recovery_admitted(session, journal)
}

fn launch_recovery_admitted(
    session: &WindowsSession,
    journal: &Journal,
) -> Result<(), UpdateError> {
    if !needs_copied_recovery(journal.phase) {
        return Err(UpdateError::InstallNotValidated);
    }
    let workspace = session.workspace(journal)?;
    let helper = checked_copied_helper(&workspace.path, journal)?;
    let request_path = workspace.path.join(RECOVERY_REQUEST);
    let ready_path = workspace.path.join(RECOVERY_READY);
    if let Some(previous_bytes) = optional_private(&request_path, RECOVERY_LIMIT)? {
        let previous = RecoveryRequest::parse(&previous_bytes, journal)?;
        // A parent killed before readiness cannot leave an unrecorded late
        // helper: it can only OPEN this nonce's existing lifetime object. Never
        // delete a request while either process or the global object survives.
        if native::RecoveryLease::open(&journal.id, &previous.nonce)?.is_some()
            || native::ParentProcess::running(previous.parent_id, previous.parent_stamp)?
        {
            return Err(UpdateError::UpdateActive);
        }
        if let Some(bytes) = optional_private(&ready_path, RECOVERY_LIMIT)? {
            let ready = RecoveryReady::parse(&bytes, &previous_bytes)?;
            if native::ParentProcess::running(ready.helper_id, ready.helper_stamp)? {
                return Err(UpdateError::UpdateActive);
            }
            fs::remove_file(&ready_path).map_err(|_| UpdateError::RecoveryRequired)?;
        }
        fs::remove_file(&request_path).map_err(|_| UpdateError::RecoveryRequired)?;
    } else if optional_private(&ready_path, RECOVERY_LIMIT)?.is_some() {
        return Err(UpdateError::RecoveryRequired);
    }
    let nonce = native::recovery_nonce()?;
    let lifetime = native::RecoveryLease::create(&journal.id, &nonce)?;
    let (parent_id, parent_stamp) = native::ParentProcess::current_stamp()?;
    let request = RecoveryRequest {
        schema: 1,
        nonce,
        phase: journal.phase,
        journal_hash: hash(
            &serde_json::to_vec(journal).map_err(|_| UpdateError::RecoveryRequired)?,
        ),
        parent_id,
        parent_stamp,
    };
    let request_bytes = serde_json::to_vec(&request).map_err(|_| UpdateError::RecoveryRequired)?;
    RecoveryRequest::parse(&request_bytes, journal)?;
    native::write_new(&request_path, &request_bytes)?;
    let mut child = Command::new(helper)
        .arg("--morpheus-windows-update-recovery-supervisor")
        .arg(&session.root.path)
        .arg(&journal.id)
        .arg(&request.nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| UpdateError::RestartFailed)?;
    let stamp = match native::ParentProcess::child_stamp(&child) {
        Ok(stamp) => stamp,
        Err(error) => {
            terminate(&mut child)?;
            return Err(error);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let observed = optional_private(&ready_path, RECOVERY_LIMIT);
        match observed {
            Ok(Some(bytes)) => {
                let ready = match RecoveryReady::parse(&bytes, &request_bytes) {
                    Ok(ready) => ready,
                    Err(error) => {
                        terminate(&mut child)?;
                        return Err(error);
                    }
                };
                if ready.helper_id != child.id() || ready.helper_stamp != stamp {
                    terminate(&mut child)?;
                    return Err(UpdateError::RestartFailed);
                }
                // The child opened/retains the same lifetime object before
                // publishing readiness. Dropping our handle cannot destroy it.
                drop(lifetime);
                return Ok(());
            }
            Err(error) => {
                terminate(&mut child)?;
                return Err(error);
            }
            Ok(None) => {}
        }
        if child
            .try_wait()
            .map_err(|_| UpdateError::RestartFailed)?
            .is_some()
        {
            return Err(UpdateError::RestartFailed);
        }
        if Instant::now() >= deadline {
            terminate(&mut child)?;
            return Err(UpdateError::RestartFailed);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// This process runs only the exact private, hash-bound old executable copy.
/// It waits for the requesting installed process to EXIT, not merely release a
/// lock, before replacing any installed file (including that process's EXE).
fn recovery_supervisor(root: &Path, id: &str, nonce: &str) -> Result<(), UpdateError> {
    if !INSTALLATION_VALIDATED || !valid_id(id) || !valid_id(nonce) {
        return Err(UpdateError::InstallNotValidated);
    }
    recovery_supervisor_admitted(root, id, nonce)
}

fn recovery_supervisor_admitted(root: &Path, id: &str, nonce: &str) -> Result<(), UpdateError> {
    if !valid_id(id) || !valid_id(nonce) {
        return Err(UpdateError::InstallNotValidated);
    }
    let root = native::Directory::open(root)?;
    let mut state = root.path.as_os_str().to_os_string();
    state.push(".morpheus-update");
    let state = native::Directory::open(Path::new(&state))?;
    let workspace = native::Directory::open(&state.path.join(id))?;
    let journal_bytes = native::read(&state.path.join("journal.json"), JOURNAL_LIMIT)?;
    let journal: Journal =
        serde_json::from_slice(&journal_bytes).map_err(|_| UpdateError::RecoveryRequired)?;
    journal.validate()?;
    if journal.id != id {
        return Err(UpdateError::RecoveryRequired);
    }
    let executable = std::env::current_exe().map_err(|_| UpdateError::RestartFailed)?;
    if executable.file_name().is_none_or(|name| name != HELPER)
        || native::Directory::open(executable.parent().ok_or(UpdateError::RestartFailed)?)?.path
            != workspace.path
        || hash(&native::read(&executable, MAX_UPDATE_BYTES)?) != journal.helper_hash
    {
        return Err(UpdateError::RestartFailed);
    }
    checked_copied_helper(&workspace.path, &journal)?;
    if pe_identity(&native::read(&executable, MAX_UPDATE_BYTES)?)? != journal.old.identity {
        return Err(UpdateError::RestartFailed);
    }
    let request_bytes = native::read(&workspace.path.join(RECOVERY_REQUEST), RECOVERY_LIMIT)?;
    let request = RecoveryRequest::parse(&request_bytes, &journal)?;
    if request.nonce != nonce || !needs_copied_recovery(journal.phase) {
        return Err(UpdateError::RecoveryRequired);
    }
    let _lifetime = native::RecoveryLease::open(id, nonce)?.ok_or(UpdateError::RestartFailed)?;
    let parent = native::ParentProcess::open(request.parent_id, request.parent_stamp)?;
    let (helper_id, helper_stamp) = native::ParentProcess::current_stamp()?;
    let ready = RecoveryReady {
        schema: 1,
        request_hash: hash(&request_bytes),
        helper_id,
        helper_stamp,
    };
    native::write_new(
        &workspace.path.join(RECOVERY_READY),
        &serde_json::to_vec(&ready).map_err(|_| UpdateError::RecoveryRequired)?,
    )?;
    if !parent.wait(60_000)? {
        return Err(UpdateError::RestartFailed);
    }
    let session = WindowsSession::lock(&root.path, journal.old.identity.clone())?;
    let mut observed = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    // The installed lifetime lock makes this request recheck atomic relative to
    // every other startup/installer/uninstaller. No stale helper may mutate it.
    if !journal.same_transaction(&observed)
        || native::read(&workspace.path.join(RECOVERY_REQUEST), RECOVERY_LIMIT)? != request_bytes
    {
        return Err(UpdateError::RecoveryRequired);
    }
    RecoveryRequest::parse(&request_bytes, &observed)?;
    if !needs_copied_recovery(observed.phase)
        && !matches!(observed.phase, Phase::Committed | Phase::RolledBack)
    {
        return Err(UpdateError::RecoveryRequired);
    }
    recover(&session, &mut observed)?;
    drop(session);
    launch_current(&root.path)
}

fn supervisor(root: &Path, id: &str) -> Result<(), UpdateError> {
    if !INSTALLATION_VALIDATED || !valid_id(id) {
        return Err(UpdateError::InstallNotValidated);
    }
    supervisor_admitted(root, id)
}

fn supervisor_admitted(root: &Path, id: &str) -> Result<(), UpdateError> {
    if !valid_id(id) {
        return Err(UpdateError::InstallNotValidated);
    }
    let root = native::Directory::open(root)?;
    let mut state = root.path.as_os_str().to_os_string();
    state.push(".morpheus-update");
    let state = native::Directory::open(Path::new(&state))?;
    let workspace = native::Directory::open(&state.path.join(id))?;
    let executable = std::env::current_exe().map_err(|_| UpdateError::RestartFailed)?;
    let mut journal: Journal = serde_json::from_slice(&native::read(
        &state.path.join("journal.json"),
        JOURNAL_LIMIT,
    )?)
    .map_err(|_| UpdateError::RecoveryRequired)?;
    journal.validate()?;
    if journal.id != id
        || journal.phase != Phase::Prepared
        || native::Directory::open(executable.parent().ok_or(UpdateError::RestartFailed)?)?.path
            != workspace.path
        || executable.file_name().is_none_or(|n| n != HELPER)
        || hash(&native::read(&executable, MAX_UPDATE_BYTES)?) != journal.helper_hash
    {
        return Err(UpdateError::RestartFailed);
    }
    let parent = native::ParentProcess::open(journal.parent_id, journal.parent_stamp)?;
    native::write_new(
        &workspace.path.join("ready"),
        journal.helper_hash.as_bytes(),
    )?;
    // This real OS handle is bound to creation time, never a process-name kill
    // or a reusable PID wait. Only a confirmed successful old-app exit permits
    // forward apply. Timeout, query failure and nonzero/exception exit leave the
    // prepared installation and backup untouched for subsequent recovery.
    if !parent.wait_for_successful_exit(60_000)? {
        return Err(UpdateError::RestartFailed);
    }
    let session = WindowsSession::lock(&root.path, journal.old.identity.clone())?;
    let observed = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    if serde_json::to_vec(&observed).ok() != serde_json::to_vec(&journal).ok() {
        return Err(UpdateError::RecoveryRequired);
    }
    check_tree(
        &workspace.path.join("candidate"),
        &journal.new,
        &journal.new_marker,
        &journal.uninstaller,
    )?;
    journal.phase = Phase::ApplyIntent;
    session.write_journal(&journal)?;
    if replace_payload(&session, &journal, true).is_err() {
        recover(&session, &mut journal)?;
        drop(session);
        Command::new(root.path.join("morpheus-app.exe"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| UpdateError::RestartFailed)?;
        return Err(UpdateError::RestartFailed);
    }
    journal.phase = Phase::AwaitingHealth;
    session.write_journal(&journal)?;
    drop(session);
    let spawn = Command::new(root.path.join("morpheus-app.exe"))
        .arg("--morpheus-windows-update-trial")
        .arg(id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(_) => {
            let session = WindowsSession::lock(&root.path, journal.new.identity.clone())?;
            recover(&session, &mut journal)?;
            drop(session);
            Command::new(root.path.join("morpheus-app.exe"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| UpdateError::RestartFailed)?;
            return Err(UpdateError::RestartFailed);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let observed: Journal = serde_json::from_slice(&native::read(
            &state.path.join("journal.json"),
            JOURNAL_LIMIT,
        )?)
        .map_err(|_| UpdateError::RecoveryRequired)?;
        observed.validate()?;
        if observed.id != id || !journal.same_transaction(&observed) {
            return Err(UpdateError::RecoveryRequired);
        }
        // An observed commit is final. A timeout racing commit may stop our
        // child, but the subsequent locked recheck still forbids rollback.
        if observed.phase == Phase::Committed {
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|_| UpdateError::RestartFailed)?
            .is_some()
            || Instant::now() >= deadline
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    terminate(&mut child)?;
    let session = WindowsSession::lock(&root.path, journal.new.identity.clone())?;
    let mut observed = session.journal()?.ok_or(UpdateError::RecoveryRequired)?;
    if !journal.same_transaction(&observed) {
        return Err(UpdateError::RecoveryRequired);
    }
    // A health acknowledgement racing the timeout is authoritative. Our owned
    // child was stopped above, so relaunch the validated committed cohort after
    // releasing its lifetime lock; never leave the owner without an app.
    if observed.phase == Phase::Committed {
        confirm_commit(&session, &observed)?;
        drop(session);
        launch_current(&root.path)?;
        return Ok(());
    }
    recover(&session, &mut observed)?;
    drop(session);
    Command::new(root.path.join("morpheus-app.exe"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| UpdateError::RestartFailed)?;
    Err(UpdateError::RestartFailed)
}

pub(super) fn early_dispatch() -> Result<bool, UpdateError> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let reserved = args.first().is_some_and(|argument| {
        argument
            .to_string_lossy()
            .starts_with("--morpheus-windows-update-")
    });
    if reserved
        && (option_env!("MORPHEUS_PACKAGE_FORMAT") != Some("nsis-payload-v2")
            || !INSTALLATION_VALIDATED)
    {
        return Err(UpdateError::InstallNotValidated);
    }
    if args
        .first()
        .is_some_and(|a| a == "--morpheus-windows-update-recovery-supervisor")
    {
        if args.len() != 4 {
            return Err(UpdateError::UnsupportedPackage);
        }
        recovery_supervisor(
            Path::new(&args[1]),
            args[2].to_str().ok_or(UpdateError::UnsupportedPackage)?,
            args[3].to_str().ok_or(UpdateError::UnsupportedPackage)?,
        )?;
        return Ok(true);
    }
    if args
        .first()
        .is_some_and(|a| a == "--morpheus-windows-update-supervisor")
    {
        if args.len() != 3 {
            return Err(UpdateError::UnsupportedPackage);
        }
        supervisor(
            Path::new(&args[1]),
            args[2].to_str().ok_or(UpdateError::UnsupportedPackage)?,
        )?;
        return Ok(true);
    }
    if reserved
        && args
            .first()
            .is_none_or(|argument| argument != "--morpheus-windows-update-trial")
    {
        return Err(UpdateError::UnsupportedPackage);
    }
    Ok(false)
}

pub(super) fn begin_startup() -> Result<(Option<WindowsSession>, bool), UpdateError> {
    begin_startup_with_validation(INSTALLATION_VALIDATED)
}

fn begin_startup_with_validation(
    validated: bool,
) -> Result<(Option<WindowsSession>, bool), UpdateError> {
    if option_env!("MORPHEUS_PACKAGE_FORMAT") != Some("nsis-payload-v2") {
        return Ok((None, false));
    }
    let executable = std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
    if executable
        .file_name()
        .is_none_or(|n| n != "morpheus-app.exe")
    {
        return Err(UpdateError::UnsupportedPackage);
    }
    let identity = embedded_identity()?;
    let mut session = WindowsSession::lock(
        executable.parent().ok_or(UpdateError::UnsupportedPackage)?,
        identity,
    )?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(mut journal) = session.journal()? {
        if args
            .first()
            .is_some_and(|a| a == "--morpheus-windows-update-trial")
        {
            if !validated
                || args.len() != 2
                || args[1].to_str() != Some(journal.id.as_str())
                || journal.phase != Phase::AwaitingHealth
                || session.identity != journal.new.identity
            {
                return Err(UpdateError::RecoveryRequired);
            }
            check_tree(
                &session.root.path,
                &journal.new,
                &journal.new_marker,
                &journal.uninstaller,
            )?;
            session.trial = Some(journal.id.clone());
        } else {
            if !args.is_empty()
                && args.first().is_some_and(|a| {
                    a.to_string_lossy()
                        .starts_with("--morpheus-windows-update-")
                })
            {
                return Err(UpdateError::RecoveryRequired);
            }
            if needs_copied_recovery(journal.phase) {
                if validated {
                    launch_recovery_admitted(&session, &journal)?;
                } else {
                    launch_recovery(&session, &journal)?;
                }
                drop(session);
                // main exits on this flag before UI, stores or call workers.
                return Ok((None, true));
            }
            recover(&session, &mut journal)?;
            let required = if journal.phase == Phase::Committed {
                &journal.new.identity
            } else {
                &journal.old.identity
            };
            if session.identity != *required {
                // Do not open stores in an executable whose payload was rolled
                // back. Relaunch the restored code and finish this process.
                let path = session.root.path.join("morpheus-app.exe");
                drop(session);
                Command::new(path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|_| UpdateError::RestartFailed)?;
                return Ok((None, true));
            }
            // Failure here retains the terminal journal and owned old version;
            // it cannot invalidate the healthy current application's stores.
            let _ = cleanup_finished(&session, &journal);
        }
    } else if args.first().is_some_and(|a| {
        a.to_string_lossy()
            .starts_with("--morpheus-windows-update-")
    }) {
        return Err(UpdateError::RecoveryRequired);
    }
    session.current()?;
    Ok((Some(session), false))
}

#[cfg(test)]
mod tests;
