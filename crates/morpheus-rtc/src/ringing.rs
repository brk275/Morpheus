//! Pure validation for the deployed MSC4075 `m.rtc.notification` dialect.
//!
//! This matches matrix-rust-rtc v0.5.0-rc.2 and the proposal at
//! `39cc54743a4f2187fdee1a69909b4a84eb7af014`, not the later `m.rtc.invite`
//! proposal. The pinned sender emits both nested and flat call fields; its
//! compatibility adapter emits only flat fields. Neither shape proves that a
//! call exists. A receiver must resolve the referenced membership and supply
//! independently checked, live participation before using a parsed ring.
//!
//! Parsing examines only bounded fields; it does not clone arbitrary fallback
//! text, unsigned data or unknown extensions. The transport must also bound
//! the JSON input before decoding it into a [`Value`]. No network, account
//! selection, event deduplication, sound, or UI work happens here.

use std::{collections::BTreeSet, fmt};

use matrix_sdk::ruma::{EventId, RoomId, UserId};
use serde_json::{Map, Value};

pub const MAX_RING_LIFETIME_MS: u64 = 120_000;
const MAX_CLOCK_SKEW_MS: u64 = 20_000;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
const MAX_ID_BYTES: usize = 255;
const MAX_MENTIONED_USERS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallIntent {
    Audio,
    Video,
}

/// Parsed, untrusted notification data. Successful parsing is not permission
/// to ring: use [`Self::validate_for`] with independently resolved context.
#[derive(Clone, Eq, PartialEq)]
pub struct RingNotification {
    event_id: String,
    room_id: String,
    sender: String,
    membership_event_id: String,
    sender_device_id: Option<String>,
    sender_ts_ms: u64,
    origin_server_ts_ms: u64,
    lifetime_ms: u64,
    mentions_room: bool,
    mentioned_users: BTreeSet<String>,
    intent: Option<CallIntent>,
}

// Accidental diagnostics must not publish participant, room or device IDs.
impl fmt::Debug for RingNotification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingNotification")
            .field("lifetime_ms", &self.lifetime_ms)
            .field("mentions_room", &self.mentions_room)
            .field("mentioned_user_count", &self.mentioned_users.len())
            .field("intent", &self.intent)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RingValidationError {
    #[error("unsupported call notification event")]
    UnsupportedEvent,
    #[error("invalid call notification envelope")]
    InvalidEnvelope,
    #[error("invalid call notification content")]
    InvalidContent,
    #[error("conflicting call notification fields")]
    ConflictingFields,
    #[error("unsupported call notification application or intent")]
    UnsupportedCall,
    #[error("invalid call notification membership reference")]
    InvalidReference,
    #[error("invalid call notification targets")]
    InvalidMentions,
}

/// Authorization checked against the sender's room power levels by the host.
/// This is relevant only for a room mention, not an explicit user mention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoomMentionPermission {
    Allowed,
    Denied,
}

/// Identity extracted from a membership by the host's MatrixRTC adapter.
/// These are comparison inputs, not a claim that their payload is authentic.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CallParticipation<'a> {
    pub room_id: &'a str,
    pub sender: &'a str,
    pub logical_membership_id: &'a str,
    pub slot_id: &'a str,
    pub application: &'a str,
    pub device_id: Option<&'a str>,
}

/// The host must construct this only after resolving the referenced event
/// in this account and room, checking its sender/decryption metadata, and
/// finding a currently joined membership against an open slot in the selected
/// dialect. A state lookup failure is not a valid context.
///
/// Keep the original reference identity separately from the current logical
/// participation: a sticky refresh changes the membership event ID without
/// starting a different participation. This module compares the two records;
/// it cannot establish liveness, slot state, account ownership or authenticity.
#[derive(Clone, Copy)]
pub struct ResolvedJoinedCall<'a> {
    pub referenced_event_id: &'a str,
    pub referenced: CallParticipation<'a>,
    pub current: CallParticipation<'a>,
}

pub struct RingContext<'a> {
    pub recipient_user_id: &'a str,
    pub room_mention_permission: RoomMentionPermission,
    pub call: ResolvedJoinedCall<'a>,
}

/// Capture this once when admitting a notification, then use a monotonic
/// timer for `remaining_ms`. Re-reading it must not restart an existing ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingWindow {
    pub expires_at_ms: u64,
    pub remaining_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RingIneligible {
    #[error("invalid resolved call context")]
    InvalidContext,
    #[error("notification does not match the resolved participation")]
    DifferentParticipation,
    #[error("own call notifications do not ring")]
    OwnSender,
    #[error("call notification does not target this user")]
    NotTargeted,
    #[error("call notification has expired")]
    Expired,
    #[error("call notification timestamp is too far in the future")]
    FutureTimestamp,
}

impl RingNotification {
    /// Parse a decrypted Matrix event envelope. Sync events may omit room_id;
    /// if present it must exactly match the independently supplied room.
    /// Stable and deployed unstable notification types are accepted. Older
    /// `m.call.notify`, newer `m.rtc.invite`, and silent notifications are not.
    pub fn parse(event: &Value, room_id: &str) -> Result<Self, RingValidationError> {
        use RingValidationError as E;
        let event = event.as_object().ok_or(E::InvalidEnvelope)?;
        if !matches!(
            event.get("type").and_then(Value::as_str),
            Some("m.rtc.notification" | "org.matrix.msc4075.rtc.notification")
        ) {
            return Err(E::UnsupportedEvent);
        }
        if !valid_room_id(room_id)
            || event
                .get("room_id")
                .is_some_and(|value| value.as_str() != Some(room_id))
            || event
                .get("unsigned")
                .is_some_and(|value| !value.is_object() || value.get("redacted_because").is_some())
        {
            return Err(E::InvalidEnvelope);
        }
        let event_id = field_str(event, "event_id")
            .filter(|id| valid_event_id(id))
            .ok_or(E::InvalidEnvelope)?;
        let sender = field_str(event, "sender")
            .filter(|id| valid_user_id(id))
            .ok_or(E::InvalidEnvelope)?;
        let origin_server_ts_ms =
            integer(event.get("origin_server_ts")).ok_or(E::InvalidEnvelope)?;
        let content = event
            .get("content")
            .and_then(Value::as_object)
            .ok_or(E::InvalidContent)?;
        let application = match content.get("application") {
            None => None, // The pinned compatibility adapter removes it.
            Some(Value::Object(application))
                if field_str(application, "type") == Some("m.call") =>
            {
                Some(application)
            }
            Some(_) => return Err(E::UnsupportedCall),
        };
        if shared_field(content, application, "notification_type")?.and_then(Value::as_str)
            != Some("ring")
        {
            return Err(E::UnsupportedCall);
        }
        let sender_ts_ms =
            integer(shared_field(content, application, "sender_ts")?).ok_or(E::InvalidContent)?;
        // Clamp while still u64, before adding or narrowing. In particular,
        // Ruma's deployed expiration helper casts an unclamped duration to u32.
        let lifetime_ms = integer(shared_field(content, application, "lifetime")?)
            .ok_or(E::InvalidContent)?
            .min(MAX_RING_LIFETIME_MS);
        let intent = match shared_field(content, application, "m.call.intent")? {
            None => None,
            Some(Value::String(value)) if value == "audio" => Some(CallIntent::Audio),
            Some(Value::String(value)) if value == "video" => Some(CallIntent::Video),
            Some(_) => return Err(E::UnsupportedCall),
        };
        // device_id belongs inside application in this dialect. A top-level
        // spelling is not an alternate source of authenticated device identity.
        if content.contains_key("device_id") {
            return Err(E::InvalidContent);
        }
        let sender_device_id = match application.and_then(|a| a.get("device_id")) {
            None => None,
            Some(Value::String(id)) if valid_opaque_id(id) => Some(id.clone()),
            Some(_) => return Err(E::InvalidContent),
        };
        let relation = content
            .get("m.relates_to")
            .and_then(Value::as_object)
            .ok_or(E::InvalidReference)?;
        if field_str(relation, "rel_type") != Some("m.reference") {
            return Err(E::InvalidReference);
        }
        let membership_event_id = field_str(relation, "event_id")
            .filter(|id| valid_event_id(id) && *id != event_id)
            .ok_or(E::InvalidReference)?;
        let mentions = content
            .get("m.mentions")
            .and_then(Value::as_object)
            .ok_or(E::InvalidMentions)?;
        let mentions_room = match mentions.get("room") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(E::InvalidMentions),
        };
        let mut mentioned_users = BTreeSet::new();
        if let Some(value) = mentions.get("user_ids") {
            let users = value
                .as_array()
                .filter(|users| users.len() <= MAX_MENTIONED_USERS)
                .ok_or(E::InvalidMentions)?;
            for value in users {
                let user = value
                    .as_str()
                    .filter(|id| valid_user_id(id))
                    .ok_or(E::InvalidMentions)?;
                mentioned_users.insert(user.to_owned());
            }
        }
        if !mentions_room && mentioned_users.is_empty() {
            return Err(E::InvalidMentions);
        }
        Ok(Self {
            event_id: event_id.to_owned(),
            room_id: room_id.to_owned(),
            sender: sender.to_owned(),
            membership_event_id: membership_event_id.to_owned(),
            sender_device_id,
            sender_ts_ms,
            origin_server_ts_ms,
            lifetime_ms,
            mentions_room,
            mentioned_users,
            intent,
        })
    }

    pub fn event_id(&self) -> &str {
        &self.event_id
    }
    pub fn room_id(&self) -> &str {
        &self.room_id
    }
    pub fn sender(&self) -> &str {
        &self.sender
    }
    pub fn membership_event_id(&self) -> &str {
        &self.membership_event_id
    }
    pub fn intent(&self) -> Option<CallIntent> {
        self.intent
    }

    /// Check targeting, correlation and time. The host must still suppress
    /// duplicates, own already-joined/declined participation, account changes,
    /// muted notifications and voice spaces. A video intent is descriptive;
    /// this function never grants permission to enable camera or microphone.
    pub fn validate_for(
        &self,
        context: &RingContext<'_>,
        now_ms: u64,
    ) -> Result<RingWindow, RingIneligible> {
        use RingIneligible as E;
        let call = &context.call;
        if now_ms > MAX_SAFE_INTEGER
            || !valid_user_id(context.recipient_user_id)
            || !valid_event_id(call.referenced_event_id)
            || !valid_participation(&call.referenced)
            || !valid_participation(&call.current)
        {
            return Err(E::InvalidContext);
        }
        if self.sender == context.recipient_user_id {
            return Err(E::OwnSender);
        }
        if self.membership_event_id != call.referenced_event_id
            || self.room_id != call.referenced.room_id
            || self.sender != call.referenced.sender
            || call.referenced != call.current
            || self
                .sender_device_id
                .as_deref()
                .is_some_and(|id| Some(id) != call.referenced.device_id)
        {
            return Err(E::DifferentParticipation);
        }
        if !self.mentioned_users.contains(context.recipient_user_id)
            && !(self.mentions_room
                && context.room_mention_permission == RoomMentionPermission::Allowed)
        {
            return Err(E::NotTargeted);
        }
        self.window_at(now_ms)
    }

    /// Compute only the notification's untrusted time window at receipt time.
    ///
    /// This grants no permission to ring, present an offer or join: it does
    /// not check the recipient, sender trust, membership, room policy or call
    /// state. Those checks still belong in [`Self::validate_for`] and the host.
    ///
    /// Capture this before the first network await and immediately translate
    /// `remaining_ms` to a monotonic deadline. Keep that first deadline as an
    /// upper bound: tolerated future timestamps can make a later evaluation
    /// produce a later expiry, and a wall-clock adjustment can change the
    /// remaining time. Revalidation may shorten the bound, never replace it
    /// with a longer one. The host must also cap it by the retained receipt's
    /// own expiry and prevent replay from establishing a new deadline.
    pub fn window_at(&self, now_ms: u64) -> Result<RingWindow, RingIneligible> {
        use RingIneligible as E;
        if now_ms > MAX_SAFE_INTEGER {
            return Err(E::InvalidContext);
        }
        let start = if self.sender_ts_ms.abs_diff(self.origin_server_ts_ms) > MAX_CLOCK_SKEW_MS {
            self.origin_server_ts_ms
        } else {
            self.sender_ts_ms
        };
        // A server timestamp does not permit an arbitrarily future deadline.
        // Small local clock skew is tolerated but never extends audible time
        // beyond the received, capped lifetime.
        if start > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            return Err(E::FutureTimestamp);
        }
        let expires_at_ms = start.min(now_ms).saturating_add(self.lifetime_ms);
        let remaining_ms = expires_at_ms.saturating_sub(now_ms);
        if remaining_ms == 0 {
            return Err(E::Expired);
        }
        Ok(RingWindow {
            expires_at_ms,
            remaining_ms,
        })
    }
}

fn field_str<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn shared_field<'a>(
    flat: &'a Map<String, Value>,
    nested: Option<&'a Map<String, Value>>,
    key: &str,
) -> Result<Option<&'a Value>, RingValidationError> {
    let a = flat.get(key);
    let b = nested.and_then(|nested| nested.get(key));
    // Only bounded scalars may reach Value equality. Comparing arbitrary
    // duplicate objects or arrays would recurse through attacker input.
    for value in [a, b].into_iter().flatten() {
        let valid = match key {
            "sender_ts" | "lifetime" => integer(Some(value)).is_some(),
            "notification_type" | "m.call.intent" => {
                value.as_str().is_some_and(|value| value.len() <= 32)
            }
            _ => false,
        };
        if !valid {
            return Err(RingValidationError::InvalidContent);
        }
    }
    if let (Some(a), Some(b)) = (a, b) {
        if a != b {
            return Err(RingValidationError::ConflictingFields);
        }
    }
    Ok(a.or(b))
}

fn integer(value: Option<&Value>) -> Option<u64> {
    value?.as_u64().filter(|value| *value <= MAX_SAFE_INTEGER)
}

fn valid_opaque_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && !id.chars().any(|c| c.is_control() || c.is_whitespace())
}

// Bound allocations and exclude empty/control-bearing values before calling
// the pinned SDK's identifier validators. Matrix syntax stays owned by Ruma.
fn valid_user_id(id: &str) -> bool {
    valid_opaque_id(id) && UserId::parse(id).is_ok()
}

fn valid_room_id(id: &str) -> bool {
    valid_opaque_id(id) && id.len() > 1 && RoomId::parse(id).is_ok()
}

fn valid_event_id(id: &str) -> bool {
    valid_opaque_id(id) && id.len() > 1 && EventId::parse(id).is_ok()
}

fn valid_participation(participation: &CallParticipation<'_>) -> bool {
    participation.application == "m.call"
        && valid_room_id(participation.room_id)
        && valid_user_id(participation.sender)
        && valid_opaque_id(participation.logical_membership_id)
        && participation
            .slot_id
            .strip_prefix("m.call#")
            .is_some_and(valid_opaque_id)
        && participation.slot_id.len() <= MAX_ID_BYTES
        && participation.device_id.is_none_or(valid_opaque_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> Value {
        json!({"type":"org.matrix.msc4075.rtc.notification", "event_id":"$ring",
            "sender":"@alice:example.org", "origin_server_ts":100000,
            "content":{"notification_type":"ring", "sender_ts":100000, "lifetime":30000,
                "m.call.intent":"audio", "m.mentions":{"user_ids":["@bob:example.org"]},
                "m.relates_to":{"rel_type":"m.reference", "event_id":"$member"}}})
    }

    fn context() -> RingContext<'static> {
        let participation = CallParticipation {
            room_id: "!room:example.org",
            sender: "@alice:example.org",
            logical_membership_id: "participation-1",
            slot_id: "m.call#room",
            application: "m.call",
            device_id: Some("ALICE"),
        };
        RingContext {
            recipient_user_id: "@bob:example.org",
            room_mention_permission: RoomMentionPermission::Denied,
            call: ResolvedJoinedCall {
                referenced_event_id: "$member",
                referenced: participation,
                current: participation,
            },
        }
    }

    fn parse(value: &Value) -> Result<RingNotification, RingValidationError> {
        RingNotification::parse(value, "!room:example.org")
    }

    #[test]
    fn deployed_flat_nested_and_dual_fields_have_the_same_meaning() {
        let flat = event();
        let mut dual = flat.clone();
        dual["type"] = json!("m.rtc.notification");
        dual["room_id"] = json!("!room:example.org");
        dual["content"]["application"] = json!({"type":"m.call", "notification_type":"ring",
            "sender_ts":100000, "lifetime":30000, "m.call.intent":"audio"});
        let mut nested = dual.clone();
        for key in [
            "notification_type",
            "sender_ts",
            "lifetime",
            "m.call.intent",
        ] {
            nested["content"].as_object_mut().unwrap().remove(key);
        }
        for value in [flat, dual, nested] {
            let ring = parse(&value).unwrap();
            assert_eq!(ring.event_id(), "$ring");
            assert_eq!(ring.room_id(), "!room:example.org");
            assert_eq!(ring.sender(), "@alice:example.org");
            assert_eq!(ring.membership_event_id(), "$member");
            assert_eq!(ring.intent(), Some(CallIntent::Audio));
            assert_eq!(
                ring.validate_for(&context(), 110000).unwrap(),
                RingWindow {
                    expires_at_ms: 130000,
                    remaining_ms: 20000
                }
            );
        }
    }

    #[test]
    fn conflicting_copies_are_rejected_before_clamping_or_using_either_shape() {
        for (key, conflicting) in [
            ("notification_type", json!("notification")),
            ("sender_ts", json!(100001)),
            ("lifetime", json!(u32::MAX)),
            ("m.call.intent", json!("video")),
        ] {
            let mut value = event();
            value["content"]["application"] = json!({"type":"m.call"});
            value["content"]["application"][key] = conflicting;
            assert_eq!(parse(&value), Err(RingValidationError::ConflictingFields));
        }
    }

    #[test]
    fn shared_fields_reject_non_scalar_or_oversized_values() {
        for key in [
            "notification_type",
            "sender_ts",
            "lifetime",
            "m.call.intent",
        ] {
            for invalid in [
                json!({"extension":[1,2,3]}),
                json!([1, 2, 3]),
                json!("a".repeat(256)),
            ] {
                let mut value = event();
                value["content"][key] = invalid.clone();
                value["content"]["application"] = json!({"type":"m.call"});
                value["content"]["application"][key] = invalid;
                assert_eq!(parse(&value), Err(RingValidationError::InvalidContent));
            }
        }
    }

    #[test]
    fn unsupported_silent_redacted_and_wrong_room_events_do_not_become_rings() {
        for event_type in ["m.call.notify", "m.rtc.invite", "m.room.encrypted"] {
            let mut value = event();
            value["type"] = json!(event_type);
            assert_eq!(parse(&value), Err(RingValidationError::UnsupportedEvent));
        }
        let mut value = event();
        value["content"]["notification_type"] = json!("notification");
        assert_eq!(parse(&value), Err(RingValidationError::UnsupportedCall));
        let mut value = event();
        value["content"]["application"] = json!({"type":"other.app"});
        assert_eq!(parse(&value), Err(RingValidationError::UnsupportedCall));
        let mut value = event();
        value["room_id"] = json!("!other:example.org");
        assert_eq!(parse(&value), Err(RingValidationError::InvalidEnvelope));
        let mut value = event();
        value["unsigned"] = json!({"redacted_because":{}});
        assert_eq!(parse(&value), Err(RingValidationError::InvalidEnvelope));
    }

    #[test]
    fn envelope_reference_and_bounded_ruma_ids_are_required() {
        for field in ["sender", "event_id", "origin_server_ts"] {
            let mut value = event();
            value.as_object_mut().unwrap().remove(field);
            assert_eq!(parse(&value), Err(RingValidationError::InvalidEnvelope));
        }
        for invalid in [
            "alice:example.org",
            "@a:",
            "@a:host/path",
            "@a:host:65536",
            "@a:ho st",
            "@a:[bad]",
        ] {
            let mut value = event();
            value["sender"] = json!(invalid);
            assert!(parse(&value).is_err());
        }
        for invalid in ["member", "$", "$bad space", "$x:host/path", "$ring"] {
            let mut value = event();
            value["content"]["m.relates_to"]["event_id"] = json!(invalid);
            assert_eq!(parse(&value), Err(RingValidationError::InvalidReference));
        }
        let mut value = event();
        value["content"]["m.relates_to"]["rel_type"] = json!("m.annotation");
        assert_eq!(parse(&value), Err(RingValidationError::InvalidReference));
        assert!(valid_user_id("@user:[2001:db8::1]:8448"));
        assert!(valid_room_id("!room:localhost:8008"));
        assert!(valid_room_id("!opaque_room"));
        assert!(valid_event_id("$opaque:example.org"));
        assert!(valid_event_id("$opaque_hash-0123"));
        assert!(!valid_user_id(&format!(
            "@{}:example.org",
            "a".repeat(MAX_ID_BYTES)
        )));
    }

    #[test]
    fn mentions_are_bounded_exact_and_not_replaced_by_room_permission_bypass() {
        let ring = parse(&event()).unwrap();
        let mut target = context();
        target.recipient_user_id = "@bob:other.example";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::NotTargeted)
        );
        target.recipient_user_id = "@alice:example.org";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::OwnSender)
        );
        let mut value = event();
        value["content"]["m.mentions"] = json!({"room":true});
        let ring = parse(&value).unwrap();
        assert_eq!(
            ring.validate_for(&context(), 100000),
            Err(RingIneligible::NotTargeted)
        );
        let mut target = context();
        target.room_mention_permission = RoomMentionPermission::Allowed;
        assert!(ring.validate_for(&target, 100000).is_ok());
        for mentions in [
            json!({}),
            json!({"room":"true"}),
            json!({"user_ids":[null]}),
            json!({"user_ids":vec!["@bob:example.org"; MAX_MENTIONED_USERS + 1]}),
        ] {
            value["content"]["m.mentions"] = mentions;
            assert_eq!(parse(&value), Err(RingValidationError::InvalidMentions));
        }
    }

    #[test]
    fn numbers_are_checked_before_capping_and_expiry_arithmetic() {
        for field in ["sender_ts", "lifetime"] {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!("30000"),
                json!(MAX_SAFE_INTEGER + 1),
                json!(u64::MAX),
            ] {
                let mut value = event();
                value["content"][field] = invalid;
                assert_eq!(parse(&value), Err(RingValidationError::InvalidContent));
            }
        }
        let mut value = event();
        value["origin_server_ts"] = json!(u64::MAX);
        assert_eq!(parse(&value), Err(RingValidationError::InvalidEnvelope));
        let mut value = event();
        value["content"]["lifetime"] = json!(MAX_SAFE_INTEGER);
        let window = parse(&value)
            .unwrap()
            .validate_for(&context(), 100000)
            .unwrap();
        assert_eq!(window.remaining_ms, MAX_RING_LIFETIME_MS);
        assert_eq!(window.expires_at_ms, 220000);
        value["content"]["lifetime"] = json!(0);
        assert_eq!(
            parse(&value).unwrap().validate_for(&context(), 100000),
            Err(RingIneligible::Expired)
        );
    }

    #[test]
    fn expiry_ignores_unsigned_age_and_handles_clock_skew_without_extending_ring() {
        let mut value = event();
        value["unsigned"] = json!({"age":0});
        assert_eq!(
            parse(&value).unwrap().validate_for(&context(), 130000),
            Err(RingIneligible::Expired)
        );
        value["unsigned"]["age"] = json!(u64::MAX);
        assert_eq!(
            parse(&value)
                .unwrap()
                .validate_for(&context(), 100000)
                .unwrap()
                .remaining_ms,
            30000
        );
        for skewed in [0, 150000, MAX_SAFE_INTEGER] {
            value["content"]["sender_ts"] = json!(skewed);
            assert_eq!(
                parse(&value)
                    .unwrap()
                    .validate_for(&context(), 110000)
                    .unwrap()
                    .remaining_ms,
                20000
            );
        }
        // Exactly 20 seconds preserves sender time; beyond that uses the
        // independently supplied origin timestamp instead.
        value["content"]["sender_ts"] = json!(80000);
        assert_eq!(
            parse(&value)
                .unwrap()
                .validate_for(&context(), 100000)
                .unwrap()
                .remaining_ms,
            10000
        );
        value["content"]["sender_ts"] = json!(79999);
        assert_eq!(
            parse(&value)
                .unwrap()
                .validate_for(&context(), 100000)
                .unwrap()
                .remaining_ms,
            30000
        );
        value["content"]["sender_ts"] = json!(110000);
        assert_eq!(
            parse(&value)
                .unwrap()
                .validate_for(&context(), 100000)
                .unwrap()
                .remaining_ms,
            30000
        );
        value["origin_server_ts"] = json!(MAX_SAFE_INTEGER);
        value["content"]["sender_ts"] = json!(MAX_SAFE_INTEGER);
        assert_eq!(
            parse(&value).unwrap().validate_for(&context(), 100000),
            Err(RingIneligible::FutureTimestamp)
        );
        assert_eq!(
            parse(&event()).unwrap().validate_for(&context(), u64::MAX),
            Err(RingIneligible::InvalidContext)
        );
    }

    #[test]
    fn first_receipt_window_precedes_network_and_is_not_replaced_by_a_later_window() {
        let mut value = event();
        value["content"]["sender_ts"] = json!(110000);
        let ring = parse(&value).unwrap();
        let first = ring.window_at(100000).unwrap();
        assert_eq!(
            first,
            RingWindow {
                expires_at_ms: 130000,
                remaining_ms: 30000,
            }
        );
        // Twelve seconds of admission work later, the sender timestamp is no
        // longer in the future. Recomputing alone would move the deadline ten
        // seconds later; the host must retain its first monotonic upper bound.
        let after_network = ring.validate_for(&context(), 112000).unwrap();
        assert_eq!(after_network, ring.window_at(112000).unwrap());
        assert_eq!(after_network.expires_at_ms, 140000);
        assert_eq!(after_network.remaining_ms, 28000);
        let first_deadline_ms = first.remaining_ms;
        let proposed_later_deadline_ms = 12000 + after_network.remaining_ms;
        assert_eq!(first_deadline_ms.min(proposed_later_deadline_ms), 30000);
    }

    #[test]
    fn standalone_window_does_not_authorize_an_untargeted_call() {
        let mut value = event();
        value["content"]["m.mentions"] = json!({"user_ids": ["@someone:example.org"]});
        let ring = parse(&value).unwrap();
        assert!(ring.window_at(100000).is_ok());
        assert_eq!(
            ring.validate_for(&context(), 100000),
            Err(RingIneligible::NotTargeted)
        );
    }

    #[test]
    fn standalone_window_rejects_expired_future_and_out_of_range_clock_values() {
        let ring = parse(&event()).unwrap();
        for now in [130000, MAX_SAFE_INTEGER] {
            assert_eq!(ring.window_at(now), Err(RingIneligible::Expired));
        }
        assert_eq!(ring.window_at(0), Err(RingIneligible::FutureTimestamp));
        for now in [MAX_SAFE_INTEGER + 1, u64::MAX] {
            assert_eq!(ring.window_at(now), Err(RingIneligible::InvalidContext));
        }
        let mut value = event();
        value["origin_server_ts"] = json!(MAX_SAFE_INTEGER);
        value["content"]["sender_ts"] = json!(MAX_SAFE_INTEGER);
        let boundary = parse(&value).unwrap().window_at(MAX_SAFE_INTEGER).unwrap();
        assert_eq!(boundary.remaining_ms, 30000);
        assert_eq!(boundary.expires_at_ms, MAX_SAFE_INTEGER + 30000);
        value["content"]["lifetime"] = json!(0);
        assert_eq!(
            parse(&value).unwrap().window_at(MAX_SAFE_INTEGER),
            Err(RingIneligible::Expired)
        );
    }

    #[test]
    fn checked_logical_participation_must_match_sender_room_reference_and_slot() {
        let ring = parse(&event()).unwrap();
        // Current logical identity does not contain a current event ID:
        // refreshing that event alone does not invalidate the original ref.
        assert!(ring.validate_for(&context(), 100000).is_ok());
        let mut target = context();
        target.call.referenced_event_id = "$other";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::DifferentParticipation)
        );
        let mut target = context();
        target.call.current.logical_membership_id = "later-join";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::DifferentParticipation)
        );
        let mut target = context();
        target.call.current.slot_id = "m.call#different";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::DifferentParticipation)
        );
        let mut target = context();
        target.call.current.room_id = "!different:example.org";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::DifferentParticipation)
        );
        let mut target = context();
        target.call.referenced.sender = "@mallory:example.org";
        target.call.current = target.call.referenced;
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::DifferentParticipation)
        );
        let mut target = context();
        target.call.current.application = "other.app";
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::InvalidContext)
        );
    }

    #[test]
    fn claimed_device_is_only_a_constraint_on_resolved_identity() {
        let mut value = event();
        value["content"]["application"] = json!({"type":"m.call", "device_id":"ALICE"});
        let ring = parse(&value).unwrap();
        assert!(ring.validate_for(&context(), 100000).is_ok());
        let mut target = context();
        target.call.referenced.device_id = None;
        target.call.current.device_id = None;
        assert_eq!(
            ring.validate_for(&target, 100000),
            Err(RingIneligible::DifferentParticipation)
        );
        value["content"]["device_id"] = json!("ALICE");
        assert_eq!(parse(&value), Err(RingValidationError::InvalidContent));
    }

    #[test]
    fn diagnostics_never_include_event_or_identity_payloads() {
        let ring = parse(&event()).unwrap();
        let rendered = format!("{ring:?}");
        for private in [
            ring.event_id(),
            ring.sender(),
            ring.room_id(),
            ring.membership_event_id(),
        ] {
            assert!(!rendered.contains(private));
        }
        assert_eq!(
            RingValidationError::InvalidContent.to_string(),
            "invalid call notification content"
        );
    }
}
