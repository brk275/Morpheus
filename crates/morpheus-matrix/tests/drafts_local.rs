//! Opt-in encrypted draft acceptance test against two disposable local Synapse
//! homeservers. Fixture credentials are read from a mode-0600 file and never
//! printed by this test.
#![recursion_limit = "512"]

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use morpheus_matrix::{AccountManager, QueuedSendOutcome};
use serde::Deserialize;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

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
            .expect("Missing account")
    }

    fn room(&self, label: &str) -> &str {
        self.rooms.get(label).expect("Missing room")
    }
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if manager
            .rooms_snapshot()
            .await
            .expect("Cannot read rooms")
            .iter()
            .any(|room| room.room_id == room_id)
        {
            return;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for room");
        sleep(Duration::from_millis(300)).await;
    }
}

fn contains_plaintext(path: &Path, needle: &[u8]) -> bool {
    let entries = fs::read_dir(path).expect("Cannot list account data");
    for entry in entries {
        let path = entry.expect("Cannot read account data entry").path();
        if path.is_dir() {
            if contains_plaintext(&path, needle) {
                return true;
            }
        } else if fs::read(&path)
            .expect("Cannot read account data file")
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
    }
    false
}

#[test]
fn drafts_survive_restart_without_plaintext_or_account_crossover() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local draft test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local fixture"),
    )
    .expect("Invalid local fixture");
    let root = std::env::temp_dir().join(format!("morpheus-drafts-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create account data directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect account data directory");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local test runtime");
    runtime.block_on(async {
        let alice = fixture.account("a_primary");
        let carol = fixture.account("b_primary");
        let a_room = fixture.room("a_room");
        let b_room = fixture.room("b_room");
        let a_body = format!("private-draft-A-{}", Uuid::new_v4());
        let b_body = format!("private-draft-B-{}", Uuid::new_v4());

        let manager = AccountManager::open(&root).expect("Cannot open account manager");
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, a_room).await;
        manager
            .save_composer_draft_for(&a_account.id, a_room, &a_body)
            .await
            .expect("Cannot persist first draft");
        let queued = manager
            .send_text_tracked_for(
                &a_account.id,
                a_room,
                format!("Morpheus tracked-send acceptance {}", Uuid::new_v4()),
            )
            .await
            .expect("Cannot queue tracked message");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), queued.wait_outcome())
                .await
                .expect("Timed out waiting for server send result"),
            QueuedSendOutcome::Sent
        );

        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        wait_for_room(&manager, b_room).await;
        assert_eq!(
            manager
                .load_composer_draft_for(&b_account.id, b_room)
                .await
                .unwrap(),
            ""
        );
        manager
            .save_composer_draft_for(&b_account.id, b_room, &b_body)
            .await
            .expect("Cannot persist second draft");
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            a_body
        );

        manager.stop().await;
        drop(manager);
        assert!(!contains_plaintext(&root, a_body.as_bytes()));
        assert!(!contains_plaintext(&root, b_body.as_bytes()));

        let manager = AccountManager::open(&root).expect("Cannot reopen account manager");
        let restored = manager
            .restore_last_active()
            .await
            .expect("Cannot restore session");
        assert_eq!(restored.expect("Missing restored account").id, b_account.id);
        assert_eq!(
            manager
                .load_composer_draft_for(&b_account.id, b_room)
                .await
                .unwrap(),
            b_body
        );
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch accounts");
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            a_body
        );
        manager
            .save_composer_draft_for(&a_account.id, a_room, "")
            .await
            .expect("Cannot clear sent draft");
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            ""
        );
        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        assert!(!root.join("accounts").join(&a_account.id).exists());
        manager
            .switch_to(&b_account.id)
            .await
            .expect("Cannot switch to second account");
        assert_eq!(
            manager
                .load_composer_draft_for(&b_account.id, b_room)
                .await
                .unwrap(),
            b_body
        );
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
    });
    fs::remove_dir_all(root).expect("Cannot clean local test data");
}
