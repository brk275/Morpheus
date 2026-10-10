//! Opt-in encrypted formatting acceptance against disposable loopback Synapse.
//! Fixture credentials stay in the protected JSON file and are never logged.
#![recursion_limit = "512"]

use std::{fs, path::PathBuf, time::Duration};

use matrix_sdk::{
    Client,
    room::MessagesOptions,
    ruma::{EventId, RoomId, api::client::room::get_room_event, uint},
};
use morpheus_matrix::{
    AccountManager, MatrixCoreError, QueuedSendOutcome, RoomMembership, TextFormat,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    user_id: String,
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
}

async fn wait_for_room(manager: &AccountManager, room_id: &str, expected: RoomMembership) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let membership = manager
            .rooms_snapshot()
            .await
            .expect("Cannot read disposable room list")
            .into_iter()
            .find(|room| room.room_id == room_id)
            .map(|room| room.membership);
        if membership == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Disposable room did not reach {expected:?}; last: {membership:?}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_remote_message(manager: &AccountManager, room_id: &str, body: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(event_id) = manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read disposable room timeline")
            .into_iter()
            .find(|message| message.body == body && !message.is_local_echo)
            .and_then(|message| message.event_id)
        {
            return event_id;
        }
        assert!(
            Instant::now() < deadline,
            "Remote formatted message did not sync"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn decrypted_event(client: &Client, room_id: &str, event_id: &str) -> Value {
    let room = client
        .get_room(&RoomId::parse(room_id).expect("Invalid disposable room ID"))
        .expect("Disposable room missing from SDK client");
    let event = room
        .event(
            &EventId::parse(event_id).expect("Invalid disposable event ID"),
            None,
        )
        .await
        .expect("Cannot fetch encrypted Matrix event");
    serde_json::from_str(event.raw().json().get()).expect("Invalid decrypted Matrix event JSON")
}

async fn assert_encrypted_on_server(client: &Client, room_id: &str, event_id: &str) {
    let request = get_room_event::v3::Request::new(
        RoomId::parse(room_id).expect("Invalid disposable room ID"),
        EventId::parse(event_id).expect("Invalid disposable event ID"),
    );
    let event = client
        .send(request)
        .await
        .expect("Cannot fetch authoritative Matrix event");
    let outer: Value =
        serde_json::from_str(event.event.json().get()).expect("Invalid server event JSON");
    assert_eq!(outer["type"], "m.room.encrypted");
    assert!(outer["content"]["ciphertext"].as_str().is_some());
    assert!(outer["content"].get("formatted_body").is_none());
}

async fn wait_for_relation(
    client: &Client,
    room_id: &str,
    root_event_id: &str,
    kind: &str,
    marker: &str,
) -> (String, Value) {
    let room = client
        .get_room(&RoomId::parse(room_id).expect("Invalid disposable room ID"))
        .expect("Disposable room missing from SDK client");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        // The server's `/relations` endpoint need not index encrypted replies.
        // Inspect the actual decrypted room timeline on Bob's device instead.
        let mut options = MessagesOptions::backward();
        options.limit = uint!(50);
        let messages = room
            .messages(options)
            .await
            .expect("Cannot fetch encrypted room messages");
        for event in messages.chunk {
            let value: Value = serde_json::from_str(event.raw().json().get())
                .expect("Invalid decrypted relation JSON");
            let relation = &value["content"]["m.relates_to"];
            let matches_kind = match kind {
                "reply" => relation["m.in_reply_to"]["event_id"] == root_event_id,
                "edit" => {
                    relation["rel_type"] == "m.replace" && relation["event_id"] == root_event_id
                }
                _ => unreachable!(),
            };
            let contains_marker = value["content"]["body"]
                .as_str()
                .is_some_and(|body| body.contains(marker));
            if matches_kind && contains_marker {
                return (
                    event
                        .event_id()
                        .expect("Related event lacks an ID")
                        .to_string(),
                    value,
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "Formatted {kind} did not reach Bob's decrypted timeline"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

fn assert_safe_html(content: &Value, expected_source: &str, emphasis: &str) {
    let body = content["body"].as_str().expect("Message has no plain body");
    assert!(
        body.contains(expected_source),
        "Plain body lost the source text"
    );
    assert_eq!(content["format"], "org.matrix.custom.html");
    let formatted = content["formatted_body"]
        .as_str()
        .expect("Formatted message has no HTML fallback");
    assert!(
        formatted.contains(&format!("<strong>{emphasis}</strong>")),
        "Markdown emphasis was not formatted"
    );
    assert!(
        !formatted.contains("<script"),
        "Raw HTML became active content"
    );
    assert!(
        formatted.contains("&lt;script&gt;"),
        "Raw HTML was not escaped in formatted content"
    );
}

#[tokio::test]
async fn formatted_main_reply_and_edit_round_trip_in_fresh_encrypted_room() {
    let Some(fixture_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local formatted-message test; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(fixture_path)).expect("Cannot read protected Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let alice = fixture.account("a_primary");
    let bob = fixture.account("a_second_device");
    let carol = fixture.account("b_primary");

    let root = std::env::temp_dir().join(format!("morpheus-format-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create disposable account stores");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect disposable account stores");
    }

    let creator = AccountManager::open(root.join("alice")).expect("Cannot open Alice store");
    let peer = AccountManager::open(root.join("bob")).expect("Cannot open Bob store");
    let alice_account = creator
        .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
        .await
        .expect("Cannot sign in Alice");
    let bob_account = peer
        .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
        .await
        .expect("Cannot sign in Bob");
    let alice_client = creator.active_client().await.expect("Alice client missing");
    let bob_client = peer.active_client().await.expect("Bob client missing");
    let marker = Uuid::new_v4().simple().to_string();
    let room_id = creator
        .create_private_room(&format!("Morpheus formatting {marker}"))
        .await
        .expect("Cannot create formatting test room");
    wait_for_room(&creator, &room_id, RoomMembership::Joined).await;
    let room = alice_client
        .get_room(&RoomId::parse(&room_id).expect("Invalid disposable room ID"))
        .expect("Created room missing from Alice client");
    timeout(Duration::from_secs(30), async {
        loop {
            if room
                .latest_encryption_state()
                .await
                .expect("Cannot inspect room encryption")
                .is_encrypted()
            {
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("new private room did not reach encrypted state after creation");
    creator
        .invite_user_for(&alice_account.id, &room_id, &bob.user_id)
        .await
        .expect("Cannot invite Bob to encrypted formatting room");
    wait_for_room(&peer, &room_id, RoomMembership::Invited).await;
    peer.accept_invite(&room_id)
        .await
        .expect("Bob cannot join encrypted formatting room");
    wait_for_room(&peer, &room_id, RoomMembership::Joined).await;

    let main_source = format!("Hello **main-{marker}** <script>alert(1)</script>");
    assert!(matches!(
        creator
            .send_formatted_tracked_for(
                &bob_account.id,
                &room_id,
                &main_source,
                TextFormat::Markdown
            )
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    let queued = creator
        .send_formatted_tracked_for(
            &alice_account.id,
            &room_id,
            &main_source,
            TextFormat::Markdown,
        )
        .await
        .expect("Cannot queue formatted main message");
    assert_eq!(
        timeout(Duration::from_secs(60), queued.wait_outcome())
            .await
            .expect("Formatted main send timed out"),
        QueuedSendOutcome::Sent
    );
    let root_event_id = wait_for_remote_message(&creator, &room_id, &main_source).await;
    wait_for_remote_message(&peer, &room_id, &main_source).await;
    assert_encrypted_on_server(&alice_client, &room_id, &root_event_id).await;
    let main = decrypted_event(&alice_client, &room_id, &root_event_id).await;
    assert_eq!(main["type"], "m.room.message");
    assert_eq!(main["content"]["body"], main_source);
    assert_safe_html(&main["content"], &main_source, &format!("main-{marker}"));
    let main_projection = peer
        .messages_snapshot(&room_id)
        .await
        .expect("Cannot project Bob's formatted timeline")
        .into_iter()
        .find(|message| message.event_id.as_deref() == Some(root_event_id.as_str()))
        .expect("Bob's formatted message missing from the timeline projection");
    assert_eq!(main_projection.body, main_source);
    assert!(
        main_projection
            .rich_markdown
            .as_deref()
            .is_some_and(|markdown| markdown
                .replace("\\-", "-")
                .contains(&format!("**main-{marker}**"))),
        "Bob's decrypted formatted message did not reach the safe rich projection"
    );

    let reply_source = format!("Reply **reply-{marker}** <script>alert(2)</script>");
    creator
        .send_reply_formatted_for(
            &alice_account.id,
            &room_id,
            &root_event_id,
            &reply_source,
            TextFormat::Markdown,
        )
        .await
        .expect("Cannot queue formatted reply");
    let (reply_id, reply) = wait_for_relation(
        &bob_client,
        &room_id,
        &root_event_id,
        "reply",
        &reply_source,
    )
    .await;
    assert_encrypted_on_server(&alice_client, &room_id, &reply_id).await;
    assert!(
        reply["content"]["body"]
            .as_str()
            .expect("Reply lacks a plain fallback")
            .ends_with(&reply_source),
        "Reply fallback did not preserve the authored source"
    );
    assert_safe_html(&reply["content"], &reply_source, &format!("reply-{marker}"));

    let edit_source = format!("Edited **edit-{marker}** <script>alert(3)</script>");
    creator
        .edit_formatted_for(
            &alice_account.id,
            &room_id,
            &root_event_id,
            &edit_source,
            TextFormat::Markdown,
        )
        .await
        .expect("Cannot queue formatted edit");
    let (edit_id, edit) =
        wait_for_relation(&bob_client, &room_id, &root_event_id, "edit", &edit_source).await;
    assert_encrypted_on_server(&alice_client, &room_id, &edit_id).await;
    assert_eq!(edit["content"]["m.new_content"]["body"], edit_source);
    assert_safe_html(
        &edit["content"]["m.new_content"],
        &edit_source,
        &format!("edit-{marker}"),
    );
    wait_for_remote_message(&peer, &room_id, &edit_source).await;
    let edited_projection = peer
        .messages_snapshot(&room_id)
        .await
        .expect("Cannot project Bob's edited timeline")
        .into_iter()
        .find(|message| message.event_id.as_deref() == Some(root_event_id.as_str()))
        .expect("Bob's edited message missing from the timeline projection");
    assert_eq!(edited_projection.body, edit_source);
    assert!(
        edited_projection
            .rich_markdown
            .as_deref()
            .is_some_and(|markdown| markdown
                .replace("\\-", "-")
                .contains(&format!("**edit-{marker}**"))),
        "Bob's decrypted formatted edit did not reach the safe rich projection"
    );

    // A callback holding Alice's account ID must not send after selecting a
    // different homeserver, even though the old room ID is still in memory.
    let carol_account = creator
        .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
        .await
        .expect("Cannot sign in Carol on another homeserver");
    assert!(matches!(
        creator
            .send_formatted_tracked_for(
                &alice_account.id,
                &room_id,
                "blocked",
                TextFormat::Markdown
            )
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        creator
            .send_reply_formatted_for(
                &alice_account.id,
                &room_id,
                &root_event_id,
                "blocked",
                TextFormat::Markdown,
            )
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        creator
            .edit_formatted_for(
                &alice_account.id,
                &room_id,
                &root_event_id,
                "blocked",
                TextFormat::Markdown,
            )
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));

    creator
        .logout_account(&carol_account.id)
        .await
        .expect("Cannot remove Carol test session");
    peer.logout_account(&bob_account.id)
        .await
        .expect("Cannot remove Bob test session");
    creator
        .logout_account(&alice_account.id)
        .await
        .expect("Cannot remove Alice test session");
    peer.stop().await;
    creator.stop().await;
    fs::remove_dir_all(&root).expect("Cannot remove disposable account stores");
    eprintln!("Fresh encrypted room formatted main/reply/edit and stale-account guards passed.");
}
