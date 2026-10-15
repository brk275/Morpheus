//! Explicit hosted-only native vault acceptance. No Matrix login or network.
//! The wrapper must establish platform isolation before invoking one exact test.
//! This integration target links the normal library, so its vault is not mocked.
#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use morpheus_matrix::{AccountInfo, AccountManager, MatrixCoreError, SyncStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

type Result<T> = std::result::Result<T, &'static str>;
const APP_SERVICE: &str = "Morpheus App Password";
const ACCOUNT_SERVICE: &str = "Morpheus Matrix";
const INPUT_BOUND: u64 = 4096;
const FILE_BOUND: u64 = 16_384;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepare,
    VerifyRemove,
    Cleanup,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::VerifyRemove => "verify_remove",
            Self::Cleanup => "cleanup",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u8,
    phase: Phase,
    run_nonce: String,
    root: PathBuf,
    profile: PathBuf,
    journal: PathBuf,
    progress: PathBuf,
    result: PathBuf,
    executable: PathBuf,
    account_a: String,
    account_b: String,
    password_initial: String,
    password_changed: String,
    password_restart: String,
    wrong_password: String,
    sentinel_a: String,
    sentinel_b: String,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.password_initial.zeroize();
        self.password_changed.zeroize();
        self.password_restart.zeroize();
        self.wrong_password.zeroize();
        self.sentinel_a.zeroize();
        self.sentinel_b.zeroize();
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ownership {
    absent_confirmed: bool,
    write_claimed: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    run_nonce: String,
    profile_uuid: Option<String>,
    account_a: String,
    account_b: String,
    app_password: Ownership,
    account_a_item: Ownership,
    account_b_item: Ownership,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fence {
    version: u8,
    profile: String,
    enabled: bool,
    generation: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u8,
    active_account_id: Option<String>,
    accounts: Vec<AccountInfo>,
    pending_removals: Vec<String>,
}

struct Context {
    input: Input,
    stage: &'static str,
    checks: BTreeMap<&'static str, bool>,
}

fn require(value: bool, failure: &'static str) -> Result<()> {
    if value { Ok(()) } else { Err(failure) }
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == value)
}

fn metadata(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let value = fs::symlink_metadata(path).map_err(|_| "path_metadata")?;
    require(!value.file_type().is_symlink(), "path_link")?;
    require(
        if directory {
            value.is_dir()
        } else {
            value.is_file()
        },
        "path_type",
    )?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        require(value.permissions().mode() & 0o077 == 0, "path_permissions")?;
        if !directory {
            require(value.nlink() == 1, "path_links")?;
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        require(value.file_attributes() & 0x400 == 0, "path_reparse")?;
        // The hosted wrapper establishes and verifies the owned root ACL.
        // Windows chmod bits alone do not prove an owner-only DACL.
    }
    Ok(value)
}

fn absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn private_read(path: &Path, bound: u64) -> Result<Zeroizing<Vec<u8>>> {
    let value = metadata(path, false)?;
    require(value.len() <= bound, "file_bound")?;
    let mut bytes = Zeroizing::new(Vec::new());
    File::open(path)
        .map_err(|_| "file_open")?
        .take(bound + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "file_read")?;
    require(bytes.len() as u64 <= bound, "file_bound")?;
    Ok(bytes)
}

fn write_private(path: &Path, bytes: &[u8], create: bool) -> Result<()> {
    require(bytes.len() as u64 <= FILE_BOUND, "file_bound")?;
    if create {
        require(absent(path), "file_exists")?;
    } else if !absent(path) {
        metadata(path, false)?;
    }
    let parent = path.parent().ok_or("file_parent")?;
    metadata(parent, true)?;
    let temporary = parent.join(format!("native-vault-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let outcome = (|| {
        let mut file = options.open(&temporary).map_err(|_| "file_create")?;
        file.write_all(bytes).map_err(|_| "file_write")?;
        file.sync_all().map_err(|_| "file_sync")?;
        drop(file);
        fs::rename(&temporary, path).map_err(|_| "file_replace")?;
        #[cfg(target_os = "macos")]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "directory_sync")?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    outcome
}

fn write_json(path: &Path, value: &impl Serialize, create: bool) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| "json_encode")?;
    write_private(path, &bytes, create)
}

fn make_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|_| "directory_create")?;
    metadata(path, true)?;
    Ok(())
}

fn owned_child(path: &Path, root: &Path, name: &str) -> Result<PathBuf> {
    require(path.is_absolute(), "input_paths")?;
    require(
        path.file_name() == Some(std::ffi::OsStr::new(name)),
        "input_paths",
    )?;
    let parent = path.parent().ok_or("input_paths")?;
    require(
        parent.canonicalize().map_err(|_| "input_paths")? == root,
        "input_paths",
    )?;
    // The leaf may not exist yet. Rebuild it from the canonical, verified root,
    // never from a Python/Windows path spelling or a caller's traversal path.
    Ok(root.join(name))
}

fn normalize_paths(input: &mut Input, runner_temp: &Path, checkout: &Path) -> Result<()> {
    require(input.root.is_absolute(), "root_guard")?;
    metadata(&input.root, true)?;
    let root = input.root.canonicalize().map_err(|_| "root_guard")?;
    require(
        root == runner_temp.join(format!("morpheus-native-vault-{}", input.run_nonce))
            && !root.starts_with(checkout),
        "root_guard",
    )?;
    input.profile = owned_child(&input.profile, &root, "profile")?;
    input.journal = owned_child(&input.journal, &root, "owned-items.json")?;
    input.progress = owned_child(
        &input.progress,
        &root,
        &format!("{}-progress.json", input.phase.name()),
    )?;
    input.result = owned_child(
        &input.result,
        &root,
        &format!("{}-result.json", input.phase.name()),
    )?;
    input.root = root;
    Ok(())
}

fn load_input(phase: Phase) -> Result<Input> {
    require(
        std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true"),
        "host_guard",
    )?;
    require(
        std::env::var("RUNNER_ENVIRONMENT").as_deref() == Ok("github-hosted"),
        "host_guard",
    )?;
    let mut raw = Zeroizing::new(Vec::new());
    std::io::stdin()
        .lock()
        .take(INPUT_BOUND + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "input_read")?;
    require(raw.len() as u64 <= INPUT_BOUND, "input_bound")?;
    let mut input: Input = serde_json::from_slice(&raw).map_err(|_| "input_schema")?;
    require(
        input.version == 1 && input.phase == phase && hex64(&input.run_nonce),
        "input_identity",
    )?;
    require(
        std::env::var("MORPHEUS_NATIVE_VAULT_ISOLATED").as_deref() == Ok(input.run_nonce.as_str()),
        "isolation_guard",
    )?;
    require(
        canonical_uuid(&input.account_a)
            && canonical_uuid(&input.account_b)
            && input.account_a != input.account_b,
        "input_accounts",
    )?;
    let secrets = [
        &input.password_initial,
        &input.password_changed,
        &input.password_restart,
        &input.wrong_password,
        &input.sentinel_a,
        &input.sentinel_b,
    ];
    require(
        secrets.iter().all(|value| hex64(value))
            && secrets
                .iter()
                .enumerate()
                .all(|(index, value)| !secrets[..index].contains(value)),
        "input_secrets",
    )?;
    let runner_temp = PathBuf::from(std::env::var_os("RUNNER_TEMP").ok_or("temp_guard")?)
        .canonicalize()
        .map_err(|_| "temp_guard")?;
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("checkout_guard")?
        .canonicalize()
        .map_err(|_| "checkout_guard")?;
    normalize_paths(&mut input, &runner_temp, &checkout)?;
    require(
        absent(&input.progress) && absent(&input.result),
        "phase_replay",
    )?;
    let executable = std::env::current_exe()
        .map_err(|_| "executable_guard")?
        .canonicalize()
        .map_err(|_| "executable_guard")?;
    require(input.executable.is_absolute(), "executable_guard")?;
    let supplied_metadata =
        fs::symlink_metadata(&input.executable).map_err(|_| "executable_guard")?;
    require(
        supplied_metadata.is_file() && !supplied_metadata.file_type().is_symlink(),
        "executable_guard",
    )?;
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        require(
            supplied_metadata.file_attributes() & 0x400 == 0,
            "executable_guard",
        )?;
    }
    input.executable = input
        .executable
        .canonicalize()
        .map_err(|_| "executable_guard")?;
    require(
        executable == input.executable && executable.starts_with(checkout.join("target")),
        "executable_guard",
    )?;
    #[cfg(target_os = "macos")]
    confirm_owned_keychain(&input.root)?;
    Ok(input)
}

#[cfg(target_os = "macos")]
fn confirm_owned_keychain(root: &Path) -> Result<()> {
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    let expected = root.join("native-vault.keychain-db");
    metadata(&expected, false)?;
    require(
        expected.canonicalize().map_err(|_| "keychain_guard")? == expected,
        "keychain_guard",
    )?;
    let mut child = Command::new("/usr/bin/security")
        .args(["default-keychain", "-d", "user"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "keychain_guard")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                require(status.success(), "keychain_guard")?;
                let mut bytes = Vec::new();
                child
                    .stdout
                    .take()
                    .ok_or("keychain_guard")?
                    .take(FILE_BOUND + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "keychain_guard")?;
                require(bytes.len() as u64 <= FILE_BOUND, "keychain_guard")?;
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| "keychain_guard")?
                    .trim();
                let path = value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .ok_or("keychain_guard")?;
                require(
                    !path.contains(['\n', '\r', '"']) && Path::new(path) == expected,
                    "keychain_guard",
                )?;
                return Ok(());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let cleanup_deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return Err("keychain_guard"),
                        Ok(None) if Instant::now() < cleanup_deadline => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        // The outer wrapper still owns the process group and
                        // must confirm it stopped before restoring a default.
                        _ => return Err("keychain_probe_cleanup"),
                    }
                }
            }
        }
    }
}

impl Context {
    fn stage(&mut self, stage: &'static str) -> Result<()> {
        self.stage = stage;
        self.progress("running", None)
    }

    fn check(&mut self, name: &'static str, passed: bool) -> Result<()> {
        require(passed, name)?;
        self.checks.insert(name, true);
        self.progress("running", None)
    }

    fn progress(&self, status: &'static str, failure: Option<&'static str>) -> Result<()> {
        write_json(
            &self.input.progress,
            &serde_json::json!({
                "version": 1, "phase": self.input.phase, "status": status,
                "stage": self.stage, "failure": failure, "checks": self.checks,
            }),
            false,
        )
    }

    fn finish(&self, result: Result<()>) -> Result<()> {
        let mut failure = result.err();
        let status = if failure.is_some() {
            "failed"
        } else {
            "passed"
        };
        if let Err(code) = self.progress(status, failure) {
            failure.get_or_insert(code);
        }
        let status = if failure.is_some() {
            "failed"
        } else {
            "passed"
        };
        let written = write_json(
            &self.input.result,
            &serde_json::json!({
                "version": 1, "phase": self.input.phase, "status": status,
                "stage": self.stage, "failure": failure, "checks": self.checks,
            }),
            true,
        );
        if let Err(code) = written {
            failure.get_or_insert(code);
        }
        match failure {
            Some(code) => Err(code),
            None => Ok(()),
        }
    }
}

fn entry(service: &str, id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, id).map_err(|_| "vault_entry")
}

fn missing(entry: &keyring::Entry) -> bool {
    match entry.get_password() {
        Err(keyring::Error::NoEntry) => true,
        Ok(value) => {
            // A collision or failed deletion can return an unexpected secret.
            // Even that rejected temporary must not be dropped as plain String.
            drop(Zeroizing::new(value));
            false
        }
        Err(_) => false,
    }
}

fn matches_secret(entry: &keyring::Entry, expected: &str) -> Result<bool> {
    let found = Zeroizing::new(entry.get_password().map_err(|_| "vault_read")?);
    Ok(found.as_str() == expected)
}

fn password(value: &str) -> Zeroizing<String> {
    Zeroizing::new(value.to_owned())
}

fn read_journal(input: &Input) -> Result<Journal> {
    let journal: Journal = serde_json::from_slice(&private_read(&input.journal, FILE_BOUND)?)
        .map_err(|_| "journal_schema")?;
    validate_journal(input, &journal)?;
    Ok(journal)
}

fn validate_journal(input: &Input, journal: &Journal) -> Result<()> {
    require(
        journal.version == 1
            && journal.run_nonce == input.run_nonce
            && journal.account_a == input.account_a
            && journal.account_b == input.account_b
            && journal.profile_uuid.as_deref().is_none_or(canonical_uuid),
        "journal_identity",
    )?;
    for state in [
        journal.app_password,
        journal.account_a_item,
        journal.account_b_item,
    ] {
        require(
            !state.write_claimed || state.absent_confirmed,
            "journal_claim",
        )?;
    }
    require(
        !journal.app_password.write_claimed || journal.profile_uuid.is_some(),
        "journal_claim",
    )?;
    Ok(())
}

async fn inactive(manager: &AccountManager) -> bool {
    manager.active_account().await.is_none()
        && manager.subscribe_active_account().borrow().is_none()
        && matches!(
            &*manager.subscribe_sync_status().borrow(),
            SyncStatus::Stopped
        )
        && !manager.account_access_busy()
}

fn account(input: &Input, first: bool) -> AccountInfo {
    AccountInfo {
        id: if first {
            &input.account_a
        } else {
            &input.account_b
        }
        .clone(),
        user_id: if first {
            "@casey:alpha.example.invalid"
        } else {
            "@jules:beta.example.invalid"
        }
        .into(),
        device_id: if first {
            "NATIVEVAULTA"
        } else {
            "NATIVEVAULTB"
        }
        .into(),
        homeserver: if first {
            "https://alpha.example.invalid"
        } else {
            "https://beta.example.invalid"
        }
        .into(),
    }
}

async fn prepare(context: &mut Context) -> Result<()> {
    context.stage("profile_open")?;
    require(
        absent(&context.input.profile) && absent(&context.input.journal),
        "prepare_replay",
    )?;
    let mut journal = Journal {
        version: 1,
        run_nonce: context.input.run_nonce.clone(),
        profile_uuid: None,
        account_a: context.input.account_a.clone(),
        account_b: context.input.account_b.clone(),
        app_password: Ownership::default(),
        account_a_item: Ownership::default(),
        account_b_item: Ownership::default(),
    };
    write_json(&context.input.journal, &journal, true)?;
    make_directory(&context.input.profile)?;
    make_directory(&context.input.profile.join("accounts"))?;
    let manager = AccountManager::open(&context.input.profile).map_err(|_| "profile_open")?;
    let fence: Fence = serde_json::from_slice(&private_read(
        &context.input.profile.join("account-access.json"),
        4096,
    )?)
    .map_err(|_| "fence_schema")?;
    require(
        fence.version == 1
            && canonical_uuid(&fence.profile)
            && !fence.enabled
            && fence.generation == 0,
        "fence_initial",
    )?;
    journal.profile_uuid = Some(fence.profile.clone());
    write_json(&context.input.journal, &journal, false)?;
    context.check(
        "initial_empty",
        manager.accounts().await.is_empty()
            && !manager.app_password_status().await.enabled
            && inactive(&manager).await,
    )?;
    context.stage("password_enable")?;
    let app_entry = entry(APP_SERVICE, &fence.profile)?;
    require(missing(&app_entry), "preexisting_app_entry")?;
    journal.app_password = Ownership {
        absent_confirmed: true,
        write_claimed: true,
    };
    write_json(&context.input.journal, &journal, false)?;
    manager
        .set_app_password(None, password(&context.input.password_initial))
        .await
        .map_err(|_| "password_enable")?;
    context.check(
        "password_enabled",
        manager.app_password_status().await.enabled,
    )?;
    context.stage("wrong_current")?;
    let wrong_change = manager
        .set_app_password(
            Some(password(&context.input.wrong_password)),
            password(&context.input.password_changed),
        )
        .await;
    let wrong_disable = manager
        .disable_app_password(password(&context.input.wrong_password))
        .await;
    context.check(
        "wrong_current_rejected",
        matches!(wrong_change, Err(MatrixCoreError::AppPasswordIncorrect))
            && matches!(wrong_disable, Err(MatrixCoreError::AppPasswordIncorrect))
            && manager.app_password_status().await.enabled,
    )?;
    manager
        .set_app_password(
            Some(password(&context.input.password_initial)),
            password(&context.input.password_changed),
        )
        .await
        .map_err(|_| "password_change")?;
    context.check(
        "password_changed",
        manager.app_password_status().await.enabled,
    )?;
    manager.stop().await;
    drop(manager);

    context.stage("seed_inactive_accounts")?;
    let a = account(&context.input, true);
    let b = account(&context.input, false);
    write_json(
        &context.input.profile.join("accounts.json"),
        &Registry {
            version: 1,
            active_account_id: Some(a.id.clone()),
            accounts: vec![a.clone(), b.clone()],
            pending_removals: vec![],
        },
        true,
    )?;
    for (first, info) in [(true, &a), (false, &b)] {
        let directory = context.input.profile.join("accounts").join(&info.id);
        make_directory(&directory)?;
        let sentinel = if first {
            &context.input.sentinel_a
        } else {
            &context.input.sentinel_b
        };
        write_private(
            &directory.join("fixture-sentinel"),
            sentinel.as_bytes(),
            true,
        )?;
        let credential = entry(ACCOUNT_SERVICE, &info.id)?;
        require(missing(&credential), "preexisting_account_entry")?;
        let ownership = Ownership {
            absent_confirmed: true,
            write_claimed: true,
        };
        if first {
            journal.account_a_item = ownership;
        } else {
            journal.account_b_item = ownership;
        }
        write_json(&context.input.journal, &journal, false)?;
        credential
            .set_password(sentinel)
            .map_err(|_| "seed_credential")?;
    }
    let manager = AccountManager::open(&context.input.profile).map_err(|_| "profile_reopen")?;
    context.check(
        "synthetic_accounts_seeded",
        manager.accounts().await == vec![a.clone(), b.clone()]
            && manager.last_active_account_id().await.as_deref() == Some(a.id.as_str())
            && inactive(&manager).await,
    )?;
    context.stage("protected_authorization")?;
    context.check(
        "protected_direct_switch_blocked",
        matches!(
            manager.switch_to(&b.id).await,
            Err(MatrixCoreError::AppPasswordRequired)
        ) && matches!(
            manager.restore_last_active().await,
            Err(MatrixCoreError::AppPasswordRequired)
        ),
    )?;
    let challenge = manager
        .begin_activation(&b.id)
        .await
        .map_err(|_| "challenge")?;
    require(challenge.account_id() == b.id, "challenge_target")?;
    context.check(
        "wrong_authorization_rejected",
        matches!(
            manager
                .authorize_activation(challenge, password(&context.input.wrong_password))
                .await,
            Err(MatrixCoreError::AppPasswordIncorrect)
        ),
    )?;
    let challenge = manager
        .begin_activation(&b.id)
        .await
        .map_err(|_| "challenge")?;
    let grant = manager
        .authorize_activation(challenge, password(&context.input.password_changed))
        .await
        .map_err(|_| "correct_authorization")?;
    drop(grant); // Deliberately no successful synthetic account activation.
    context.check("correct_authorization_granted", inactive(&manager).await)?;
    context.stage("cancel_before_verify")?;
    let challenge = manager
        .begin_activation(&b.id)
        .await
        .map_err(|_| "challenge")?;
    challenge.cancellation().cancel();
    context.check(
        "cancel_before_verify",
        matches!(
            manager
                .authorize_activation(challenge, password(&context.input.password_changed))
                .await,
            Err(MatrixCoreError::ActivationExpired)
        ),
    )?;
    context.stage("cancel_after_verify")?;
    let challenge = manager
        .begin_activation(&b.id)
        .await
        .map_err(|_| "challenge")?;
    let cancellation = challenge.cancellation();
    let grant = manager
        .authorize_activation(challenge, password(&context.input.password_changed))
        .await
        .map_err(|_| "correct_authorization")?;
    cancellation.cancel();
    context.check(
        "cancel_after_verify",
        matches!(
            manager.activate(grant).await,
            Err(MatrixCoreError::ActivationExpired)
        ),
    )?;
    context.stage("policy_revocation")?;
    let challenge = manager
        .begin_activation(&a.id)
        .await
        .map_err(|_| "challenge")?;
    let grant = manager
        .authorize_activation(challenge, password(&context.input.password_changed))
        .await
        .map_err(|_| "correct_authorization")?;
    manager
        .set_app_password(
            Some(password(&context.input.password_changed)),
            password(&context.input.password_restart),
        )
        .await
        .map_err(|_| "password_restart_change")?;
    context.check(
        "stale_policy_grant",
        matches!(
            manager.activate(grant).await,
            Err(MatrixCoreError::ActivationExpired)
        ),
    )?;
    context.check(
        "reset_with_accounts_blocked",
        matches!(
            manager.reset_app_password_after_removal().await,
            Err(MatrixCoreError::AccountRemovalPending)
        ),
    )?;
    context.check("prepare_inactive", inactive(&manager).await)?;
    manager.stop().await;
    drop(manager);
    context.stage("prepare_complete")
}

fn sentinel_matches(input: &Input, first: bool) -> Result<bool> {
    let (id, expected) = if first {
        (&input.account_a, &input.sentinel_a)
    } else {
        (&input.account_b, &input.sentinel_b)
    };
    let value = private_read(
        &input
            .profile
            .join("accounts")
            .join(id)
            .join("fixture-sentinel"),
        128,
    )?;
    Ok(value.as_slice() == expected.as_bytes())
}

async fn verify_remove(context: &mut Context) -> Result<()> {
    context.stage("restart_open")?;
    let journal = read_journal(&context.input)?;
    require(
        journal.app_password.write_claimed
            && journal.account_a_item.write_claimed
            && journal.account_b_item.write_claimed,
        "restart_claims",
    )?;
    check_owned_tree(&context.input.profile, &context.input.profile, 0, &mut 128)?;
    let fence: Fence = serde_json::from_slice(&private_read(
        &context.input.profile.join("account-access.json"),
        4096,
    )?)
    .map_err(|_| "fence_schema")?;
    require(
        fence.version == 1
            && journal.profile_uuid.as_deref() == Some(fence.profile.as_str())
            && fence.enabled
            && fence.generation > 0,
        "restart_fence",
    )?;
    let manager = AccountManager::open(&context.input.profile).map_err(|_| "restart_open")?;
    let a = account(&context.input, true);
    let b = account(&context.input, false);
    context.check(
        "restart_protected_inactive",
        manager.accounts().await == vec![a.clone(), b.clone()]
            && manager.app_password_status().await.enabled
            && inactive(&manager).await,
    )?;
    context.stage("restart_authorization")?;
    let challenge = manager
        .begin_activation(&b.id)
        .await
        .map_err(|_| "challenge")?;
    context.check(
        "old_password_rejected",
        matches!(
            manager
                .authorize_activation(challenge, password(&context.input.password_changed))
                .await,
            Err(MatrixCoreError::AppPasswordIncorrect)
        ),
    )?;
    let challenge = manager
        .begin_activation(&b.id)
        .await
        .map_err(|_| "challenge")?;
    let grant = manager
        .authorize_activation(challenge, password(&context.input.password_restart))
        .await
        .map_err(|_| "restart_authorization")?;
    drop(grant);
    context.check("persisted_password_authorized", inactive(&manager).await)?;
    let a_entry = entry(ACCOUNT_SERVICE, &a.id)?;
    let b_entry = entry(ACCOUNT_SERVICE, &b.id)?;
    context.check(
        "distinct_native_entries",
        matches_secret(&a_entry, &context.input.sentinel_a)?
            && matches_secret(&b_entry, &context.input.sentinel_b)?
            && sentinel_matches(&context.input, true)?
            && sentinel_matches(&context.input, false)?,
    )?;
    context.stage("remove_b")?;
    let removed = manager
        .remove_account(&b.id)
        .await
        .map_err(|_| "remove_b")?;
    context.check(
        "remove_b_complete",
        removed.account_id == b.id
            && removed.credentials_removed
            && removed.store_removed
            && removed.complete,
    )?;
    context.check(
        "remove_b_absent_a_unchanged",
        missing(&b_entry)
            && absent(&context.input.profile.join("accounts").join(&b.id))
            && manager.accounts().await == vec![a.clone()]
            && manager
                .app_password_status()
                .await
                .pending_removals
                .is_empty()
            && manager.last_active_account_id().await.as_deref() == Some(a.id.as_str())
            && matches_secret(&a_entry, &context.input.sentinel_a)?
            && sentinel_matches(&context.input, true)?,
    )?;
    context.check(
        "reset_with_one_account_blocked",
        matches!(
            manager.reset_app_password_after_removal().await,
            Err(MatrixCoreError::AccountRemovalPending)
        ),
    )?;
    context.stage("remove_a")?;
    let removed = manager
        .remove_account(&a.id)
        .await
        .map_err(|_| "remove_a")?;
    context.check(
        "remove_a_complete",
        removed.account_id == a.id
            && removed.credentials_removed
            && removed.store_removed
            && removed.complete,
    )?;
    context.check(
        "all_accounts_absent",
        missing(&a_entry)
            && absent(&context.input.profile.join("accounts").join(&a.id))
            && manager.accounts().await.is_empty()
            && manager.last_active_account_id().await.is_none()
            && manager
                .app_password_status()
                .await
                .pending_removals
                .is_empty(),
    )?;
    context.stage("password_reset")?;
    manager
        .reset_app_password_after_removal()
        .await
        .map_err(|_| "password_reset")?;
    let profile_uuid = journal.profile_uuid.as_deref().ok_or("restart_claims")?;
    context.check(
        "password_reset_complete",
        !manager.app_password_status().await.enabled
            && missing(&entry(APP_SERVICE, profile_uuid)?)
            && inactive(&manager).await,
    )?;
    manager.stop().await;
    drop(manager);
    context.stage("empty_reopen")?;
    let manager = AccountManager::open(&context.input.profile).map_err(|_| "empty_reopen")?;
    context.check(
        "empty_reopen",
        manager.accounts().await.is_empty()
            && manager.last_active_account_id().await.is_none()
            && !manager.app_password_status().await.enabled
            && manager
                .app_password_status()
                .await
                .pending_removals
                .is_empty()
            && inactive(&manager).await,
    )?;
    manager.stop().await;
    drop(manager);
    context.stage("verify_remove_complete")
}

fn check_owned_tree(path: &Path, root: &Path, depth: usize, remaining: &mut usize) -> Result<()> {
    require(depth <= 4 && *remaining > 0, "cleanup_tree_bound")?;
    *remaining -= 1;
    let value = fs::symlink_metadata(path).map_err(|_| "cleanup_tree")?;
    metadata(path, value.is_dir())?;
    require(
        path.canonicalize()
            .map_err(|_| "cleanup_tree")?
            .starts_with(root),
        "cleanup_tree",
    )?;
    if value.is_dir() {
        for child in fs::read_dir(path).map_err(|_| "cleanup_tree")? {
            check_owned_tree(
                &child.map_err(|_| "cleanup_tree")?.path(),
                root,
                depth + 1,
                remaining,
            )?;
        }
    }
    Ok(())
}

struct CleanupTarget {
    service: &'static str,
    id: String,
    fact: &'static str,
}

enum CleanupEvent {
    Begin,
    Target { fact: &'static str, passed: bool },
    ProfileBegin,
    Profile { passed: bool },
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CleanupFailure {
    stage: &'static str,
    code: &'static str,
}

fn cleanup_plan(input: &Input) -> Result<Vec<CleanupTarget>> {
    let journal = read_journal(input)?;
    let mut targets = Vec::new();
    for (service, id, state, fact) in [
        (
            APP_SERVICE,
            journal.profile_uuid.as_deref(),
            journal.app_password,
            "cleanup_app_entry",
        ),
        (
            ACCOUNT_SERVICE,
            Some(journal.account_a.as_str()),
            journal.account_a_item,
            "cleanup_account_a",
        ),
        (
            ACCOUNT_SERVICE,
            Some(journal.account_b.as_str()),
            journal.account_b_item,
            "cleanup_account_b",
        ),
    ] {
        if state.write_claimed {
            targets.push(CleanupTarget {
                service,
                id: id.ok_or("journal_claim")?.into(),
                fact,
            });
        }
    }
    Ok(targets)
}

fn retain_cleanup_failure(
    first: &mut Option<CleanupFailure>,
    result: Result<()>,
    stage: &'static str,
) {
    if let Err(code) = result {
        if first.is_none() {
            *first = Some(CleanupFailure { stage, code });
        }
    }
}

// This control-flow seam never accesses a vault. A rejected ownership journal
// authorizes no operation; diagnostics cannot interrupt a validated cleanup.
fn run_cleanup_plan<State>(
    state: &mut State,
    targets: Result<Vec<CleanupTarget>>,
    mut record: impl FnMut(&mut State, CleanupEvent) -> Result<()>,
    mut erase: impl FnMut(&mut State, &CleanupTarget) -> Result<()>,
    mut remove_profile: impl FnMut(&mut State) -> Result<()>,
) -> std::result::Result<(), CleanupFailure> {
    let targets = targets.map_err(|code| CleanupFailure {
        stage: "cleanup_validate_journal",
        code,
    })?;
    let mut first = None;
    retain_cleanup_failure(
        &mut first,
        record(state, CleanupEvent::Begin),
        "cleanup_owned_entries",
    );
    for target in targets {
        let outcome = erase(state, &target);
        retain_cleanup_failure(&mut first, outcome, "cleanup_owned_entries");
        retain_cleanup_failure(
            &mut first,
            record(
                state,
                CleanupEvent::Target {
                    fact: target.fact,
                    passed: outcome.is_ok(),
                },
            ),
            "cleanup_owned_entries",
        );
    }
    retain_cleanup_failure(
        &mut first,
        record(state, CleanupEvent::ProfileBegin),
        "cleanup_owned_profile",
    );
    let outcome = remove_profile(state);
    retain_cleanup_failure(&mut first, outcome, "cleanup_owned_profile");
    retain_cleanup_failure(
        &mut first,
        record(
            state,
            CleanupEvent::Profile {
                passed: outcome.is_ok(),
            },
        ),
        "cleanup_owned_profile",
    );
    if first.is_none() {
        retain_cleanup_failure(
            &mut first,
            record(state, CleanupEvent::Complete),
            "cleanup_complete",
        );
    }
    match first {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

fn cleanup(context: &mut Context) -> Result<()> {
    let targets = cleanup_plan(&context.input);
    let outcome = run_cleanup_plan(
        context,
        targets,
        |context, event| match event {
            CleanupEvent::Begin => context.stage("cleanup_owned_entries"),
            CleanupEvent::Target { fact, passed } => {
                context.checks.insert(fact, passed);
                context.progress("running", None)
            }
            CleanupEvent::ProfileBegin => context.stage("cleanup_owned_profile"),
            CleanupEvent::Profile { passed } => {
                context.checks.insert("cleanup_profile_absent", passed);
                context.progress("running", None)
            }
            CleanupEvent::Complete => context.stage("cleanup_complete"),
        },
        |_, target| {
            let credential = entry(target.service, &target.id)?;
            require(
                matches!(
                    credential.delete_credential(),
                    Ok(()) | Err(keyring::Error::NoEntry)
                ),
                "cleanup_vault",
            )?;
            require(missing(&credential), "cleanup_vault")
        },
        |context| {
            if !absent(&context.input.profile) {
                check_owned_tree(&context.input.profile, &context.input.profile, 0, &mut 128)?;
                fs::remove_dir_all(&context.input.profile).map_err(|_| "cleanup_profile")?;
            }
            require(absent(&context.input.profile), "cleanup_profile")
        },
    );
    match outcome {
        Ok(()) => Ok(()),
        Err(failure) => {
            // Preserve the first failure's position after later cleanup work.
            context.stage = failure.stage;
            Err(failure.code)
        }
    }
}

fn run(phase: Phase) {
    // Fixed failures only: native OS/vault error messages are never printed.
    let result = (|| {
        let input = load_input(phase)?;
        let mut context = Context {
            input,
            stage: "input_validated",
            checks: BTreeMap::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime")?;
        let outcome = match phase {
            Phase::Prepare => runtime.block_on(prepare(&mut context)),
            Phase::VerifyRemove => runtime.block_on(verify_remove(&mut context)),
            Phase::Cleanup => cleanup(&mut context),
        };
        // The result cannot prove process exit or platform restoration. The
        // wrapper requires those independently, after this process terminates.
        context.finish(outcome)
    })();
    if let Err(code) = result {
        eprintln!("MORPHEUS_NATIVE_VAULT_FAILURE {code}");
        panic!("native vault fixture failed; see fixed phase receipt");
    }
}

#[test]
#[ignore = "fresh hosted OS vault fixture only"]
fn native_vault_prepare() {
    run(Phase::Prepare);
}

#[test]
#[ignore = "fresh hosted OS vault fixture only"]
fn native_vault_verify_remove() {
    run(Phase::VerifyRemove);
}

#[test]
#[ignore = "fresh hosted OS vault fixture only"]
fn native_vault_cleanup() {
    run(Phase::Cleanup);
}

// These parser/path regressions never instantiate a keyring Entry or manager.
// Ordinary native model tests may execute them without vault opt-in.
mod contract_tests {
    use super::*;

    fn fixture(root: PathBuf, nonce: String) -> Input {
        Input {
            version: 1,
            phase: Phase::Prepare,
            run_nonce: nonce,
            profile: root.join("profile"),
            journal: root.join("owned-items.json"),
            progress: root.join("prepare-progress.json"),
            result: root.join("prepare-result.json"),
            executable: root.join("fixture-executable"),
            root,
            account_a: Uuid::new_v4().to_string(),
            account_b: Uuid::new_v4().to_string(),
            password_initial: "1".repeat(64),
            password_changed: "2".repeat(64),
            password_restart: "3".repeat(64),
            wrong_password: "4".repeat(64),
            sentinel_a: "5".repeat(64),
            sentinel_b: "6".repeat(64),
        }
    }

    fn journal(input: &Input) -> Journal {
        Journal {
            version: 1,
            run_nonce: input.run_nonce.clone(),
            profile_uuid: Some(Uuid::new_v4().to_string()),
            account_a: input.account_a.clone(),
            account_b: input.account_b.clone(),
            app_password: Ownership::default(),
            account_a_item: Ownership::default(),
            account_b_item: Ownership::default(),
        }
    }

    #[test]
    fn journal_rejects_cross_run_targets_and_unconfirmed_claims() {
        let input = fixture(PathBuf::from("unused"), "7".repeat(64));
        let mut value = journal(&input);
        assert!(validate_journal(&input, &value).is_ok());
        value.run_nonce = "8".repeat(64);
        assert!(validate_journal(&input, &value).is_err());
        let mut value = journal(&input);
        value.account_b = input.account_a.clone();
        assert!(validate_journal(&input, &value).is_err());
        let mut value = journal(&input);
        value.account_a_item.write_claimed = true;
        assert!(validate_journal(&input, &value).is_err());
        let mut value = journal(&input);
        value.app_password = Ownership {
            absent_confirmed: true,
            write_claimed: true,
        };
        value.profile_uuid = None;
        assert!(validate_journal(&input, &value).is_err());
        let mut value = journal(&input);
        value.profile_uuid = Some("not-an-owned-uuid".into());
        assert!(validate_journal(&input, &value).is_err());
        let mut value = journal(&input);
        value.version = 2;
        assert!(validate_journal(&input, &value).is_err());
    }

    #[test]
    fn journal_schema_rejects_extra_services_duplicate_fields_and_unknown_ownership() {
        let input = fixture(PathBuf::from("unused"), "7".repeat(64));
        let value = journal(&input);
        let mut encoded = serde_json::to_value(&value).expect("synthetic journal serializes");
        encoded
            .as_object_mut()
            .expect("synthetic journal object")
            .insert("service".into(), serde_json::json!("foreign"));
        assert!(serde_json::from_value::<Journal>(encoded).is_err());
        let encoded = serde_json::to_string(&value).expect("synthetic journal serializes");
        assert!(encoded.contains("\"version\":1"));
        let duplicate = encoded.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
        assert!(serde_json::from_str::<Journal>(&duplicate).is_err());
        assert!(
            serde_json::from_str::<Ownership>(
                r#"{"absent_confirmed":true,"write_claimed":true,"delete_any":true}"#
            )
            .is_err()
        );
    }

    fn ordinary_spelling(path: &Path) -> PathBuf {
        #[cfg(target_os = "windows")]
        {
            let value = path.to_str().expect("synthetic temp path is Unicode");
            if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
                return PathBuf::from(format!("\\\\{rest}"));
            }
            if let Some(rest) = value.strip_prefix("\\\\?\\") {
                return PathBuf::from(rest);
            }
        }
        path.to_path_buf()
    }

    #[test]
    fn ordinary_windows_spelling_normalizes_without_weakening_owned_root() {
        struct OwnedDirectory(PathBuf);
        impl Drop for OwnedDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let parent = std::env::temp_dir().join(format!("morpheus-vault-paths-{}", Uuid::new_v4()));
        make_directory(&parent).expect("create synthetic owned parent");
        let cleanup = OwnedDirectory(parent.clone());
        let parent = parent.canonicalize().expect("canonical synthetic parent");
        let nonce = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let root = parent.join(format!("morpheus-native-vault-{nonce}"));
        make_directory(&root).expect("create synthetic owned root");
        let root = root.canonicalize().expect("canonical synthetic root");
        let checkout = parent.join("unrelated-checkout");
        let mut input = fixture(ordinary_spelling(&root), nonce.clone());
        assert!(normalize_paths(&mut input, &parent, &checkout).is_ok());
        assert!(
            input.root == root
                && input.profile == root.join("profile")
                && input.journal == root.join("owned-items.json")
        );
        let mut outside = fixture(ordinary_spelling(&root), nonce.clone());
        outside.journal = parent.join("owned-items.json");
        assert!(normalize_paths(&mut outside, &parent, &checkout).is_err());
        let mut wrong_nonce = fixture(ordinary_spelling(&root), "0".repeat(64));
        assert!(normalize_paths(&mut wrong_nonce, &parent, &checkout).is_err());
        let mut inside_checkout = fixture(ordinary_spelling(&root), nonce);
        assert!(normalize_paths(&mut inside_checkout, &parent, &parent).is_err());
        fs::remove_dir_all(&cleanup.0).expect("remove synthetic owned paths");
    }

    #[derive(Default)]
    struct CleanupObservations {
        targets: Vec<String>,
        profile_attempted: bool,
        diagnostic_attempts: usize,
    }

    fn cleanup_targets() -> Vec<CleanupTarget> {
        ["first", "second", "third"]
            .into_iter()
            .map(|id| CleanupTarget {
                service: ACCOUNT_SERVICE,
                id: id.into(),
                fact: "synthetic_cleanup_fact",
            })
            .collect()
    }

    #[test]
    fn rejected_cleanup_journal_authorizes_no_target_or_profile_operation() {
        let mut observed = CleanupObservations::default();
        let outcome = run_cleanup_plan(
            &mut observed,
            Err("journal_schema"),
            |state, _| {
                state.diagnostic_attempts += 1;
                Ok(())
            },
            |state, target| {
                state.targets.push(target.id.clone());
                Ok(())
            },
            |state| {
                state.profile_attempted = true;
                Ok(())
            },
        );
        assert!(
            outcome
                == Err(CleanupFailure {
                    stage: "cleanup_validate_journal",
                    code: "journal_schema"
                })
        );
        assert!(
            observed.targets.is_empty()
                && !observed.profile_attempted
                && observed.diagnostic_attempts == 0
        );
    }

    #[test]
    fn failed_cleanup_diagnostics_do_not_skip_validated_targets_or_profile() {
        let mut observed = CleanupObservations::default();
        let outcome = run_cleanup_plan(
            &mut observed,
            Ok(cleanup_targets()),
            |state, _| {
                state.diagnostic_attempts += 1;
                Err("file_write")
            },
            |state, target| {
                state.targets.push(target.id.clone());
                if target.id == "first" {
                    Err("cleanup_vault")
                } else {
                    Ok(())
                }
            },
            |state| {
                state.profile_attempted = true;
                Ok(())
            },
        );
        assert!(
            outcome
                == Err(CleanupFailure {
                    stage: "cleanup_owned_entries",
                    code: "file_write"
                })
        );
        assert!(observed.targets == ["first", "second", "third"] && observed.profile_attempted);
        assert!(observed.diagnostic_attempts == 6);
    }
}
