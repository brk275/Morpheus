//! Opt-in encrypted formatting acceptance against disposable loopback Synapse.
//! Fixture credentials stay in the protected JSON file and are never logged.
#![recursion_limit = "512"]

#[path = "formatted_local/identity.rs"]
mod identity;

use std::{fs, path::PathBuf, time::Duration};

use matrix_sdk::{
    Client,
    room::MessagesOptions,
    ruma::{EventId, RoomId, api::client::room::get_room_event, uint},
};
use morpheus_matrix::{
    AccountManager, ComposeSendTarget, ComposeText, MatrixCoreError, MentionTarget,
    QueuedSendOutcome, RoomMembership, TextFormat,
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
                "thread" => {
                    relation["rel_type"] == "m.thread" && relation["event_id"] == root_event_id
                }
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

fn selected(body: String, format: TextFormat, user: &str, room: bool) -> ComposeText {
    let mut text = ComposeText::new(body, format);
    let start = text.body.find(user).expect("Selected user token missing");
    text = text
        .insert_mention(
            start..start + user.len(),
            MentionTarget::User(user.to_owned()),
        )
        .expect("Cannot encode selected user")
        .0;
    if room {
        let start = text
            .body
            .find("@room")
            .expect("Selected room token missing");
        text = text
            .insert_mention(start..start + 5, MentionTarget::Room)
            .expect("Cannot encode selected room")
            .0;
    }
    text
}

fn mentions(content: &Value, user: Option<&str>, room: bool) {
    let value = content
        .get("m.mentions")
        .expect("Explicit mention metadata missing");
    let expected: Vec<&str> = user.into_iter().collect();
    let actual: Vec<&str> = value
        .get("user_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(actual == expected, "Selected user metadata differs");
    assert!(
        value.get("room").and_then(Value::as_bool).unwrap_or(false) == room,
        "Room notification metadata differs"
    );
    assert!(value.is_object(), "Mention metadata is not an object");
}

async fn delivered(queued: morpheus_matrix::QueuedMessage) {
    assert!(
        matches!(
            timeout(Duration::from_secs(60), queued.wait_outcome()).await,
            Ok(QueuedSendOutcome::Sent)
        ),
        "Typed message was not acknowledged"
    );
}

async fn ready_edit(
    manager: &AccountManager,
    selection: &morpheus_matrix::AccountSelection,
    room: &str,
    event: &str,
    expected: &ComposeText,
) -> morpheus_matrix::EditComposition {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        // Room and thread timelines plus queue ack must actually converge. An
        // observed body alone is not an effective-edit compare-and-set token.
        if let Ok(edit) = manager.edit_composition_for(selection, room, event).await {
            if edit.text == *expected {
                return edit;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Effective typed edit snapshot did not converge"
        );
        sleep(Duration::from_millis(200)).await;
    }
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

    let selection = creator
        .account_selection(&alice_account.id)
        .expect("Active selection missing");
    timeout(Duration::from_secs(60), async {
        loop {
            let rows = creator
                .mention_candidates_for(&selection, &room_id, "")
                .await
                .expect("Cannot inspect local mention candidates");
            if rows
                .iter()
                .any(|row| row.target == MentionTarget::User(bob.user_id.clone()))
                && rows.iter().any(|row| row.target == MentionTarget::Room)
            {
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Joined member/power cache did not become ready for mentions");
    let bob_selection = peer
        .account_selection(&bob_account.id)
        .expect("Peer selection missing");
    let denied_room = ComposeText::plain("@room")
        .insert_mention(0..5, MentionTarget::Room)
        .expect("Cannot encode room authorization probe")
        .0;
    assert!(
        peer.send_composed_tracked_for(
            &bob_selection,
            &room_id,
            ComposeSendTarget::Room,
            &denied_room,
            None
        )
        .await
        .is_err(),
        "Unprivileged room mention was accepted"
    );

    let main_source = format!(
        "Hello **main-{marker}** <script>alert(1)</script> {} @room",
        bob.user_id
    );
    let main_text = selected(
        main_source.clone(),
        TextFormat::Markdown,
        &bob.user_id,
        true,
    );
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
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Room,
                &main_text,
                None,
            )
            .await
            .expect("Cannot queue selected main message"),
    )
    .await;
    let root_event_id = wait_for_remote_message(&creator, &room_id, &main_source).await;
    wait_for_remote_message(&peer, &room_id, &main_source).await;
    assert_encrypted_on_server(&alice_client, &room_id, &root_event_id).await;
    let main = decrypted_event(&alice_client, &room_id, &root_event_id).await;
    assert_eq!(main["type"], "m.room.message");
    assert_eq!(main["content"]["body"], main_source);
    assert_safe_html(&main["content"], &main_source, &format!("main-{marker}"));
    mentions(&main["content"], Some(&bob.user_id), true);
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

    let reply_source = format!(
        "Reply **reply-{marker}** <script>alert(2)</script> {}",
        bob.user_id
    );
    let reply_text = selected(
        reply_source.clone(),
        TextFormat::Markdown,
        &bob.user_id,
        false,
    );
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Reply(root_event_id.clone()),
                &reply_text,
                None,
            )
            .await
            .expect("Cannot queue selected reply"),
    )
    .await;
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
    mentions(&reply["content"], Some(&bob.user_id), false);

    let thread_source = format!("Thread plain-{marker} {}", bob.user_id);
    let thread_text = selected(
        thread_source.clone(),
        TextFormat::Plain,
        &bob.user_id,
        false,
    );
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Thread(root_event_id.clone()),
                &thread_text,
                None,
            )
            .await
            .expect("Cannot queue selected thread reply"),
    )
    .await;
    let (thread_id, thread) = wait_for_relation(
        &bob_client,
        &room_id,
        &root_event_id,
        "thread",
        &thread_source,
    )
    .await;
    assert_encrypted_on_server(&alice_client, &room_id, &thread_id).await;
    mentions(&thread["content"], Some(&bob.user_id), false);
    assert!(
        thread["content"]["body"]
            .as_str()
            .is_some_and(|body| body.ends_with(&thread_source)),
        "Thread source differs"
    );
    assert!(
        thread["content"]["formatted_body"]
            .as_str()
            .is_some_and(|html| html.contains("https://matrix.to/#/")),
        "Selected plain mention lacks link fallback"
    );

    let baseline = ready_edit(&creator, &selection, &room_id, &root_event_id, &main_text).await;
    assert!(
        baseline.text == main_text,
        "Typed edit source/intent did not round trip"
    );
    let edit_source = format!(
        "Edited **edit-{marker}** <script>alert(3)</script> {} @room",
        bob.user_id
    );
    let edit_text = selected(
        edit_source.clone(),
        TextFormat::Markdown,
        &bob.user_id,
        true,
    );
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Edit(root_event_id.clone()),
                &edit_text,
                Some(&baseline.baseline),
            )
            .await
            .expect("Cannot queue selected edit"),
    )
    .await;
    let (edit_id, edit) =
        wait_for_relation(&bob_client, &room_id, &root_event_id, "edit", &edit_source).await;
    assert_encrypted_on_server(&alice_client, &room_id, &edit_id).await;
    assert_eq!(edit["content"]["m.new_content"]["body"], edit_source);
    mentions(&edit["content"]["m.new_content"], Some(&bob.user_id), true);
    mentions(&edit["content"], None, false);
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

    wait_for_remote_message(&creator, &room_id, &edit_source).await;
    assert!(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Edit(root_event_id.clone()),
                &edit_text,
                Some(&baseline.baseline)
            )
            .await
            .is_err(),
        "Stale effective edit baseline was accepted"
    );
    // Removal followed by a deliberate new selection must use the effective
    // previous edit, not the original event's notification metadata.
    let removed_source = format!("Removed selections-{marker}");
    let removal = ready_edit(&creator, &selection, &room_id, &root_event_id, &edit_text).await;
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Edit(root_event_id.clone()),
                &ComposeText::plain(removed_source.clone()),
                Some(&removal.baseline),
            )
            .await
            .expect("Cannot queue mention removal"),
    )
    .await;
    let (_, removed) = wait_for_relation(
        &bob_client,
        &room_id,
        &root_event_id,
        "edit",
        &removed_source,
    )
    .await;
    mentions(&removed["content"], None, false);
    mentions(&removed["content"]["m.new_content"], None, false);
    wait_for_remote_message(&creator, &room_id, &removed_source).await;
    let restored_source = format!("Selected again-{marker} {}", bob.user_id);
    let restored_text = selected(
        restored_source.clone(),
        TextFormat::Plain,
        &bob.user_id,
        false,
    );
    let reselect = ready_edit(
        &creator,
        &selection,
        &room_id,
        &root_event_id,
        &ComposeText::plain(removed_source.clone()),
    )
    .await;
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Edit(root_event_id.clone()),
                &restored_text,
                Some(&reselect.baseline),
            )
            .await
            .expect("Cannot queue deliberately reselected mention"),
    )
    .await;
    let (_, reselected) = wait_for_relation(
        &bob_client,
        &room_id,
        &root_event_id,
        "edit",
        &restored_source,
    )
    .await;
    mentions(&reselected["content"], Some(&bob.user_id), false);
    mentions(
        &reselected["content"]["m.new_content"],
        Some(&bob.user_id),
        false,
    );

    let pasted = ComposeText::plain(format!("Literal ID-{marker} {} @room", bob.user_id));
    delivered(
        creator
            .send_composed_tracked_for(&selection, &room_id, ComposeSendTarget::Room, &pasted, None)
            .await
            .expect("Cannot queue literal identifier message"),
    )
    .await;
    let pasted_id = wait_for_remote_message(&peer, &room_id, &pasted.body).await;
    mentions(
        &decrypted_event(&bob_client, &room_id, &pasted_id).await["content"],
        None,
        false,
    );

    // The reply-author mention has no visible ID span. Preserve it only after
    // independently establishing fixture identity trust; production never uses
    // this helper or silently verifies an arbitrary room participant.
    identity::verify_fixture_pair(&alice_client, alice, &bob_client, bob)
        .await
        .expect("Cannot establish controlled fixture identity trust");
    let bob_source = format!("Verified reply target-{marker}");
    delivered(
        peer.send_composed_tracked_for(
            &bob_selection,
            &room_id,
            ComposeSendTarget::Room,
            &ComposeText::plain(bob_source.clone()),
            None,
        )
        .await
        .expect("Cannot queue verified reply target"),
    )
    .await;
    let bob_root = wait_for_remote_message(&creator, &room_id, &bob_source).await;
    let inherited_source = format!("Reply with inherited author-{marker}");
    let inherited_text = ComposeText::plain(inherited_source.clone());
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Reply(bob_root.clone()),
                &inherited_text,
                None,
            )
            .await
            .expect("Cannot queue inherited-author reply"),
    )
    .await;
    let (inherited_id, inherited) =
        wait_for_relation(&bob_client, &room_id, &bob_root, "reply", &inherited_source).await;
    mentions(&inherited["content"], Some(&bob.user_id), false);
    wait_for_remote_message(&creator, &room_id, &inherited_source).await;
    let inherited_edit = ready_edit(
        &creator,
        &selection,
        &room_id,
        &inherited_id,
        &inherited_text,
    )
    .await;
    assert!(
        inherited_edit.text.mentions.is_empty(),
        "Hidden reply author became a visible selection"
    );
    let inherited_changed = format!("Edited inherited author-{marker}");
    delivered(
        creator
            .send_composed_tracked_for(
                &selection,
                &room_id,
                ComposeSendTarget::Edit(inherited_id.clone()),
                &ComposeText::plain(inherited_changed.clone()),
                Some(&inherited_edit.baseline),
            )
            .await
            .expect("Cannot edit authenticated inherited-author reply"),
    )
    .await;
    let (inherited_edit_id, edited_reply) = wait_for_relation(
        &bob_client,
        &room_id,
        &inherited_id,
        "edit",
        &inherited_changed,
    )
    .await;
    assert_encrypted_on_server(&alice_client, &room_id, &inherited_edit_id).await;
    mentions(&edited_reply["content"], None, false);
    mentions(
        &edited_reply["content"]["m.new_content"],
        Some(&bob.user_id),
        false,
    );
    assert!(
        edited_reply["content"]["m.new_content"]["body"] == inherited_changed,
        "Inherited-author edit changed visible source"
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

    assert!(
        matches!(
            creator
                .send_composed_tracked_for(
                    &selection,
                    &room_id,
                    ComposeSendTarget::Room,
                    &main_text,
                    None
                )
                .await,
            Err(MatrixCoreError::SelectedAccountChanged)
        ),
        "Revoked typed composer selection was accepted"
    );

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
    eprintln!(
        "Fresh encrypted typed mentions, main/reply/thread/effective-edit deltas and stale-account guards passed."
    );
}
