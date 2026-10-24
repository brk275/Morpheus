//! Negative format-conflict admission for an explicit outgoing call.
//!
//! This is not protocol negotiation, an incoming-call authorization, or a
//! source of transport URLs. Only the original client's own homeserver state
//! and its live, decrypted sticky map enter the check. The normalizers have no
//! backend and never join, spawn a feed, request OpenID, or open a device.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    time::Duration,
};

use matrix_rtc_call_sdk::SdkMatrixBackend;
use matrix_rtc_core::{
    BaseRtcRoom, EventOrigin, JoinedMembership, RawSlotEvent, RawSlotEventContent, RawStickyEvent,
    RawStickyEventContent, RoomEncryption, SLOT_EVENT_TYPE, SlotState,
    compat::{
        STATE_MEMBER_EVENT_TYPE, StateMemberEvent,
        room_state::{now_ms, translate_state_memberships},
        sticky_2025::{MemberContent, normalize_member_content},
    },
    wire_event_type,
};
use matrix_sdk::{
    Client, Room, RoomState,
    config::RequestConfig,
    ruma::{EventId, UserId, api::client::state::get_state_events},
};
use serde_json::Value;

use crate::{InteropMode, RtcError};

const SLOT: &str = "m.call#room";
const MAX_MEMBERS: usize = 128;
const MAX_STICKY_EVENTS: usize = 512;
const MAX_STATE_EVENTS: usize = 8192;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
// Pinned core compat/room_state.rs DEFAULT_EXPIRES_MS, also used by its
// outbound refresh. A refresh keeps created_ts and grows expires; it is the
// effective deadline relative to this event's origin, not expires itself,
// that is bounded below. This does not authenticate a claimed legacy device.
const LEGACY_LIFETIME_MS: u64 = 4 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OutgoingCallError {
    #[error("the call's original account or room is no longer available")]
    Revoked,
    #[error("the current call format could not be checked; nothing was joined")]
    Unavailable,
    #[error("the current call evidence exceeds the supported limit; nothing was joined")]
    Limit,
    #[error("active call evidence changed; try the call again")]
    Changed,
    #[error(
        "this room has mixed active call formats; wait for those calls to end before starting another"
    )]
    Mixed,
    #[error("this room's active call uses Standard; choose Standard in this room's call stage")]
    Current,
    #[error(
        "this room's active call uses Element Call Matrix 2.0; choose that mode in this room's call stage"
    )]
    Sticky2025,
    #[error("this room's active call uses Legacy; choose Legacy in this room's call stage")]
    RoomState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StickyEvidence {
    event_id: String,
    sender: String,
    device: String,
    key: String,
    event_type: String,
    origin_server_ts: u64,
    content: Value,
    mode: InteropMode,
}

#[derive(Clone, Debug)]
struct LegacyEvidence {
    event_id: String,
    sender: String,
    state_key: String,
    origin_server_ts: u64,
    content: Value,
}

#[derive(Default)]
struct StateEvidence {
    joined: BTreeSet<String>,
    legacy: Vec<LegacyEvidence>,
    stable_slot: Option<Value>,
    unstable_slot: Option<Value>,
    stable_sticky_slot: Option<Value>,
    unstable_sticky_slot: Option<Value>,
}

/// Check only the ordinary outgoing handoff. `admit` must be the caller's
/// ORIGINAL account/room operation authority, not a lookup by account ID.
/// Incoming Answer and reconnect use their existing, separate authorization.
///
/// The bounded authenticated state read can race remote changes after its
/// snapshot. This check neither locks a distributed room nor chooses a mode.
pub(crate) async fn check(
    client: &Client,
    room: &Room,
    selected: InteropMode,
    encrypted: bool,
    admit: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(), OutgoingCallError> {
    admitted(client, room, admit)?;
    let request = get_state_events::v3::Request::new(room.room_id().to_owned());
    let response = client
        .send(request)
        .with_request_config(
            RequestConfig::new()
                .retry_limit(0)
                .timeout(READ_TIMEOUT)
                .max_response_bytes(MAX_STATE_BYTES),
        )
        .await
        .map_err(|_| OutgoingCallError::Unavailable)?;
    admitted(client, room, admit)?;
    if response.room_state.len() > MAX_STATE_EVENTS {
        return Err(OutgoingCallError::Limit);
    }
    let now = now_ms();
    let mut state = StateEvidence::default();
    for raw in response.room_state {
        // Invalid unrelated state cannot choose or veto a format. The whole
        // response is already bounded while streaming, before parsing.
        if raw.json().get().len() > MAX_EVENT_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(raw.json().get()) else {
            continue;
        };
        if value
            .get("room_id")
            .is_some_and(|id| id.as_str() != Some(room.room_id().as_str()))
        {
            continue;
        }
        state.add(&value, now);
    }
    let sticky = sticky_snapshot(room)?;
    let modes = normalized_modes(room.room_id().as_str(), encrypted, &state, &sticky, now).await?;
    // No network/backend work occurs in normalization. Still refuse a local
    // sticky refresh/expiry racing the read rather than reusing old evidence.
    admitted(client, room, admit)?;
    if sticky_snapshot(room)? != sticky {
        return Err(OutgoingCallError::Changed);
    }
    choose(selected, &modes)
}

fn admitted(
    client: &Client,
    room: &Room,
    admit: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(), OutgoingCallError> {
    if !admit()
        || client.user_id().is_none()
        || client.device_id().is_none()
        || room.state() != RoomState::Joined
        || room.client().user_id() != client.user_id()
        || room.client().device_id() != client.device_id()
    {
        return Err(OutgoingCallError::Revoked);
    }
    Ok(())
}

impl StateEvidence {
    fn add(&mut self, event: &Value, now: u64) {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return;
        };
        let Some(key) = event.get("state_key").and_then(Value::as_str) else {
            return;
        };
        let Some(content) = event.get("content") else {
            return;
        };
        if kind == "m.room.member"
            && content.get("membership").and_then(Value::as_str) == Some("join")
            && UserId::parse(key).is_ok()
        {
            self.joined.insert(key.to_owned());
        } else if key == SLOT && kind == SLOT_EVENT_TYPE {
            self.stable_slot = Some(content.clone());
        } else if key == SLOT && kind == wire_event_type(SLOT_EVENT_TYPE) {
            self.unstable_slot = Some(content.clone());
        } else if key == "m.call#ROOM" && kind == SLOT_EVENT_TYPE {
            self.stable_sticky_slot = Some(content.clone());
        } else if key == "m.call#ROOM" && kind == wire_event_type(SLOT_EVENT_TYPE) {
            self.unstable_sticky_slot = Some(content.clone());
        } else if kind == STATE_MEMBER_EVENT_TYPE {
            if let Some(evidence) = legacy_evidence(event, now) {
                self.legacy.push(evidence);
            }
        }
    }
}

fn opaque(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

fn legacy_evidence(event: &Value, now: u64) -> Option<LegacyEvidence> {
    let event_id = event.get("event_id")?.as_str()?;
    let sender = event.get("sender")?.as_str()?;
    let state_key = event.get("state_key")?.as_str()?;
    let origin = event.get("origin_server_ts")?.as_u64()?;
    let content = event.get("content")?;
    let device = content.get("device_id")?.as_str()?;
    if EventId::parse(event_id).is_err()
        || event_id.len() > 1024
        || UserId::parse(sender).is_err()
        || !opaque(device)
        || origin == 0
        || origin > now
        || content.get("memberships").is_some()
        || content
            .get("membershipID")
            .is_some_and(|id| id.as_str().is_none_or(|id| !opaque(id)))
        || content.get("application")?.as_str()? != "m.call"
        || content
            .get("call_id")
            .is_some_and(|id| id.as_str() != Some(""))
        || content
            .get("scope")
            .is_some_and(|scope| scope.as_str() != Some("m.room"))
        || content
            .get("user_id")
            .is_some_and(|user| user.as_str() != Some(sender))
    {
        return None;
    }
    let expected_key = format!("{sender}_{device}_m.call");
    if state_key != expected_key && state_key.strip_prefix('_') != Some(expected_key.as_str()) {
        return None;
    }
    let created = match content.get("created_ts") {
        Some(value) => value.as_u64()?,
        None => origin,
    }
    .min(origin);
    let expires = match content.get("expires") {
        Some(value) => value.as_u64()?,
        None => LEGACY_LIFETIME_MS,
    };
    let deadline = created.checked_add(expires)?;
    // Outbound refresh: expires = now-created_ts + lifetime. Do not reject a
    // valid refreshed event merely because its raw expires exceeds four hours.
    if deadline <= now || deadline > origin.checked_add(LEGACY_LIFETIME_MS)? {
        return None;
    }
    Some(LegacyEvidence {
        event_id: event_id.to_owned(),
        sender: sender.to_owned(),
        state_key: state_key.to_owned(),
        origin_server_ts: origin,
        content: content.clone(),
    })
}

fn sticky_snapshot(room: &Room) -> Result<BTreeMap<String, StickyEvidence>, OutgoingCallError> {
    let live = room.sticky_events().live();
    if live.len() > MAX_STICKY_EVENTS {
        return Err(OutgoingCallError::Limit);
    }
    let mut result = BTreeMap::new();
    for entry in live {
        let kind = entry.key.event_type.to_string();
        if !matches!(
            kind.as_str(),
            "m.rtc.member" | "org.matrix.msc4143.rtc.member"
        ) || entry.raw().json().get().len() > MAX_EVENT_BYTES
        {
            continue;
        }
        let Some(metadata) = entry.encryption_info() else {
            continue;
        };
        let Some(device) = metadata.sender_device.as_deref() else {
            continue;
        };
        if entry.key.sender != metadata.sender {
            continue;
        }
        let Ok(Some(content)) = entry.raw().get_field::<Value>("content") else {
            continue;
        };
        let sender = entry.key.sender.as_str();
        let device = device.as_str();
        let key = entry.key.sticky_key.as_str();
        let Some(mode) = sticky_mode(&content, sender, device, key) else {
            continue;
        };
        let event_id = entry.event_id.to_string();
        if event_id.len() > 1024 || EventId::parse(&event_id).is_err() {
            continue;
        }
        let evidence = StickyEvidence {
            event_id: event_id.clone(),
            sender: sender.to_owned(),
            device: device.to_owned(),
            key: key.to_owned(),
            event_type: kind,
            origin_server_ts: entry
                .raw()
                .get_field("origin_server_ts")
                .ok()
                .flatten()
                .unwrap_or(0),
            content,
            mode,
        };
        if result.insert(event_id, evidence).is_some() {
            return Err(OutgoingCallError::Changed);
        }
    }
    Ok(result)
}

fn sticky_mode(content: &Value, sender: &str, device: &str, key: &str) -> Option<InteropMode> {
    if UserId::parse(sender).is_err()
        || !opaque(device)
        || !opaque(key)
        || content.pointer("/application/type")?.as_str()? != "m.call"
        || content.pointer("/member/id")?.as_str()? != key
    {
        return None;
    }
    let mode = match content.get("slot_id")?.as_str()? {
        SLOT => InteropMode::Current,
        "m.call#ROOM" => InteropMode::Sticky2025,
        _ => return None,
    };
    if mode == InteropMode::Sticky2025
        && (content.pointer("/member/user_id").and_then(Value::as_str) != Some(sender)
            || content.pointer("/member/device_id").and_then(Value::as_str) != Some(device))
    {
        return None;
    }
    let membership = content.pointer("/member/membership");
    if membership.and_then(Value::as_str) != Some("join")
        && !(mode == InteropMode::Sticky2025 && membership.is_none())
    {
        return None;
    }
    let keys = [content.get("sticky_key"), content.get("msc4354_sticky_key")];
    if keys.iter().all(Option::is_none)
        || keys
            .into_iter()
            .flatten()
            .any(|value| value.as_str() != Some(key))
    {
        return None;
    }
    for (path, expected) in [("/member/user_id", sender), ("/member/device_id", device)] {
        if content
            .pointer(path)
            .is_some_and(|value| value.as_str() != Some(expected))
        {
            return None;
        }
    }
    Some(mode)
}

async fn normalized_modes(
    room_id: &str,
    encrypted: bool,
    state: &StateEvidence,
    sticky: &BTreeMap<String, StickyEvidence>,
    now: u64,
) -> Result<Vec<InteropMode>, OutgoingCallError> {
    // No backend is installed: these objects cannot send or spawn a room
    // feeder. Without own participation, set_current_state only projects the
    // received set; there are no encryption timers or key sends to settle.
    let mut current = BaseRtcRoom::<SdkMatrixBackend>::new(room_id);
    current.on_encryption_received(encrypted).await;
    current
        .on_members_received(state.joined.iter().cloned())
        .await;
    // The projection has no invented slot. Current requires its real lower
    // slot to resolve open; stock Sticky uses its original upper slot and can
    // legitimately omit that state. It must not vanish merely because there
    // is no Morpheus lower-case slot (or that separate slot is closed).
    let events = sticky
        .values()
        .filter(|entry| {
            state.joined.contains(&entry.sender)
                && slot_allows(state, entry.mode, encrypted, room_id)
        })
        .filter_map(|entry| {
            let mut content = entry.content.clone();
            if normalize_member_content(&mut content) == MemberContent::BareLeave {
                return None;
            }
            let content = serde_json::from_value::<RawStickyEventContent>(content).ok()?;
            valid_event(RawStickyEvent {
                room_id: room_id.to_owned(),
                event_id: Some(entry.event_id.clone()),
                sender: entry.sender.clone(),
                origin: EventOrigin::encrypted(Some(entry.device.clone())),
                origin_server_ts: Some(entry.origin_server_ts),
                event_type: entry.event_type.clone(),
                content,
            })
        })
        .collect::<Vec<_>>();
    if events.len() > MAX_MEMBERS {
        return Err(OutgoingCallError::Limit);
    }
    current
        .set_current_sticky_state(events)
        .await
        .map_err(|_| OutgoingCallError::Unavailable)?;
    let mut modes = Vec::new();
    let current_roster = current.observe_slot(SLOT);
    for member in current_roster.borrow().iter() {
        let Some(evidence) = member
            .membership_event_id
            .as_ref()
            .and_then(|id| sticky.get(id))
        else {
            return Err(OutgoingCallError::Changed);
        };
        if !same_member(member, room_id, &evidence.sender, &evidence.key)
            || member.origin != EventOrigin::encrypted(Some(evidence.device.clone()))
        {
            return Err(OutgoingCallError::Changed);
        }
        if !modes.contains(&evidence.mode) {
            modes.push(evidence.mode);
        }
    }
    let mut legacy = BaseRtcRoom::<SdkMatrixBackend>::new(room_id);
    legacy.on_encryption_received(encrypted).await;
    legacy
        .on_members_received(state.joined.iter().cloned())
        .await;
    let events = state
        .legacy
        .iter()
        .filter(|entry| state.joined.contains(&entry.sender))
        .map(|entry| StateMemberEvent {
            event_id: Some(entry.event_id.clone()),
            sender: entry.sender.clone(),
            state_key: entry.state_key.clone(),
            origin_server_ts: entry.origin_server_ts,
            content: entry.content.clone(),
        })
        .collect::<Vec<_>>();
    if events.len() > MAX_MEMBERS {
        return Err(OutgoingCallError::Limit);
    }
    // Use the pinned batch translation (including focus/expiry rules) but do
    // fallible typed decoding here: malformed peers must not fail the whole
    // batch or pass arbitrary serde/native text into diagnostics.
    let events = translate_state_memberships(&events, now)
        .into_iter()
        .filter_map(|entry| {
            let content = serde_json::from_value::<RawStickyEventContent>(entry.content).ok()?;
            valid_event(RawStickyEvent {
                room_id: room_id.to_owned(),
                event_id: entry.event_id,
                sender: entry.sender,
                origin: EventOrigin::claimed(entry.claimed_device_id),
                origin_server_ts: None,
                event_type: "m.rtc.member".to_owned(),
                content,
            })
        });
    legacy
        .set_current_sticky_state(events)
        .await
        .map_err(|_| OutgoingCallError::Unavailable)?;
    let roster = legacy.observe_slot(SLOT);
    if roster.borrow().len() + current_roster.borrow().len() > MAX_MEMBERS {
        return Err(OutgoingCallError::Limit);
    }
    for member in roster.borrow().iter() {
        let Some(evidence) = state
            .legacy
            .iter()
            .find(|entry| Some(entry.event_id.as_str()) == member.membership_event_id.as_deref())
        else {
            return Err(OutgoingCallError::Changed);
        };
        let device = evidence
            .content
            .get("device_id")
            .and_then(Value::as_str)
            .ok_or(OutgoingCallError::Changed)?;
        let expected = evidence
            .content
            .get("membershipID")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:{device}", evidence.sender));
        if !same_member(member, room_id, &evidence.sender, &expected)
            || member.origin != EventOrigin::claimed(device.to_owned())
        {
            return Err(OutgoingCallError::Changed);
        }
        if !modes.contains(&InteropMode::RoomState) {
            modes.push(InteropMode::RoomState);
        }
    }
    Ok(modes)
}

fn slot_allows(state: &StateEvidence, mode: InteropMode, encrypted: bool, room_id: &str) -> bool {
    match mode {
        InteropMode::Current => {
            let Some(value) = state.stable_slot.as_ref().or(state.unstable_slot.as_ref()) else {
                return false;
            };
            let Ok(content) = serde_json::from_value::<RawSlotEventContent>(value.clone()) else {
                return false;
            };
            let slot = RawSlotEvent {
                room_id: room_id.to_owned(),
                slot_id: SLOT.to_owned(),
                content,
            };
            matches!(slot.resolve(if encrypted { RoomEncryption::Encrypted } else { RoomEncryption::Unencrypted }),
                SlotState::Open(open) if open.application_type == "m.call")
        }
        InteropMode::Sticky2025 => {
            // Pinned JS isSlotClosed: absence is unspecified/allowed, but a
            // PRESENT slot is active only when explicitly open for m.call.
            // This remains negative conflict evidence, never join/Answer or
            // encryption authority. Native join enforces its own key policy.
            match state
                .stable_sticky_slot
                .as_ref()
                .or(state.unstable_sticky_slot.as_ref())
            {
                None => true,
                Some(content) => {
                    content.get("status").and_then(Value::as_str) == Some("open")
                        && content.get("application").is_some_and(Value::is_object)
                        && content.pointer("/application/type").and_then(Value::as_str)
                            == Some("m.call")
                }
            }
        }
        InteropMode::RoomState => true,
    }
}

fn valid_event(event: RawStickyEvent) -> Option<RawStickyEvent> {
    // Check per-event before the core's batch `?`: one malformed event must
    // contribute nothing rather than veto every otherwise valid candidate.
    event.clone().try_into_membership_event().ok()?;
    Some(event)
}

fn same_member(member: &JoinedMembership, room: &str, sender: &str, key: &str) -> bool {
    member.room_id == room
        && member.slot_id == SLOT
        && member.sender == sender
        && member.member_id == key
        && member.sticky_key == key
        && member.application.application_type() == Some("m.call")
}

fn choose(selected: InteropMode, modes: &[InteropMode]) -> Result<(), OutgoingCallError> {
    match modes {
        [] => Ok(()),
        [mode] if *mode == selected => Ok(()),
        [InteropMode::Current] => Err(OutgoingCallError::Current),
        [InteropMode::Sticky2025] => Err(OutgoingCallError::Sticky2025),
        [InteropMode::RoomState] => Err(OutgoingCallError::RoomState),
        _ => Err(OutgoingCallError::Mixed),
    }
}

/// Both native outgoing entry points put their actual operation behind this
/// lazy boundary. Failed evidence/cancellation never constructs that operation.
/// An operation already handed off keeps the existing owned completion policy.
pub(crate) async fn handover<T, F, Fut>(
    preflight: impl Future<Output = Result<(), OutgoingCallError>>,
    admit: &(dyn Fn() -> bool + Send + Sync),
    operation: F,
) -> Result<T, RtcError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, RtcError>>,
{
    preflight.await?;
    if !admit() {
        return Err(OutgoingCallError::Revoked.into());
    }
    operation().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ROOM: &str = "!preflight:example.org";
    const USER: &str = "@peer:example.org";
    const NOW: u64 = 10 * LEGACY_LIFETIME_MS;

    fn state() -> StateEvidence {
        let mut state = StateEvidence::default();
        state.joined.insert(USER.to_owned());
        state.stable_slot = Some(
            serde_json::to_value(RawSlotEventContent::for_open(
                "m.call".to_owned(),
                Some(matrix_rtc_core::SlotEncryption {
                    encryption_type: "m.per_member".to_owned(),
                    extra: Default::default(),
                }),
            ))
            .unwrap(),
        );
        state
    }

    fn sticky(mode: InteropMode) -> BTreeMap<String, StickyEvidence> {
        let wire_slot = if mode == InteropMode::Sticky2025 {
            "m.call#ROOM"
        } else {
            SLOT
        };
        let mut content = json!({"slot_id":wire_slot,"application":{"type":"m.call"},
            "member":{"id":"fresh","membership":"join"},"sticky_key":"fresh"});
        if mode == InteropMode::Sticky2025 {
            content["member"]["user_id"] = json!(USER);
            content["member"]["device_id"] = json!("DEVICE");
            content["versions"] = json!([]);
        }
        assert_eq!(sticky_mode(&content, USER, "DEVICE", "fresh"), Some(mode));
        BTreeMap::from([(
            "$sticky".to_owned(),
            StickyEvidence {
                event_id: "$sticky".to_owned(),
                sender: USER.to_owned(),
                device: "DEVICE".to_owned(),
                key: "fresh".to_owned(),
                event_type: "m.rtc.member".to_owned(),
                origin_server_ts: NOW - 1,
                content,
                mode,
            },
        )])
    }

    fn legacy() -> Value {
        json!({"type":STATE_MEMBER_EVENT_TYPE,"event_id":"$legacy","sender":USER,
            "state_key":format!("_{USER}_DEVICE_m.call"),"origin_server_ts":NOW-1,
            "content":{"application":"m.call","call_id":"","scope":"m.room",
                "device_id":"DEVICE","membershipID":format!("{USER}:DEVICE"),
                "created_ts":NOW-1,"expires":LEGACY_LIFETIME_MS}})
    }

    #[tokio::test]
    async fn current_original_wire_matches_live_core_members_and_refuses_mixed_formats() {
        for (mode, conflict) in [
            (InteropMode::Current, OutgoingCallError::Current),
            (InteropMode::Sticky2025, OutgoingCallError::Sticky2025),
        ] {
            let state = state();
            let evidence = sticky(mode);
            let modes = normalized_modes(ROOM, true, &state, &evidence, NOW)
                .await
                .unwrap();
            assert_eq!(modes, vec![mode]);
            assert_eq!(choose(mode, &modes), Ok(()));
            assert_eq!(choose(InteropMode::RoomState, &modes), Err(conflict));
        }
        let mut stock = state();
        stock.stable_slot = None;
        assert_eq!(
            normalized_modes(ROOM, true, &stock, &sticky(InteropMode::Sticky2025), NOW)
                .await
                .unwrap(),
            vec![InteropMode::Sticky2025]
        );
        stock.stable_sticky_slot = Some(json!({"status":"open","application":{"type":"m.call"}}));
        assert_eq!(
            normalized_modes(ROOM, true, &stock, &sticky(InteropMode::Sticky2025), NOW)
                .await
                .unwrap(),
            vec![InteropMode::Sticky2025]
        );
        for content in [
            json!({"status":"closed","application":{"type":"m.call"}}),
            json!({"status":"unknown","application":{"type":"m.call"}}),
            json!({"application":{"type":"m.call"}}),
            json!({"status":"open"}),
            json!({"status":"open","application":null}),
            json!({"status":"open","application":"m.call"}),
            json!({"status":"open","application":[]}),
            json!({"status":"open","application":{"type":"other"}}),
            json!({}),
        ] {
            stock.stable_sticky_slot = Some(content);
            assert!(
                normalized_modes(ROOM, true, &stock, &sticky(InteropMode::Sticky2025), NOW)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let mut state = state();
        state.add(&legacy(), NOW);
        let modes = normalized_modes(ROOM, true, &state, &sticky(InteropMode::Current), NOW)
            .await
            .unwrap();
        assert_eq!(
            choose(InteropMode::Current, &modes),
            Err(OutgoingCallError::Mixed)
        );
        assert_eq!(
            choose(InteropMode::RoomState, &modes),
            Err(OutgoingCallError::Mixed)
        );
        let modes = normalized_modes(ROOM, true, &state, &BTreeMap::new(), NOW)
            .await
            .unwrap();
        assert_eq!(modes, vec![InteropMode::RoomState]);
        assert_eq!(choose(InteropMode::RoomState, &modes), Ok(()));
        assert_eq!(
            choose(InteropMode::Current, &modes),
            Err(OutgoingCallError::RoomState)
        );
    }

    #[tokio::test]
    async fn stale_malformed_unjoined_or_closed_evidence_cannot_choose_or_block_a_format() {
        let mut departed = state();
        departed.joined.clear();
        departed.add(&legacy(), NOW);
        assert!(
            normalized_modes(ROOM, true, &departed, &sticky(InteropMode::Sticky2025), NOW)
                .await
                .unwrap()
                .is_empty()
        );
        let mut closed = state();
        // Stable closed/invalid content remains authoritative over an unstable
        // open alias, as in the pinned feeder. No fresh call member is active.
        closed.unstable_slot = closed.stable_slot.clone();
        closed.stable_slot = Some(json!({}));
        assert!(
            normalized_modes(ROOM, true, &closed, &sticky(InteropMode::Current), NOW)
                .await
                .unwrap()
                .is_empty()
        );
        let mut bad = sticky(InteropMode::Sticky2025);
        bad.get_mut("$sticky").unwrap().content["transports"] = json!(false);
        // Typed parsing refuses this one event before the whole core batch.
        assert!(
            normalized_modes(ROOM, true, &state(), &bad, NOW)
                .await
                .unwrap()
                .is_empty()
        );
        let original = sticky(InteropMode::Current)
            .remove("$sticky")
            .unwrap()
            .content;
        let mut wrong = original.clone();
        wrong["member"]["device_id"] = json!("OTHER");
        assert_eq!(sticky_mode(&wrong, USER, "DEVICE", "fresh"), None);
        let mut wrong = original.clone();
        wrong["msc4354_sticky_key"] = json!("different");
        assert_eq!(sticky_mode(&wrong, USER, "DEVICE", "fresh"), None);
        let mut wrong = original;
        wrong["member"]["membership"] = json!("leave");
        assert_eq!(sticky_mode(&wrong, USER, "DEVICE", "fresh"), None);
        assert_eq!(choose(InteropMode::Current, &[]), Ok(()));
    }

    #[test]
    fn legacy_deadlines_use_current_refresh_origin_without_trusting_claimed_devices() {
        let original = legacy();
        assert!(legacy_evidence(&original, NOW).is_some());
        let mut refreshed = original.clone();
        refreshed["content"]["created_ts"] = json!(NOW - 2 * LEGACY_LIFETIME_MS);
        refreshed["content"]["expires"] = json!(3 * LEGACY_LIFETIME_MS - 1);
        assert!(legacy_evidence(&refreshed, NOW).is_some());
        let mut omitted = original.clone();
        omitted["content"]
            .as_object_mut()
            .unwrap()
            .remove("created_ts");
        omitted["content"]
            .as_object_mut()
            .unwrap()
            .remove("expires");
        assert!(legacy_evidence(&omitted, NOW).is_some());
        // A future content created_ts is clamped to the authenticated event
        // origin, exactly as the core does; a future event origin is refused.
        let mut clamped = original.clone();
        clamped["content"]["created_ts"] = json!(u64::MAX);
        assert!(legacy_evidence(&clamped, NOW).is_some());
        for (path, value) in [
            ("/origin_server_ts", json!(NOW + 1)),
            ("/content/expires", json!(1)),
            ("/content/expires", json!(LEGACY_LIFETIME_MS + 1)),
            ("/content/expires", json!(u64::MAX)),
            ("/content/created_ts", json!("not a timestamp")),
            ("/state_key", json!("_@other:example.org_DEVICE_m.call")),
            ("/content/user_id", json!("@other:example.org")),
            ("/content/device_id", json!("different")),
            ("/content/call_id", json!("another-slot")),
        ] {
            let mut bad = original.clone();
            // user_id is optional, so create it explicitly for that control.
            if path == "/content/user_id" {
                bad["content"]["user_id"] = value;
            } else {
                *bad.pointer_mut(path).unwrap() = value;
            }
            assert!(legacy_evidence(&bad, NOW).is_none(), "{path}");
        }
        let mut overflow = original;
        overflow["origin_server_ts"] = json!(u64::MAX - 1);
        overflow["content"]["created_ts"] = json!(u64::MAX - 1);
        assert!(legacy_evidence(&overflow, u64::MAX - 1).is_none());
    }

    #[tokio::test]
    async fn actual_lazy_handover_refuses_conflict_and_revocation_before_any_operation() {
        let entered = AtomicUsize::new(0);
        let live = AtomicBool::new(true);
        let evidence = sticky(InteropMode::Sticky2025);
        let state = state();
        let conflict = async {
            let modes = normalized_modes(ROOM, true, &state, &evidence, NOW).await?;
            choose(InteropMode::Current, &modes)
        };
        let result = handover(conflict, &|| live.load(Ordering::SeqCst), || async {
            entered.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(
            result,
            Err(RtcError::OutgoingPreflight(OutgoingCallError::Sticky2025))
        ));
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        // Revocation after the awaited preflight but before lazy operation
        // creation is the same production boundary used by slot/join callers.
        let result = handover(
            async {
                live.store(false, Ordering::SeqCst);
                Ok(())
            },
            &|| live.load(Ordering::SeqCst),
            || async {
                entered.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(RtcError::OutgoingPreflight(OutgoingCallError::Revoked))
        ));
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        live.store(true, Ordering::SeqCst);
        let result = handover(
            async {
                let modes = normalized_modes(ROOM, true, &state, &evidence, NOW).await?;
                choose(InteropMode::Sticky2025, &modes)
            },
            &|| live.load(Ordering::SeqCst),
            || async {
                entered.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(entered.load(Ordering::SeqCst), 1);
    }
}
