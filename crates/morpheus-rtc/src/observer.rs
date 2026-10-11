//! Passive, account-owned MatrixRTC signalling observation.
//!
//! No join, membership write, media connection, or device verification occurs
//! here. The application must hold its revocable account selection while using
//! this observer, check SDK verification for the referenced/current events, and
//! revalidate snapshots on a timer as well as on membership changes. A roster
//! alone does not prove trust, consent, invitation delivery, or a callable SFU.

use std::{collections::BTreeSet, fmt, sync::Arc, time::Duration};

use matrix_rtc_call_sdk::SdkMatrixBackend;
use matrix_rtc_core::{
    BaseRtcClient, BaseRtcRoomHandle, EventOrigin, JoinedMembership, RawSlotEvent,
    RawSlotEventContent, RoomEncryption, RoomOptions, SlotState, wire_event_type,
};
use matrix_sdk::{
    Client, Room, RoomState,
    config::RequestConfig,
    ruma::{
        EventId, OwnedRoomId, RoomId, UserId, api::client::state::get_state_event_for_key,
        events::StateEventType,
    },
};
use serde_json::Value;
use tokio::{sync::watch, time};

use crate::InteropMode;

const SLOT: &str = "m.call#room";
const MAX_PARTICIPANTS: usize = 128;
const MAX_LIVE_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// Fixed categories only: SDK errors can contain private room or server data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ObserverError {
    #[error("the passive call dialect is unsupported")]
    UnsupportedDialect,
    #[error("the Matrix room identifier is invalid")]
    InvalidRoom,
    #[error("the account is not joined to the room")]
    NotJoined,
    #[error("sticky call signalling is unavailable")]
    UnsupportedServer,
    #[error("the room encryption state is unavailable or unencrypted")]
    NotEncrypted,
    #[error("the room-wide encrypted call slot is unavailable or closed")]
    SlotUnavailable,
    #[error("call observation could not be established")]
    Unavailable,
    #[error("call observation exceeded its time limit")]
    TimedOut,
    #[error("the call observation subscription closed")]
    Closed,
    #[error("the call membership cannot be confirmed in the selected dialect")]
    InvalidMembership,
    #[error("the call observation exceeds the supported participant limit")]
    TooManyParticipants,
}

/// Authenticated decryption attribution plus current logical participation.
/// This deliberately does not carry a `Verified` trust claim. The app must
/// compare these facts with the SDK's independently verified event metadata.
#[derive(Clone, Eq, PartialEq)]
pub struct ObservedParticipant {
    pub user_id: String,
    pub device_id: String,
    pub membership_id: String,
    /// Current refresh event, which may differ from an invitation's reference.
    pub membership_event_id: String,
}

impl fmt::Debug for ObservedParticipant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObservedParticipant { .. }")
    }
}

/// A point-in-time roster admitted only after a successful fresh slot read.
/// Do not reuse it as proof after waiting for other network operations.
#[derive(Clone, Eq, PartialEq)]
pub struct ObservedCall {
    pub room_id: String,
    /// Canonical core slot. Sticky2025 membership content uses `m.call#ROOM`.
    pub slot_id: String,
    pub mode: InteropMode,
    pub participants: Vec<ObservedParticipant>,
}

impl fmt::Debug for ObservedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservedCall")
            .field("participants", &self.participants.len())
            .finish_non_exhaustive()
    }
}

/// Own one SDK room feed and its roster subscription. Dropping this object
/// detaches/aborts the pinned core's room feed and releases its to-device feed
/// when it is the last room; there are no detached Morpheus worker tasks.
pub struct PassiveCallObserver {
    room: Room,
    room_id: OwnedRoomId,
    mode: InteropMode,
    handle: BaseRtcRoomHandle<SdkMatrixBackend>,
    roster: watch::Receiver<Vec<JoinedMembership>>,
}

impl fmt::Debug for PassiveCallObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PassiveCallObserver { .. }")
    }
}

impl PassiveCallObserver {
    /// The caller must keep this account's sticky sync running. Opening does
    /// not mean a call is open; only `snapshot` performs fresh slot validation.
    pub async fn open(
        client: Client,
        room_id: &str,
        mode: InteropMode,
    ) -> Result<Self, ObserverError> {
        wire_slot(mode)?;
        let room_id = RoomId::parse(room_id).map_err(|_| ObserverError::InvalidRoom)?;
        time::timeout(OPEN_TIMEOUT, async move {
            if client.user_id().is_none() {
                return Err(ObserverError::NotJoined);
            }
            let room = client.get_room(&room_id).ok_or(ObserverError::NotJoined)?;
            require_joined(&room)?;
            if !client
                .supports_sticky_events()
                .await
                .map_err(|_| ObserverError::UnsupportedServer)?
            {
                return Err(ObserverError::UnsupportedServer);
            }
            require_encrypted(&room).await?;
            let rtc = BaseRtcClient::new(Arc::new(SdkMatrixBackend::new(client)));
            let handle = rtc
                .room(
                    room_id.to_string(),
                    RoomOptions {
                        format: mode.into(),
                    },
                )
                .await
                .map_err(|_| ObserverError::Unavailable)?;
            handle.seeded().await;
            let roster = handle.observe(SLOT).await;
            require_joined(&room)?;
            Ok(Self {
                room,
                room_id,
                mode,
                handle,
                roster,
            })
        })
        .await
        .map_err(|_| ObserverError::TimedOut)?
    }

    /// Wait for roster changes. Cancellation drops only this wait; dropping
    /// the observer cancels the owned feed. Slot-only changes need periodic
    /// `snapshot` calls, because the roster can remain empty/unchanged.
    pub async fn changed(&mut self) -> Result<(), ObserverError> {
        self.roster
            .changed()
            .await
            .map_err(|_| ObserverError::Closed)
    }

    /// Re-fetch and resolve the encrypted room-wide slot, then cross-check the
    /// core's current roster against live, decrypted SDK sticky entries. Mixed
    /// Current/Sticky2025 rosters fail closed; normalization never chooses a
    /// compatibility mode for the caller. Transient feed/cache races also fail
    /// closed and may be checked again after a change, without joining.
    pub async fn snapshot(&mut self) -> Result<ObservedCall, ObserverError> {
        time::timeout(SNAPSHOT_TIMEOUT, self.snapshot_inner())
            .await
            .map_err(|_| ObserverError::TimedOut)?
    }

    async fn snapshot_inner(&mut self) -> Result<ObservedCall, ObserverError> {
        require_joined(&self.room)?;
        require_encrypted(&self.room).await?;
        // Pinned call-sdk::open_slot sends this exact core slot unchanged as
        // the state key. Sticky2025 changes the membership/token slot only;
        // neither that writer nor core state ingestion uppercases this key.
        let request = get_state_event_for_key::v3::Request::new(
            self.room_id.clone(),
            StateEventType::from(wire_event_type("m.rtc.slot")),
            SLOT.to_owned(),
        );
        let response = self
            .room
            .client()
            .send(request)
            .with_request_config(
                RequestConfig::new()
                    .retry_limit(0)
                    .timeout(SNAPSHOT_TIMEOUT),
            )
            .await
            .map_err(|_| ObserverError::SlotUnavailable)?;
        validate_slot(self.room_id.as_str(), response.event_or_content.get())?;
        require_joined(&self.room)?;
        // No await after taking either snapshot: a feed refresh racing the two
        // snapshots is rejected by exact current event identity below.
        let roster = self.roster.borrow_and_update();
        if roster.len() > MAX_PARTICIPANTS {
            return Err(ObserverError::TooManyParticipants);
        }
        let live = self.room.sticky_events().live();
        if live.len() > MAX_LIVE_EVENTS {
            return Err(ObserverError::TooManyParticipants);
        }
        let mut seen = BTreeSet::new();
        let mut participants = Vec::with_capacity(roster.len());
        for member in roster.iter() {
            let participant = participant(member, self.room_id.as_str())?;
            if !seen.insert((
                participant.user_id.clone(),
                participant.membership_id.clone(),
            )) {
                return Err(ObserverError::InvalidMembership);
            }
            let mut matches = 0;
            for entry in &live {
                if entry.event_id.as_str() != participant.membership_event_id {
                    continue;
                }
                if !matches!(
                    entry.key.event_type.to_string().as_str(),
                    "m.rtc.member" | "org.matrix.msc4143.rtc.member"
                ) || entry.raw().json().get().len() > MAX_EVENT_BYTES
                {
                    return Err(ObserverError::InvalidMembership);
                }
                let metadata = entry
                    .encryption_info()
                    .ok_or(ObserverError::InvalidMembership)?;
                if metadata.sender.as_str() != participant.user_id
                    || entry.key.sender != metadata.sender
                    || metadata.sender_device.as_deref().map(|id| id.as_str())
                        != Some(participant.device_id.as_str())
                    || entry.key.sticky_key != participant.membership_id
                {
                    return Err(ObserverError::InvalidMembership);
                }
                let content = entry
                    .raw()
                    .get_field::<Value>("content")
                    .ok()
                    .flatten()
                    .ok_or(ObserverError::InvalidMembership)?;
                validate_content(&content, &participant, self.mode)?;
                matches += 1;
            }
            if matches != 1 {
                return Err(ObserverError::InvalidMembership);
            }
            participants.push(participant);
        }
        Ok(ObservedCall {
            room_id: self.room_id.to_string(),
            slot_id: SLOT.to_owned(),
            mode: self.mode,
            participants,
        })
    }
}

impl Drop for PassiveCallObserver {
    fn drop(&mut self) {
        // Explicitly stop callbacks before dropping the receiver and SDK room.
        self.handle.detach();
    }
}

fn require_joined(room: &Room) -> Result<(), ObserverError> {
    if room.state() == RoomState::Joined {
        Ok(())
    } else {
        Err(ObserverError::NotJoined)
    }
}

async fn require_encrypted(room: &Room) -> Result<(), ObserverError> {
    if room
        .latest_encryption_state()
        .await
        .map_err(|_| ObserverError::NotEncrypted)?
        .is_encrypted()
    {
        Ok(())
    } else {
        Err(ObserverError::NotEncrypted)
    }
}

fn wire_slot(mode: InteropMode) -> Result<&'static str, ObserverError> {
    match mode {
        InteropMode::Current => Ok(SLOT),
        InteropMode::Sticky2025 => Ok("m.call#ROOM"),
        InteropMode::RoomState => Err(ObserverError::UnsupportedDialect),
    }
}

fn validate_slot(room_id: &str, raw: &str) -> Result<(), ObserverError> {
    if raw.len() > MAX_EVENT_BYTES {
        return Err(ObserverError::SlotUnavailable);
    }
    let content: RawSlotEventContent =
        serde_json::from_str(raw).map_err(|_| ObserverError::SlotUnavailable)?;
    let slot = RawSlotEvent {
        room_id: room_id.to_owned(),
        slot_id: SLOT.to_owned(),
        content,
    };
    match slot.resolve(RoomEncryption::Encrypted) {
        SlotState::Open(open) if open.application_type == "m.call" => Ok(()),
        _ => Err(ObserverError::SlotUnavailable),
    }
}

fn opaque(value: &str) -> bool {
    !value.is_empty() && value.len() <= 255 && !value.chars().any(char::is_control)
}

fn participant(
    member: &JoinedMembership,
    room_id: &str,
) -> Result<ObservedParticipant, ObserverError> {
    let EventOrigin::Encrypted {
        sender_device_id: Some(device),
    } = &member.origin
    else {
        return Err(ObserverError::InvalidMembership);
    };
    let event = member
        .membership_event_id
        .as_deref()
        .ok_or(ObserverError::InvalidMembership)?;
    if member.room_id != room_id
        || member.slot_id != SLOT
        || member.application.application_type() != Some("m.call")
        || member.sticky_key != member.member_id
        || !opaque(&member.member_id)
        || !opaque(device)
        || member.sender.len() > 255
        || event.len() > 1024
        || UserId::parse(&member.sender).is_err()
        || EventId::parse(event).is_err()
    {
        return Err(ObserverError::InvalidMembership);
    }
    Ok(ObservedParticipant {
        user_id: member.sender.clone(),
        device_id: device.clone(),
        membership_id: member.member_id.clone(),
        membership_event_id: event.to_owned(),
    })
}

fn validate_content(
    content: &Value,
    member: &ObservedParticipant,
    mode: InteropMode,
) -> Result<(), ObserverError> {
    if content.get("slot_id").and_then(Value::as_str) != Some(wire_slot(mode)?)
        || content.pointer("/application/type").and_then(Value::as_str) != Some("m.call")
        || content.pointer("/member/id").and_then(Value::as_str)
            != Some(member.membership_id.as_str())
    {
        return Err(ObserverError::InvalidMembership);
    }
    let membership = content.pointer("/member/membership");
    if !(membership.and_then(Value::as_str) == Some("join")
        || (mode == InteropMode::Sticky2025 && membership.is_none()))
    {
        return Err(ObserverError::InvalidMembership);
    }
    let keys = [content.get("sticky_key"), content.get("msc4354_sticky_key")];
    if keys.iter().all(Option::is_none)
        || keys
            .into_iter()
            .flatten()
            .any(|key| key.as_str() != Some(member.membership_id.as_str()))
    {
        return Err(ObserverError::InvalidMembership);
    }
    for (path, expected) in [
        ("/member/user_id", member.user_id.as_str()),
        ("/member/device_id", member.device_id.as_str()),
    ] {
        if content
            .pointer(path)
            .is_some_and(|value| value.as_str() != Some(expected))
        {
            return Err(ObserverError::InvalidMembership);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn member() -> ObservedParticipant {
        ObservedParticipant {
            user_id: "@observer:example.org".into(),
            device_id: "DEVICE".into(),
            membership_id: "logical".into(),
            membership_event_id: "$refresh".into(),
        }
    }

    fn content() -> Value {
        json!({"slot_id":"m.call#room", "application":{"type":"m.call"},
            "member":{"id":"logical", "membership":"join"}, "sticky_key":"logical"})
    }

    #[test]
    fn slot_requires_explicit_open_known_encryption_and_call_application() {
        for encryption in ["m.per_member", "org.matrix.msc4143.per_member"] {
            let slot = json!({"status":"open", "application":{"type":"m.call"},
                "encryption":{"type":encryption}});
            assert_eq!(
                validate_slot("!room:example.org", &slot.to_string()),
                Ok(())
            );
        }
        for slot in [
            json!({}),
            json!({"status":"open", "application":{"type":"m.call"}}),
            json!({"status":"closed", "application":{"type":"m.call"}, "encryption":{"type":"m.per_member"}}),
            json!({"status":"open", "application":{"type":"other"}, "encryption":{"type":"m.per_member"}}),
            json!({"status":"open", "application":{"type":"m.call"}, "encryption":{"type":"other"}}),
        ] {
            assert_eq!(
                validate_slot("!room:example.org", &slot.to_string()),
                Err(ObserverError::SlotUnavailable)
            );
        }
    }

    #[test]
    fn selected_dialect_never_accepts_the_other_wire_slot_or_legacy() {
        let mut value = content();
        assert_eq!(
            validate_content(&value, &member(), InteropMode::Current),
            Ok(())
        );
        assert!(validate_content(&value, &member(), InteropMode::Sticky2025).is_err());
        value["slot_id"] = json!("m.call#ROOM");
        value["member"]
            .as_object_mut()
            .unwrap()
            .remove("membership");
        assert_eq!(
            validate_content(&value, &member(), InteropMode::Sticky2025),
            Ok(())
        );
        assert!(validate_content(&value, &member(), InteropMode::Current).is_err());
        assert!(wire_slot(InteropMode::RoomState).is_err());
    }

    #[test]
    fn membership_claims_and_dual_keys_cannot_conflict() {
        for (pointer, replacement) in [
            ("/member/id", "other"),
            ("/member/membership", "leave"),
            ("/sticky_key", "other"),
        ] {
            let mut value = content();
            *value.pointer_mut(pointer).unwrap() = json!(replacement);
            assert!(validate_content(&value, &member(), InteropMode::Current).is_err());
        }
        for field in ["user_id", "device_id"] {
            let mut value = content();
            value["member"][field] = json!("other");
            assert!(validate_content(&value, &member(), InteropMode::Current).is_err());
        }
        let mut value = content();
        value["msc4354_sticky_key"] = json!("other");
        assert!(validate_content(&value, &member(), InteropMode::Current).is_err());
    }

    #[test]
    fn roster_requires_actual_encrypted_device_and_current_refresh_identity() {
        let mut joined = JoinedMembership {
            room_id: "!room:example.org".into(),
            slot_id: SLOT.into(),
            sender: "@observer:example.org".into(),
            origin: EventOrigin::encrypted(Some("DEVICE".into())),
            sticky_key: "logical".into(),
            member_id: "logical".into(),
            membership_event_id: Some("$first".into()),
            membership_ts: None,
            origin_server_ts: None,
            application: "m.call".into(),
            transports: vec![],
            can_subscribe: vec![],
        };
        let first = participant(&joined, "!room:example.org").unwrap();
        joined.membership_event_id = Some("$refresh".into());
        let refresh = participant(&joined, "!room:example.org").unwrap();
        assert_eq!(first.membership_id, refresh.membership_id);
        assert_ne!(first.membership_event_id, refresh.membership_event_id);
        for origin in [
            EventOrigin::Unknown,
            EventOrigin::Cleartext,
            EventOrigin::claimed("DEVICE"),
            EventOrigin::encrypted(None),
        ] {
            joined.origin = origin;
            assert_eq!(
                participant(&joined, "!room:example.org"),
                Err(ObserverError::InvalidMembership)
            );
        }
        joined.origin = EventOrigin::encrypted(Some("DEVICE".into()));
        joined.membership_event_id = None;
        assert!(participant(&joined, "!room:example.org").is_err());
        joined.membership_event_id = Some("$refresh".into());
        assert!(participant(&joined, "!other:example.org").is_err());
        joined.sticky_key = "other".into();
        assert!(participant(&joined, "!room:example.org").is_err());
    }

    #[test]
    fn debug_redacts_identity_and_opaque_ids_are_bounded() {
        let member = member();
        assert_eq!(format!("{member:?}"), "ObservedParticipant { .. }");
        assert!(!opaque(""));
        assert!(!opaque("line\nnext"));
        assert!(!opaque(&"a".repeat(256)));
        assert!(opaque("logical"));
        assert!(validate_slot("!room:example.org", &" ".repeat(MAX_EVENT_BYTES + 1)).is_err());
    }
}
