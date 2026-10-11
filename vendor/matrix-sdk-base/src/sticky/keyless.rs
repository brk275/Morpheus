// Copyright 2026 Morpheus contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0

//! Receipt retention for sticky events without a protocol sticky key.
//! This is separate from MSC4354's keyed conflict-resolution map.

use std::{collections::BTreeMap, sync::Arc};

use matrix_sdk_common::deserialized_responses::{EncryptionInfo, TimelineEventKind};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{AnySyncTimelineEvent, sticky::StickyDurationMs},
    serde::Raw,
};

const MAX_EVENTS: usize = 128;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_REDACTIONS: usize = 1024;

/// A received sticky event that has no `content.sticky_key`.
///
/// Retention does not assert authenticity or permission to notify. Consumers
/// must inspect the original SDK decryption metadata and validate the event's
/// application semantics. Nothing in this API starts another sync connection.
#[derive(Clone, Debug)]
pub struct KeylessStickyEvent {
    /// The original event ID; this is a receipt identifier, not a sticky key.
    pub event_id: OwnedEventId,
    /// The sender from the original event envelope.
    pub sender: OwnedUserId,
    /// The actual plaintext or SDK-decrypted event, with its encryption info.
    pub kind: TimelineEventKind,
    /// The original retained expiry. Repeated delivery cannot extend it.
    pub expires_at: MilliSecondsSinceUnixEpoch,
}

impl KeylessStickyEvent {
    /// The event, decrypted if it arrived encrypted.
    pub fn raw(&self) -> &Raw<AnySyncTimelineEvent> {
        self.kind.raw()
    }

    /// The unmodified SDK encryption metadata, if this event was encrypted.
    pub fn encryption_info(&self) -> Option<&Arc<EncryptionInfo>> {
        self.kind.encryption_info()
    }
}

/// A complete local snapshot of retained keyless receipts.
///
/// This is not an authoritative server snapshot. An invalidated snapshot must
/// never authorize an incoming invitation. Capacity invalidation is permanent
/// for this cache instance, including across room leave/rejoin; recovery needs
/// a new client cache and a fresh server sync. No receipts are persisted.
#[derive(Clone, Debug)]
pub struct KeylessStickyEventsSnapshot {
    /// Changes whenever the room's receipt lifetime is invalidated or restarted.
    pub generation: u64,
    /// True after overflow or while this room is no longer joined.
    pub invalidated: bool,
    /// Unexpired receipts. Empty whenever `invalidated` is true.
    pub events: Vec<KeylessStickyEvent>,
}

/// A wake-up indicating that the keyless receipt snapshot may have changed.
///
/// Subscribe before reading the snapshot, and reconcile using
/// [`super::StickyEvents::keyless_live`] after every update. On broadcast lag,
/// cancel admission until a new non-invalidated snapshot has been reconciled;
/// this is not a stream of application actions or complete event deltas.
#[derive(Clone, Copy, Debug)]
pub struct KeylessStickyEventsUpdate {
    /// The current receipt lifetime generation.
    pub generation: u64,
    /// Whether the cache is invalidated.
    pub invalidated: bool,
}

#[derive(Debug)]
pub(super) struct KeylessState {
    generation: u64,
    active: bool,
    overflowed: bool,
    events: BTreeMap<OwnedEventId, KeylessStickyEvent>,
    redactions: BTreeMap<OwnedEventId, u64>,
    bytes: usize,
}

impl Default for KeylessState {
    fn default() -> Self {
        Self {
            generation: 1,
            active: true,
            overflowed: false,
            events: BTreeMap::new(),
            redactions: BTreeMap::new(),
            bytes: 0,
        }
    }
}

impl KeylessState {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn snapshot(&self, now: u64) -> KeylessStickyEventsSnapshot {
        let invalidated = self.overflowed || !self.active;
        KeylessStickyEventsSnapshot {
            generation: self.generation,
            invalidated,
            events: if invalidated {
                Vec::new()
            } else {
                self.events.values().filter(|event| expiry(event) > now).cloned().collect()
            },
        }
    }

    pub(super) fn update(&self) -> KeylessStickyEventsUpdate {
        KeylessStickyEventsUpdate {
            generation: self.generation,
            invalidated: self.overflowed || !self.active,
        }
    }

    fn advance_generation(&mut self) {
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            self.overflowed = true;
        }
    }

    pub(super) fn invalidate_capacity(&mut self) -> bool {
        if self.overflowed {
            return false;
        }
        self.overflowed = true;
        self.events.clear();
        self.redactions.clear();
        self.bytes = 0;
        self.advance_generation();
        true
    }

    pub(super) fn activate(&mut self) -> bool {
        if self.active {
            return false;
        }
        self.active = true;
        self.advance_generation();
        true
    }

    pub(super) fn leave(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        self.events.clear();
        self.bytes = 0;
        self.advance_generation();
        true
    }

    pub(super) fn redact(&mut self, now: u64, event_id: &OwnedEventId) -> bool {
        if event_id.as_str().len() > 255 {
            return self.invalidate_capacity();
        }
        if self.overflowed {
            return false;
        }
        self.expire(now);
        if !self.redactions.contains_key(event_id) && self.redactions.len() == MAX_REDACTIONS {
            return self.invalidate_capacity();
        }
        // Remember even a redaction preceding its original receipt or a delayed
        // decryption. No sticky event may outlive this protocol maximum.
        self.redactions
            .entry(event_id.clone())
            .or_insert_with(|| now.saturating_add(u64::from(StickyDurationMs::MAX)));
        if let Some(event) = self.events.remove(event_id) {
            self.bytes -= event.raw().json().get().len();
        }
        true
    }

    pub(super) fn ingest(
        &mut self,
        now: u64,
        generation: u64,
        events: Vec<KeylessStickyEvent>,
    ) -> bool {
        if !self.active || self.overflowed || generation != self.generation {
            return false;
        }
        let mut changed = self.expire(now);
        for event in events {
            if expiry(&event) <= now || self.redactions.contains_key(&event.event_id) {
                continue;
            }
            let size = event.raw().json().get().len();
            if size > MAX_EVENT_BYTES {
                return self.invalidate_capacity() || changed;
            }
            if let Some(previous) = self.events.get_mut(&event.event_id) {
                // An event ID is immutable. Keep the first decrypted metadata
                // and earliest deadline; a replay cannot upgrade its trust.
                if event.expires_at < previous.expires_at {
                    previous.expires_at = event.expires_at;
                    changed = true;
                }
                continue;
            }
            if self.events.len() == MAX_EVENTS || size > MAX_TOTAL_BYTES - self.bytes {
                return self.invalidate_capacity() || changed;
            }
            self.bytes += size;
            self.events.insert(event.event_id.clone(), event);
            changed = true;
        }
        changed
    }

    pub(super) fn expire(&mut self, now: u64) -> bool {
        let before = self.events.len();
        self.events.retain(|_, event| expiry(event) > now);
        self.redactions.retain(|_, until| *until > now);
        self.bytes = self.events.values().map(|event| event.raw().json().get().len()).sum();
        before != self.events.len()
    }

    pub(super) fn next_expiry(&self) -> Option<u64> {
        self.events.values().map(expiry).chain(self.redactions.values().copied()).min()
    }
}

fn expiry(event: &KeylessStickyEvent) -> u64 {
    event.expires_at.get().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{owned_user_id, serde::Raw};
    use serde_json::json;

    fn receipt(id: usize, until: u64, body_bytes: usize) -> KeylessStickyEvent {
        let event_id: OwnedEventId = format!("$receipt{id}:example.org").try_into().unwrap();
        let raw = Raw::from_json_string(
            json!({
                "type": "m.rtc.notification", "event_id": event_id,
                "sender": "@alice:example.org", "origin_server_ts": 1,
                "content": {"body": "x".repeat(body_bytes)},
            })
            .to_string(),
        )
        .unwrap();
        KeylessStickyEvent {
            event_id,
            sender: owned_user_id!("@alice:example.org"),
            kind: TimelineEventKind::PlainText { event: raw },
            expires_at: MilliSecondsSinceUnixEpoch(until.try_into().unwrap()),
        }
    }

    #[test]
    fn keyless_replay_cannot_extend_deadline_or_forge_encryption() {
        let mut cache = KeylessState::default();
        assert!(cache.ingest(1, 1, vec![receipt(0, 20, 0)]));
        assert!(!cache.ingest(2, 1, vec![receipt(0, 500, 0)]));
        let snapshot = cache.snapshot(3);
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(expiry(&snapshot.events[0]), 20);
        assert!(snapshot.events[0].encryption_info().is_none());
        assert!(cache.snapshot(20).events.is_empty());
    }

    #[test]
    fn keyless_count_overflow_latches_across_expiry_and_rejoin() {
        let mut cache = KeylessState::default();
        cache.ingest(1, 1, (0..=MAX_EVENTS).map(|n| receipt(n, 20, 0)).collect());
        let generation = cache.generation();
        assert!(cache.snapshot(2).invalidated);
        assert!(cache.snapshot(2).events.is_empty());
        cache.expire(100);
        cache.leave();
        cache.activate();
        cache.ingest(100, cache.generation(), vec![receipt(999, 200, 0)]);
        assert!(cache.generation() > generation);
        assert!(cache.snapshot(101).invalidated);
        assert!(cache.snapshot(101).events.is_empty());
    }

    #[test]
    fn keyless_raw_byte_caps_fail_closed() {
        let mut cache = KeylessState::default();
        cache.ingest(1, 1, vec![receipt(0, 20, MAX_EVENT_BYTES)]);
        assert!(cache.snapshot(2).invalidated);
        let mut cache = KeylessState::default();
        cache.ingest(1, 1, (0..32).map(|n| receipt(n, 20, 40 * 1024)).collect());
        assert!(cache.snapshot(2).invalidated);
    }

    #[test]
    fn keyless_redaction_precedes_receipt_and_survives_replay() {
        let mut cache = KeylessState::default();
        let event = receipt(0, 500, 0);
        cache.redact(1, &event.event_id);
        cache.ingest(2, 1, vec![event.clone()]);
        assert!(cache.snapshot(3).events.is_empty());
        cache.leave();
        cache.activate();
        cache.ingest(4, cache.generation(), vec![event]);
        assert!(cache.snapshot(5).events.is_empty());
    }

    #[test]
    fn keyless_leave_rejects_in_flight_old_generation() {
        let mut cache = KeylessState::default();
        let old_generation = cache.generation();
        cache.leave();
        assert!(cache.snapshot(2).invalidated);
        cache.activate();
        assert!(!cache.snapshot(2).invalidated);
        assert!(!cache.ingest(3, old_generation, vec![receipt(0, 20, 0)]));
        assert!(cache.snapshot(4).events.is_empty());
        assert!(cache.ingest(3, cache.generation(), vec![receipt(1, 20, 0)]));
    }

    #[test]
    fn keyless_redaction_bounds_are_fail_closed() {
        let mut cache = KeylessState::default();
        for id in 0..=MAX_REDACTIONS {
            cache.redact(1, &receipt(id, 20, 0).event_id);
        }
        assert!(cache.snapshot(2).invalidated);
    }

    #[test]
    fn keyless_rejects_non_sticky_malformed_keyed_and_redacted_events() {
        use super::super::{classify, resolve_keyless};
        let original = json!({
            "type": "m.rtc.notification", "sender": "@alice:example.org",
            "event_id": "$ring:example.org", "origin_server_ts": 1,
            "content": {"notification_type": "ring"},
            "msc4354_sticky": {"duration_ms": 500},
        });
        for content in [json!({"sticky_key": 3}), json!({"msc4354_sticky_key": null}), json!([])] {
            let mut value = original.clone();
            value["content"] = content;
            let raw = Raw::from_json_string(value.to_string()).unwrap();
            assert!(classify(2, &raw).is_none());
        }
        for content in [json!({"sticky_key": "slot"}), json!({"msc4354_sticky_key": "slot"})] {
            let mut value = original.clone();
            value["content"] = content;
            let raw = Raw::from_json_string(value.to_string()).unwrap();
            assert!(
                resolve_keyless(
                    classify(2, &raw).unwrap(),
                    TimelineEventKind::PlainText { event: raw }
                )
                .is_none()
            );
        }
        let mut value = original.clone();
        value.as_object_mut().unwrap().remove("msc4354_sticky");
        assert!(classify(2, &Raw::from_json_string(value.to_string()).unwrap()).is_none());
        let mut value = original;
        value["unsigned"] = json!({"redacted_because": {}});
        let raw = Raw::from_json_string(value.to_string()).unwrap();
        assert!(
            resolve_keyless(
                classify(2, &raw).unwrap(),
                TimelineEventKind::PlainText { event: raw }
            )
            .is_none()
        );
    }

    #[matrix_sdk_test::async_test]
    async fn test_keyless_lag_is_explicit_and_snapshot_is_complete() {
        use super::super::{StickyEvents, now_ms};
        let sticky = StickyEvents::new(ruma::owned_room_id!("!ring:example.org"));
        let mut updates = sticky.subscribe_keyless();
        let generation = sticky.keyless_live().generation;
        let now = now_ms();
        for id in 0..40 {
            sticky.ingest_keyless(now, generation, vec![receipt(id, now + 60_000, 0)]);
        }
        assert!(matches!(
            updates.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
        ));
        let snapshot = sticky.keyless_live();
        assert_eq!(snapshot.generation, generation);
        assert!(!snapshot.invalidated);
        assert_eq!(snapshot.events.len(), 40);
        sticky.leave_keyless();
        assert!(sticky.keyless_live().invalidated);
        assert!(sticky.keyless_live().events.is_empty());
    }
}
