// Copyright 2026 Morpheus contributors
// Licensed under the Apache License, Version 2.0.

//! These regressions exercise the real SDK sync/decrypt/room-key paths. They
//! need both `unstable-msc4354` and `e2e-encryption`; no native devices or server.

use std::{sync::Arc, time::Duration};

use matrix_sdk_common::deserialized_responses::{TimelineEventKind, VerificationState};
use matrix_sdk_crypto::{EncryptionSettings, OlmMachine};
use matrix_sdk_test::async_test;
use ruma::{
    api::client::sync::sync_events::v5 as http,
    device_id,
    events::{AnyMessageLikeEventContent, AnySyncTimelineEvent},
    room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;

use super::{Decryption, PendingEvent, classify, decrypt, now_ms, resolve_keyless, retry_pending};
use crate::{
    BaseClient, RequestedRequiredStates, response_processors::e2ee::E2EE,
    test_utils::logged_in_base_client,
};

async fn process(client: &BaseClient, response: &http::Response) {
    client
        .process_sliding_sync(
            response,
            &RequestedRequiredStates::default(),
            &client.state_store_lock().lock().await,
        )
        .await
        .unwrap();
}

async fn receiver() -> BaseClient {
    let client = logged_in_base_client(Some(user_id!("@bob:example.org"))).await;
    let mut response = http::Response::new("initial".into());
    response.rooms.insert(room_id!("!ring:example.org").to_owned(), http::response::Room::new());
    process(&client, &response).await;
    client
}

async fn encrypted_event() -> (OlmMachine, Raw<AnySyncTimelineEvent>) {
    let alice = OlmMachine::new(user_id!("@alice:example.org"), device_id!("ALICE")).await;
    let room_id = room_id!("!ring:example.org");
    alice.share_room_key(room_id, std::iter::empty(), EncryptionSettings::default()).await.unwrap();
    let content: Raw<AnyMessageLikeEventContent> = Raw::from_json_string(
        json!({
            "notification_type": "ring", "sender_ts": now_ms(), "lifetime": 30_000,
            "application": {"type": "m.call", "device_id": "ALICE"},
            "m.relates_to": {"rel_type": "m.reference", "event_id": "$member:example.org"},
        })
        .to_string(),
    )
    .unwrap();
    let encrypted =
        alice.encrypt_room_event_raw(room_id, "m.rtc.notification", &content).await.unwrap();
    let raw = Raw::from_json_string(
        json!({
            "type": "m.room.encrypted", "sender": "@alice:example.org",
            "event_id": "$ring:example.org", "origin_server_ts": now_ms(),
            "content": encrypted.content,
            "msc4354_sticky": {"duration_ms": 60_000},
            "unsigned": {"msc4354_sticky_duration_ttl_ms": 60_000},
        })
        .to_string(),
    )
    .unwrap();
    (alice, raw)
}

async fn import_keys(alice: &OlmMachine, bob: &BaseClient) {
    let keys = alice.store().export_room_keys(|_| true).await.unwrap();
    assert_eq!(keys.len(), 1);
    let machine = bob.olm_machine().await;
    let result =
        machine.as_ref().unwrap().store().import_exported_room_keys(keys, |_, _| {}).await.unwrap();
    assert_eq!(result.imported_count, 1);
}

fn extension(raw: Raw<AnySyncTimelineEvent>) -> http::Response {
    let mut response = http::Response::new("extension-only".into());
    let mut room = http::response::StickyEventsRoom::default();
    room.events.push(raw);
    response.extensions.sticky_events.rooms.insert(room_id!("!ring:example.org").to_owned(), room);
    response
}

#[async_test]
async fn keyless_extension_decryption_keeps_actual_unverified_metadata_and_redaction() {
    let bob = receiver().await;
    let (alice, raw) = encrypted_event().await;
    import_keys(&alice, &bob).await;
    let room = bob.get_room(room_id!("!ring:example.org")).unwrap();
    let sticky = room.sticky_events();
    let mut updates = sticky.subscribe_keyless();
    let generation = sticky.keyless_live().generation;
    process(&bob, &extension(raw.clone())).await;
    let wake = updates.try_recv().unwrap();
    assert!(!wake.invalidated);
    assert_eq!(wake.generation, generation);
    let snapshot = sticky.keyless_live();
    assert_eq!(snapshot.events.len(), 1);
    assert!(sticky.live().is_empty());
    let receipt = &snapshot.events[0];
    assert!(matches!(receipt.kind, TimelineEventKind::Decrypted(_)));
    let info = receipt.encryption_info().unwrap();
    // Exported keys carry no fabricated verification. Compare the complete
    // real decryption metadata, not a test-created Verified flag.
    assert_ne!(info.verification_state, VerificationState::Verified);
    let guard = bob.olm_machine().await;
    let e2ee = E2EE::new(guard.as_ref(), &bob.decryption_settings, false);
    let Decryption::Keyless(direct) = decrypt(
        &e2ee,
        room.room_id(),
        PendingEvent {
            keyless_generation: generation,
            meta: classify(now_ms(), &raw).unwrap(),
            event: raw.clone(),
        },
    )
    .await
    else {
        panic!("expected keyless SDK decryption")
    };
    assert_eq!(direct.encryption_info(), receipt.encryption_info());
    let original_info = direct.encryption_info().unwrap().clone();
    let preserved =
        resolve_keyless(classify(now_ms(), &raw).unwrap(), direct.kind.clone()).unwrap();
    assert!(Arc::ptr_eq(&original_info, preserved.encryption_info().unwrap()));
    drop(guard);

    let until = receipt.expires_at;
    process(&bob, &extension(raw.clone())).await;
    assert!(sticky.keyless_live().events[0].expires_at <= until);

    let mut response = http::Response::new("redaction".into());
    let mut update = http::response::Room::new();
    update.timeline.push(Raw::from_json_string(json!({
        "type": "m.room.redaction", "sender": "@alice:example.org", "event_id": "$redaction:example.org",
        "origin_server_ts": now_ms(), "redacts": "$ring:example.org",
        "content": {"redacts": "$ring:example.org"},
    }).to_string()).unwrap());
    response.rooms.insert(room.room_id().to_owned(), update);
    process(&bob, &response).await;
    assert!(sticky.keyless_live().events.is_empty());
    process(&bob, &extension(raw)).await;
    assert!(sticky.keyless_live().events.is_empty());
}

#[async_test]
async fn keyless_late_room_key_stream_delivers_extension_receipt() {
    let bob = receiver().await;
    let (alice, raw) = encrypted_event().await;
    let room = bob.get_room(room_id!("!ring:example.org")).unwrap();
    let sticky = room.sticky_events();
    let mut updates = sticky.subscribe_keyless();
    process(&bob, &extension(raw)).await;
    assert!(sticky.keyless_live().events.is_empty());
    assert!(sticky.has_pending());
    import_keys(&alice, &bob).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            updates.recv().await.unwrap();
            let snapshot = sticky.keyless_live();
            assert!(!snapshot.invalidated);
            if snapshot.events.len() == 1 {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(sticky.keyless_live().events[0].encryption_info().is_some());
}

#[async_test]
async fn keyless_key_before_initial_park_and_old_repark_generation_are_safe() {
    let bob = receiver().await;
    let (alice, raw) = encrypted_event().await;
    let room = bob.get_room(room_id!("!ring:example.org")).unwrap();
    let sticky = room.sticky_events();
    let generation = sticky.keyless_live().generation;
    let guard = bob.olm_machine().await;
    let e2ee = E2EE::new(guard.as_ref(), &bob.decryption_settings, false);
    let Decryption::Pending(pending) = decrypt(
        &e2ee,
        room.room_id(),
        PendingEvent {
            keyless_generation: generation,
            meta: classify(now_ms(), &raw).unwrap(),
            event: raw.clone(),
        },
    )
    .await
    else {
        panic!("missing key must remain pending")
    };
    drop(guard);
    import_keys(&alice, &bob).await;
    // A key already in the store must work even if its notification preceded
    // the first park. The collector invokes this same post-park retry path.
    sticky.park(vec![pending]);
    let guard = bob.olm_machine().await;
    let e2ee = E2EE::new(guard.as_ref(), &bob.decryption_settings, false);
    retry_pending(sticky, &e2ee, None).await;
    assert_eq!(sticky.keyless_live().events.len(), 1);
    sticky.clear();
    sticky.activate_keyless();
    // Simulate an old retry re-parking after the room was left and rejoined.
    sticky.park(vec![PendingEvent {
        keyless_generation: generation,
        meta: classify(now_ms(), &raw).unwrap(),
        event: raw,
    }]);
    retry_pending(sticky, &e2ee, None).await;
    assert!(sticky.keyless_live().events.is_empty());
}

#[async_test]
async fn keyless_delayed_key_after_redaction_does_not_revive_receipt() {
    let bob = receiver().await;
    let (alice, raw) = encrypted_event().await;
    let room = bob.get_room(room_id!("!ring:example.org")).unwrap();
    let sticky = room.sticky_events();
    process(&bob, &extension(raw)).await;
    sticky.redact_keyless(&"$ring:example.org".try_into().unwrap());
    import_keys(&alice, &bob).await;
    let guard = bob.olm_machine().await;
    let e2ee = E2EE::new(guard.as_ref(), &bob.decryption_settings, false);
    retry_pending(sticky, &e2ee, None).await;
    assert!(sticky.keyless_live().events.is_empty());
    assert!(!sticky.has_pending());
}
