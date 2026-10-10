//! Opt-in service-gate acceptance against disposable loopback Synapse B.
//! The protected fixture contains a fresh account password; never log it.

#![cfg(feature = "native-rtc")]
#![recursion_limit = "512"]

use std::{fs, path::PathBuf, time::Duration};

use matrix_sdk::{
    Client,
    ruma::{
        EventEncryptionAlgorithm, EventId, RoomId,
        api::client::{room::get_room_event, state::get_state_events},
        events::room::encryption::RoomEncryptionEventContent,
    },
};
use morpheus_matrix::{AccountManager, QueuedSendOutcome, RoomMembership};
use morpheus_rtc::{CallOptions, RtcError, RtcService, TransportSupport, install_crypto_provider};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout};

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    username: String,
    homeserver_url: String,
    password: String,
}

async fn authoritative_room_state(client: &Client, room_id: &str) -> Vec<Value> {
    let response = client
        .send(get_state_events::v3::Request::new(
            RoomId::parse(room_id).expect("Invalid disposable room ID"),
        ))
        .await
        .expect("Cannot fetch authoritative room state from Synapse B");
    response
        .room_state
        .into_iter()
        .map(|event| {
            serde_json::from_str(event.json().get()).expect("Invalid authoritative state event")
        })
        .collect()
}

fn assert_encrypted_and_slot_free(state: &[Value]) {
    let encryption = state
        .iter()
        .find(|event| event["type"] == "m.room.encryption" && event["state_key"] == "")
        .expect("Fresh room lacks server-side encryption state");
    let content: RoomEncryptionEventContent =
        serde_json::from_value(encryption["content"].clone()).expect("Invalid encryption state");
    assert_eq!(content.algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
    assert!(
        state.iter().all(|event| {
            event["type"] != "m.rtc.slot" && event["type"] != "org.matrix.msc4143.rtc.slot"
        }),
        "Unsupported-server room contains a MatrixRTC slot state event"
    );
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let membership = manager
            .rooms_snapshot()
            .await
            .expect("Cannot read Synapse B room list")
            .into_iter()
            .find(|room| room.room_id == room_id)
            .map(|room| room.membership);
        if membership == Some(RoomMembership::Joined) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Fresh room did not become joined"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_sent_message(manager: &AccountManager, room_id: &str, body: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(event_id) = manager
            .messages_snapshot(room_id)
            .await
            .expect("Cannot read encrypted room timeline")
            .into_iter()
            .find(|message| message.body == body && !message.is_local_echo)
            .and_then(|message| message.event_id)
        {
            return event_id;
        }
        assert!(
            Instant::now() < deadline,
            "Encrypted chat send did not sync"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn unsupported_rtc_transport_rejects_call_without_writing_state_and_chat_still_works() {
    let Some(path) = std::env::var_os("MORPHEUS_MATRIX_RTC_GATE_TEST_ACCOUNTS") else {
        eprintln!(
            "Skipped RTC service-gate acceptance; set MORPHEUS_MATRIX_RTC_GATE_TEST_ACCOUNTS."
        );
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(path)).expect("Cannot read protected RTC gate fixture"),
    )
    .expect("Invalid RTC gate fixture");
    let account = fixture
        .accounts
        .iter()
        .find(|account| account.label == "b_gate")
        .expect("Missing disposable Synapse B account");
    assert_eq!(
        account.homeserver_url, "http://127.0.0.1:18009",
        "RTC gate fixture must target local Synapse B"
    );

    install_crypto_provider();
    let root = std::env::temp_dir().join(format!(
        "morpheus-rtc-gate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Clock before Unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("Cannot create temporary RTC gate store");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect RTC gate store");
    }
    let manager = AccountManager::open(root.join("account")).expect("Cannot open account store");
    let signed_in = manager
        .add_password_account(
            &account.homeserver_url,
            &account.username,
            &account.password,
        )
        .await
        .expect("Cannot sign in to Synapse B");
    let room_id = manager
        .create_private_room("Morpheus RTC service gate")
        .await
        .expect("Cannot create disposable encrypted room");
    wait_for_room(&manager, &room_id).await;
    let client = manager
        .active_client()
        .await
        .expect("Active client missing");
    assert_encrypted_and_slot_free(&authoritative_room_state(&client, &room_id).await);

    let mut rtc = RtcService::new(client.clone());
    assert_eq!(
        rtc.discover_transport()
            .await
            .expect("Synapse B transport discovery failed"),
        TransportSupport::NoLiveKitTransport
    );
    assert!(matches!(
        rtc.open_slot(&room_id, "m.call#room", true).await,
        Err(RtcError::NoLiveKitTransport)
    ));
    assert!(matches!(
        rtc.join(&room_id, CallOptions::default()).await,
        Err(RtcError::NoLiveKitTransport)
    ));
    assert_encrypted_and_slot_free(&authoritative_room_state(&client, &room_id).await);

    let body = "Chat still works without a MatrixRTC service";
    let queued = manager
        .send_text_tracked_for(&signed_in.id, &room_id, body)
        .await
        .expect("Cannot queue encrypted chat after RTC rejection");
    assert_eq!(
        timeout(Duration::from_secs(60), queued.wait_outcome())
            .await
            .expect("Encrypted chat send timed out"),
        QueuedSendOutcome::Sent
    );
    let event_id = wait_for_sent_message(&manager, &room_id, body).await;
    let response = client
        .send(get_room_event::v3::Request::new(
            RoomId::parse(&room_id).expect("Invalid disposable room ID"),
            EventId::parse(&event_id).expect("Invalid chat event ID"),
        ))
        .await
        .expect("Cannot read sent chat event from Synapse B");
    let outer: Value =
        serde_json::from_str(response.event.json().get()).expect("Invalid chat event JSON");
    assert_eq!(outer["type"], "m.room.encrypted");
    assert!(outer["content"]["ciphertext"].as_str().is_some());
    assert_encrypted_and_slot_free(&authoritative_room_state(&client, &room_id).await);

    manager
        .logout_account(&signed_in.id)
        .await
        .expect("Cannot remove RTC gate test session");
    manager.stop().await;
    fs::remove_dir_all(&root).expect("Cannot remove temporary RTC gate store");
    eprintln!("Synapse B rejected native calls without slot state; encrypted chat passed.");
}
