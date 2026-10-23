use super::*;
use payload::{MAGIC, UNINSTALL_CONTRACT};

fn identity(version: &str) -> Identity {
    Identity {
        protocol: 1,
        product: "io.github.brk275.morpheus".into(),
        version: version.into(),
        arch: "x86_64".into(),
        persistence: "a".repeat(64),
    }
}

fn pe(identity: &Identity) -> Vec<u8> {
    let bytes = serde_json::to_vec(identity).unwrap();
    assert!(bytes.len() <= 4096);
    let mut image = vec![0; 8192];
    image[..2].copy_from_slice(b"MZ");
    image[60..64].copy_from_slice(&128_u32.to_le_bytes());
    image[128..132].copy_from_slice(b"PE\0\0");
    image[132..134].copy_from_slice(&0x8664_u16.to_le_bytes());
    image[134..136].copy_from_slice(&1_u16.to_le_bytes());
    image[148..150].copy_from_slice(&240_u16.to_le_bytes());
    image[150..152].copy_from_slice(&2_u16.to_le_bytes());
    image[152..154].copy_from_slice(&0x20b_u16.to_le_bytes());
    image[220..222].copy_from_slice(&2_u16.to_le_bytes());
    image[392..400].copy_from_slice(b".mrphupd");
    image[400..404].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    image[408..412].copy_from_slice(&4096_u32.to_le_bytes());
    image[412..416].copy_from_slice(&4096_u32.to_le_bytes());
    image[428..432].copy_from_slice(&0x4000_0040_u32.to_le_bytes());
    image[4096..4096 + bytes.len()].copy_from_slice(&bytes);
    image
}

fn envelope(next: &Identity) -> (Vec<u8>, Manifest) {
    let files = FILES
        .into_iter()
        .map(|path| {
            if path == "morpheus-app.exe" {
                pe(next)
            } else {
                b"synthetic resource".to_vec()
            }
        })
        .collect::<Vec<_>>();
    let manifest = Manifest {
        schema: 1,
        identity: next.clone(),
        uninstaller_contract: UNINSTALL_CONTRACT.into(),
        files: FILES
            .into_iter()
            .zip(&files)
            .map(|(path, bytes)| Entry {
                path: path.into(),
                bytes: bytes.len() as u64,
                sha256: hash(bytes),
            })
            .collect(),
    };
    let marker = serde_json::to_vec(&manifest).unwrap();
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(marker.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&marker);
    for file in files {
        bytes.extend_from_slice(&file);
    }
    (bytes, manifest)
}

#[test]
fn windows_payload_is_data_not_a_signed_executable_installer() {
    let (bytes, manifest) = envelope(&identity("0.2.0"));
    assert_eq!(
        Payload::parse(&bytes, &identity("0.1.0"), "0.2.0")
            .unwrap()
            .manifest,
        manifest
    );
    assert!(Payload::parse(&pe(&identity("0.2.0")), &identity("0.1.0"), "0.2.0").is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Payload::parse(&trailing, &identity("0.1.0"), "0.2.0").is_err());
    let mut damaged = bytes.clone();
    *damaged.last_mut().unwrap() ^= 1;
    assert!(Payload::parse(&damaged, &identity("0.1.0"), "0.2.0").is_err());
    for end in [0, MAGIC.len(), MAGIC.len() + 3, bytes.len() - 1] {
        assert!(Payload::parse(&bytes[..end], &identity("0.1.0"), "0.2.0").is_err());
    }
}

#[test]
fn windows_payload_rejects_uninstaller_drift_unknown_paths_and_cohorts() {
    let (_, manifest) = envelope(&identity("0.2.0"));
    for path in [
        "../profile",
        "LICENSE:stream",
        "CON",
        "licenses/../LICENSE",
        "license",
        "uninstall.exe",
        "extra.dll",
    ] {
        let mut value = manifest.clone();
        value.files[0].path = path.into();
        assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut value = manifest.clone();
    value.uninstaller_contract = "b".repeat(64);
    assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value = manifest.clone();
    value.files.push(value.files[0].clone());
    assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut foreign = identity("0.2.0");
    foreign.persistence = "b".repeat(64);
    assert!(Payload::parse(&envelope(&foreign).0, &identity("0.1.0"), "0.2.0").is_err());
    assert!(Payload::parse(&envelope(&identity("0.2.0")).0, &identity("0.2.0"), "0.2.0").is_err());
    assert!(Payload::parse(&envelope(&identity("0.2.0")).0, &identity("0.1.0"), "0.3.0").is_err());
}

#[test]
fn windows_pe_identity_requires_a_unique_bounded_read_only_x64_gui_section() {
    let original = pe(&identity("0.2.0"));
    assert_eq!(pe_identity(&original).unwrap(), identity("0.2.0"));
    for (offset, bytes) in [
        (132, 0x14c_u16.to_le_bytes()),
        (220, 3_u16.to_le_bytes()),
        (150, 0x2002_u16.to_le_bytes()),
    ] {
        let mut image = original.clone();
        image[offset..offset + 2].copy_from_slice(&bytes);
        assert!(pe_identity(&image).is_err());
    }
    for flags in [0xc000_0040_u32, 0x6000_0040, 0x4200_0040] {
        let mut image = original.clone();
        image[428..432].copy_from_slice(&flags.to_le_bytes());
        assert!(pe_identity(&image).is_err());
    }
    let mut image = original.clone();
    image[400..404].copy_from_slice(&4097_u32.to_le_bytes());
    assert!(pe_identity(&image).is_err());
    let mut image = original.clone();
    image[412..416].copy_from_slice(&400_u32.to_le_bytes());
    assert!(pe_identity(&image).is_err());
    let mut image = original.clone();
    *image.last_mut().unwrap() = 1;
    assert!(pe_identity(&image).is_err());
    let mut image = original.clone();
    image[134..136].copy_from_slice(&2_u16.to_le_bytes());
    image[432..472].copy_from_slice(&original[392..432]);
    assert!(pe_identity(&image).is_err());
}

#[test]
fn windows_install_gate_does_not_create_stage_or_release_a_call() {
    assert!(!INSTALLATION_VALIDATED);
    let activity = super::super::UpdateActivity::default();
    let call = activity.try_start_call().unwrap();
    assert!(matches!(
        activity.try_install(),
        Err(UpdateError::CallActive)
    ));
    drop(call);
    let lease = activity.try_install().unwrap();
    assert!(activity.try_start_call().is_err());
    drop(lease);
    assert!(activity.try_start_call().is_ok());
}

#[test]
fn windows_private_files_reject_hardlinks_and_exclusive_locks_cover_installer() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = native::create_directory(&temporary.path().join("private")).unwrap();
    native::write_new(&directory.path.join("file"), b"synthetic fixture").unwrap();
    assert_eq!(
        native::read(&directory.path.join("file"), 32).unwrap(),
        b"synthetic fixture"
    );
    fs::hard_link(directory.path.join("file"), directory.path.join("alias")).unwrap();
    assert!(native::open(&directory.path.join("file")).is_err());
    assert!(native::open(&directory.path.join("alias")).is_err());
    fs::remove_file(directory.path.join("alias")).unwrap();
    let lock = native::lock(&directory.path.join("lock")).unwrap();
    assert!(native::lock(&directory.path.join("lock")).is_err());
    drop(lock);
    assert!(native::lock(&directory.path.join("lock")).is_ok());
}

#[test]
fn windows_parent_wait_uses_the_exact_process_creation_stamp() {
    let (id, stamp) = native::ParentProcess::current_stamp().unwrap();
    // The updater never accepts itself or an unbound process ID as its parent.
    assert!(native::ParentProcess::open(id, stamp).is_err());
    assert!(native::ParentProcess::open(0, stamp).is_err());
    assert!(native::ParentProcess::open(id, 0).is_err());
}

fn journal_fixture() -> Journal {
    let old = envelope(&identity("0.1.0")).1;
    let new = envelope(&identity("0.2.0")).1;
    let old_marker = serde_json::to_vec(&old).unwrap();
    let new_marker = serde_json::to_vec(&new).unwrap();
    Journal {
        schema: 1,
        id: "transaction-fixture-001".into(),
        phase: Phase::Prepared,
        old_marker: hash(&old_marker),
        old_marker_bytes: old_marker.len() as u64,
        new_marker: hash(&new_marker),
        new_marker_bytes: new_marker.len() as u64,
        old,
        new,
        uninstaller: Entry {
            path: UNINSTALLER.into(),
            bytes: 128,
            sha256: "a".repeat(64),
        },
        old_registration: native::Registration {
            version: "0.1.0".into(),
            estimated_size: 1,
        },
        new_registration: native::Registration {
            version: "0.2.0".into(),
            estimated_size: 2,
        },
        parent_id: 1,
        parent_stamp: 1,
        helper_hash: "b".repeat(64),
    }
}

#[test]
fn windows_journal_never_rolls_back_a_committed_store_cohort() {
    for next in [
        Phase::Prepared,
        Phase::ApplyIntent,
        Phase::AwaitingHealth,
        Phase::RollbackIntent,
        Phase::RolledBack,
    ] {
        assert!(!valid_transition(Phase::Committed, next));
    }
    assert!(valid_transition(Phase::Prepared, Phase::ApplyIntent));
    assert!(valid_transition(Phase::ApplyIntent, Phase::RollbackIntent));
    assert!(valid_transition(Phase::AwaitingHealth, Phase::Committed));
    assert!(!valid_transition(Phase::Prepared, Phase::Committed));
    let journal = journal_fixture();
    journal.validate().unwrap();
    let mut changed = journal.clone();
    changed.phase = Phase::ApplyIntent;
    assert!(journal.same_transaction(&changed));
    changed.new_registration.estimated_size = 500;
    assert!(!journal.same_transaction(&changed));
    changed = journal.clone();
    changed.new.identity.persistence = "c".repeat(64);
    assert!(changed.validate().is_err());
}

#[test]
fn windows_rollback_accepts_a_later_file_temp_before_restoring_an_earlier_marker() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = native::create_directory(&temporary.path().join("private")).unwrap();
    let journal = journal_fixture();
    journal.validate().unwrap();
    let later_executable = pe(&journal.new.identity);
    assert_ne!(hash(&later_executable), journal.old_marker);
    assert_ne!(hash(&later_executable), journal.new_marker);
    let path = workspace.path.join("replace.next");
    native::write_new(&path, &later_executable).unwrap();
    // Replacement is executable-last. A crash here can leave the new marker
    // installed, so rollback must clear this EXE temp before restoring it.
    clear_replacement_temp(&workspace.path, &journal).unwrap();
    assert!(!path.exists());
    let marker = serde_json::to_vec(&journal.old).unwrap();
    native::write_new(&path, &marker).unwrap();
    clear_replacement_temp(&workspace.path, &journal).unwrap();
    assert!(!path.exists());
    let mut changed_size = journal.clone();
    changed_size.new.files[3].bytes += 1;
    native::write_new(&path, &later_executable).unwrap();
    assert_eq!(
        clear_replacement_temp(&workspace.path, &changed_size),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(
        native::read(&path, MAX_UPDATE_BYTES).unwrap(),
        later_executable
    );
    fs::remove_file(&path).unwrap();
    let unknown = b"unrecognized retained bytes";
    native::write_new(&path, unknown).unwrap();
    assert_eq!(
        clear_replacement_temp(&workspace.path, &journal),
        Err(UpdateError::RecoveryRequired)
    );
    assert_eq!(native::read(&path, 64).unwrap(), unknown);
}

#[test]
fn windows_recovery_request_is_bound_to_phase_nonce_and_the_full_immutable_journal() {
    let mut journal = journal_fixture();
    journal.phase = Phase::AwaitingHealth;
    let request = RecoveryRequest {
        schema: 1,
        nonce: "recovery-fixture-001".into(),
        phase: journal.phase,
        journal_hash: hash(&serde_json::to_vec(&journal).unwrap()),
        parent_id: 12,
        parent_stamp: 34,
    };
    let bytes = serde_json::to_vec(&request).unwrap();
    assert_eq!(RecoveryRequest::parse(&bytes, &journal).unwrap(), request);
    for terminal in [Phase::Committed, Phase::RolledBack] {
        let mut terminal_journal = journal.clone();
        terminal_journal.phase = terminal;
        // Terminal cleanup binds the exact pre-recovery snapshot, not a new
        // request that could authorize another immutable payload or process.
        assert!(RecoveryRequest::parse(&bytes, &terminal_journal).is_ok());
        assert!(!needs_copied_recovery(terminal));
    }
    for phase in [Phase::Prepared, Phase::Committed, Phase::RolledBack] {
        let mut bad = request.clone();
        bad.phase = phase;
        assert!(RecoveryRequest::parse(&serde_json::to_vec(&bad).unwrap(), &journal).is_err());
    }
    let mut bad_journal = journal.clone();
    bad_journal.helper_hash = "c".repeat(64);
    assert!(RecoveryRequest::parse(&bytes, &bad_journal).is_err());
    let mut bad_journal = journal.clone();
    bad_journal.new_registration.estimated_size += 1;
    assert!(RecoveryRequest::parse(&bytes, &bad_journal).is_err());
    for nonce in [
        "",
        "..",
        "recovery/fixture",
        "recovery\\fixture",
        "Global:foreign",
        "x",
    ] {
        let mut bad = request.clone();
        bad.nonce = nonce.into();
        assert!(RecoveryRequest::parse(&serde_json::to_vec(&bad).unwrap(), &journal).is_err());
    }
    for (id, stamp) in [(0, 1), (1, 0)] {
        let mut bad = request.clone();
        bad.parent_id = id;
        bad.parent_stamp = stamp;
        assert!(RecoveryRequest::parse(&serde_json::to_vec(&bad).unwrap(), &journal).is_err());
    }
    let mut unknown = serde_json::to_value(&request).unwrap();
    unknown["extra"] = true.into();
    assert!(RecoveryRequest::parse(&serde_json::to_vec(&unknown).unwrap(), &journal).is_err());
    assert!(RecoveryRequest::parse(&vec![b' '; RECOVERY_LIMIT as usize + 1], &journal).is_err());
    let ready = RecoveryReady {
        schema: 1,
        request_hash: hash(&bytes),
        helper_id: 56,
        helper_stamp: 78,
    };
    let ready_bytes = serde_json::to_vec(&ready).unwrap();
    assert_eq!(RecoveryReady::parse(&ready_bytes, &bytes).unwrap(), ready);
    let mut different = request.clone();
    different.parent_stamp += 1;
    assert!(RecoveryReady::parse(&ready_bytes, &serde_json::to_vec(&different).unwrap()).is_err());
    let mut bad = ready.clone();
    bad.helper_stamp = 0;
    assert!(RecoveryReady::parse(&serde_json::to_vec(&bad).unwrap(), &bytes).is_err());
    let mut unknown = serde_json::to_value(&ready).unwrap();
    unknown["extra"] = true.into();
    assert!(RecoveryReady::parse(&serde_json::to_vec(&unknown).unwrap(), &bytes).is_err());
}

#[test]
fn windows_recovery_lifetime_object_survives_only_actual_parent_or_helper_handles() {
    let nonce = native::recovery_nonce().unwrap();
    assert_eq!(nonce.len(), 64);
    assert!(valid_id(&nonce));
    let nonce = nonce.as_str();
    let id = "transaction-fixture-001";
    assert!(native::RecoveryLease::open(id, nonce).unwrap().is_none());
    let parent = native::RecoveryLease::create(id, nonce).unwrap();
    assert!(native::RecoveryLease::create(id, nonce).is_err());
    let helper = native::RecoveryLease::open(id, nonce).unwrap().unwrap();
    drop(parent);
    assert!(native::RecoveryLease::open(id, nonce).unwrap().is_some());
    drop(helper);
    // The helper cannot recreate a lost authorization object. This is essential
    // when the parent dies before a spawned child reaches its first instruction.
    assert!(native::RecoveryLease::open(id, nonce).unwrap().is_none());
    assert!(native::RecoveryLease::open(id, "bad\\nonce").is_err());
    let (pid, stamp) = native::ParentProcess::current_stamp().unwrap();
    assert!(native::ParentProcess::running(pid, stamp).unwrap());
    assert!(!native::ParentProcess::running(pid, stamp + 1).unwrap());
}

#[test]
fn windows_updates_require_an_explicit_fixed_volume_result() {
    assert!(native::require_fixed_drive(3).is_ok());
    for kind in [0, 1, 2, 4, 5, 6, u32::MAX] {
        assert_eq!(
            native::require_fixed_drive(kind),
            Err(UpdateError::UnsupportedPackage)
        );
    }
}

#[test]
fn windows_committed_recovery_requires_the_journal_flush_before_accepting_stores() {
    let temporary = tempfile::tempdir().unwrap();
    let root = native::create_directory(&temporary.path().join("installed")).unwrap();
    let state = native::create_directory(&temporary.path().join("state")).unwrap();
    let mut journal = journal_fixture();
    journal.phase = Phase::Committed;
    let envelope_bytes = envelope(&journal.new.identity).0;
    let payload = Payload::parse(
        &envelope_bytes,
        &journal.old.identity,
        &journal.new.identity.version,
    )
    .unwrap();
    let mut uninstaller = vec![0_u8; 128];
    uninstaller[..2].copy_from_slice(b"MZ");
    journal.uninstaller.sha256 = hash(&uninstaller);
    journal.validate().unwrap();
    create_payload_tree(
        &root.path,
        &journal.new,
        payload.marker,
        &payload.contents,
        &uninstaller,
    )
    .unwrap();
    let journal_path = state.path.join("journal.json");
    native::write_new(&journal_path, &serde_json::to_vec(&journal).unwrap()).unwrap();
    confirm_commit_data(
        &root.path,
        &journal_path,
        &journal,
        &journal.new_registration,
    )
    .unwrap();
    fs::remove_file(&journal_path).unwrap();
    // Correct payload/registry alone cannot authorize stores when the checked
    // journal flush cannot run. A visible commit is still final and never turns
    // into rollback intent merely because durability needs a retry.
    assert!(
        confirm_commit_data(
            &root.path,
            &journal_path,
            &journal,
            &journal.new_registration
        )
        .is_err()
    );
    assert_eq!(journal.phase, Phase::Committed);
    assert!(!valid_transition(journal.phase, Phase::RollbackIntent));
    check_tree(
        &root.path,
        &journal.new,
        &journal.new_marker,
        &journal.uninstaller,
    )
    .unwrap();
}
