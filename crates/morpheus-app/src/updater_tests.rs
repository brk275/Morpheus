use super::*;

// Public minisign-verify 0.2.5 verification vector; no signing private key.
// Its license is retained at vendor/cargo-packager-updater/LICENSE-TEST-VECTOR.
const PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXkgRTc2MjBGMTg0MkI0RTgxRgpSV1FmNkxSQ0dBOWk1M21sWWVjTzRJelQ1MVRHUHB2V3VjTlNDaDFDQk0wUVRhTG43M1k3R0ZPMw==";
const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIG1pbmlzaWduIHNlY3JldCBrZXkKUlVRZjZMUkNHQTlpNTU5cjNnN1YxcU55SkRBcEdpcDhNZnFjYWRJZ1Q5Q3VoVjNFTWhIb04xbUdUa1VpZEYvejdTcmxRZ1hkeThvZmpiN2JOSkp5bERPb2NyQ284S0x6WndvPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNTU2MTkzMzM1CWZpbGU6dGVzdAp5L3JVdzJ5OC9oT1VZalpVNzFlSHAvV28xS1o0MGZHeTJWSkVEbDM0WE1KTStUWDQ4U3MvMTd1M0l2SWZiVlIxRmtaWlNOQ2lzUWJ1UVkrYkh3aEVCZz09";

fn signed_update(format: UpdateFormat) -> Update {
    Update {
        config: Config {
            pubkey: PUBLIC_KEY.into(),
            ..Default::default()
        },
        body: None,
        current_version: "0.1.0".into(),
        version: "0.2.0".into(),
        date: None,
        target: "macos".into(),
        extract_path: std::env::temp_dir().join("must-not-install.app"),
        download_url: "https://updates.example.org/app".parse().unwrap(),
        signature: SIGNATURE.into(),
        timeout: None,
        headers: Default::default(),
        format,
    }
}

#[test]
fn rejects_formats_before_download_or_install() {
    let current = Version::parse("0.1.0").unwrap();
    for (kind, accepted) in [
        (PackageKind::MacApp, UpdateFormat::App),
        (PackageKind::WindowsNsis, UpdateFormat::WindowsPayloadV1),
        (PackageKind::LinuxAppImage, UpdateFormat::AppImage),
    ] {
        validate_candidate(&signed_update(accepted), &current, kind).unwrap();
        for format in [
            UpdateFormat::App,
            UpdateFormat::Nsis,
            UpdateFormat::WindowsPayloadV1,
            UpdateFormat::Wix,
            UpdateFormat::AppImage,
        ] {
            if !kind.supports(format) {
                assert_eq!(
                    validate_candidate(&signed_update(format), &current, kind),
                    Err(UpdateError::UnsupportedFormat)
                );
            }
        }
    }
    // DMG/deb/Pacman cannot masquerade as official update formats.
    for format in ["dmg", "deb", "pacman"] {
        assert!(serde_json::from_value::<UpdateFormat>(serde_json::json!(format)).is_err());
    }
}

#[test]
fn prevents_downgrades_and_build_metadata_only_updates() {
    let current = Version::parse("1.0.0").unwrap();
    for version in ["0.9.9", "1.0.0", "1.0.0+rebuild", "1.0.0-beta.1", "invalid"] {
        let mut update = signed_update(UpdateFormat::App);
        update.version = version.into();
        assert_eq!(
            validate_candidate(&update, &current, PackageKind::MacApp),
            Err(UpdateError::NotNewer)
        );
    }
}

#[test]
fn verified_stage_detects_corruption_and_preserves_installation() {
    let installation = tempfile::tempdir().unwrap();
    let application = installation.path().join("existing-app");
    fs::write(&application, b"existing installed app").unwrap();
    let mut update = signed_update(UpdateFormat::App);
    update.extract_path = application.clone();
    let mut stage = stage_bytes(update, PackageKind::MacApp, b"test").unwrap();
    let staged_path = stage.file.path().to_path_buf();
    stage.verify().unwrap();
    assert_eq!(
        stage.install(&UpdateActivity::default()),
        Err(UpdateError::InstallNotValidated)
    );
    assert_eq!(fs::read(&application).unwrap(), b"existing installed app");
    // Files can be truncated or modified after download. Re-read the retained
    // file handle and verify its signature again instead of trusting a flag.
    stage.file.as_file_mut().set_len(3).unwrap();
    assert_eq!(stage.verify(), Err(UpdateError::CorruptStage));
    assert_eq!(
        stage.install(&UpdateActivity::default()),
        Err(UpdateError::CorruptStage)
    );
    assert_eq!(fs::read(&application).unwrap(), b"existing installed app");
    drop(stage);
    assert!(!staged_path.exists());
}

#[test]
fn invalid_signature_never_becomes_a_stage() {
    let result = stage_bytes(
        signed_update(UpdateFormat::App),
        PackageKind::MacApp,
        b"altered",
    );
    assert!(matches!(result, Err(UpdateError::CorruptStage)));
}

#[test]
fn calls_and_installs_share_an_exclusive_lifetime() {
    let activity = UpdateActivity::default();
    let call = activity.try_start_call().unwrap();
    assert!(matches!(
        activity.try_install(),
        Err(UpdateError::CallActive)
    ));
    let mut stage = stage_bytes(
        signed_update(UpdateFormat::App),
        PackageKind::MacApp,
        b"test",
    )
    .unwrap();
    assert_eq!(stage.install(&activity), Err(UpdateError::CallActive));
    drop(call);
    let installing = activity.try_install().unwrap();
    assert!(matches!(
        activity.clone().try_start_call(),
        Err(UpdateError::UpdateActive)
    ));
    drop(installing);
    let _next_call = activity.try_start_call().unwrap();
}

#[test]
fn race_between_call_and_install_has_only_one_winner() {
    let activity = UpdateActivity::default();
    let start = Arc::new(std::sync::Barrier::new(3));
    let finish = Arc::new(std::sync::Barrier::new(3));
    let call_thread = {
        let activity = activity.clone();
        let start = start.clone();
        let finish = finish.clone();
        std::thread::spawn(move || {
            start.wait();
            let lease = activity.try_start_call();
            finish.wait();
            lease.is_ok()
        })
    };
    let install_thread = {
        let activity = activity.clone();
        let start = start.clone();
        let finish = finish.clone();
        std::thread::spawn(move || {
            start.wait();
            let lease = activity.try_install();
            finish.wait();
            lease.is_ok()
        })
    };
    start.wait();
    finish.wait();
    assert_ne!(call_thread.join().unwrap(), install_thread.join().unwrap());
    assert!(activity.try_start_call().is_ok());
}

#[test]
fn stage_failure_never_touches_account_data() {
    let account_dir = tempfile::tempdir().unwrap();
    let account_file = account_dir.path().join("crypto-store-sentinel");
    fs::write(&account_file, b"keep me").unwrap();
    let mut update = signed_update(UpdateFormat::App);
    update.extract_path = account_dir.path().to_path_buf();
    assert!(stage_bytes(update, PackageKind::MacApp, b"invalid").is_err());
    assert_eq!(fs::read(&account_file).unwrap(), b"keep me");
}
