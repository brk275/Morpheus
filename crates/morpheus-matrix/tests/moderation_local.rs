//! Opt-in moderation and room-settings test against disposable loopback Matrix.
//! Fixture credentials are read from a protected file and never printed.
#![recursion_limit = "512"]

#[path = "support/opened_room.rs"]
mod opened_room;

#[path = "moderation_local/room_settings.rs"]
mod room_settings;

use std::{fs, path::PathBuf, time::Duration};

use matrix_sdk::{
    Client,
    ruma::{
        EventId, RoomId,
        api::client::{room::get_room_event, state::get_state_event_for_key},
        events::StateEventType,
    },
};
use morpheus_matrix::{AccountManager, MatrixCoreError, QueuedSendOutcome, RoomMembership};
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

fn invitation_failure(error: &MatrixCoreError) -> &'static str {
    match error {
        MatrixCoreError::SelectedAccountChanged => "selected_account_changed",
        MatrixCoreError::RoomNotFound(_) => "room_not_found",
        MatrixCoreError::InvalidUserId(_) => "invalid_user_id",
        // MatrixCoreError currently retains SDK errors as text. Match only
        // complete, pinned SDK messages; never publish the arbitrary payload.
        MatrixCoreError::Sdk(message)
            if message
                == "encryption failed due to an error collecting the recipient devices: Encryption failed because your device is not verified" =>
        {
            "history_sender_unverified"
        }
        MatrixCoreError::Sdk(message)
            if message
                == "encryption failed due to an error collecting the recipient devices: Encryption failed because cross-signing is not set up on your account" =>
        {
            "history_cross_signing_missing"
        }
        MatrixCoreError::Sdk(_) => "sdk",
        _ => "other",
    }
}

async fn invite(
    manager: &AccountManager,
    account_id: &str,
    room_id: &str,
    user_id: &str,
    stage: &'static str,
) {
    eprintln!("MORPHEUS_MODERATION_STAGE {stage}");
    if let Err(error) = manager.invite_user_for(account_id, room_id, user_id).await {
        eprintln!("MORPHEUS_MODERATION_FAILURE {}", invitation_failure(&error));
        panic!("Moderation invitation failed; fixed category emitted");
    }
}

async fn own_identity_verified(client: &Client) -> Option<bool> {
    client
        .encryption()
        .get_user_identity(client.user_id().expect("Test account is not signed in"))
        .await
        .expect("Cannot inspect test account identity")
        .map(|identity| identity.is_verified())
}

#[test]
fn invitation_diagnostics_discard_arbitrary_sdk_payloads() {
    assert_eq!(
        invitation_failure(&MatrixCoreError::Sdk(
            "encryption failed due to an error collecting the recipient devices: Encryption failed because your device is not verified".into()
        )),
        "history_sender_unverified"
    );
    assert_eq!(
        invitation_failure(&MatrixCoreError::Sdk(
            "encryption failed due to an error collecting the recipient devices: Encryption failed because cross-signing is not set up on your account".into()
        )),
        "history_cross_signing_missing"
    );
    for payload in [
        "private server response with credentials",
        "encryption failed due to an error collecting the recipient devices: Encryption failed because your device is not verified; private response",
    ] {
        assert_eq!(
            invitation_failure(&MatrixCoreError::Sdk(payload.into())),
            "sdk"
        );
    }
}

async fn wait_for_room(manager: &AccountManager, room_id: &str, expected: RoomMembership) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let membership = manager
            .rooms_snapshot()
            .await
            .expect("Cannot read room list")
            .into_iter()
            .find(|room| room.room_id == room_id)
            .map(|room| room.membership);
        if membership == Some(expected) {
            if expected == RoomMembership::Joined {
                opened_room::scan(manager, room_id)
                    .await
                    .expect("Opened moderation fixture room must be scanned");
            }
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Room did not reach {expected:?}; latest state: {membership:?}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn read_state(
    client: &Client,
    room_id: &str,
    event_type: StateEventType,
    state_key: &str,
) -> Value {
    let request = get_state_event_for_key::v3::Request::new(
        RoomId::parse(room_id).expect("Invalid disposable room ID"),
        event_type,
        state_key.to_owned(),
    );
    let response = client
        .send(request)
        .await
        .expect("Cannot fetch authoritative room state");
    serde_json::from_str(response.event_or_content.get()).expect("Invalid room state content")
}

async fn wait_for_member_state(client: &Client, room_id: &str, user_id: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let content = read_state(client, room_id, StateEventType::RoomMember, user_id).await;
        if content["membership"] == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Room member did not reach expected membership {expected}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_message(manager: &AccountManager, room_id: &str, body: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(event_id) = manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read room timeline")
            .into_iter()
            .find(|message| message.body == body && !message.is_local_echo)
            .and_then(|message| message.event_id)
        {
            return event_id;
        }
        assert!(
            Instant::now() < deadline,
            "Message did not reach the room timeline"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_redaction(manager: &AccountManager, room_id: &str, event_id: &str, body: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let visible = manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read room timeline after redaction")
            .iter()
            .any(|message| message.event_id.as_deref() == Some(event_id) && message.body == body);
        if !visible {
            return;
        }
        assert!(Instant::now() < deadline, "Redacted body remained visible");
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_server_redaction(client: &Client, room_id: &str, event_id: &str, body: &str) {
    let request = get_room_event::v3::Request::new(
        RoomId::parse(room_id).expect("Invalid disposable room ID"),
        EventId::parse(event_id).expect("Invalid disposable event ID"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = client
            .send(request.clone())
            .await
            .expect("Cannot fetch message after redaction");
        let event: Value = serde_json::from_str(response.event.json().get())
            .expect("Invalid remote event after redaction");
        if event["content"]["body"].is_null() && event["unsigned"]["redacted_because"].is_object() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Remote event did not become a redacted event; original body still present: {}",
            event["content"]["body"] == body
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_power(
    manager: &AccountManager,
    account_id: &str,
    room_id: &str,
    user_id: &str,
    expected: i64,
) {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let members = manager
            .room_members_for(account_id, room_id)
            .await
            .expect("Cannot read room members and power levels");
        let actual = members
            .iter()
            .find(|member| member.user_id == user_id)
            .and_then(|member| member.power_level);
        if actual == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Member did not reach power level {expected}; latest: {actual:?}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_member_list_state(
    manager: &AccountManager,
    account_id: &str,
    room_id: &str,
    user_id: &str,
    expected: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let members = manager
            .room_members_for(account_id, room_id)
            .await
            .expect("Cannot read room member list");
        let actual = members
            .iter()
            .find(|member| member.user_id == user_id)
            .map(|member| member.membership.as_str());
        if actual == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Member list did not show {expected}; latest: {actual:?}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn room_settings_redaction_and_moderation_are_enforced() {
    let Some(fixture_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local moderation test; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(fixture_path)).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let alice = fixture.account("a_primary");
    let bob = fixture.account("a_second_device");
    let root = std::env::temp_dir().join(format!("morpheus-moderation-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create test account stores");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect test account stores");
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
    let marker = Uuid::new_v4().simple().to_string();
    let room_id = creator
        .create_private_room(&format!("Morpheus moderation {marker}"))
        .await
        .expect("Cannot create disposable room");
    wait_for_room(&creator, &room_id, RoomMembership::Joined).await;
    assert!(matches!(
        creator
            .invite_user_for(&bob_account.id, &room_id, &bob.user_id)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    invite(
        &creator,
        &alice_account.id,
        &room_id,
        &bob.user_id,
        "invite_initial",
    )
    .await;
    wait_for_room(&peer, &room_id, RoomMembership::Invited).await;
    wait_for_member_list_state(
        &creator,
        &alice_account.id,
        &room_id,
        &bob.user_id,
        "invite",
    )
    .await;
    assert!(
        creator
            .room_members_for(&bob_account.id, &room_id)
            .await
            .is_err(),
        "A mismatched selected account unexpectedly read the room member list"
    );
    peer.accept_invite(&room_id)
        .await
        .expect("Bob cannot accept invitation");
    wait_for_room(&peer, &room_id, RoomMembership::Joined).await;
    wait_for_member_state(&alice_client, &room_id, &bob.user_id, "join").await;
    wait_for_member_list_state(&creator, &alice_account.id, &room_id, &bob.user_id, "join").await;

    let renamed = format!("Morpheus renamed {marker}");
    let topic = format!("Disposable moderation topic {marker}");
    assert!(matches!(
        creator
            .rename_room_for(&bob_account.id, &room_id, &renamed)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        creator
            .set_room_topic_for(&bob_account.id, &room_id, &topic)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    creator
        .rename_room_for(&alice_account.id, &room_id, &renamed)
        .await
        .expect("Creator cannot rename room");
    creator
        .set_room_topic_for(&alice_account.id, &room_id, &topic)
        .await
        .expect("Creator cannot set topic");
    let name_state = read_state(&alice_client, &room_id, StateEventType::RoomName, "").await;
    let topic_state = read_state(&alice_client, &room_id, StateEventType::RoomTopic, "").await;
    assert_eq!(name_state["name"].as_str(), Some(renamed.as_str()));
    assert_eq!(topic_state["topic"].as_str(), Some(topic.as_str()));
    assert!(
        peer.rename_room_for(&bob_account.id, &room_id, "unauthorized rename")
            .await
            .is_err(),
        "Default-power member unexpectedly renamed the room"
    );
    assert!(
        peer.kick_user_for(&bob_account.id, &room_id, &alice.user_id, None)
            .await
            .is_err(),
        "Default-power member unexpectedly kicked the creator"
    );

    assert!(
        creator
            .set_member_power_level_for(&bob_account.id, &room_id, &bob.user_id, 50)
            .await
            .is_err(),
        "A mismatched selected account unexpectedly changed a room role"
    );
    creator
        .set_member_power_level_for(&alice_account.id, &room_id, &bob.user_id, 50)
        .await
        .expect("Creator cannot grant Bob moderator power");
    wait_for_power(&creator, &alice_account.id, &room_id, &bob.user_id, 50).await;
    let elevated_name = format!("Moderator renamed {marker}");
    peer.rename_room_for(&bob_account.id, &room_id, &elevated_name)
        .await
        .expect("Moderator cannot rename room");
    let name_state = read_state(&alice_client, &room_id, StateEventType::RoomName, "").await;
    assert_eq!(name_state["name"].as_str(), Some(elevated_name.as_str()));
    assert!(
        peer.set_member_power_level_for(&bob_account.id, &room_id, &bob.user_id, 100)
            .await
            .is_err(),
        "Moderator unexpectedly promoted self above own power"
    );
    creator
        .set_member_power_level_for(&alice_account.id, &room_id, &bob.user_id, 0)
        .await
        .expect("Creator cannot restore Bob's default power");
    wait_for_power(&creator, &alice_account.id, &room_id, &bob.user_id, 0).await;
    assert!(
        peer.rename_room_for(&bob_account.id, &room_id, "unauthorized after demotion")
            .await
            .is_err(),
        "Demoted member unexpectedly renamed the room"
    );

    let body = format!("Moderation redaction {marker}");
    let send = peer
        .send_text_tracked_for(&bob_account.id, &room_id, body.clone())
        .await
        .expect("Cannot queue Bob's message");
    assert_eq!(
        timeout(Duration::from_secs(45), send.wait_outcome())
            .await
            .expect("Bob's message send timed out"),
        QueuedSendOutcome::Sent
    );
    let event_id = wait_for_message(&creator, &room_id, &body).await;
    wait_for_message(&peer, &room_id, &body).await;
    assert!(matches!(
        creator
            .redact_message_for(&bob_account.id, &room_id, &event_id, None)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    creator
        .redact_message_for(
            &alice_account.id,
            &room_id,
            &event_id,
            Some("Disposable moderation test"),
        )
        .await
        .expect("Creator cannot redact Bob's message");
    wait_for_server_redaction(&alice_client, &room_id, &event_id, &body).await;
    wait_for_redaction(&creator, &room_id, &event_id, &body).await;
    wait_for_redaction(&peer, &room_id, &event_id, &body).await;
    eprintln!("Room rename, topic, permission rejection, and remote redaction passed.");

    assert!(matches!(
        creator
            .kick_user_for(&bob_account.id, &room_id, &bob.user_id, None)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    creator
        .kick_user_for(
            &alice_account.id,
            &room_id,
            &bob.user_id,
            Some("Disposable moderation test"),
        )
        .await
        .expect("Creator cannot kick Bob");
    wait_for_member_state(&alice_client, &room_id, &bob.user_id, "leave").await;
    // The hosted fixture reuses an account identity from the preceding live
    // tests with a fresh local device. Do not verify/reset that identity to
    // make an ordinary invitation pass, or enable an unsafe sharing strategy.
    let trust_before_invite = own_identity_verified(&alice_client).await;
    eprintln!(
        "MORPHEUS_MODERATION_CHECK reinvite_identity_unverified={}",
        trust_before_invite == Some(false)
    );
    invite(
        &creator,
        &alice_account.id,
        &room_id,
        &bob.user_id,
        "invite_after_kick",
    )
    .await;
    let trust_unchanged = own_identity_verified(&alice_client).await == trust_before_invite;
    eprintln!("MORPHEUS_MODERATION_CHECK reinvite_trust_unchanged={trust_unchanged}");
    assert!(
        trust_unchanged,
        "Inviting changed the account identity trust"
    );
    wait_for_room(&peer, &room_id, RoomMembership::Invited).await;
    peer.accept_invite(&room_id)
        .await
        .expect("Bob cannot rejoin after kick");
    wait_for_member_state(&alice_client, &room_id, &bob.user_id, "join").await;
    eprintln!("MORPHEUS_MODERATION_CHECK reinvite_after_kick=true");

    assert!(matches!(
        creator
            .ban_user_for(&bob_account.id, &room_id, &bob.user_id, None)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    creator
        .ban_user_for(
            &alice_account.id,
            &room_id,
            &bob.user_id,
            Some("Disposable moderation test"),
        )
        .await
        .expect("Creator cannot ban Bob");
    wait_for_member_state(&alice_client, &room_id, &bob.user_id, "ban").await;
    wait_for_member_list_state(&creator, &alice_account.id, &room_id, &bob.user_id, "ban").await;
    assert!(
        peer.join_room(&room_id).await.is_err(),
        "Banned member unexpectedly rejoined"
    );
    assert!(matches!(
        creator
            .unban_user_for(&bob_account.id, &room_id, &bob.user_id, None)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    creator
        .unban_user_for(
            &alice_account.id,
            &room_id,
            &bob.user_id,
            Some("Disposable moderation test"),
        )
        .await
        .expect("Creator cannot unban Bob");
    wait_for_member_state(&alice_client, &room_id, &bob.user_id, "leave").await;
    invite(
        &creator,
        &alice_account.id,
        &room_id,
        &bob.user_id,
        "invite_after_unban",
    )
    .await;
    wait_for_room(&peer, &room_id, RoomMembership::Invited).await;
    peer.accept_invite(&room_id)
        .await
        .expect("Bob cannot rejoin after unban");
    wait_for_member_state(&alice_client, &room_id, &bob.user_id, "join").await;
    eprintln!("MORPHEUS_MODERATION_CHECK reinvite_after_unban=true");
    eprintln!("Room kick, reinvite, ban, blocked join, unban, and rejoin passed.");

    wait_for_room(&peer, &room_id, RoomMembership::Joined).await;
    timeout(
        Duration::from_secs(180),
        room_settings::exercise(
            &creator,
            &peer,
            &alice_account.id,
            &bob_account.id,
            &alice_client,
            &room_id,
        ),
    )
    .await
    .expect("Live room settings acceptance timed out");

    peer.leave_room(&room_id)
        .await
        .expect("Bob cannot leave test room");
    creator
        .leave_room(&room_id)
        .await
        .expect("Alice cannot leave test room");
    peer.logout_account(&bob_account.id)
        .await
        .expect("Cannot remove Bob test session");
    creator
        .logout_account(&alice_account.id)
        .await
        .expect("Cannot remove Alice test session");
    peer.stop().await;
    creator.stop().await;
    drop(peer);
    drop(creator);
    fs::remove_dir_all(&root).expect("Cannot remove test account stores");
}
