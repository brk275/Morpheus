use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn image(metadata: &PackageMetadata, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[8..11].copy_from_slice(b"AI\x02");
    bytes[18] = 62;
    bytes.extend_from_slice(body);
    let metadata = serde_json::to_vec(metadata).unwrap();
    bytes.extend_from_slice(&metadata);
    bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    bytes.extend_from_slice(FOOTER_MAGIC);
    bytes
}

struct Fixture {
    root: tempfile::TempDir,
    installed: PathBuf,
    old: Vec<u8>,
    next: Vec<u8>,
    next_metadata: PackageMetadata,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("Morpheus.AppImage");
        let old = image(&PackageMetadata::embedded(), b"old application");
        fs::write(&installed, &old).unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o700)).unwrap();
        let mut next_metadata = PackageMetadata::embedded();
        let version =
            cargo_packager_updater::semver::Version::parse(&next_metadata.version).unwrap();
        next_metadata.version = format!("{}.0.0", version.major + 1);
        let next = image(&next_metadata, b"new application");
        Self {
            root,
            installed,
            old,
            next,
            next_metadata,
        }
    }

    fn prepared(&self) -> AppImageSession {
        let mut session = AppImageSession::lock(&self.installed, Duration::ZERO).unwrap();
        session
            .prepare(&self.next, &self.next_metadata.version)
            .unwrap();
        session
    }
}

#[test]
fn atomic_replacement_retains_old_bytes_on_same_filesystem_and_rolls_back() {
    let fixture = Fixture::new();
    let account = fixture.root.path().join("crypto-database-sentinel");
    fs::write(&account, b"never rewind encryption state").unwrap();
    let session = fixture.prepared();
    let backup = session.state.join("previous.AppImage");
    assert_eq!(
        fs::metadata(&backup).unwrap().dev(),
        fs::metadata(&fixture.installed).unwrap().dev()
    );
    assert_ne!(
        fs::metadata(&backup).unwrap().ino(),
        fs::metadata(&fixture.installed).unwrap().ino()
    );
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
    let mut journal = session.journal().unwrap().unwrap();
    session.apply(&mut journal).unwrap();
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.next);
    assert_eq!(fs::read(backup).unwrap(), fixture.old);
    session.rollback(&mut journal).unwrap();
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
    assert!(session.journal().unwrap().is_none());
    assert_eq!(fs::read(account).unwrap(), b"never rewind encryption state");
}

#[test]
fn interruption_at_each_replace_boundary_recovers_without_missing_installed_path() {
    for point in 0..4 {
        let fixture = Fixture::new();
        let session = fixture.prepared();
        let mut journal = session.journal().unwrap().unwrap();
        if point >= 1 {
            journal.phase = Phase::ApplyIntent;
            session.save(&journal).unwrap();
        }
        if point >= 2 {
            fs::rename(session.state.join("next.AppImage"), &fixture.installed).unwrap();
        }
        if point >= 3 {
            journal.phase = Phase::AwaitingHealth;
            session.save(&journal).unwrap();
        }
        assert!(fixture.installed.is_file());
        drop(session); // Simulate loss of process state/RAII values.
        let recovered = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
        let mut journal = recovered.journal().unwrap().unwrap();
        recovered.rollback(&mut journal).unwrap();
        assert_eq!(
            fs::read(&fixture.installed).unwrap(),
            fixture.old,
            "cut point {point}"
        );
    }
}

#[test]
fn interrupted_rollback_recognizes_already_restored_old_image() {
    let fixture = Fixture::new();
    let session = fixture.prepared();
    let mut journal = session.journal().unwrap().unwrap();
    session.apply(&mut journal).unwrap();
    journal.phase = Phase::RollbackIntent;
    session.save(&journal).unwrap();
    fs::rename(session.state.join("previous.AppImage"), &fixture.installed).unwrap();
    drop(session);
    let recovered = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
    recovered
        .rollback(&mut recovered.journal().unwrap().unwrap())
        .unwrap();
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
}

#[test]
fn committed_pre_store_acknowledgement_never_allows_later_binary_rollback() {
    let fixture = Fixture::new();
    let mut session = fixture.prepared();
    let mut journal = session.journal().unwrap().unwrap();
    session.apply(&mut journal).unwrap();
    session.trial = Some(journal.id.clone());
    session.acknowledge(&fixture.next_metadata).unwrap();
    drop(session);
    let recovered = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
    let mut journal = recovered.journal().unwrap().unwrap();
    assert_eq!(journal.phase, Phase::Committed);
    assert_eq!(
        recovered.rollback(&mut journal),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.next);
    // Cleanup interrupted after backup deletion is also repeatable.
    fs::remove_file(recovered.state.join("previous.AppImage")).unwrap();
    recovered.clean().unwrap();
    recovered.clean().unwrap();
}

#[test]
fn changed_persistence_or_unsigned_feed_version_mismatch_fails_before_staging() {
    let fixture = Fixture::new();
    let mut session = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
    let mut mismatch = fixture.next_metadata.clone();
    mismatch.persistence = "different crypto schema/code".into();
    assert_eq!(
        session.prepare(&image(&mismatch, b"new"), &mismatch.version),
        Err(UpdateError::IncompatibleUpdate)
    );
    assert_eq!(
        session.prepare(&fixture.next, "99.0.0"),
        Err(UpdateError::IncompatibleUpdate)
    );
    let mut wrong_arch = fixture.next.clone();
    wrong_arch[18] = 183;
    assert_eq!(
        session.prepare(&wrong_arch, &fixture.next_metadata.version),
        Err(UpdateError::UnsupportedPackage)
    );
    assert!(!session.state.join("previous.AppImage").exists());
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
}

#[test]
fn mutated_candidate_backup_and_unrelated_installation_are_not_overwritten() {
    for target in ["next.AppImage", "previous.AppImage", "installed"] {
        let fixture = Fixture::new();
        let session = fixture.prepared();
        let path = if target == "installed" {
            fixture.installed.clone()
        } else {
            session.state.join(target)
        };
        fs::write(&path, b"independent changed content").unwrap();
        let mut journal = session.journal().unwrap().unwrap();
        assert_eq!(
            session.apply(&mut journal),
            Err(UpdateError::RecoveryRequired)
        );
        assert_eq!(fs::read(&path).unwrap(), b"independent changed content");
        if target != "installed" {
            assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
        }
    }
}

#[test]
fn symlink_journal_and_second_process_lock_fail_closed() {
    let fixture = Fixture::new();
    let session = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
    assert!(matches!(
        AppImageSession::lock(&fixture.installed, Duration::ZERO),
        Err(UpdateError::UpdateActive)
    ));
    let other = fixture.root.path().join("private-account-data");
    fs::write(&other, b"must not be read as a journal").unwrap();
    symlink(&other, session.state.join("journal.json")).unwrap();
    assert!(matches!(
        session.journal(),
        Err(UpdateError::RecoveryRequired)
    ));
    assert_eq!(fs::read(other).unwrap(), b"must not be read as a journal");
}

#[test]
fn corrupt_new_image_cannot_acknowledge_and_durable_old_image_is_retained() {
    let fixture = Fixture::new();
    let mut session = fixture.prepared();
    let mut journal = session.journal().unwrap().unwrap();
    session.apply(&mut journal).unwrap();
    session.trial = Some(journal.id);
    fs::write(&fixture.installed, b"disk corruption").unwrap();
    assert_eq!(
        session.acknowledge(&fixture.next_metadata),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(
        fs::read(session.state.join("previous.AppImage")).unwrap(),
        fixture.old
    );
}

#[test]
fn malformed_footer_is_bounded_and_never_falls_back_to_executing_probe() {
    let fixture = Fixture::new();
    let mut malformed = fixture.next;
    let length = malformed.len() - FOOTER_MAGIC.len() - 4;
    malformed[length..length + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(package_metadata(&malformed), Err(UpdateError::CorruptStage));
    assert_eq!(
        package_metadata(b"not an executable"),
        Err(UpdateError::UnsupportedPackage)
    );
}

#[test]
fn cancelled_preparation_releases_call_exclusion_and_owned_files() {
    let fixture = Fixture::new();
    let mut session = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
    let activity = super::super::UpdateActivity::default();
    let prepared = prepare_restart(
        &mut session,
        &fixture.next,
        &fixture.next_metadata.version,
        activity.try_install().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        activity.try_start_call(),
        Err(UpdateError::UpdateActive)
    ));
    drop(prepared);
    assert!(session.journal().unwrap().is_none());
    assert!(!session.state.join("previous.AppImage").exists());
    assert!(activity.try_start_call().is_ok());
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
}

#[test]
fn failed_supervisor_spawn_preserves_current_app_and_releases_call_exclusion() {
    let fixture = Fixture::new();
    let mut session = AppImageSession::lock(&fixture.installed, Duration::ZERO).unwrap();
    let activity = super::super::UpdateActivity::default();
    let prepared = prepare_restart(
        &mut session,
        &fixture.next,
        &fixture.next_metadata.version,
        activity.try_install().unwrap(),
    )
    .unwrap();
    // Fixture bytes have valid AppImage identification/metadata but no ELF
    // program headers, so the kernel cannot execute this synthetic helper.
    assert_eq!(
        prepared.launch_supervisor(),
        Err(UpdateError::RestartFailed)
    );
    assert!(session.journal().unwrap().is_none());
    assert!(activity.try_start_call().is_ok());
    assert_eq!(fs::read(&fixture.installed).unwrap(), fixture.old);
}

fn process_fixture_command(mode: &str, root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg("updater::linux_transaction::tests::process_fixture_child")
        .arg("--nocapture")
        .env("MORPHEUS_UPDATE_PROCESS_FIXTURE_MODE", mode)
        .env("MORPHEUS_UPDATE_PROCESS_FIXTURE_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for_fixture_lock(root: &Path) -> File {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_to_string(root.join("ready"))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .is_some()
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.join("held-lock"))
                .unwrap();
            assert!(matches!(
                file.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            return file;
        }
        assert!(Instant::now() < deadline, "fixture did not hold its lock");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_fixture_lock_released(file: &File) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match file.try_lock() {
            Ok(()) => return,
            Err(std::fs::TryLockError::WouldBlock) => {
                assert!(
                    Instant::now() < deadline,
                    "owned descendant retained the lifetime lock"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("could not observe fixture lock: {error}"),
        }
    }
}

// Re-executed by process tests instead of forking from a multithreaded test
// harness or depending on Python/flock binaries. Ordinary runs return at once.
#[test]
fn process_fixture_child() {
    let Ok(mode) = std::env::var("MORPHEUS_UPDATE_PROCESS_FIXTURE_MODE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("MORPHEUS_UPDATE_PROCESS_FIXTURE_ROOT").unwrap());
    match mode.as_str() {
        "leader" => {
            // Inherit the leader's process group. std::process::Child's Drop
            // deliberately leaves this descendant alive when the leader exits.
            let _descendant = process_fixture_command("descendant", &root)
                .spawn()
                .unwrap();
            let _lock_probe = wait_for_fixture_lock(&root);
        }
        "descendant" => {
            // SAFETY: this isolated fixture process installs a constant signal
            // disposition; the parent must use group SIGKILL to stop it.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(root.join("held-lock"))
                .unwrap();
            file.try_lock().unwrap();
            fs::write(root.join("ready"), std::process::id().to_string()).unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        _ => panic!("unknown process fixture mode"),
    }
}

#[test]
fn exited_leader_is_not_reaped_before_resistant_descendant_group_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let mut owned = OwnedChild::spawn(&mut process_fixture_command("leader", root.path())).unwrap();
    let lock = wait_for_fixture_lock(root.path());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !owned.exited().unwrap() {
        assert!(Instant::now() < deadline, "fixture leader did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Repeated observation succeeds because WNOWAIT leaves the zombie/PID
    // owned by us. Descendants still hold their lock after that leader exits.
    assert!(owned.exited().unwrap());
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    owned.stop().unwrap();
    assert_fixture_lock_released(&lock);
    assert!(owned.child.is_none());
    owned.stop().unwrap(); // Idempotent; never signal a previously reaped PID.
}

#[test]
fn early_error_after_spawn_cleans_the_owned_process_group() {
    let root = tempfile::tempdir().unwrap();
    let result: Result<(), UpdateError> = (|| {
        let _owned = OwnedChild::spawn(&mut process_fixture_command("descendant", root.path()))?;
        let observed = wait_for_fixture_lock(root.path());
        drop(observed);
        // Represents a malformed/unreadable journal after successful spawn.
        Err(UpdateError::RecoveryRequired)
    })();
    assert_eq!(result, Err(UpdateError::RecoveryRequired));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("held-lock"))
        .unwrap();
    assert_fixture_lock_released(&lock);
}

#[test]
fn confirmed_handoff_does_not_kill_the_committed_application() {
    let root = tempfile::tempdir().unwrap();
    let owned = OwnedChild::spawn(&mut process_fixture_command("descendant", root.path())).unwrap();
    let lock = wait_for_fixture_lock(root.path());
    let handed_off = owned.handoff();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    // Reclaim the still-unreaped child only for fixture cleanup. Production
    // hands it off and exits; the continuing application must stay alive.
    let mut cleanup = OwnedChild {
        child: Some(handed_off),
    };
    assert!(!cleanup.exited().unwrap());
    cleanup.stop().unwrap();
    assert_fixture_lock_released(&lock);
}
