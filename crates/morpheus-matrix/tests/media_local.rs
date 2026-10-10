//! Opt-in encrypted attachment roundtrip against two disposable local Matrix
//! accounts. Credentials are read from a protected fixture and never logged.
#![recursion_limit = "512"]

use std::{collections::HashMap, fs, path::Path, time::Duration};

use morpheus_matrix::{AccountManager, AttachmentKind, AttachmentSendUpdate, MatrixCoreError};
use serde::Deserialize;
use tokio::time::{Instant, sleep, timeout};
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
            .expect("Missing local account")
    }
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if manager
            .rooms_snapshot()
            .await
            .expect("Room snapshot failed")
            .iter()
            .any(|room| room.room_id == room_id)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for encrypted room"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

fn contains_plaintext(path: &Path, needle: &[u8]) -> bool {
    for entry in fs::read_dir(path).expect("Cannot inspect account store") {
        let path = entry.expect("Cannot inspect store entry").path();
        if path.is_dir() {
            if contains_plaintext(&path, needle) {
                return true;
            }
        } else if fs::read(&path)
            .expect("Cannot read account store file")
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
    }
    false
}

#[test]
fn encrypted_file_roundtrips_between_disposable_accounts() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped live media test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(credentials_path).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let root = std::env::temp_dir().join(format!("morpheus-media-test-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create test root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let filename = format!("encrypted-{}.bin", Uuid::new_v4());
    let file_path = root.join(&filename);
    let payload = format!("morpheus-private-media-{}", Uuid::new_v4())
        .repeat(512)
        .into_bytes();
    fs::write(&file_path, &payload).expect("Cannot create test attachment");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot create test runtime");
    runtime.block_on(async {
        let alice = fixture.account("a_primary");
        let bob = fixture.account("a_second_device");
        let room_id = fixture
            .rooms
            .get("encrypted_room")
            .expect("Missing encrypted room");
        let sender = AccountManager::open(root.join("sender")).expect("Cannot open sender store");
        let sender_account = sender
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Sender login failed");
        let receiver =
            AccountManager::open(root.join("receiver")).expect("Cannot open receiver store");
        let receiver_account = receiver
            .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
            .await
            .expect("Receiver login failed");
        wait_for_room(&sender, room_id).await;
        wait_for_room(&receiver, room_id).await;
        let receiver_feed = receiver
            .message_feed(room_id)
            .await
            .expect("Cannot subscribe to receiver timeline");

        let mut queued = sender
            .send_attachment_for(
                &sender_account.id,
                room_id,
                &file_path,
                "application/octet-stream",
            )
            .await
            .expect("Cannot queue encrypted attachment");
        let event_id = timeout(Duration::from_secs(90), async {
            loop {
                match queued.next_update().await {
                    AttachmentSendUpdate::Progress { .. } => {}
                    AttachmentSendUpdate::Sent { event_id } => return event_id,
                    AttachmentSendUpdate::Retained { recoverable: true } => {
                        // The selected-account queue supervisor will resume a
                        // transient failure without enqueueing a duplicate.
                    }
                    AttachmentSendUpdate::Retained { recoverable: false }
                    | AttachmentSendUpdate::Cancelled
                    | AttachmentSendUpdate::Unknown => panic!("Encrypted upload did not finish"),
                }
            }
        })
        .await
        .expect("Timed out waiting for encrypted upload");
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(item) = receiver_feed
                .snapshot()
                .into_iter()
                .find(|item| item.event_id.as_deref() == Some(event_id.as_str()))
            {
                assert_eq!(item.attachment_name.as_deref(), Some(filename.as_str()));
                assert!(!item.body.contains("mxc://"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out awaiting attachment card"
            );
            sleep(Duration::from_millis(300)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let downloaded = loop {
            if let Ok(downloaded) = receiver
                .download_attachment_for(&receiver_account.id, room_id, &event_id)
                .await
            {
                break downloaded;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out decrypting received attachment"
            );
            sleep(Duration::from_millis(350)).await;
        };
        assert_eq!(downloaded.bytes, payload);
        assert_eq!(downloaded.metadata.filename, filename);
        assert_eq!(downloaded.metadata.mime_type, "application/octet-stream");
        assert_eq!(
            downloaded.metadata.declared_size,
            Some(payload.len() as u64)
        );
        assert_eq!(downloaded.metadata.kind, AttachmentKind::File);
        assert!(downloaded.metadata.encrypted);

        let mismatch = receiver
            .download_attachment_for(&sender_account.id, room_id, &event_id)
            .await;
        assert!(matches!(
            mismatch,
            Err(MatrixCoreError::SelectedAccountChanged)
        ));
        sender.stop().await;
        receiver.stop().await;

        // The input file is intentionally plaintext; remove it before
        // checking the encrypted SDK stores. Then log out the disposable
        // devices so their OS-vault entries do not accumulate across runs.
        fs::remove_file(&file_path).expect("Cannot remove test input");
        assert!(!contains_plaintext(&root.join("sender"), &payload[..80]));
        assert!(!contains_plaintext(&root.join("receiver"), &payload[..80]));
        sender
            .logout_account(&sender_account.id)
            .await
            .expect("Cannot clean sender device");
        receiver
            .logout_account(&receiver_account.id)
            .await
            .expect("Cannot clean receiver device");
    });
    fs::remove_dir_all(&root).expect("Cannot clean test root");
}
