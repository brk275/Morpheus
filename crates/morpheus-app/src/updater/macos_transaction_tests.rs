use super::*;

fn metadata(version: &str) -> PackageMetadata {
    let mut identity = PackageMetadata::embedded().unwrap();
    identity.version = version.into();
    identity
}

// Inert native header + identity section. Never executed; application startup
// and signing/installed acceptance require separate fresh hosted fixtures.
fn macho(identity: &PackageMetadata) -> Vec<u8> {
    let json = serde_json::to_vec(identity).unwrap();
    let mut bytes = vec![0; 512 + json.len()];
    let cpu: u32 = if std::env::consts::ARCH == "aarch64" {
        0x0100_000c
    } else {
        0x0100_0007
    };
    for (offset, value) in [
        (0, 0xfeedfacf_u32),
        (4, cpu),
        (12, 2),
        (16, 1),
        (20, 152),
        (32, 0x19),
        (36, 152),
        (32 + 60, 5),
        (32 + 64, 1),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[40..46].copy_from_slice(b"__TEXT");
    let len = bytes.len() as u64;
    bytes[80..88].copy_from_slice(&len.to_le_bytes());
    let section = 32 + 72;
    bytes[section..section + 14].copy_from_slice(b"__morpheus_upd");
    bytes[section + 16..section + 22].copy_from_slice(b"__TEXT");
    bytes[section + 40..section + 48].copy_from_slice(&(json.len() as u64).to_le_bytes());
    bytes[section + 48..section + 52].copy_from_slice(&512_u32.to_le_bytes());
    bytes[512..].copy_from_slice(&json);
    bytes
}

fn bundle(path: &Path, identity: &PackageMetadata) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    for part in ["Contents", "Contents/MacOS", "Contents/Resources"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path.join(part))
            .unwrap();
    }
    fs::write(path.join(EXECUTABLE), macho(identity)).unwrap();
    fs::set_permissions(path.join(EXECUTABLE), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        path.join("Contents/Resources/theme.toml"),
        b"preserve resources",
    )
    .unwrap();
    fs::write(
        path.join("Contents/Info.plist"),
        format!(
            "<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{}</string>\
         <key>CFBundleExecutable</key><string>morpheus-app</string>\
         <key>CFBundleShortVersionString</key><string>{}</string>\
         <key>CFBundlePackageType</key><string>APPL</string></dict></plist>",
            identity.product, identity.version
        ),
    )
    .unwrap();
}

fn prepared() -> (tempfile::TempDir, MacAppSession, Journal) {
    let root = tempfile::tempdir().unwrap();
    let installed = root.path().join("Morpheus.app");
    let current = PackageMetadata::embedded().unwrap();
    bundle(&installed, &current);
    let session = MacAppSession::lock(&installed, Duration::ZERO).unwrap();
    copy_bundle(&installed, &session.state.join("previous.app")).unwrap();
    let next = metadata("9.0.0");
    bundle(&session.state.join("next.app"), &next);
    let journal = Journal {
        protocol: 1,
        id: "FixtureTransaction1".into(),
        phase: Phase::Prepared,
        old_hash: tree_hash(&installed).unwrap(),
        new_hash: tree_hash(&session.state.join("next.app")).unwrap(),
        candidate: next,
    };
    session.save(&journal).unwrap();
    (root, session, journal)
}

fn awaiting_store_health() -> (tempfile::TempDir, MacAppSession, Journal) {
    let (root, session, mut journal) = prepared();
    remove_bundle(&session.state.join("next.app")).unwrap();
    journal.candidate = PackageMetadata::embedded().unwrap();
    bundle(&session.state.join("next.app"), &journal.candidate);
    fs::write(
        session.state.join("next.app/Contents/Resources/theme.toml"),
        b"new compatible resources",
    )
    .unwrap();
    journal.new_hash = tree_hash(&session.state.join("next.app")).unwrap();
    session.save(&journal).unwrap();
    session.apply(&mut journal).unwrap();
    (root, session, journal)
}

#[test]
fn native_identity_rejects_wrong_cpu_writable_or_truncated_sections_and_unknown_fields() {
    let identity = PackageMetadata::embedded().unwrap();
    let valid = macho(&identity);
    assert_eq!(executable_metadata(&valid).unwrap(), identity);
    for (offset, value) in [
        (4, 1_u32),
        (12, 6),
        (20, 8),
        (32 + 60, 7),
        (104 + 48, u32::MAX),
        (104 + 64, 1),
    ] {
        let mut invalid = valid.clone();
        invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(executable_metadata(&invalid).is_err(), "offset {offset}");
    }
    for len in [0, 31, 183, 511] {
        assert!(executable_metadata(&valid[..len]).is_err());
    }
    let json = serde_json::to_string(&identity).unwrap();
    let duplicate = json.replacen("{", "{\"protocol\":2,", 1);
    assert!(PackageMetadata::parse(duplicate.as_bytes()).is_err());
    let unknown = json.replacen("{", "{\"plugin\":true,", 1);
    assert!(PackageMetadata::parse(unknown.as_bytes()).is_err());
    assert!(PackageMetadata::parse(&vec![b' '; 4097]).is_err());
}

#[test]
fn bundle_identity_binds_plist_resources_file_modes_and_internal_links() {
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("Morpheus.app");
    let identity = PackageMetadata::embedded().unwrap();
    bundle(&app, &identity);
    assert_eq!(bundle_metadata(&app).unwrap(), identity);
    let original = tree_hash(&app).unwrap();
    fs::write(app.join("Contents/Resources/theme.toml"), b"changed").unwrap();
    assert_ne!(tree_hash(&app).unwrap(), original);
    fs::write(
        app.join("Contents/Resources/theme.toml"),
        b"preserve resources",
    )
    .unwrap();
    assert_eq!(tree_hash(&app).unwrap(), original);
    std::os::unix::fs::symlink("theme.toml", app.join("Contents/Resources/link")).unwrap();
    assert_ne!(tree_hash(&app).unwrap(), original);
    fs::remove_file(app.join("Contents/Resources/link")).unwrap();
    std::os::unix::fs::symlink(root.path(), app.join("Contents/Resources/outside")).unwrap();
    assert!(tree_hash(&app).is_err());
    // Owned failed staging cleanup unlinks the bad link, never its target.
    let sentinel = root.path().join("sentinel");
    fs::write(&sentinel, b"keep").unwrap();
    remove_bundle(&app).unwrap();
    assert_eq!(fs::read(sentinel).unwrap(), b"keep");
}

#[test]
fn real_unsigned_producer_roundtrips_exact_link_modes_through_native_reader_and_backup_copy() {
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("Morpheus.app");
    let identity = PackageMetadata::embedded().unwrap();
    bundle(&app, &identity);
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let purposes = fs::read_to_string(repository.join("packaging/macos/Info.plist")).unwrap();
    let purposes = purposes
        .split_once("<dict>")
        .unwrap()
        .1
        .rsplit_once("</dict>")
        .unwrap()
        .0;
    let plist = fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        plist.replace("</dict>", &format!("{purposes}</dict>")),
    )
    .unwrap();
    for (name, source) in [
        ("aurora.toml", "themes/aurora.toml"),
        ("LICENSE", "LICENSE"),
        ("Symphonia-MPL-2.0.txt", "licenses/Symphonia-MPL-2.0.txt"),
        ("Symphonia-NOTICE.txt", "licenses/Symphonia-NOTICE.txt"),
    ] {
        let output = app.join("Contents/Resources").join(name);
        fs::copy(repository.join(source), &output).unwrap();
        fs::set_permissions(output, fs::Permissions::from_mode(0o444)).unwrap();
    }
    let modes = [
        ("ordinary-link", 0o777),
        ("private-link", 0o700),
        ("readable-link", 0o640),
        ("owner-read-link", 0o400),
        ("all-read-link", 0o444),
    ];
    for (name, mode) in modes {
        let path = app.join("Contents/Resources").join(name);
        std::os::unix::fs::symlink("LICENSE", &path).unwrap();
        cargo_packager_updater::macos_archive::restore_owned_symlink_mode(&path, mode).unwrap();
    }
    let expected = tree_hash(&app).unwrap();
    let archive = root.path().join("Morpheus.app.tar.gz");
    // Only the reviewed producer runs on synthetic fixture data, on the fresh
    // Mac runner. The inert Mach-O bytes are never executed or installed.
    let produced = Command::new("python3")
        .args(["-I", "-B"])
        .arg(repository.join("scripts/prepare_macos_update.py"))
        .arg("--app")
        .arg(&app)
        .arg("--output")
        .arg(&archive)
        .arg("--version")
        .arg(&identity.version)
        .arg("--arch")
        .arg(&identity.arch)
        .env_remove("CARGO_PACKAGER_SIGN_PRIVATE_KEY")
        .env_remove("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")
        .output()
        .unwrap();
    assert!(
        produced.status.success(),
        "producer failed: {}",
        String::from_utf8_lossy(&produced.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(
        produced
            .stdout
            .strip_prefix(b"MORPHEUS_MAC_UPDATE ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["bundle_sha256"].as_str(), Some(expected.as_str()));
    assert_eq!(receipt["signed"], false);
    let bytes = fs::read(&archive).unwrap();
    let destination = root.path().join("extracted");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&destination)
        .unwrap();
    let unpacked =
        cargo_packager_updater::macos_archive::unpack_verified_bundle(&bytes, &destination)
            .unwrap();
    assert_eq!(bundle_metadata(&unpacked).unwrap(), identity);
    assert_eq!(tree_hash(&unpacked).unwrap(), expected);
    let backup = root.path().join("backup.app");
    copy_bundle(&unpacked, &backup).unwrap();
    assert_eq!(tree_hash(&backup).unwrap(), expected);
    assert_eq!(tree_hash(&app).unwrap(), expected);
    for bundle in [&app, &unpacked, &backup] {
        for (name, mode) in modes {
            assert_eq!(
                fs::symlink_metadata(bundle.join("Contents/Resources").join(name))
                    .unwrap()
                    .mode()
                    & 0o7777,
                mode
            );
        }
        assert_eq!(
            fs::metadata(bundle.join("Contents/Resources/LICENSE"))
                .unwrap()
                .mode()
                & 0o7777,
            0o444
        );
    }
}

#[test]
fn atomic_bundle_swap_and_failed_trial_restore_complete_previous_app() {
    let (_root, session, mut journal) = prepared();
    session.apply(&mut journal).unwrap();
    assert_eq!(journal.phase, Phase::AwaitingHealth);
    assert_eq!(tree_hash(&session.installed).unwrap(), journal.new_hash);
    assert_eq!(
        tree_hash(&session.state.join("previous.app")).unwrap(),
        journal.old_hash
    );
    assert_eq!(
        tree_hash(&session.state.join("next.app")).unwrap(),
        journal.old_hash
    );
    session.rollback(&mut journal).unwrap();
    assert_eq!(tree_hash(&session.installed).unwrap(), journal.old_hash);
    assert!(session.journal().unwrap().is_none());
}

#[test]
fn crash_between_swap_and_health_journal_is_recoverable_without_deleting_old_app() {
    let (_root, session, mut journal) = prepared();
    journal.phase = Phase::ApplyIntent;
    session.save(&journal).unwrap();
    swap_bundles(&session.installed, &session.state.join("next.app")).unwrap();
    let mut interrupted = session.journal().unwrap().unwrap();
    session.rollback(&mut interrupted).unwrap();
    assert_eq!(tree_hash(&session.installed).unwrap(), journal.old_hash);
}

#[test]
fn committed_transaction_and_unrelated_replacement_can_never_roll_back() {
    let (_root, session, mut journal) = prepared();
    session.apply(&mut journal).unwrap();
    journal.phase = Phase::Committed;
    session.save(&journal).unwrap();
    assert_eq!(
        session.rollback(&mut journal),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(tree_hash(&session.installed).unwrap(), journal.new_hash);
    journal.phase = Phase::AwaitingHealth;
    fs::write(
        session.installed.join("Contents/Resources/theme.toml"),
        b"unrelated change",
    )
    .unwrap();
    assert_eq!(
        session.rollback(&mut journal),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(
        tree_hash(&session.state.join("previous.app")).unwrap(),
        journal.old_hash
    );
}

#[test]
fn corrupt_stage_never_replaces_app_and_lifetime_lock_excludes_a_second_session() {
    let (_root, session, mut journal) = prepared();
    let old = journal.old_hash.clone();
    fs::write(
        session.state.join("next.app/Contents/Resources/theme.toml"),
        b"tampered",
    )
    .unwrap();
    assert_eq!(
        session.apply(&mut journal),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(tree_hash(&session.installed).unwrap(), old);
    assert!(matches!(
        MacAppSession::lock(&session.installed, Duration::ZERO),
        Err(UpdateError::UpdateActive)
    ));
}

#[test]
fn abandoned_restart_preserves_installed_app_and_releases_shared_call_gate() {
    let (_root, mut session, journal) = prepared();
    let activity = super::super::UpdateActivity::default();
    let restart = PreparedMacRestart {
        session: &mut session,
        id: journal.id,
        lease: Some(activity.try_install().unwrap()),
        handed_off: false,
    };
    assert!(matches!(
        activity.try_start_call(),
        Err(UpdateError::UpdateActive)
    ));
    drop(restart);
    assert_eq!(tree_hash(&session.installed).unwrap(), journal.old_hash);
    assert!(session.journal().unwrap().is_none());
    assert!(activity.try_start_call().is_ok());
}

#[test]
fn public_install_gate_stays_closed_and_journal_rejects_links_and_unbounded_fields() {
    assert!(!INSTALLATION_VALIDATED);
    let (_root, mut session, journal) = prepared();
    let activity = super::super::UpdateActivity::default();
    assert!(matches!(
        prepare_restart(
            &mut session,
            b"untrusted",
            "9.0.0",
            activity.try_install().unwrap()
        ),
        Err(UpdateError::InstallNotValidated)
    ));
    assert_eq!(tree_hash(&session.installed).unwrap(), journal.old_hash);
    fs::remove_file(session.state.join("journal.json")).unwrap();
    std::os::unix::fs::symlink(
        session.installed.join(EXECUTABLE),
        session.state.join("journal.json"),
    )
    .unwrap();
    assert!(session.journal().is_err());
}

#[test]
fn health_commit_before_store_is_durable_and_cannot_be_reversed() {
    let (_root, mut session, journal) = awaiting_store_health();
    session.trial = Some(journal.id.clone());
    session.acknowledge_before_store().unwrap();
    let mut durable = session.journal().unwrap().unwrap();
    assert_eq!(durable.phase, Phase::Committed);
    assert_eq!(
        session.rollback(&mut durable),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(
        tree_hash(&session.state.join("previous.app")).unwrap(),
        journal.old_hash
    );
}

#[test]
fn owned_exit_fixture() {
    // Only the fresh hosted test process's explicitly selected child receives
    // this value. This is the native test harness, never the GUI application.
    if std::env::var_os("MORPHEUS_MAC_OWNED_EXIT_FIXTURE").as_deref()
        == Some(std::ffi::OsStr::new("exit23"))
    {
        std::process::exit(23);
    }
}

// Test-only registration for a retained direct child. The production constructor
// deliberately requires our actual parent, which cannot exit while retaining a
// reap capability for its monitoring child. This independently exercises the
// real kernel status payload through the unchanged production wait/predicate;
// the real-parent arm admission is covered below. It is not a helper-launch or
// ordinary application shutdown acceptance path.
fn monitor_retained_child_status(
    child: &OwnedChild,
    transaction: &str,
) -> Result<ParentExitMonitor, UpdateError> {
    if child.exited()? {
        return Err(UpdateError::RecoveryRequired);
    }
    let parent = child
        .child
        .as_ref()
        .and_then(|child| i32::try_from(child.id()).ok())
        .ok_or(UpdateError::RecoveryRequired)?;
    // SAFETY: successful kqueue returns a new descriptor owned by this call.
    let descriptor = unsafe { libc::kqueue() };
    if descriptor < 0 {
        return Err(UpdateError::RecoveryRequired);
    }
    // SAFETY: this is the exact successful new descriptor, closed only by RAII.
    let queue = unsafe { OwnedFd::from_raw_fd(descriptor) };
    if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(UpdateError::RecoveryRequired);
    }
    let monitor = ParentExitMonitor {
        queue,
        cookie: Box::new(ParentExitCookie {
            transaction: transaction.to_owned(),
        }),
        parent,
    };
    let change = libc::kevent {
        ident: parent as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT | libc::EV_RECEIPT,
        fflags: PARENT_EXIT_NOTES,
        data: 0,
        udata: monitor.cookie_pointer(),
    };
    // SAFETY: one initialized change, one writable plain output and a zero
    // timeout stay valid for this synchronous call on the retained queue.
    let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
    let immediate = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let count = unsafe {
        libc::kevent(
            monitor.queue.as_raw_fd(),
            &change,
            1,
            &mut receipt,
            1,
            &immediate,
        )
    };
    if count != 1 || !monitor.receipt_matches(&receipt) || child.exited()? {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(monitor)
}

#[test]
fn macos_parent_status_fixture() {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    let Some(mode) = std::env::var_os("MORPHEUS_MAC_PARENT_STATUS_FIXTURE") else {
        return;
    };
    let mode = match mode.to_str() {
        Some("zero") => 0,
        Some("failed17") => 17,
        Some("abrupt86") => 86,
        Some("owned-kill") => -1,
        _ => std::process::exit(91),
    };
    let descriptor = match std::io::stdin().as_fd().try_clone_to_owned() {
        Ok(descriptor) => descriptor,
        Err(_) => std::process::exit(92),
    };
    let mut channel = UnixStream::from(descriptor);
    if channel
        .set_read_timeout(Some(Duration::from_secs(10)))
        .is_err()
        || channel
            .set_write_timeout(Some(Duration::from_secs(1)))
            .is_err()
        || channel.write_all(b"R").is_err()
    {
        std::process::exit(93);
    }
    // Stay alive until the owning test has registered the real kernel filter.
    // The kill case receives no release: it is killed through the actual Child.
    let mut release = [0_u8; 1];
    if channel.read_exact(&mut release).is_err() || release != *b"G" {
        std::process::exit(94);
    }
    if mode == 86 {
        // SAFETY: terminate only this explicitly selected disposable fixture;
        // _exit intentionally skips every Rust/atexit cleanup path.
        unsafe { libc::_exit(86) }
    }
    if mode < 0 {
        std::process::exit(95);
    }
    std::process::exit(mode);
}

#[test]
fn real_owned_child_status_allows_only_zero_through_the_production_parent_wait() {
    use std::os::unix::{net::UnixStream, process::ExitStatusExt};

    let executable = std::env::current_exe().unwrap();
    let module = module_path!().split_once("::").unwrap().1;
    let selector = format!("{module}::macos_parent_status_fixture");
    for (fixture, expected_code) in [
        ("zero", Some(0)),
        ("failed17", Some(17)),
        ("abrupt86", Some(86)),
        ("owned-kill", None),
    ] {
        let (mut channel, actor) = UnixStream::pair().unwrap();
        let mut command = Command::new(&executable);
        command
            .args(["--exact", &selector, "--nocapture"])
            .env("MORPHEUS_MAC_PARENT_STATUS_FIXTURE", fixture)
            .stdin(Stdio::from(OwnedFd::from(actor)))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = OwnedChild::spawn(&mut command).unwrap();
        drop(command); // No retained child-side stdin alias in the owner.
        let observation = (|| -> Result<_, UpdateError> {
            channel
                .set_read_timeout(Some(Duration::from_secs(10)))
                .map_err(|_| UpdateError::RecoveryRequired)?;
            channel
                .set_write_timeout(Some(Duration::from_secs(1)))
                .map_err(|_| UpdateError::RecoveryRequired)?;
            let mut ready = [0_u8; 1];
            channel
                .read_exact(&mut ready)
                .map_err(|_| UpdateError::RecoveryRequired)?;
            if ready != *b"R" {
                return Err(UpdateError::RecoveryRequired);
            }
            let monitor = monitor_retained_child_status(&child, "OwnedKernelStatus1")?;
            if fixture == "owned-kill" {
                child
                    .child
                    .as_mut()
                    .ok_or(UpdateError::RecoveryRequired)?
                    .kill()
                    .map_err(|_| UpdateError::RecoveryRequired)?;
            } else {
                channel
                    .write_all(b"G")
                    .map_err(|_| UpdateError::RecoveryRequired)?;
            }
            let result = monitor.wait("OwnedKernelStatus1", Duration::from_secs(10));
            // Kernel event delivery and waitable-child publication can be
            // adjacent. Observe real exit without reaping or cleanup signals.
            let deadline = Instant::now() + Duration::from_secs(5);
            let exited_before_cleanup = loop {
                if child.exited()? {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            Ok((result, exited_before_cleanup))
        })();
        // Always finish real child ownership before any observation assertion,
        // including failed setup/registration/wait paths. Never use a PID lookup
        // or a process/group-name signal. Lost child ownership forbids killing.
        let cleanup = (|| -> Result<_, UpdateError> {
            if !child.exited()? {
                let _ = child
                    .child
                    .as_mut()
                    .ok_or(UpdateError::RecoveryRequired)?
                    .kill();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if child.exited()? {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(UpdateError::RecoveryRequired);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            child.reap_owned()
        })();
        let status = match cleanup {
            Ok(status) => status,
            Err(_) => {
                // Prevent OwnedChild::Drop from guessing a signal after lost
                // ownership. Preserve its actual owner for ephemeral-host
                // disposal and fail; never claim unconfirmed cleanup passed.
                std::mem::forget(child);
                panic!("Mac parent-status fixture cleanup could not be confirmed");
            }
        };
        let (result, exited_before_cleanup) = observation.unwrap();
        assert!(
            exited_before_cleanup,
            "Mac status fixture needed cleanup to exit"
        );
        assert!(child.child.is_none(), "Actual child was not reaped");
        if let Some(code) = expected_code {
            assert_eq!(status.code(), Some(code));
        } else {
            assert_eq!(status.signal(), Some(libc::SIGKILL));
        }
        if expected_code == Some(0) {
            assert_eq!(result, Ok(()));
        } else {
            assert_eq!(result, Err(UpdateError::RecoveryRequired));
        }
    }
}

#[test]
fn exited_owned_trial_reaps_only_its_leader_and_reports_unproved_group_cleanup() {
    let executable = std::env::current_exe().unwrap();
    // libtest names omit the crate prefix present in module_path!().
    let module = module_path!().split_once("::").unwrap().1;
    let mut command = Command::new(executable);
    command
        .arg("--exact")
        .arg(format!("{module}::owned_exit_fixture"))
        .arg("--nocapture")
        .env("MORPHEUS_MAC_OWNED_EXIT_FIXTURE", "exit23")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = OwnedChild::spawn(&mut command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !child.exited().unwrap() {
        assert!(Instant::now() < deadline, "owned fixture did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    // WNOWAIT has left the exact owned Child available as the group anchor.
    assert!(child.child.is_some());
    match child.stop_before_health() {
        Err(TrialStopError::ExitedLeaderGroupUnproven(status)) => {
            assert_eq!(status.code(), Some(23));
        }
        other => panic!("expected explicit Darwin cleanup uncertainty: {other:?}"),
    }
    assert!(child.child.is_none());
    assert_eq!(child.exited(), Err(UpdateError::RecoveryRequired));
    // No cached numeric identity survives the reap for Drop or another stop.
    child.stop().unwrap();
}

#[test]
fn serialized_trial_recovery_restores_old_tree_and_late_health_cannot_open_stores() {
    let (_root, session, journal) = awaiting_store_health();
    let installed = session.installed.clone();
    drop(session);
    assert_eq!(
        recover_failed_trial(&installed, &journal),
        Ok(TrialRecovery::RolledBack)
    );
    let mut late = MacAppSession::lock(&installed, Duration::ZERO).unwrap();
    assert_eq!(tree_hash(&installed).unwrap(), journal.old_hash);
    assert!(late.journal().unwrap().is_none());
    late.trial = Some(journal.id);
    assert_eq!(
        late.acknowledge_before_store(),
        Err(UpdateError::RecoveryRequired)
    );
    assert!(late.journal().unwrap().is_none());
    assert_eq!(tree_hash(&installed).unwrap(), journal.old_hash);
}

#[test]
fn serialized_trial_recovery_preserves_health_commit_that_won_the_lock() {
    let (_root, mut session, journal) = awaiting_store_health();
    let installed = session.installed.clone();
    session.trial = Some(journal.id.clone());
    session.acknowledge_before_store().unwrap();
    drop(session);
    assert_eq!(
        recover_failed_trial(&installed, &journal),
        Ok(TrialRecovery::Committed)
    );
    let session = MacAppSession::lock(&installed, Duration::ZERO).unwrap();
    assert_eq!(session.journal().unwrap().unwrap().phase, Phase::Committed);
    assert_eq!(tree_hash(&installed).unwrap(), journal.new_hash);
    assert_eq!(
        tree_hash(&session.state.join("previous.app")).unwrap(),
        journal.old_hash
    );
}

#[test]
fn serialized_trial_recovery_rejects_other_transactions_phases_and_changed_tree() {
    let (_root, session, journal) = awaiting_store_health();
    let installed = session.installed.clone();
    let state = session.state.clone();
    drop(session);
    for field in 0..6 {
        let mut other = journal.clone();
        match field {
            0 => other.id.push('2'),
            1 => other.candidate.version = "99.0.0".into(),
            2 => other.old_hash = "0".repeat(64),
            3 => other.new_hash = "0".repeat(64),
            4 => other.protocol = 2,
            _ => other.phase = Phase::Prepared,
        }
        assert_eq!(
            recover_failed_trial(&installed, &other),
            Err(UpdateError::RecoveryRequired)
        );
        assert_eq!(tree_hash(&installed).unwrap(), journal.new_hash);
        assert_eq!(
            tree_hash(&state.join("previous.app")).unwrap(),
            journal.old_hash
        );
    }
    let session = MacAppSession::lock(&installed, Duration::ZERO).unwrap();
    let mut unrelated = journal.clone();
    unrelated.phase = Phase::Prepared;
    session.save(&unrelated).unwrap();
    drop(session);
    assert_eq!(
        recover_failed_trial(&installed, &journal),
        Err(UpdateError::RecoveryRequired)
    );
    let session = MacAppSession::lock(&installed, Duration::ZERO).unwrap();
    session.save(&journal).unwrap();
    fs::write(installed.join("Contents/Resources/theme.toml"), b"changed").unwrap();
    let changed = tree_hash(&installed).unwrap();
    drop(session);
    assert_eq!(
        recover_failed_trial(&installed, &journal),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(tree_hash(&installed).unwrap(), changed);
    assert_eq!(
        MacAppSession::read_journal(&state).unwrap().unwrap().phase,
        Phase::AwaitingHealth
    );
}

#[test]
fn parent_exit_registration_is_owned_and_receipts_cannot_authorize_apply() {
    // This real native registration observes the test process's live parent;
    // it does not signal it or assume anything from its process name/PID poll.
    // SAFETY: getppid has no pointer arguments or side effects.
    let parent = unsafe { libc::getppid() };
    let monitor = ParentExitMonitor::arm(parent, "FixtureParentExit1").unwrap();
    let event = |flags| libc::kevent {
        ident: parent as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags,
        fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
        data: 0,
        udata: monitor.cookie_pointer(),
    };
    let receipt = event(libc::EV_ERROR | libc::EV_RECEIPT);
    assert!(monitor.receipt_matches(&receipt));
    assert!(!monitor.exit_matches(&receipt));
    let exit = event(libc::EV_EOF | libc::EV_ONESHOT | libc::EV_CLEAR);
    assert!(monitor.exit_matches(&exit));
    assert!(!monitor.receipt_matches(&exit));
    for field in 0..9 {
        let mut invalid = event(libc::EV_EOF | libc::EV_ONESHOT | libc::EV_CLEAR);
        match field {
            0 => invalid.ident = (parent as libc::uintptr_t).wrapping_add(1),
            1 => invalid.filter = libc::EVFILT_READ,
            2 => invalid.udata = std::ptr::null_mut(),
            3 => invalid.flags |= libc::EV_ERROR,
            4 => invalid.flags &= !libc::EV_EOF,
            5 => invalid.flags &= !libc::EV_ONESHOT,
            6 => invalid.fflags |= libc::NOTE_FORK,
            7 => invalid.fflags = libc::NOTE_EXIT,
            _ => invalid.data = 1,
        }
        assert!(!monitor.exit_matches(&invalid), "event field {field}");
    }
    // Darwin's NOTE_EXITSTATUS carries wait status, not a plain exit code.
    // These predicate checks do not substitute for actual hosted kernel exits.
    for status in [
        1 << 8,
        255 << 8,
        libc::SIGTERM,
        libc::SIGABRT | 0x80,
        1 << 16,
        -1,
    ] {
        let mut failed_exit = event(libc::EV_EOF | libc::EV_ONESHOT);
        failed_exit.data = status as libc::intptr_t;
        assert!(!monitor.exit_matches(&failed_exit), "wait status {status}");
    }
    for error in [libc::ESRCH, libc::EACCES] {
        let mut failed_receipt = event(libc::EV_ERROR | libc::EV_RECEIPT);
        failed_receipt.data = error as libc::intptr_t;
        assert!(!monitor.receipt_matches(&failed_receipt));
    }
    let mut status_missing = event(libc::EV_ERROR | libc::EV_RECEIPT);
    status_missing.fflags = libc::NOTE_EXIT;
    assert!(!monitor.receipt_matches(&status_missing));
    // The real live-parent registration also proves that the kernel accepted
    // this exact status request. Its receipt still cannot authorize apply.
    // Only a later normal-success exit can authorize the separate lock step.
    assert_eq!(
        monitor.wait("FixtureParentExit1", Duration::from_millis(10)),
        Err(UpdateError::RecoveryRequired)
    );
}

#[test]
fn parent_exit_monitor_rejects_wrong_parent_transaction_and_noncanonical_arguments() {
    for parent in [-1, 0, 1, std::process::id() as libc::pid_t] {
        assert!(matches!(
            ParentExitMonitor::arm(parent, "FixtureParentExit1"),
            Err(UpdateError::RecoveryRequired)
        ));
    }
    // SAFETY: getppid has no pointer arguments or side effects.
    let parent = unsafe { libc::getppid() };
    let monitor = ParentExitMonitor::arm(parent, "FixtureParentExit1").unwrap();
    assert_eq!(
        monitor.wait("OtherTransaction", Duration::from_millis(10)),
        Err(UpdateError::RecoveryRequired)
    );
    for invalid in [
        "",
        "0",
        "1",
        "-2",
        "+2",
        "02",
        " 2",
        "2 ",
        "2147483648",
        "99999999999",
    ] {
        assert_eq!(
            parse_parent_pid(std::ffi::OsStr::new(invalid)),
            Err(UpdateError::RecoveryRequired)
        );
    }
    assert_eq!(parse_parent_pid(std::ffi::OsStr::new("42")), Ok(42));
}

#[test]
fn copied_file_mode_is_restored_through_owned_handle_without_changing_global_umask() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("payload");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    restore_payload_mode(&file, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o755);
    restore_payload_mode(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o644);
}

#[test]
fn plist_identity_requires_one_root_dictionary_and_top_level_unique_scalar_keys() {
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("Morpheus.app");
    let identity = PackageMetadata::embedded().unwrap();
    bundle(&app, &identity);
    let valid = fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
    validate_plist(&valid, &identity).unwrap();
    let body = valid
        .strip_prefix("<plist version=\"1.0\"><dict>")
        .unwrap()
        .strip_suffix("</dict></plist>")
        .unwrap();
    for invalid in [
        format!("<plist version=\"1.0\"><dict><key>nested</key><dict>{body}</dict></dict></plist>"),
        format!("<plist version=\"1.0\"><dict></dict></plist>{body}"),
        format!(
            "<plist version=\"1.0\"><dict><key>array</key><array><dict>{body}</dict></array></dict></plist>"
        ),
        format!("{valid}{valid}"),
        valid.replace(
            "</dict>",
            "<key>CFBundleExecutable</key><string>evil</string></dict>",
        ),
        valid.replace(
            "<string>morpheus-app</string>",
            "<string><![CDATA[morpheus-app]]></string>",
        ),
        valid.replace("<dict>", "<dict><!--"),
    ] {
        assert!(validate_plist(&invalid, &identity).is_err());
    }
    let with_header = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n{valid}"
    );
    validate_plist(&with_header, &identity).unwrap();
}

#[test]
fn full_media_flush_uses_owned_regular_descriptors_including_readonly_handles() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("payload");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"generated disposable update payload")
        .unwrap();
    full_sync_regular(&file).unwrap();
    // Extraction/copy closes its writing descriptors; later preparation flushes
    // the exact no-follow read descriptor. Unsupported FD/filesystem behavior
    // is a failing platform check, never silently accepted or downgraded.
    let readonly = open_file(&path).unwrap();
    full_sync_regular(&readonly).unwrap();
    let directory = File::open(root.path()).unwrap();
    assert_eq!(
        full_sync_regular(&directory),
        Err(UpdateError::RecoveryRequired)
    );
    file.set_permissions(fs::Permissions::from_mode(0o666))
        .unwrap();
    assert_eq!(full_sync_regular(&file), Err(UpdateError::RecoveryRequired));
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    fs::hard_link(&path, root.path().join("alias")).unwrap();
    assert_eq!(full_sync_regular(&file), Err(UpdateError::RecoveryRequired));
}

#[test]
fn journal_publication_retains_the_new_inode_and_recovery_retries_commit_flush() {
    let (_root, session, mut journal) = prepared();
    let mut previous = open_file(&session.state.join("journal.json")).unwrap();
    let old_inode = previous.metadata().unwrap().ino();
    journal.phase = Phase::ApplyIntent;
    session.save(&journal).unwrap();
    let published = open_file(&session.state.join("journal.json")).unwrap();
    assert_ne!(published.metadata().unwrap().ino(), old_inode);
    let mut old = Vec::new();
    previous.read_to_end(&mut old).unwrap();
    let previous: Journal = serde_json::from_slice(&old).unwrap();
    assert_eq!(previous.phase, Phase::Prepared);
    assert_eq!(
        session.journal().unwrap().unwrap().phase,
        Phase::ApplyIntent
    );
    journal.phase = Phase::Committed;
    session.save(&journal).unwrap();
    session.confirm_commit().unwrap();
    assert_eq!(session.journal().unwrap().unwrap().phase, Phase::Committed);
}

#[test]
fn helper_loader_cleanup_strips_every_raw_dyld_name_and_preserves_unrelated_environment() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let mut removed: Vec<OsString> = [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_FALLBACK_FRAMEWORK_PATH",
        "DYLD_ROOT_PATH",
        "DYLD_VERSIONED_LIBRARY_PATH",
        "DYLD_VERSIONED_FRAMEWORK_PATH",
        "DYLD_IMAGE_SUFFIX",
        "DYLD_FUTURE_FIXTURE",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    removed.push(OsString::from_vec(b"DYLD_\xff".to_vec()));
    let preserved: Vec<OsString> = [
        "PATH",
        "HOME",
        "MORPHEUS_FIXTURE_PROFILE",
        "DYLD",
        "XDYLD_IMAGE_SUFFIX",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let names: Vec<_> = removed.iter().chain(&preserved).cloned().collect();
    // This command is never spawned. Supply synthetic inherited names rather
    // than mutating process-global environment in parallel unit tests.
    let mut command = Command::new("/synthetic/never-executed");
    for name in &names {
        command.env(name, "fixture-value");
    }
    remove_loader_environment(&mut command, names);
    let recorded: std::collections::BTreeMap<_, _> = command
        .get_envs()
        .map(|(name, value)| (name.to_os_string(), value.map(|value| value.to_os_string())))
        .collect();
    for name in &removed {
        assert_eq!(recorded.get(name), Some(&None));
    }
    for name in &preserved {
        assert_eq!(
            recorded.get(name),
            Some(&Some(OsString::from("fixture-value")))
        );
    }
    assert_eq!(recorded.len(), removed.len() + preserved.len());
}

#[test]
fn unreadable_links_fail_identity_and_copy_without_mutation_but_safe_cleanup_can_unlink_them() {
    // Raw no-follow setup is intentionally test-only: the production restore
    // helper rejects these unsupported modes before any mutation.
    unsafe extern "C" {
        fn lchmod(path: *const std::ffi::c_char, mode: u16) -> std::ffi::c_int;
    }
    for mode in [0, 0o200, 0o100, 0o300, 0o077] {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Morpheus.app");
        bundle(&app, &PackageMetadata::embedded().unwrap());
        let target = app.join("Contents/Resources/theme.toml");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = app.join("Contents/Resources/unreadable-link");
        std::os::unix::fs::symlink("theme.toml", &link).unwrap();
        let raw = CString::new(link.as_os_str().as_bytes()).unwrap();
        // SAFETY: the private fixture path is NUL-terminated and lchmod acts
        // only on this new owned symlink, never its target.
        assert_eq!(unsafe { lchmod(raw.as_ptr(), mode) }, 0);
        let before = fs::symlink_metadata(&link).unwrap();
        assert!(matches!(
            tree_hash(&app),
            Err(UpdateError::RecoveryRequired)
        ));
        let copy = root.path().join("backup.app");
        assert!(copy_bundle(&app, &copy).is_err());
        assert!(!copy.exists());
        let after = fs::symlink_metadata(&link).unwrap();
        assert_eq!(
            (
                after.dev(),
                after.ino(),
                after.uid(),
                after.nlink(),
                after.mode()
            ),
            (
                before.dev(),
                before.ino(),
                before.uid(),
                before.nlink(),
                before.mode()
            )
        );
        assert_eq!(after.mode() & 0o7777, u32::from(mode));
        assert_eq!(fs::read(&target).unwrap(), b"preserve resources");
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o7777, 0o600);
        // An outside retained file proves cleanup unlinks only its own tree entries;
        // the preserved inode/bytes/mode survive outside the removed app.
        let retained = root.path().join("sentinel");
        fs::rename(&target, &retained).unwrap();
        // The original link is now dangling. A second unreadable link points
        // outside the tree; cleanup must unlink both without reading targets.
        let external = app.join("Contents/Resources/outside-link");
        std::os::unix::fs::symlink(&retained, &external).unwrap();
        let external_raw = CString::new(external.as_os_str().as_bytes()).unwrap();
        // SAFETY: the exact new private fixture link is owned and never followed.
        assert_eq!(unsafe { lchmod(external_raw.as_ptr(), 0) }, 0);
        remove_bundle(&app).unwrap();
        assert!(!app.exists());
        assert_eq!(fs::read(&retained).unwrap(), b"preserve resources");
        assert_eq!(fs::metadata(&retained).unwrap().mode() & 0o7777, 0o600);
    }
}

#[test]
fn native_identity_rejects_legacy_and_unknown_helper_protocols() {
    let current = PackageMetadata::embedded().unwrap();
    assert_eq!(current.protocol, 2);
    for protocol in [0, 1, 3, u8::MAX] {
        let mut rejected = current.clone();
        rejected.protocol = protocol;
        assert!(PackageMetadata::parse(&serde_json::to_vec(&rejected).unwrap()).is_err());
        assert!(executable_metadata(&macho(&rejected)).is_err());
        assert!(!rejected.compatible(&current));
        assert!(!current.compatible(&rejected));
    }
}
