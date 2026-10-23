//! Version-aware writes of the settings file.
//!
//! A change is decided on the file as it was read.
//! [`ConfigStore::load_revision`] returns that read's exact text as a
//! [`ConfigRevision`]. [`ConfigStore::commit`] writes only while the file
//! still holds that revision. Otherwise it returns [`CommitError::Changed`]
//! with the version it found. The file is left as it was, and the caller
//! still holds its own change, so both versions survive for a person to
//! choose between.
//!
//! # What is guaranteed
//!
//! - Morpheus writers are serialized. Every `commit` and every
//!   [`ConfigStore::save`] holds two locks from before the comparison until
//!   after the rename:
//!   - this process's claim on the file's path (one writer per path at a
//!     time across this process's threads);
//!   - an exclusive advisory lock on `<file name>.lock` beside the file
//!     (`std::fs::File::try_lock`, which is `flock` on Unix and `LockFileEx`
//!     on Windows).
//!
//!   Between Morpheus threads and processes, on a filesystem that honors
//!   those locks (local disks; some network filesystems do not), the
//!   comparison and the replacement are one step. No other Morpheus writer
//!   can change the file in between. Where the lock file cannot be used,
//!   see the last section.
//! - A file that differs from the expected revision is never written over.
//!   That includes other bytes, a file that was removed, and a file created
//!   while the revision expected none. Any difference that is on disk when
//!   the comparison reads is detected.
//! - Creating a file that was absent uses tempfile's no-clobber rename, so a
//!   file that appears after the comparison is not replaced either. Creation
//!   fails closed: any failure of that rename (a filesystem without it, a
//!   permission error, any other I/O error) returns the original error and
//!   removes the staged file. It is never retried as a replacing rename.
//! - The new text appears whole or not at all. It is written and synced to a
//!   temporary file in the same folder, then renamed over the file.
//!
//! # What is not guaranteed
//!
//! Other programs (text editors, sync clients, scripts) do not take the
//! advisory lock, and nothing here can make them. For an existing file, the
//! comparison and the rename are separate system calls. A write by another
//! program that lands after the comparison has read the file, and before the
//! rename completes, is replaced, and that version is lost. The window is
//! one bounded read (at most `MAX_CONFIG_BYTES`) plus one rename, with the
//! new text already staged. This is a pre-write comparison that narrows the
//! window. It is not an atomic filesystem compare-and-swap: the standard
//! library has no way to replace a file only if it is unchanged.
//!
//! A program that keeps the old file open and writes into it after the
//! rename writes to the replaced file, which is no longer the settings file.
//!
//! The version displaced in that window is not kept anywhere and cannot be
//! recovered. Preserving it needs a platform primitive that atomically
//! exchanges two directory entries, so the displaced file survives under
//! the staging name (`renameat2(RENAME_EXCHANGE)` on Linux,
//! `renameatx_np(RENAME_SWAP)` on macOS). That is native code awaiting
//! review, not implemented here. Even with it, a program that writes
//! continuously can still have later versions replaced; only the one version
//! displaced by each publication would be recoverable.
//!
//! # Waiting for other writers
//!
//! [`ConfigStore::commit`] never waits. Settings and the theme studio call it
//! on the UI thread, so a commit that finds another Morpheus writer
//! mid-publish (this process or another) returns [`CommitError::Busy`] at
//! once, having written nothing. [`ConfigStore::save`] and
//! [`ConfigStore::load_or_create`] wait up to `WRITER_LOCK_WAIT` for both
//! locks; they run on background tasks and at startup.
//!
//! # The lock file never blocks saves on its own
//!
//! The lock is the operating system's lock on an open handle, not the
//! file's existence:
//! - `flock` and `LockFileEx` locks are released when the holder closes the
//!   handle or exits, crashes included. A `config.toml.lock` left behind
//!   locks nothing.
//! - A live holder makes a commit fail with [`CommitError::Busy`] at once,
//!   and a save fail after at most `WRITER_LOCK_WAIT`. Nothing waits on
//!   another writer indefinitely. A Morpheus process suspended while holding
//!   the lock makes other processes' saves fail with `Busy` until it resumes
//!   or exits.
//! - A lock file that cannot be opened or locked never stops a save. That
//!   covers a file left by another user, a link or folder in its place, and
//!   a filesystem without locks. The save then proceeds with this process's
//!   path claim only, so between processes it has just the comparison, as for
//!   other programs.
//!
//! Nothing in Morpheus lists the settings folder or counts its files; the
//! lock file is the only file this adds there.

use std::fmt;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{
    Config, ConfigError, ConfigStore, InputError, MAX_CONFIG_BYTES, diagnostics, parse_toml,
    read_bounded,
};

/// How long `save` and `load_or_create` wait for the writer locks before
/// giving up with [`CommitError::Busy`]. `commit` does not wait. UI commits
/// hold their locks through staging and publication; background writers stage
/// before acquiring them.
const WRITER_LOCK_WAIT: Duration = Duration::from_secs(2);
const WRITER_LOCK_POLL: Duration = Duration::from_millis(10);

/// The settings files this process is publishing right now, on any thread.
/// A writer claims its path before the file lock and releases it after, so
/// this process holds at most one file lock per path. Writers of different
/// paths never wait for each other. The mutex itself is held only to look
/// up, add or remove a path.
static PUBLISHING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The exact text of the settings file as one read found it, or its
/// absence.
///
/// Comparing the whole text has no timestamp-granularity or same-size blind
/// spots. A file changed and changed back to the same bytes counts as
/// unchanged, which loses nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfigRevision(Option<Arc<str>>);

impl ConfigRevision {
    /// The revision of a file that does not exist.
    pub fn absent() -> Self {
        Self(None)
    }

    /// The file's exact text, or `None` when there was no file.
    pub fn text(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// The configuration this revision holds, checked as
    /// [`ConfigStore::load`] checks a file. An absent file is
    /// [`InputError::Missing`].
    pub fn config(&self) -> Result<Config, ConfigError> {
        parse_config(self.text().ok_or(InputError::Missing)?)
    }
}

/// Never shows the text, which is the person's file.
impl fmt::Debug for ConfigRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigRevision")
            .field("bytes", &self.0.as_ref().map(|text| text.len()))
            .finish()
    }
}

/// Why a commit wrote nothing. In every case the settings file is left as
/// it was found, and the caller keeps its change.
#[derive(Debug, Error)]
pub enum CommitError {
    /// The file is no longer the revision the change was decided on.
    /// `current` is the version found (absent if it was removed). It was not
    /// written over, so the caller can show it next to its own change.
    #[error("the settings file changed after it was read; nothing was written")]
    Changed { current: ConfigRevision },
    /// Another Morpheus writer (a window or a background task) was saving
    /// settings at that moment. `commit` does not wait for it.
    #[error("another Morpheus window or task is saving settings right now; nothing was written")]
    Busy,
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl CommitError {
    /// For the unconditional writers (`save`, `load_or_create`), whose
    /// callers expect a `ConfigError`.
    pub(crate) fn into_config_error(self) -> ConfigError {
        match self {
            Self::Config(error) => error,
            Self::Busy => ConfigError::Io(std::io::Error::new(
                ErrorKind::WouldBlock,
                Self::Busy.to_string(),
            )),
            changed @ Self::Changed { .. } => {
                ConfigError::Io(std::io::Error::other(changed.to_string()))
            }
        }
    }
}

pub(crate) fn parse_config(raw: &str) -> Result<Config, ConfigError> {
    let config: Config = parse_toml(raw, diagnostics::FileKind::Config)?;
    config.validate()?;
    Ok(config)
}

impl ConfigStore {
    /// [`ConfigStore::load`], together with the exact text that was read. A
    /// change decided on this configuration is committed over that revision.
    pub fn load_revision(&self) -> Result<(Config, ConfigRevision), ConfigError> {
        let raw = read_bounded(&self.path, MAX_CONFIG_BYTES)?;
        let config = parse_config(&raw)?;
        Ok((config, ConfigRevision(Some(raw.into()))))
    }

    /// Write `config` only if the file still holds `expected`, and return
    /// the revision written. See the module documentation for the exact
    /// guarantee and the remaining race with other programs.
    pub fn commit(
        &self,
        config: &Config,
        expected: &ConfigRevision,
    ) -> Result<ConfigRevision, CommitError> {
        self.commit_with(config, expected, |_| {})
    }

    /// [`ConfigStore::commit`], running `before_publish` at the pre-publish
    /// boundary. It runs after the new text is staged and both writer locks
    /// are held, immediately before the file is compared with `expected`
    /// and replaced.
    ///
    /// Tests put an intervening edit there. Anything it does to the file is
    /// what another program could do at that moment. A write through a
    /// `ConfigStore` from there finds the writer locks held and fails with
    /// [`CommitError::Busy`]: at once for `commit`, after the wait for
    /// `save`.
    #[doc(hidden)]
    pub fn commit_with(
        &self,
        config: &Config,
        expected: &ConfigRevision,
        before_publish: impl FnOnce(&Path),
    ) -> Result<ConfigRevision, CommitError> {
        self.publish(config, Some(expected), Duration::ZERO, before_publish)
    }

    /// Create the file with `config` only while there is none, waiting for
    /// other writers like `save`. Used by `load_or_create` at startup.
    pub(crate) fn create(&self, config: &Config) -> Result<ConfigRevision, CommitError> {
        self.publish(
            config,
            Some(&ConfigRevision::absent()),
            WRITER_LOCK_WAIT,
            |_| {},
        )
    }

    /// Replace the file with `config` whatever it holds, waiting for other
    /// writers. Used by `save`.
    pub(crate) fn replace(&self, config: &Config) -> Result<ConfigRevision, CommitError> {
        self.publish(config, None, WRITER_LOCK_WAIT, |_| {})
    }

    fn publish(
        &self,
        config: &Config,
        expected: Option<&ConfigRevision>,
        wait: Duration,
        before_publish: impl FnOnce(&Path),
    ) -> Result<ConfigRevision, CommitError> {
        self.publish_placed(config, expected, wait, before_publish, place)
    }

    /// Take the UI writer locks before staging; background writers stage first.
    /// Then compare with `expected` (when given) and publish with `place`.
    /// Without `expected`, the file is replaced whatever it holds, but never
    /// between another Morpheus writer's comparison and rename.
    fn publish_placed(
        &self,
        config: &Config,
        expected: Option<&ConfigRevision>,
        wait: Duration,
        before_publish: impl FnOnce(&Path),
        place: impl FnOnce(tempfile::NamedTempFile, &Path, Placement) -> std::io::Result<()>,
    ) -> Result<ConfigRevision, CommitError> {
        // A contended UI commit returns Busy before validation, encoding or
        // staging I/O. Its draft stays with the caller; after contention clears,
        // the same draft must still pass the ordinary validation below.
        let early_writer = if wait.is_zero() {
            Some(lock_writers(&self.path, wait)?)
        } else {
            None
        };
        // Background writers retain stage-first behavior. The UI's early lock
        // is held through staging and publication, and drops on any stage error.
        // On every early return, dropping `staging` removes the temporary file.
        let (staging, raw) = self.stage(config)?;
        let _writers = match early_writer {
            Some(mut writer) => {
                // A missing parent could not hold an advisory lock before
                // staging created it. Retry now, with the process claim still
                // held; never reacquire an advisory lock we already own.
                if writer._file.is_none() {
                    writer._file = lock_file(&self.path, Instant::now())?;
                }
                writer
            }
            None => lock_writers(&self.path, wait)?,
        };
        before_publish(self.path.as_path());
        let placement = match expected {
            None => Placement::Replace,
            Some(expected) => {
                let current = self.current_revision()?;
                if current != *expected {
                    return Err(CommitError::Changed { current });
                }
                // For an existing file, the remaining window: another
                // program's write that lands between the read above and the
                // rename is replaced (see the module documentation).
                if expected.0.is_some() {
                    Placement::Replace
                } else {
                    Placement::CreateNew
                }
            }
        };
        match place(staging, self.path.as_path(), placement) {
            Ok(()) => Ok(ConfigRevision(Some(raw.into()))),
            // A file that appeared after the comparison is kept.
            Err(error)
                if placement == Placement::CreateNew
                    && error.kind() == ErrorKind::AlreadyExists =>
            {
                Err(CommitError::Changed {
                    current: self.current_revision()?,
                })
            }
            // Fails closed: the original error, nothing retried, and the
            // staged file already removed by `place`.
            Err(error) => Err(ConfigError::Io(error).into()),
        }
    }

    /// The file as it is now. An absent file is a revision. A file that
    /// cannot be read is an error, and it is never written over.
    fn current_revision(&self) -> Result<ConfigRevision, ConfigError> {
        match read_bounded(&self.path, MAX_CONFIG_BYTES) {
            Ok(raw) => Ok(ConfigRevision(Some(raw.into()))),
            Err(InputError::Missing) => Ok(ConfigRevision(None)),
            Err(error) => Err(error.into()),
        }
    }

    fn stage(&self, config: &Config) -> Result<(tempfile::NamedTempFile, String), ConfigError> {
        config.validate()?;
        let parent = self.path.parent().ok_or_else(|| {
            ConfigError::Invalid("configuration path has no parent directory".into())
        })?;
        fs::create_dir_all(parent)?;
        let raw = toml::to_string_pretty(config)?;
        // A unique staging file in the same directory keeps writers out of
        // each other's files and lets the rename replace the destination in
        // one step on each platform. If the rename fails, the previous
        // config stays in place.
        let mut staging = tempfile::NamedTempFile::new_in(parent)?;
        staging.write_all(raw.as_bytes())?;
        staging.as_file().sync_all()?;
        Ok((staging, raw))
    }
}

/// How a staged file is moved into place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// Rename over whatever is at the target.
    Replace,
    /// Rename only while nothing is at the target.
    CreateNew,
}

/// Move `staging` to `target`. `CreateNew` uses tempfile's no-clobber
/// rename (`renameat2(RENAME_NOREPLACE)` or `renameatx_np(RENAME_EXCL)`, an
/// exclusive hard link where those are missing, or `MoveFileExW` without
/// replace) and never replaces anything. On any failure the staged file is
/// removed (dropping tempfile's `PersistError` deletes it) and the original
/// error is returned.
fn place(
    staging: tempfile::NamedTempFile,
    target: &Path,
    placement: Placement,
) -> std::io::Result<()> {
    #[cfg(windows)]
    if placement == Placement::Replace {
        return replace_staged(staging, target);
    }
    let placed = match placement {
        Placement::Replace => staging.persist(target).map(drop),
        Placement::CreateNew => staging.persist_noclobber(target).map(drop),
    };
    placed.map_err(|error| error.error)
}

/// Windows' legacy replacing move rejects an open destination, including a
/// bounded read by our watcher. Rust's pinned `fs::rename` handles that case
/// with FileRenameInfoEx and POSIX replacement semantics. Readers retain the
/// old file while subsequent opens see the new one. No permission is relaxed:
/// readonly targets and handles that deny delete sharing still fail.
#[cfg(windows)]
fn replace_staged(mut staging: tempfile::NamedTempFile, target: &Path) -> std::io::Result<()> {
    // Apply the same normal attributes as tempfile::persist before the move.
    // This second TempPath never owns cleanup: the original NamedTempFile
    // remains armed on every attribute/rename error, without a keep/rename gap.
    let mut attributes = tempfile::TempPath::try_from_path(staging.path())?;
    attributes.disable_cleanup(true);
    attributes.keep().map_err(|error| error.error)?;
    fs::rename(staging.path(), target)?;
    // Only a successful move disarms cleanup of the old staging name.
    staging.disable_cleanup(true);
    Ok(())
}

/// This process's claim on one path in [`PUBLISHING`], released on drop.
struct ProcessClaim(PathBuf);

impl ProcessClaim {
    fn take(path: &Path) -> Option<Self> {
        // A writer that panicked (a test hook) left nothing half done here:
        // the file is only ever replaced by a rename, and its claim was
        // released by unwinding.
        let mut publishing = PUBLISHING.lock().unwrap_or_else(PoisonError::into_inner);
        if publishing.iter().any(|claimed| claimed == path) {
            return None;
        }
        publishing.push(path.to_path_buf());
        Some(Self(path.to_path_buf()))
    }
}

impl Drop for ProcessClaim {
    fn drop(&mut self) {
        let mut publishing = PUBLISHING.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = publishing.iter().position(|claimed| *claimed == self.0) {
            publishing.swap_remove(index);
        }
    }
}

/// The writer locks held for one publish. Fields drop in order: the file
/// lock (when one was taken) is released before the process claim.
struct WriterLock {
    _file: Option<fs::File>,
    _process: ProcessClaim,
}

/// Take both writer locks, waiting at most `wait` in all (no waiting at all
/// for `Duration::ZERO`), so a caller on the UI thread is never blocked by
/// another writer.
fn lock_writers(target: &Path, wait: Duration) -> Result<WriterLock, CommitError> {
    let deadline = Instant::now() + wait;
    let process = loop {
        match ProcessClaim::take(target) {
            Some(process) => break process,
            None if Instant::now() < deadline => thread::sleep(WRITER_LOCK_POLL),
            None => return Err(CommitError::Busy),
        }
    };
    Ok(WriterLock {
        _file: lock_file(target, deadline)?,
        _process: process,
    })
}

/// Take only the advisory lock, keeping the caller's process claim alive.
/// A zero-wait retry after staging covers a parent directory just created there.
fn lock_file(target: &Path, deadline: Instant) -> Result<Option<fs::File>, CommitError> {
    // A lock file that cannot be opened (another user's, a link or a folder
    // in its place) must never stop saves; the comparison still runs.
    let Ok(file) = open_lock_file(&lock_path(target)?) else {
        return Ok(None);
    };
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(WRITER_LOCK_POLL);
            }
            Err(fs::TryLockError::WouldBlock) => return Err(CommitError::Busy),
            // A filesystem without locks: the same fallback.
            Err(fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// `<file name>.lock` beside the settings file. It holds no data and is
/// left in place, because removing a lock file races with the next writer
/// opening it.
fn lock_path(target: &Path) -> Result<PathBuf, ConfigError> {
    let name = target
        .file_name()
        .ok_or_else(|| ConfigError::Invalid("configuration path has no file name".into()))?;
    let mut lock = name.to_owned();
    lock.push(".lock");
    Ok(target.with_file_name(lock))
}

fn open_lock_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A link planted in the lock's place is refused, not followed.
        // Nothing is ever written to the lock file either way.
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The real publication, except that the move into place fails as a
    /// filesystem without a no-clobber rename, or any I/O error, would.
    fn failing(
        error: ErrorKind,
        also: impl FnOnce(&Path),
    ) -> impl FnOnce(tempfile::NamedTempFile, &Path, Placement) -> std::io::Result<()> {
        move |staging, target, _| {
            // As `place` does on failure: the staged file is removed.
            drop(staging);
            also(target);
            Err(std::io::Error::from(error))
        }
    }

    #[test]
    fn creating_fails_closed_on_any_error_but_already_exists() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut placement = None;

        let result = store.publish_placed(
            &Config::default(),
            Some(&ConfigRevision::absent()),
            Duration::ZERO,
            |_| {},
            |staging, target, how| {
                placement = Some(how);
                failing(ErrorKind::Unsupported, |_| {})(staging, target, how)
            },
        );

        assert_eq!(placement, Some(Placement::CreateNew));
        match result {
            Err(CommitError::Config(ConfigError::Io(error))) => {
                assert_eq!(error.kind(), ErrorKind::Unsupported, "the original error");
            }
            other => panic!("expected the original I/O error, got {other:?}"),
        }
        // No replacing rename was tried instead: nothing was created, and no
        // staged file is left behind.
        assert!(!store.path().exists());
        assert_eq!(names(directory.path()), ["config.toml.lock"]);
    }

    #[test]
    fn a_file_appearing_when_creation_fails_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));

        let result = store.publish_placed(
            &Config::default(),
            Some(&ConfigRevision::absent()),
            Duration::ZERO,
            |_| {},
            failing(ErrorKind::PermissionDenied, |target| {
                fs::write(target, "# another program's file\n").unwrap();
            }),
        );

        match result {
            Err(CommitError::Config(ConfigError::Io(error))) => {
                assert_eq!(error.kind(), ErrorKind::PermissionDenied);
            }
            other => panic!("expected the original I/O error, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(store.path()).unwrap(),
            "# another program's file\n"
        );
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
    }

    #[test]
    fn a_failed_replacement_keeps_the_file_and_the_original_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let (base, revision) = {
            store.load_or_create().unwrap();
            store.load_revision().unwrap()
        };
        let before = fs::read(store.path()).unwrap();
        let mut changed = base.clone();
        changed.geometry.corner_radius = 3;

        let result = store.publish_placed(
            &changed,
            Some(&revision),
            Duration::ZERO,
            |_| {},
            failing(ErrorKind::Other, |_| {}),
        );

        assert!(
            matches!(&result, Err(CommitError::Config(ConfigError::Io(error))) if error.kind() == ErrorKind::Other),
            "{result:?}"
        );
        assert_eq!(fs::read(store.path()).unwrap(), before);
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
    }

    #[test]
    fn the_real_creation_never_replaces_and_cleans_up() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config.toml");
        fs::write(&target, "kept").unwrap();
        #[cfg(windows)]
        let _reader = fs::File::open(&target).unwrap();
        let mut staging = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        staging.write_all(b"new").unwrap();

        let error = place(staging, &target, Placement::CreateNew).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
        assert_eq!(names(directory.path()), ["config.toml"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_replacement_keeps_open_readers_on_the_original_revision() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        let (_, revision) = store.load_revision().unwrap();
        let before = fs::read(store.path()).unwrap();
        let mut reader = fs::File::open(store.path()).unwrap();
        config.geometry.corner_radius = 3;

        store.commit(&config, &revision).unwrap();

        assert_eq!(store.load().unwrap(), config);
        let mut still_readable = Vec::new();
        reader.read_to_end(&mut still_readable).unwrap();
        assert_eq!(still_readable, before, "the old handle keeps the old file");
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_replacement_refuses_a_reader_that_does_not_share_delete() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        let (_, revision) = store.load_revision().unwrap();
        let before = fs::read(store.path()).unwrap();
        // FILE_SHARE_READ | FILE_SHARE_WRITE, deliberately no FILE_SHARE_DELETE.
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0001 | 0x0002)
            .open(store.path())
            .unwrap();
        config.geometry.corner_radius = 3;
        let mut attempted = false;
        let result = store.publish_placed(
            &config,
            Some(&revision),
            Duration::ZERO,
            |_| {},
            |staging, target, placement| {
                attempted = true;
                assert_eq!(placement, Placement::Replace);
                place(staging, target, placement)
            },
        );

        drop(reader);
        assert!(
            attempted,
            "reach the real placement, not an earlier I/O failure"
        );
        assert!(matches!(
            result,
            Err(CommitError::Config(ConfigError::Io(_)))
        ));
        assert_eq!(fs::read(store.path()).unwrap(), before);
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_replacement_refuses_a_readonly_target() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        let (_, revision) = store.load_revision().unwrap();
        let before = fs::read(store.path()).unwrap();
        let mut permissions = fs::metadata(store.path()).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(store.path(), permissions.clone()).unwrap();
        config.geometry.corner_radius = 3;
        let mut attempted = false;
        let result = store.publish_placed(
            &config,
            Some(&revision),
            Duration::ZERO,
            |_| {},
            |staging, target, placement| {
                attempted = true;
                assert_eq!(placement, Placement::Replace);
                place(staging, target, placement)
            },
        );
        let retained = fs::read(store.path()).unwrap();
        let still_readonly = fs::metadata(store.path()).unwrap().permissions().readonly();
        // Restore our synthetic file for owned TempDir cleanup before assertions.
        permissions.set_readonly(false);
        fs::set_permissions(store.path(), permissions).unwrap();

        assert!(
            attempted,
            "reach the real placement, not an earlier I/O failure"
        );
        assert!(matches!(
            result,
            Err(CommitError::Config(ConfigError::Io(_)))
        ));
        assert!(
            still_readonly,
            "replacement never clears the target's readonly bit"
        );
        assert_eq!(retained, before);
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
    }

    #[test]
    fn a_busy_process_claim_never_creates_a_staging_directory() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("not-created");
        let store = ConfigStore::at(parent.join("config.toml"));
        let _claim = ProcessClaim::take(store.path()).unwrap();

        let result = store.commit(&Config::default(), &ConfigRevision::absent());

        assert!(matches!(result, Err(CommitError::Busy)), "{result:?}");
        assert!(!parent.exists(), "a busy commit must not stage on disk");
    }

    #[test]
    fn a_busy_advisory_lock_defers_validation_without_losing_the_draft() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        store.load_or_create().unwrap();
        let (_, revision) = store.load_revision().unwrap();
        let before = fs::read(store.path()).unwrap();
        let holder = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path(store.path()).unwrap())
            .unwrap();
        holder.lock().unwrap();
        let mut draft = Config::default();
        draft.geometry.corner_radius = u16::MAX;

        let result = store.commit(&draft, &revision);

        assert!(matches!(result, Err(CommitError::Busy)), "{result:?}");
        assert_eq!(draft.geometry.corner_radius, u16::MAX);
        assert_eq!(fs::read(store.path()).unwrap(), before);
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
        holder.unlock().unwrap();
        let result = store.commit(&draft, &revision);
        assert!(matches!(result, Err(CommitError::Config(_))), "{result:?}");
        assert_eq!(fs::read(store.path()).unwrap(), before);
        assert_eq!(names(directory.path()), ["config.toml", "config.toml.lock"]);
    }

    #[test]
    fn a_new_parent_gets_its_advisory_lock_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("not-created/config.toml"));
        let config = Config::default();
        let mut checked = false;

        store
            .commit_with(&config, &ConfigRevision::absent(), |path| {
                let competing = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(lock_path(path).unwrap())
                    .unwrap();
                assert!(matches!(
                    competing.try_lock(),
                    Err(fs::TryLockError::WouldBlock)
                ));
                checked = true;
            })
            .unwrap();

        assert!(checked);
        assert_eq!(store.load().unwrap(), config);
    }

    #[test]
    fn a_commit_never_waits_for_another_morpheus_writer() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let base = store.load_or_create().unwrap();
        let (_, revision) = store.load_revision().unwrap();
        let mut changed = base.clone();
        changed.geometry.corner_radius = 3;
        let mut inner = None;
        let mut inner_elapsed = None;

        store
            .commit_with(&changed, &revision, |_| {
                // Another writer while this one holds the locks, as a UI
                // save during a background save would be.
                // Only the contended call belongs to this responsiveness check;
                // the outer writer also stages, syncs and publishes its file.
                let started = Instant::now();
                inner = Some(store.commit(&changed, &revision));
                inner_elapsed = Some(started.elapsed());
            })
            .unwrap();

        assert!(matches!(inner, Some(Err(CommitError::Busy))), "{inner:?}");
        assert!(inner_elapsed.unwrap() < Duration::from_millis(500));
        assert_eq!(store.load().unwrap().geometry.corner_radius, 3);
    }
}
