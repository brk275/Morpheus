//! Incoming invitation admission, without joining or opening media devices.
//!
//! Hosted live tests exercise admission. The private controller/worker/UI wiring
//! adds bounded discovery and single-use Answer ownership, but its runtime
//! acceptance remains pending. An admitted offer is a snapshot, not permission
//! to capture: account/receipt/trust/policy checks and exact own-join commitment
//! precede media. One replay ledger survives account switches for this process.

mod audio;
pub(crate) mod controller;
pub(crate) mod ledger;
mod policy;
pub(crate) mod ui;
pub(crate) mod worker;

use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use matrix_sdk::{
    Client, Room, RoomState,
    config::RequestConfig,
    deserialized_responses::{EncryptionInfo, VerificationState},
    ruma::{EventId, RoomId, events::AnySyncTimelineEvent, serde::Raw},
};
use morpheus_matrix::{AccountManager, AccountSelection, SyncBackend, SyncStatus};
use morpheus_rtc::{
    InteropMode, PassiveCallObserver, RtcService,
    ringing::{
        CallParticipation, MAX_RING_LIFETIME_MS, ResolvedJoinedCall, RingContext, RingNotification,
        RingWindow,
    },
};
use serde_json::Value;
use tokio::{sync::broadcast, time};

use crate::{
    call_membership::{MembershipEvidence, MembershipIdentity, matches_membership},
    voice_spaces::VoiceSpaces,
};
use ledger::{Observation, OfferKey, OfferLedger};

const MAX_BYTES: usize = 64 * 1024;
const MAX_LIVE_MEMBERS: usize = 512;
const ADMISSION_LIMIT: Duration = Duration::from_secs(20);
const REQUEST_LIMIT: Duration = Duration::from_secs(5);

/// Deliberately contains no server error, event body, room or device identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionFailure {
    Account,
    Sync,
    Receipt,
    Replay,
    Expired,
    Membership,
    Trust,
    Policy,
    Quiet,
    Transport,
    Unavailable,
    TimedOut,
    Revoked,
}

struct Receipt {
    raw: Raw<AnySyncTimelineEvent>,
    encryption: Arc<EncryptionInfo>,
    ring: RingNotification,
    expires_at_ms: u64,
}

/// No public constructor: callers cannot supply a synthetic Verified flag.
struct Membership {
    user: String,
    device: String,
    logical_id: String,
    mode: InteropMode,
}

impl Membership {
    fn participation<'a>(&'a self, room: &'a str) -> CallParticipation<'a> {
        CallParticipation {
            room_id: room,
            sender: &self.user,
            logical_membership_id: &self.logical_id,
            slot_id: wire_slot(self.mode),
            application: "m.call",
            device_id: Some(&self.device),
        }
    }
}

pub(crate) struct IncomingOffer {
    selection: AccountSelection,
    client: Client,
    room: Room,
    own_user: String,
    own_device: String,
    ring: RingNotification,
    generation: u64,
    // The pinned SDK does not re-export the receipt update type. Keep its
    // receiver private in a bounded drain closure; errors permanently revoke.
    drain_receipts: Box<dyn FnMut() -> Result<(), AdmissionFailure> + Send>,
    observer: PassiveCallObserver,
    membership: Membership,
    deadline: Instant,
    revoked: bool,
}

// A caller may cancel by dropping the revalidation future while it is awaiting
// the server. That path never returns an Err, so it needs the same terminal
// revocation as an explicit failure or timeout.
struct Revalidation<'a> {
    offer: &'a mut IncomingOffer,
    succeeded: bool,
}

impl Drop for Revalidation<'_> {
    fn drop(&mut self) {
        if !self.succeeded {
            self.offer.revoked = true;
        }
    }
}

impl fmt::Debug for IncomingOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingOffer")
            .field("revoked", &self.revoked)
            .finish_non_exhaustive()
    }
}

impl IncomingOffer {
    /// Admit only an unsolicited receipt already retained by this account's
    /// running sticky sync. Fetching a notification by event ID cannot ring.
    /// The ledger claim occurs before network work and is never released on
    /// failure. No membership, key verification or media write happens here.
    pub(crate) async fn admit(
        manager: &AccountManager,
        preferences: &VoiceSpaces,
        room_id: &RoomId,
        event_id: &EventId,
        ledger: &mut OfferLedger,
    ) -> Result<Self, AdmissionFailure> {
        time::timeout(ADMISSION_LIMIT, async {
            let account = manager
                .active_account()
                .await
                .ok_or(AdmissionFailure::Account)?;
            let selection = manager
                .account_selection(&account.id)
                .ok_or(AdmissionFailure::Account)?;
            let client = manager
                .active_client()
                .await
                .map_err(|_| AdmissionFailure::Account)?;
            require_client(&client, &account.user_id, &account.device_id)?;
            let room = client.get_room(room_id).ok_or(AdmissionFailure::Account)?;
            require_scope(manager, preferences, &selection, &room)?;
            // Subscription precedes the first snapshot. Even if a later full
            // snapshot looks useful, lag/generation changes revoke this attempt.
            let mut updates = room.sticky_events().subscribe_keyless();
            let initial = room.sticky_events().keyless_live();
            if initial.invalidated {
                return Err(AdmissionFailure::Receipt);
            }
            let generation = initial.generation;
            let received = receipt(&room, event_id, generation)?;
            if received.ring.sender() == account.user_id {
                return Err(AdmissionFailure::Policy);
            }
            let observed_at = Instant::now();
            let now_ms = now_ms()?;
            // Capture semantic time before the first network await. A skewed
            // future sender timestamp must not move the deadline while we
            // fetch membership or open the passive observer. Invalid/expired
            // attempts still get a ledger tombstone, including on rollback.
            let window = received.ring.window_at(now_ms);
            let key = OfferKey::new(&account.id, room_id.as_str(), event_id.as_str());
            let observation = ledger
                .observe(
                    key,
                    RingWindow {
                        expires_at_ms: received.expires_at_ms,
                        remaining_ms: received
                            .expires_at_ms
                            .saturating_sub(now_ms)
                            .min(window.as_ref().map_or(0, |window| window.remaining_ms))
                            .min(MAX_RING_LIFETIME_MS),
                    },
                    observed_at,
                )
                .map_err(|_| AdmissionFailure::Replay)?;
            let Observation::FirstSeen { deadline } = observation else {
                return Err(AdmissionFailure::Replay);
            };
            let work = async {
                policy::require_current_sender(&client, &received.encryption)
                    .await
                    .map_err(|_| AdmissionFailure::Trust)?;
                let referenced =
                    fetch_membership(&room, &received.ring, &received.encryption).await?;
                let observer =
                    PassiveCallObserver::open(client.clone(), room_id.as_str(), referenced.mode)
                        .await
                        .map_err(|_| AdmissionFailure::Membership)?;
                let drain_receipts = Box::new(move || {
                    for _ in 0..64 {
                        match updates.try_recv() {
                            Ok(update)
                                if !update.invalidated && update.generation == generation => {}
                            Err(broadcast::error::TryRecvError::Empty) => return Ok(()),
                            _ => return Err(AdmissionFailure::Receipt),
                        }
                    }
                    Err(AdmissionFailure::Receipt)
                });
                let mut offer = Self {
                    selection,
                    client,
                    room,
                    own_user: account.user_id,
                    own_device: account.device_id,
                    ring: received.ring,
                    generation,
                    drain_receipts,
                    observer,
                    membership: referenced,
                    deadline,
                    revoked: false,
                };
                offer.validate_inner(manager, preferences).await?;
                Ok(offer)
            };
            let result = time::timeout(deadline.saturating_duration_since(Instant::now()), work)
                .await
                .unwrap_or(Err(AdmissionFailure::TimedOut));
            if result.is_err() {
                let _ = ledger.retire(key);
            }
            result
        })
        .await
        .map_err(|_| AdmissionFailure::TimedOut)?
    }

    pub(crate) fn mode(&self) -> InteropMode {
        self.membership.mode
    }
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Every failure is terminal for this offer; cached success is not reused.
    /// This method neither joins nor grants any permission to activate media.
    pub(crate) async fn revalidate(
        &mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
    ) -> Result<(), AdmissionFailure> {
        if self.revoked {
            return Err(AdmissionFailure::Revoked);
        }
        let limit = ADMISSION_LIMIT.min(self.deadline.saturating_duration_since(Instant::now()));
        let mut validation = Revalidation {
            offer: self,
            succeeded: false,
        };
        let result = time::timeout(limit, validation.offer.validate_inner(manager, preferences))
            .await
            .unwrap_or(Err(AdmissionFailure::TimedOut));
        validation.succeeded = result.is_ok();
        result
    }

    /// Separate post-join path: the controller has checked the actual RTC
    /// client's local device, participation and live call handle. This retains
    /// every receipt/caller/trust/policy check while allowing precisely that own
    /// participation, never another device or generation of our user.
    async fn revalidate_after_join(
        &mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
        own: &controller::JoinedOwn,
    ) -> Result<(), AdmissionFailure> {
        if self.revoked {
            return Err(AdmissionFailure::Revoked);
        }
        let limit = ADMISSION_LIMIT.min(self.deadline.saturating_duration_since(Instant::now()));
        let mut validation = Revalidation {
            offer: self,
            succeeded: false,
        };
        let result = time::timeout(
            limit,
            validation
                .offer
                .validate_inner_for(manager, preferences, Some(own)),
        )
        .await
        .unwrap_or(Err(AdmissionFailure::TimedOut));
        validation.succeeded = result.is_ok();
        result
    }

    fn current_receipt(&mut self) -> Result<Receipt, AdmissionFailure> {
        if self.revoked || !self.selection.is_current() {
            return Err(AdmissionFailure::Revoked);
        }
        if Instant::now() >= self.deadline {
            return Err(AdmissionFailure::Expired);
        }
        (self.drain_receipts)()?;
        let id = EventId::parse(self.ring.event_id()).map_err(|_| AdmissionFailure::Receipt)?;
        let received = receipt(&self.room, &id, self.generation)?;
        if received.ring != self.ring {
            return Err(AdmissionFailure::Receipt);
        }
        Ok(received)
    }

    async fn validate_inner(
        &mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
    ) -> Result<(), AdmissionFailure> {
        self.validate_inner_for(manager, preferences, None).await
    }

    async fn validate_inner_for(
        &mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
        own_join: Option<&controller::JoinedOwn>,
    ) -> Result<(), AdmissionFailure> {
        require_scope(manager, preferences, &self.selection, &self.room)?;
        require_client(&self.client, &self.own_user, &self.own_device)?;
        let received = self.current_receipt()?;
        let rtc = RtcService::new(self.client.clone());
        let transport = time::timeout(REQUEST_LIMIT, rtc.discover_transport())
            .await
            .map_err(|_| AdmissionFailure::Transport)?
            .map_err(|_| AdmissionFailure::Transport)?;
        if !transport.is_supported() {
            return Err(AdmissionFailure::Transport);
        }
        let marker = time::timeout(
            REQUEST_LIMIT,
            rtc.voice_space_marker(self.room.room_id().as_str()),
        )
        .await
        .map_err(|_| AdmissionFailure::Quiet)?
        .map_err(|_| AdmissionFailure::Quiet)?;
        if marker.is_some_and(|marker| marker.enabled) {
            return Err(AdmissionFailure::Quiet);
        }
        let permission =
            policy::require_event_policy(&self.room, &received.encryption.sender, &received.raw)
                .await
                .map_err(|_| AdmissionFailure::Policy)?;
        let referenced = fetch_membership(&self.room, &received.ring, &received.encryption).await?;
        if !same_join(&referenced, &self.membership) {
            return Err(AdmissionFailure::Membership);
        }
        let observed = self
            .observer
            .snapshot()
            .await
            .map_err(|_| AdmissionFailure::Membership)?;
        if observed.mode != referenced.mode
            || observed.slot_id != "m.call#room"
            || observed.room_id != self.room.room_id().as_str()
            || !controller::own_observation_allowed(
                &observed.participants,
                &self.own_user,
                &self.own_device,
                own_join,
            )
        {
            return Err(AdmissionFailure::Membership);
        }
        let mut matches = observed.participants.iter().filter(|member| {
            member.user_id == referenced.user
                && member.device_id == referenced.device
                && member.membership_id == referenced.logical_id
        });
        let participant = matches.next().ok_or(AdmissionFailure::Membership)?;
        if matches.next().is_some() {
            return Err(AdmissionFailure::Membership);
        }
        let live = self.room.sticky_events().live();
        if live.len() > MAX_LIVE_MEMBERS {
            return Err(AdmissionFailure::Membership);
        }
        let mut entries = live
            .iter()
            .filter(|entry| entry.event_id.as_str() == participant.membership_event_id);
        let current = entries.next().ok_or(AdmissionFailure::Membership)?;
        if entries.next().is_some() {
            return Err(AdmissionFailure::Membership);
        }
        let encryption = current.encryption_info().ok_or(AdmissionFailure::Trust)?;
        let membership = membership(
            current.raw(),
            encryption,
            self.room.room_id().as_str(),
            &participant.membership_event_id,
            &received.encryption,
        )?;
        if current.key.sender != encryption.sender
            || current.key.sticky_key != membership.logical_id
            || !same_join(&membership, &referenced)
        {
            return Err(AdmissionFailure::Membership);
        }
        let window = received
            .ring
            .validate_for(
                &RingContext {
                    recipient_user_id: &self.own_user,
                    room_mention_permission: permission,
                    call: ResolvedJoinedCall {
                        referenced_event_id: received.ring.membership_event_id(),
                        referenced: referenced.participation(self.room.room_id().as_str()),
                        current: membership.participation(self.room.room_id().as_str()),
                    },
                },
                now_ms()?,
            )
            .map_err(|_| AdmissionFailure::Expired)?;
        // Revalidation and clock rollback can only shorten the original bound.
        self.deadline = self.deadline.min(
            Instant::now()
                .checked_add(Duration::from_millis(window.remaining_ms))
                .ok_or(AdmissionFailure::Expired)?,
        );
        policy::require_current_sender(&self.client, &received.encryption)
            .await
            .map_err(|_| AdmissionFailure::Trust)?;
        policy::require_current_sender(&self.client, encryption)
            .await
            .map_err(|_| AdmissionFailure::Trust)?;
        // Close the network-await gap: the receipt and exactly this current
        // membership must still exist. A refresh is safe to retry as a new
        // validation, never evidence that an old snapshot remains current.
        self.current_receipt()?;
        let fresh = self.room.sticky_events().live();
        if fresh.len() > MAX_LIVE_MEMBERS
            || !fresh
                .iter()
                .any(|entry| entry.event_id == current.event_id && entry.key == current.key)
        {
            return Err(AdmissionFailure::Membership);
        }
        require_scope(manager, preferences, &self.selection, &self.room)
    }
}

fn require_scope(
    manager: &AccountManager,
    preferences: &VoiceSpaces,
    selection: &AccountSelection,
    room: &Room,
) -> Result<(), AdmissionFailure> {
    if !selection.is_current() || room.state() != RoomState::Joined {
        return Err(AdmissionFailure::Account);
    }
    if !matches!(
        *manager.subscribe_sync_status().borrow(),
        SyncStatus::Running {
            backend: SyncBackend::SlidingSticky
        }
    ) {
        return Err(AdmissionFailure::Sync);
    }
    if !preferences.notifications_enabled(selection.account_id()) {
        return Err(AdmissionFailure::Policy);
    }
    if preferences
        .local_rooms(selection.account_id())
        .contains(room.room_id().as_str())
    {
        return Err(AdmissionFailure::Quiet);
    }
    Ok(())
}

fn require_client(client: &Client, user: &str, device: &str) -> Result<(), AdmissionFailure> {
    if client.user_id().map(|id| id.as_str()) == Some(user)
        && client.device_id().map(|id| id.as_str()) == Some(device)
    {
        Ok(())
    } else {
        Err(AdmissionFailure::Account)
    }
}

fn receipt(room: &Room, event: &EventId, generation: u64) -> Result<Receipt, AdmissionFailure> {
    let snapshot = room.sticky_events().keyless_live();
    if snapshot.invalidated || snapshot.generation != generation {
        return Err(AdmissionFailure::Receipt);
    }
    let mut matches = snapshot
        .events
        .into_iter()
        .filter(|entry| entry.event_id.as_str() == event.as_str());
    let received = matches.next().ok_or(AdmissionFailure::Receipt)?;
    if matches.next().is_some() || received.raw().json().get().len() > MAX_BYTES {
        return Err(AdmissionFailure::Receipt);
    }
    let encryption = received.encryption_info().ok_or(AdmissionFailure::Trust)?;
    if encryption.verification_state != VerificationState::Verified
        || encryption.sender != received.sender
        || encryption.sender_device.is_none()
    {
        return Err(AdmissionFailure::Trust);
    }
    let envelope: Value =
        serde_json::from_str(received.raw().json().get()).map_err(|_| AdmissionFailure::Receipt)?;
    let ring = RingNotification::parse(&envelope, room.room_id().as_str())
        .map_err(|_| AdmissionFailure::Receipt)?;
    if ring.event_id() != event.as_str() || ring.sender() != received.sender.as_str() {
        return Err(AdmissionFailure::Receipt);
    }
    Ok(Receipt {
        raw: received.raw().clone(),
        encryption: Arc::clone(encryption),
        ring,
        expires_at_ms: received.expires_at.get().into(),
    })
}

async fn fetch_membership(
    room: &Room,
    ring: &RingNotification,
    notification: &EncryptionInfo,
) -> Result<Membership, AdmissionFailure> {
    let id =
        EventId::parse(ring.membership_event_id()).map_err(|_| AdmissionFailure::Membership)?;
    let event = time::timeout(
        REQUEST_LIMIT,
        room.event(
            &id,
            Some(RequestConfig::new().retry_limit(0).timeout(REQUEST_LIMIT)),
        ),
    )
    .await
    .map_err(|_| AdmissionFailure::TimedOut)?
    .map_err(|_| AdmissionFailure::Membership)?;
    let encryption = event.encryption_info().ok_or(AdmissionFailure::Trust)?;
    let member = membership(
        event.raw(),
        encryption,
        room.room_id().as_str(),
        ring.membership_event_id(),
        notification,
    )?;
    policy::require_current_sender(&room.client(), encryption)
        .await
        .map_err(|_| AdmissionFailure::Trust)?;
    Ok(member)
}

fn membership<T>(
    raw: &Raw<T>,
    encryption: &EncryptionInfo,
    room: &str,
    event: &str,
    notification: &EncryptionInfo,
) -> Result<Membership, AdmissionFailure> {
    if raw.json().get().len() > MAX_BYTES
        || encryption.verification_state != VerificationState::Verified
        || encryption.sender != notification.sender
        || encryption.sender_device != notification.sender_device
    {
        return Err(AdmissionFailure::Trust);
    }
    let device = encryption
        .sender_device
        .as_deref()
        .ok_or(AdmissionFailure::Trust)?
        .as_str();
    let envelope: Value =
        serde_json::from_str(raw.json().get()).map_err(|_| AdmissionFailure::Membership)?;
    membership_envelope(&envelope, room, event, encryption.sender.as_str(), device)
}

fn membership_envelope(
    envelope: &Value,
    room: &str,
    event: &str,
    user: &str,
    device: &str,
) -> Result<Membership, AdmissionFailure> {
    if !matches!(
        envelope.get("type").and_then(Value::as_str),
        Some("m.rtc.member" | "org.matrix.msc4143.rtc.member")
    ) || envelope.get("event_id").and_then(Value::as_str) != Some(event)
        || envelope.get("sender").and_then(Value::as_str) != Some(user)
        || envelope
            .get("room_id")
            .is_some_and(|id| id.as_str() != Some(room))
        || envelope.get("unsigned").is_some_and(|unsigned| {
            !unsigned.is_object() || unsigned.get("redacted_because").is_some()
        })
    {
        return Err(AdmissionFailure::Membership);
    }
    let content = envelope
        .get("content")
        .ok_or(AdmissionFailure::Membership)?;
    let logical_id = content
        .pointer("/member/id")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 255
                && !id.chars().any(|c| c.is_control() || c.is_whitespace())
        })
        .ok_or(AdmissionFailure::Membership)?;
    let mode = match content.get("slot_id").and_then(Value::as_str) {
        Some("m.call#room") => InteropMode::Current,
        Some("m.call#ROOM") => InteropMode::Sticky2025,
        _ => return Err(AdmissionFailure::Membership),
    };
    if !matches_membership(
        MembershipEvidence {
            sender: user,
            device: Some(device),
            verified: true,
            sticky_key: logical_id,
            content,
        },
        MembershipIdentity {
            user_id: user,
            device_id: device,
            membership_id: logical_id,
        },
        wire_slot(mode),
        mode,
    ) {
        return Err(AdmissionFailure::Membership);
    }
    Ok(Membership {
        user: user.to_owned(),
        device: device.to_owned(),
        logical_id: logical_id.to_owned(),
        mode,
    })
}

fn same_join(a: &Membership, b: &Membership) -> bool {
    a.user == b.user && a.device == b.device && a.logical_id == b.logical_id && a.mode == b.mode
}

fn wire_slot(mode: InteropMode) -> &'static str {
    match mode {
        InteropMode::Current => "m.call#room",
        InteropMode::Sticky2025 => "m.call#ROOM",
        InteropMode::RoomState => "",
    }
}

fn now_ms() -> Result<u64, AdmissionFailure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
        .ok_or(AdmissionFailure::Expired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> Value {
        json!({"type":"m.rtc.member","sender":"@caller:example.org","event_id":"$member", "content":{
            "slot_id":"m.call#room", "sticky_key":"logical", "member":{"id":"logical","membership":"join"},
            "application":{"type":"m.call"}}})
    }

    fn parse(value: &Value) -> Result<Membership, AdmissionFailure> {
        membership_envelope(
            value,
            "!room:example.org",
            "$member",
            "@caller:example.org",
            "CALLER",
        )
    }

    #[test]
    fn membership_dialect_is_exact_and_current_requires_join() {
        let mut value = event();
        assert_eq!(parse(&value).unwrap().mode, InteropMode::Current);
        value["content"]["member"]
            .as_object_mut()
            .unwrap()
            .remove("membership");
        assert!(parse(&value).is_err());
        value["content"]["slot_id"] = json!("m.call#ROOM");
        assert_eq!(parse(&value).unwrap().mode, InteropMode::Sticky2025);
        value["content"]["slot_id"] = json!("m.call#Room");
        assert!(parse(&value).is_err());
    }

    #[test]
    fn membership_rejects_cross_room_sender_key_redaction_and_leave() {
        for (pointer, value) in [
            ("/room_id", json!("!different:example.org")),
            ("/sender", json!("@other:example.org")),
            ("/event_id", json!("$other")),
            ("/unsigned", json!({"redacted_because":{}})),
        ] {
            let mut candidate = event();
            candidate[pointer.trim_start_matches('/')] = value;
            assert!(parse(&candidate).is_err());
        }
        for (field, value) in [
            ("sticky_key", json!("other")),
            ("msc4354_sticky_key", json!("other")),
        ] {
            let mut candidate = event();
            candidate["content"][field] = value;
            assert!(parse(&candidate).is_err());
        }
        for slot in ["m.call#room", "m.call#ROOM"] {
            let mut candidate = event();
            candidate["content"]["slot_id"] = json!(slot);
            candidate["content"]["member"]["membership"] = json!("leave");
            assert!(parse(&candidate).is_err());
        }
    }
}
