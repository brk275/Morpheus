//! Version-aware commits at the real pre-publish boundary. Each test places
//! a deterministic edit, made the way another program would make it, after
//! the new text is staged and the writer locks are held and immediately
//! before the comparison and the rename. The edit must survive in the file,
//! and the caller's change must survive in the caller's hands.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use morpheus_config::{CommitError, Config, ConfigRevision, ConfigStore};

const EXTERNAL: &str = "# edited in another program\n[geometry]\ncorner_radius = 9\n";

fn store() -> (tempfile::TempDir, ConfigStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::at(directory.path().join("config.toml"));
    store.load_or_create().unwrap();
    (directory, store)
}

fn drafted(base: &Config) -> Config {
    let mut draft = base.clone();
    draft.geometry.corner_radius = 3;
    draft
}

fn changed(result: Result<ConfigRevision, CommitError>) -> ConfigRevision {
    match result {
        Err(CommitError::Changed { current }) => current,
        other => panic!("expected a typed conflict, got {other:?}"),
    }
}

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn an_edit_at_the_pre_publish_boundary_is_kept_with_the_draft() {
    let (directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();
    let draft = drafted(&base);

    let current = changed(store.commit_with(&draft, &revision, |path| {
        fs::write(path, EXTERNAL).unwrap();
    }));

    // The file keeps the other version, byte for byte.
    assert_eq!(fs::read_to_string(store.path()).unwrap(), EXTERNAL);
    // The conflict carries that version for the person to choose from.
    assert_eq!(current.text(), Some(EXTERNAL));
    assert_eq!(current.config().unwrap().geometry.corner_radius, 9);
    // The draft is the caller's and is untouched.
    assert_eq!(draft.geometry.corner_radius, 3);
    // Nothing staged is left behind: only the file and its lock.
    assert_eq!(entries(directory.path()), ["config.toml", "config.toml.lock"]);

    // Committing over the version now on disk succeeds.
    let (_, now) = store.load_revision().unwrap();
    assert_eq!(now, current);
    let written = store.commit(&draft, &now).unwrap();
    assert_eq!(store.load().unwrap().geometry.corner_radius, 3);
    assert_eq!(store.load_revision().unwrap().1, written);
}

#[test]
fn a_replacement_by_rename_at_the_boundary_is_kept() {
    let (directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();
    let staged = directory.path().join("editor-save.tmp");

    let current = changed(store.commit_with(&drafted(&base), &revision, |path| {
        // An editor's "atomic save": write elsewhere, rename over the file.
        fs::write(&staged, EXTERNAL).unwrap();
        fs::rename(&staged, path).unwrap();
    }));

    assert_eq!(current.text(), Some(EXTERNAL));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), EXTERNAL);
}

#[test]
fn a_removal_at_the_boundary_is_not_undone() {
    let (_directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();

    let current = changed(store.commit_with(&drafted(&base), &revision, |path| {
        fs::remove_file(path).unwrap();
    }));

    assert_eq!(current, ConfigRevision::absent());
    assert!(!store.path().exists(), "a removed file is not written again");
}

#[test]
fn a_file_created_at_the_boundary_is_never_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::at(directory.path().join("config.toml"));

    let current = changed(store.commit_with(
        &Config::default(),
        &ConfigRevision::absent(),
        |path| fs::write(path, EXTERNAL).unwrap(),
    ));

    assert_eq!(current.text(), Some(EXTERNAL));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), EXTERNAL);
}

#[test]
fn an_unreadable_replacement_at_the_boundary_is_not_written_over() {
    let (_directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();
    let invalid = [0xff_u8, 0xfe, 0x00];

    let result = store.commit_with(&drafted(&base), &revision, |path| {
        fs::write(path, invalid).unwrap();
    });

    assert!(matches!(result, Err(CommitError::Config(_))), "{result:?}");
    assert_eq!(fs::read(store.path()).unwrap(), invalid);
}

#[test]
fn an_unchanged_file_is_committed_and_the_new_revision_is_returned() {
    let (_directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();
    let mut hook_ran = false;

    let written = store
        .commit_with(&drafted(&base), &revision, |_| hook_ran = true)
        .unwrap();

    assert!(hook_ran);
    assert_eq!(store.load().unwrap().geometry.corner_radius, 3);
    assert_eq!(store.load_revision().unwrap().1, written);
    // The revision just written is stale once a later save lands.
    let mut later = store.load().unwrap();
    later.geometry.corner_radius = 5;
    store.save(&later).unwrap();
    changed(store.commit(&drafted(&base), &written));
    assert_eq!(store.load().unwrap().geometry.corner_radius, 5);
}

#[test]
fn another_holder_of_the_writer_lock_is_never_waited_for_by_a_commit() {
    let (directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();
    let before = fs::read_to_string(store.path()).unwrap();
    // Another Morpheus process would hold this lock while it compares and
    // renames.
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.path().join("config.toml.lock"))
        .unwrap();
    holder.lock().unwrap();

    // A commit (the UI's path) does not wait: Busy at once, nothing written.
    let started = Instant::now();
    let result = store.commit(&drafted(&base), &revision);
    assert!(matches!(result, Err(CommitError::Busy)), "{result:?}");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), before);

    // A save (background tasks, startup) waits a bounded time, then gives up.
    let started = Instant::now();
    let error = store.save(&drafted(&base)).unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(
        matches!(&error, morpheus_config::ConfigError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock),
        "{error:?}"
    );
    assert_eq!(fs::read_to_string(store.path()).unwrap(), before);

    holder.unlock().unwrap();
    store.commit(&drafted(&base), &revision).unwrap();
}

#[test]
fn a_lock_file_left_behind_never_blocks_a_save() {
    let (directory, store) = store();
    let lock = directory.path().join("config.toml.lock");
    // What a crashed or exited process leaves: the file, with no lock held.
    fs::write(&lock, "left behind").unwrap();
    let (base, revision) = store.load_revision().unwrap();

    let started = Instant::now();
    store.commit(&drafted(&base), &revision).unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(store.load().unwrap().geometry.corner_radius, 3);
    assert_eq!(fs::read_to_string(&lock).unwrap(), "left behind");
}

#[test]
fn a_lock_path_that_cannot_be_opened_never_stops_saves() {
    let directory = tempfile::tempdir().unwrap();
    // A folder where the lock file belongs cannot be opened as a file.
    fs::create_dir(directory.path().join("config.toml.lock")).unwrap();
    let store = ConfigStore::at(directory.path().join("config.toml"));

    let base = store.load_or_create().unwrap();
    let (_, revision) = store.load_revision().unwrap();
    store.commit(&drafted(&base), &revision).unwrap();
    store.save(&base).unwrap();

    // The comparison still runs without the file lock.
    let (_, revision) = store.load_revision().unwrap();
    changed(store.commit_with(&drafted(&base), &revision, |path| {
        fs::write(path, EXTERNAL).unwrap();
    }));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), EXTERNAL);
}

#[test]
fn revisions_and_conflicts_never_print_the_file() {
    let (_directory, store) = store();
    let (base, revision) = store.load_revision().unwrap();
    let secret_looking = "# private-marker-71\n[geometry]\ncorner_radius = 9\n";

    let error = store
        .commit_with(&drafted(&base), &revision, |path| {
            fs::write(path, secret_looking).unwrap();
        })
        .unwrap_err();

    for shown in [format!("{error}"), format!("{error:?}"), format!("{revision:?}")] {
        assert!(!shown.contains("private-marker-71"), "{shown}");
        assert!(!shown.contains("corner_radius"), "{shown}");
    }
}
