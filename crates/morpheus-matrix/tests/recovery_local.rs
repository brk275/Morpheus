//! Opt-in encrypted-history recovery test against scripts/local_matrix_env.py.
//! Set MORPHEUS_MATRIX_TEST_RECOVERY=1 and MORPHEUS_MATRIX_TEST_ACCOUNTS to
//! the fixture's mode-0600 JSON path. The recovery key stays in memory only.
#![recursion_limit = "512"]

#[path = "support/opened_room.rs"]
mod opened_room;

use std::{collections::HashMap, fs, future::Future, path::PathBuf, time::Duration};

use morpheus_matrix::{AccountInfo, AccountManager, MatrixCoreError, MessageInfo};
use serde::Deserialize;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

type TestResult<T> = Result<T, &'static str>;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(45);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
    rooms: HashMap<String, String>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    username: String,
    homeserver_url: String,
    password: String,
}

impl Fixture {
    fn account(&self, label: &str) -> &TestAccount {
        self.accounts
            .iter()
            .find(|account| account.label == label)
            .unwrap_or_else(|| panic!("Missing local test account {label}"))
    }

    fn room(&self, key: &str) -> &str {
        self.rooms
            .get(key)
            .unwrap_or_else(|| panic!("Missing local test room {key}"))
    }
}

async fn sdk_call<T>(
    future: impl Future<Output = Result<T, MatrixCoreError>>,
    limit: Duration,
    timeout_stage: &'static str,
    error_stage: &'static str,
) -> TestResult<T> {
    timeout(limit, future)
        .await
        .map_err(|_| timeout_stage)?
        .map_err(|_| error_stage)
}

async fn sign_in(
    manager: &AccountManager,
    account: &TestAccount,
    timeout_stage: &'static str,
    error_stage: &'static str,
) -> TestResult<AccountInfo> {
    sdk_call(
        manager.add_password_account(
            &account.homeserver_url,
            &account.username,
            &account.password,
        ),
        NETWORK_TIMEOUT,
        timeout_stage,
        error_stage,
    )
    .await
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) -> TestResult<()> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let rooms = sdk_call(
            manager.rooms_snapshot(),
            SNAPSHOT_TIMEOUT,
            "Room snapshot timed out",
            "Room snapshot failed",
        )
        .await?;
        if rooms.iter().any(|room| room.room_id == room_id) {
            opened_room::scan(manager, room_id).await?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Encrypted room did not appear before deadline");
        }
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_decrypted_message(
    manager: &AccountManager,
    room_id: &str,
    body: &str,
    require_remote_echo: bool,
    deadline_stage: &'static str,
) -> TestResult<MessageInfo> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let messages = sdk_call(
            manager.messages_snapshot(room_id),
            SNAPSHOT_TIMEOUT,
            "Timeline snapshot timed out",
            "Timeline snapshot failed",
        )
        .await?;
        if let Some(message) = messages.into_iter().find(|message| {
            message.body == body && (!require_remote_echo || !message.is_local_echo)
        }) {
            return Ok(message);
        }
        if Instant::now() >= deadline {
            return Err(deadline_stage);
        }
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_raw_event(
    manager: &AccountManager,
    room_id: &str,
    event_id: &str,
) -> TestResult<()> {
    let timeline = sdk_call(
        manager.timeline(room_id),
        SNAPSHOT_TIMEOUT,
        "Fresh-device timeline creation timed out",
        "Fresh-device timeline creation failed",
    )
    .await?;
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let (items, _) = timeout(SNAPSHOT_TIMEOUT, timeline.subscribe())
            .await
            .map_err(|_| "Fresh-device timeline subscription timed out")?;
        if items.iter().any(|item| {
            item.as_event()
                .and_then(|event| event.event_id())
                .is_some_and(|id| id.as_str() == event_id)
        }) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Encrypted event did not reach fresh device before deadline");
        }
        sleep(Duration::from_millis(300)).await;
    }
}

async fn best_effort_logout(manager: &AccountManager, account_id: Option<&str>) {
    if let Some(account_id) = account_id {
        let _ = timeout(NETWORK_TIMEOUT, manager.logout_account(account_id)).await;
    }
    let _ = timeout(SNAPSHOT_TIMEOUT, manager.stop()).await;
}

#[test]
fn encrypted_history_recovers_on_fresh_alice_device() {
    if std::env::var("MORPHEUS_MATRIX_TEST_RECOVERY").as_deref() != Ok("1") {
        eprintln!("Skipped local recovery test; set MORPHEUS_MATRIX_TEST_RECOVERY=1 to opt in.");
        return;
    }
    let credentials_path = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS")
        .map(PathBuf::from)
        .expect("Set MORPHEUS_MATRIX_TEST_ACCOUNTS to the local fixture path");
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(credentials_path).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let root = std::env::temp_dir().join(format!("morpheus-recovery-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create local recovery test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect local recovery test directory");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local recovery test runtime");
    let result = runtime.block_on(async {
        let old = AccountManager::open(root.join("alice-old"))
            .expect("Cannot open old Alice account store");
        let bob = AccountManager::open(root.join("bob")).expect("Cannot open Bob account store");
        let fresh = AccountManager::open(root.join("alice-fresh"))
            .expect("Cannot open fresh Alice account store");
        let mut old_id = None;
        let mut bob_id = None;
        let mut fresh_id = None;
        let outcome: TestResult<()> = async {
            let alice_fixture = fixture.account("a_primary");
            let bob_fixture = fixture.account("a_second_device");
            let room_id = fixture.room("encrypted_room");

            let alice = sign_in(
                &old,
                alice_fixture,
                "Old Alice login timed out",
                "Old Alice login failed",
            )
            .await?;
            old_id = Some(alice.id.clone());
            wait_for_room(&old, room_id).await?;
            let old_client = sdk_call(
                old.active_client(),
                SNAPSHOT_TIMEOUT,
                "Old Alice client lookup timed out",
                "Old Alice client lookup failed",
            )
            .await?;
            let backup_exists = timeout(
                SNAPSHOT_TIMEOUT,
                old_client.encryption().backups().fetch_exists_on_server(),
            )
            .await
            .map_err(|_| "Existing-backup check timed out")?
            .map_err(|_| "Existing-backup check failed")?;
            if backup_exists {
                return Err("Fixture Alice already has a key backup; reset the disposable fixture");
            }
            drop(old_client);
            let bob_account =
                sign_in(&bob, bob_fixture, "Bob login timed out", "Bob login failed").await?;
            bob_id = Some(bob_account.id.clone());
            wait_for_room(&bob, room_id).await?;

            let body = format!("Morpheus recovery acceptance {}", Uuid::new_v4());
            sdk_call(
                bob.send_text_for(&bob_account.id, room_id, body.clone()),
                NETWORK_TIMEOUT,
                "Encrypted send timed out",
                "Encrypted send failed",
            )
            .await?;
            let sent = wait_for_decrypted_message(
                &bob,
                room_id,
                &body,
                true,
                "Second-device remote echo did not decrypt before deadline",
            )
            .await?;
            let event_id = sent.event_id.ok_or("Sent event has no Matrix event ID")?;
            wait_for_decrypted_message(
                &old,
                room_id,
                &body,
                false,
                "Old Alice did not decrypt message before deadline",
            )
            .await?;

            // Wait for the old Alice device's room keys to reach server backup.
            // The returned RecoveryKey is zeroized on drop and never formatted.
            let recovery_key = sdk_call(
                old.enable_recovery(None, true),
                RECOVERY_TIMEOUT,
                "Recovery setup or key-backup upload timed out",
                "Recovery setup or key-backup upload failed",
            )
            .await?;
            // The pinned SDK's Enable future logs upload errors and can still
            // return a key. Require a clean backup state before retiring the
            // device holding this inbound Megolm session.
            let old_client = sdk_call(
                old.active_client(),
                SNAPSHOT_TIMEOUT,
                "Old Alice client lookup timed out",
                "Old Alice client lookup failed",
            )
            .await?;
            timeout(
                RECOVERY_TIMEOUT,
                old_client.encryption().backups().wait_for_steady_state(),
            )
            .await
            .map_err(|_| "Room-key backup did not reach steady state before deadline")?
            .map_err(|_| "Room-key backup upload failed")?;

            // Retire both devices that hold the room key before creating Alice's
            // new device, so live key sharing cannot satisfy the assertion.
            sdk_call(
                old.logout_account(&alice.id),
                NETWORK_TIMEOUT,
                "Old Alice logout timed out",
                "Old Alice logout failed",
            )
            .await?;
            old_id = None;
            sdk_call(
                bob.logout_account(&bob_account.id),
                NETWORK_TIMEOUT,
                "Bob logout timed out",
                "Bob logout failed",
            )
            .await?;
            bob_id = None;

            let alice_fresh = sign_in(
                &fresh,
                alice_fixture,
                "Fresh Alice login timed out",
                "Fresh Alice login failed",
            )
            .await?;
            fresh_id = Some(alice_fresh.id.clone());
            wait_for_room(&fresh, room_id).await?;
            wait_for_raw_event(&fresh, room_id, &event_id).await?;
            let before = sdk_call(
                fresh.messages_snapshot(room_id),
                SNAPSHOT_TIMEOUT,
                "Pre-recovery timeline snapshot timed out",
                "Pre-recovery timeline snapshot failed",
            )
            .await?;
            if before.iter().any(|message| message.body == body) {
                return Err("Fresh device decrypted history before recovery");
            }

            sdk_call(
                fresh.recover_account(recovery_key.reveal()),
                RECOVERY_TIMEOUT,
                "Fresh-device recovery timed out",
                "Fresh-device recovery failed",
            )
            .await?;
            wait_for_decrypted_message(
                &fresh,
                room_id,
                &body,
                false,
                "Fresh Alice did not decrypt backed-up history before deadline",
            )
            .await?;
            Ok(())
        }
        .await;

        best_effort_logout(&fresh, fresh_id.as_deref()).await;
        best_effort_logout(&bob, bob_id.as_deref()).await;
        best_effort_logout(&old, old_id.as_deref()).await;
        outcome
    });
    fs::remove_dir_all(&root).expect("Cannot remove local recovery test directory");
    assert!(
        result.is_ok(),
        "Local recovery acceptance failed: {result:?}"
    );
    eprintln!("Local Matrix recovery passed: encrypted history decrypted on a fresh device.");
}
