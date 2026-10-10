//! Opt-in account-isolated local message search against the loopback fixture.
//! Credentials remain in its protected JSON file and are never logged.

#![recursion_limit = "512"]

use std::{collections::HashMap, fs, path::Path, time::Duration};

use morpheus_matrix::{AccountManager, MatrixCoreError, QueuedSendOutcome, SyncPolicy};
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
            .expect("Local test account is missing")
    }
}

fn contains_plaintext(path: &Path, needle: &[u8]) -> bool {
    fs::read_dir(path)
        .expect("Cannot list account data")
        .map(|entry| entry.expect("Cannot read account data entry").path())
        .any(|path| {
            if path.is_dir() {
                contains_plaintext(&path, needle)
            } else {
                fs::read(&path)
                    .expect("Cannot read account data file")
                    .windows(needle.len())
                    .any(|window| window == needle)
            }
        })
}

#[tokio::test]
async fn encrypted_message_search_is_account_scoped() {
    let Some(fixture_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local search test; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(fixture_path).expect("Cannot read protected local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let room_id = fixture
        .rooms
        .get("encrypted_room")
        .expect("Encrypted local room is missing");
    let root = std::env::temp_dir().join(format!("morpheus-search-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create local search store");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect local search store");
    }
    let sync_policy = if std::env::var_os("MORPHEUS_MATRIX_SEARCH_CLASSIC").is_some() {
        SyncPolicy::ClassicOnly
    } else {
        SyncPolicy::Auto
    };
    let manager = AccountManager::open_with_sync_policy(&root, sync_policy)
        .expect("Cannot open local search store");
    let alice = fixture.account("a_primary");
    let alice_account = manager
        .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
        .await
        .expect("Cannot sign in Alice");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if manager
            .rooms_snapshot()
            .await
            .expect("Cannot read Alice rooms")
            .iter()
            .any(|room| room.room_id == *room_id)
        {
            break;
        }
        assert!(Instant::now() < deadline, "Alice room did not sync");
        sleep(Duration::from_millis(300)).await;
    }
    assert!(matches!(
        manager
            .search_messages_for(&alice_account.id, room_id, "", 20, 0)
            .await,
        Err(MatrixCoreError::InvalidSearchQuery(_))
    ));

    let marker = format!("morpheus{}", &Uuid::new_v4().simple().to_string()[..10]);
    let body = format!("Searchable encrypted message {marker}");
    let queued = manager
        .send_text_tracked_for(&alice_account.id, room_id, body.clone())
        .await
        .expect("Cannot queue Alice message");
    assert_eq!(
        timeout(Duration::from_secs(60), queued.wait_outcome())
            .await
            .expect("Alice send timed out"),
        QueuedSendOutcome::Sent
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read Alice timeline")
            .iter()
            .any(|message| message.body == body && !message.is_local_echo)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Message remote echo did not sync"
        );
        sleep(Duration::from_millis(300)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(25);
    let found = loop {
        let found = manager
            .search_messages_for(&alice_account.id, room_id, "Searchable", 20, 0)
            .await
            .expect("Cannot search Alice encrypted room");
        if let Some(found) = found.into_iter().find(|hit| hit.body == body) {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "Encrypted message was not indexed"
        );
        sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(found.room_id, *room_id);
    assert!(found.event_id.starts_with('$'));
    assert!(found.timestamp_ms > 0);
    let unique_hits = manager
        .search_messages_for(&alice_account.id, room_id, &marker, 20, 0)
        .await
        .expect("Cannot search for the unique marker");
    assert!(unique_hits.iter().any(|hit| hit.event_id == found.event_id));
    assert!(
        manager
            .search_all_messages_for(&alice_account.id, &marker, 20, 0)
            .await
            .expect("Cannot search Alice rooms")
            .iter()
            .any(|hit| hit.event_id == found.event_id && hit.room_id == *room_id)
    );
    let context = timeout(
        Duration::from_secs(20),
        manager.search_result_context_for(&alice_account.id, room_id, &found.event_id, 4),
    )
    .await
    .expect("Event-focused timeline did not load")
    .expect("Cannot load search result context");
    assert!(context.iter().any(|message| {
        message.event_id.as_deref() == Some(found.event_id.as_str()) && message.body == body
    }));

    // The SDK indexes an edit under its replacement event ID while the
    // timeline displays the original ID. A hit must open that visible message
    // and show m.new_content without the wire-format "* " fallback prefix.
    let original_edit_body = format!("Before edit {}", Uuid::new_v4().simple());
    let edit_send = manager
        .send_text_tracked_for(&alice_account.id, room_id, original_edit_body.clone())
        .await
        .expect("Cannot queue message to edit");
    assert_eq!(
        timeout(Duration::from_secs(60), edit_send.wait_outcome())
            .await
            .expect("Message to edit timed out"),
        QueuedSendOutcome::Sent
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let original_edit_id = loop {
        let event_id = manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read message to edit")
            .into_iter()
            .find(|message| message.body == original_edit_body && !message.is_local_echo)
            .and_then(|message| message.event_id);
        if let Some(event_id) = event_id {
            break event_id;
        }
        assert!(Instant::now() < deadline, "Message to edit did not sync");
        sleep(Duration::from_millis(300)).await;
    };
    let edit_marker = format!("morpheusedit{}", &Uuid::new_v4().simple().to_string()[..10]);
    let edited_body = format!("After edit {edit_marker}");
    manager
        .edit_text(room_id, &original_edit_id, edited_body.clone())
        .await
        .expect("Cannot edit searchable message");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let visible = manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read edited timeline")
            .iter()
            .any(|message| {
                message.event_id.as_deref() == Some(original_edit_id.as_str())
                    && message.body == edited_body
            });
        if visible {
            break;
        }
        assert!(Instant::now() < deadline, "Edited message did not appear");
        sleep(Duration::from_millis(300)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let edited_hit = loop {
        let hits = manager
            .search_messages_for(&alice_account.id, room_id, &edit_marker, 20, 0)
            .await
            .expect("Cannot search edited message");
        if let Some(hit) = hits.into_iter().find(|hit| hit.body == edited_body) {
            break hit;
        }
        assert!(Instant::now() < deadline, "Edited message was not indexed");
        sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(edited_hit.event_id, original_edit_id);
    assert_eq!(edited_hit.body, edited_body);
    let edited_context = manager
        .search_result_context_for(&alice_account.id, room_id, &edited_hit.event_id, 4)
        .await
        .expect("Cannot load edited message context");
    assert!(edited_context.iter().any(|message| {
        message.event_id.as_deref() == Some(original_edit_id.as_str())
            && message.body == edited_body
    }));

    let carol = fixture.account("b_primary");
    let carol_account = manager
        .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
        .await
        .expect("Cannot sign in Carol");
    assert!(matches!(
        manager
            .search_messages_for(&alice_account.id, room_id, &marker, 20, 0)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    let carol_room = fixture.rooms.get("b_room").expect("Carol room is missing");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if manager
            .rooms_snapshot()
            .await
            .expect("Cannot read Carol rooms")
            .iter()
            .any(|room| room.room_id == *carol_room)
        {
            break;
        }
        assert!(Instant::now() < deadline, "Carol room did not sync");
        sleep(Duration::from_millis(300)).await;
    }
    assert!(
        manager
            .search_messages_for(&carol_account.id, carol_room, &marker, 20, 0)
            .await
            .expect("Cannot search Carol room")
            .is_empty()
    );
    assert!(
        manager
            .search_all_messages_for(&carol_account.id, &marker, 20, 0)
            .await
            .expect("Cannot search Carol rooms")
            .is_empty()
    );
    manager
        .switch_to(&alice_account.id)
        .await
        .expect("Cannot return to Alice");
    manager.stop().await;
    drop(manager);
    assert!(
        !contains_plaintext(&root, body.as_bytes()),
        "Encrypted message or search index leaked plaintext into account data"
    );

    let manager = AccountManager::open_with_sync_policy(&root, sync_policy)
        .expect("Cannot reopen search account store");
    let restored = manager
        .restore_last_active()
        .await
        .expect("Cannot restore Alice search session")
        .expect("Alice search session was not restored");
    assert_eq!(restored.id, alice_account.id);
    assert!(
        manager
            .search_messages_for(&alice_account.id, room_id, &marker, 20, 0)
            .await
            .expect("Cannot read persisted encrypted search index")
            .iter()
            .any(|hit| hit.event_id == found.event_id && hit.body == body)
    );
    assert!(
        manager
            .search_all_messages_for(&alice_account.id, &marker, 20, 0)
            .await
            .expect("Cannot read persisted global search index")
            .iter()
            .any(|hit| hit.event_id == found.event_id)
    );
    manager
        .logout_account(&carol_account.id)
        .await
        .expect("Cannot remove Carol test device");
    manager
        .logout_account(&alice_account.id)
        .await
        .expect("Cannot remove Alice test device");
    manager.stop().await;
    fs::remove_dir_all(&root).expect("Cannot remove local search stores");
    eprintln!(
        "Local encrypted search passed: message context, exact term, account isolation, restart, plaintext scan."
    );
}
