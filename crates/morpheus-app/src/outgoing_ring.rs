//! One explicit outgoing invitation for an already joined call.
//!
//! The worker owns this future; there are no detached tasks or retry actions.
//! A successful receipt means the homeserver accepted an event, not that a
//! recipient rang. Dropping the future after sending begins is ambiguous.
//! Incoming ringing, acknowledgement and durable outbox recovery are separate.

use std::{
    collections::BTreeSet,
    fmt,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use matrix_sdk::{
    RoomMemberships, RoomState,
    config::RequestConfig,
    deserialized_responses::VerificationState,
    room::Room,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedTransactionId, RoomId, TransactionId, UserId,
        api::client::redact::redact_event,
    },
};
use morpheus_matrix::{AccountInfo, AccountSelection};
use morpheus_rtc::{CallInfo, InteropMode, NotificationDialect, ParticipantInfo, RtcService};
use serde_json::{Value, json};
use tokio::{sync::broadcast, time};

use crate::local_video::CaptureStop;

#[cfg(test)]
mod live_tests;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const RETRACTION_TIMEOUT: Duration = Duration::from_secs(3);
const GUARD_INTERVAL: Duration = Duration::from_millis(100);
const RING_LIFETIME: Duration = Duration::from_secs(30);
// Pinned matrix-rtc-call/src/notification.rs:147–153 requires at least twice
// the ring lifetime for MSC4075 acknowledgement compatibility. This retention
// does not extend our 30-second deadline or implement acknowledgements.
const STICKY_DURATION: Duration = Duration::from_secs(60);
const MAX_TARGETS: usize = 128;
const MAX_MEMBERSHIP_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Suppression {
    Legacy,
    VoiceSpace,
    Occupied,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvitationFailure {
    AccountMismatch,
    CallUnavailable,
    NotEncrypted,
    UnsupportedServer,
    VoiceSpaceCheck,
    TargetCheck,
    RoomMentionForbidden,
    NoTargets,
    TooManyTargets,
    MembershipUnconfirmed,
    TimedOut,
    Clock,
}

#[derive(Debug)]
pub(crate) enum InvitationOutcome {
    Sent(InvitationReceipt),
    Suppressed(Suppression),
    Failed(InvitationFailure),
    /// No retry is offered: the original transaction may have succeeded.
    Unconfirmed,
}

impl InvitationOutcome {
    pub(crate) fn status_text(&self) -> &'static str {
        match self {
            Self::Sent(_) => "Invitation sent. Recipients may ring for 30 seconds.",
            Self::Suppressed(Suppression::Legacy) => {
                "Joined quietly. Invitations are unavailable in Legacy mode."
            }
            Self::Suppressed(Suppression::VoiceSpace) => "Joined the voice space quietly.",
            Self::Suppressed(Suppression::Occupied) => {
                "Joined quietly: the call already has participants."
            }
            Self::Suppressed(Suppression::Cancelled) => "Invitation cancelled before sending.",
            Self::Failed(InvitationFailure::RoomMentionForbidden) => {
                "Joined, but you do not have permission to notify this room."
            }
            Self::Failed(InvitationFailure::NoTargets) => {
                "Joined, but this direct chat has no other joined members to notify."
            }
            Self::Failed(InvitationFailure::TooManyTargets) => {
                "Joined, but this direct chat has too many members to notify."
            }
            Self::Failed(InvitationFailure::MembershipUnconfirmed) => {
                "Joined, but the invitation was not sent because your encrypted call membership could not be confirmed."
            }
            Self::Failed(_) => {
                "Joined, but the invitation was not sent because its safety checks did not complete."
            }
            Self::Unconfirmed => {
                "Joined. Invitation delivery is unconfirmed; it may still arrive. Do not retry."
            }
        }
    }
}

/// Private event/transaction identities must never enter status text or logs.
pub(crate) struct InvitationReceipt {
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    _transaction_id: OwnedTransactionId,
    retraction_transaction_id: OwnedTransactionId,
    user_id: String,
    device_id: String,
    expires_at: Instant,
}

impl fmt::Debug for InvitationReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InvitationReceipt { .. }")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetractionOutcome {
    Accepted,
    Unconfirmed,
}

impl InvitationReceipt {
    pub(crate) fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Best effort after media shutdown. This uses the original account's
    /// client even after its selection was revoked. A receipt is never moved
    /// to a newly selected account. Repeating a retraction uses the same txn.
    pub(crate) async fn redact(&self, rtc: &RtcService) -> RetractionOutcome {
        if rtc.client().user_id().map(UserId::as_str) != Some(self.user_id.as_str())
            || rtc.client().device_id().map(|id| id.as_str()) != Some(self.device_id.as_str())
        {
            return RetractionOutcome::Unconfirmed;
        }
        let request = redact_event::v3::Request::new(
            self.room_id.clone(),
            self.event_id.clone(),
            self.retraction_transaction_id.clone(),
        );
        match time::timeout(
            RETRACTION_TIMEOUT,
            rtc.client().send(request).with_request_config(
                RequestConfig::new()
                    .retry_limit(0)
                    .timeout(RETRACTION_TIMEOUT),
            ),
        )
        .await
        {
            Ok(Ok(_)) => RetractionOutcome::Accepted,
            _ => RetractionOutcome::Unconfirmed,
        }
    }
}

/// Borrowed call-generation and cancellation boundaries; account selection is
/// revocable across A→B→A and the local voice preference is read live.
pub(crate) struct OutgoingRing<'a> {
    account: &'a AccountInfo,
    selection: AccountSelection,
    info: &'a CallInfo,
    mode: InteropMode,
    token: u64,
    active_token: &'a AtomicU64,
    stop: &'a CaptureStop,
    local_voice_space: &'a (dyn Fn() -> bool + Sync),
}

impl fmt::Debug for OutgoingRing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutgoingRing { .. }")
    }
}

impl<'a> OutgoingRing<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        account: &'a AccountInfo,
        selection: AccountSelection,
        info: &'a CallInfo,
        mode: InteropMode,
        token: u64,
        active_token: &'a AtomicU64,
        stop: &'a CaptureStop,
        local_voice_space: &'a (dyn Fn() -> bool + Sync),
    ) -> Self {
        Self {
            account,
            selection,
            info,
            mode,
            token,
            active_token,
            stop,
            local_voice_space,
        }
    }

    fn blocked(&self) -> Option<Suppression> {
        guard_blocked(
            self.selection.is_current(),
            self.token,
            self.active_token.load(Ordering::SeqCst),
            self.stop.requested(),
            (self.local_voice_space)(),
        )
    }

    /// Consuming self prevents retrying this attempt with a fresh transaction.
    /// The caller must also prohibit another Notify action for this live join.
    pub(crate) async fn send(self, rtc: &RtcService) -> InvitationOutcome {
        if let Some(reason) = self.blocked() {
            return InvitationOutcome::Suppressed(reason);
        }
        if self.mode == InteropMode::RoomState {
            return InvitationOutcome::Suppressed(Suppression::Legacy);
        }
        let started = AtomicBool::new(false);
        let work = self.send_inner(rtc, &started);
        tokio::pin!(work);
        let deadline = time::sleep(OPERATION_TIMEOUT);
        tokio::pin!(deadline);
        let mut checks = time::interval(GUARD_INTERVAL);
        checks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                // Keep a known receipt even if cancellation raced the response;
                // the worker suppresses stale UI and uses it for retraction.
                result = &mut work => return result,
                _ = self.selection.revoked() => return interrupted(
                    started.load(Ordering::SeqCst), Some(Suppression::Cancelled)),
                _ = &mut deadline => return interrupted(started.load(Ordering::SeqCst), None),
                _ = checks.tick() => {
                    if let Some(reason) = self.blocked() {
                        return interrupted(started.load(Ordering::SeqCst), Some(reason));
                    }
                }
            }
        }
    }

    async fn send_inner(&self, rtc: &RtcService, started: &AtomicBool) -> InvitationOutcome {
        match self.prepare_and_send(rtc, started).await {
            Ok(outcome) => outcome,
            Err(reason) => InvitationOutcome::Failed(reason),
        }
    }

    async fn prepare_and_send(
        &self,
        rtc: &RtcService,
        started: &AtomicBool,
    ) -> Result<InvitationOutcome, InvitationFailure> {
        if self.selection.account_id() != self.account.id
            || rtc.client().user_id().map(UserId::as_str) != Some(self.account.user_id.as_str())
            || rtc.client().device_id().map(|id| id.as_str())
                != Some(self.account.device_id.as_str())
        {
            return Err(InvitationFailure::AccountMismatch);
        }
        let user =
            UserId::parse(&self.account.user_id).map_err(|_| InvitationFailure::AccountMismatch)?;
        let room_id =
            RoomId::parse(&self.info.room_id).map_err(|_| InvitationFailure::CallUnavailable)?;
        let call = rtc
            .call(&self.info.room_id)
            .ok_or(InvitationFailure::CallUnavailable)?;
        if call.membership_id() != self.info.membership_id
            || call.local_identity() != self.info.local_identity
        {
            return Err(InvitationFailure::CallUnavailable);
        }
        let dialect = NotificationDialect::new(
            self.mode,
            &self.account.user_id,
            &self.account.device_id,
            &self.info.room_id,
            &self.info.slot_id,
        )
        .ok_or(InvitationFailure::CallUnavailable)?;
        let room = rtc
            .client()
            .get_room(&room_id)
            .ok_or(InvitationFailure::CallUnavailable)?;
        if room.state() != RoomState::Joined {
            return Err(InvitationFailure::CallUnavailable);
        }
        if !room
            .latest_encryption_state()
            .await
            .map_err(|_| InvitationFailure::NotEncrypted)?
            .is_encrypted()
        {
            return Err(InvitationFailure::NotEncrypted);
        }
        if let Some(reason) = self.blocked() {
            return Ok(InvitationOutcome::Suppressed(reason));
        }
        if !rtc
            .client()
            .supports_sticky_events()
            .await
            .map_err(|_| InvitationFailure::UnsupportedServer)?
        {
            return Err(InvitationFailure::UnsupportedServer);
        }
        // Subscribe before snapshotting. Delayed decryption/refresh can replace
        // the reference, so re-read live entries instead of holding an old ID.
        let mut sticky_updates = room.sticky_events().subscribe();
        let mut roster_updates = call.subscribe_participants();
        loop {
            if let Some(reason) = self.blocked() {
                return Ok(InvitationOutcome::Suppressed(reason));
            }
            let roster = rtc
                .participants(&self.info.room_id)
                .map_err(|_| InvitationFailure::CallUnavailable)?;
            match occupancy(&roster, self.account, &self.info.membership_id) {
                Occupancy::Occupied => {
                    return Ok(InvitationOutcome::Suppressed(Suppression::Occupied));
                }
                Occupancy::OwnOnly => {
                    if own_membership(&room, self.account, self.info, &dialect, self.mode)?
                        .is_some()
                    {
                        break;
                    }
                }
                Occupancy::Pending => {}
            }
            tokio::select! {
                changed = sticky_updates.recv() => {
                    if matches!(changed, Err(broadcast::error::RecvError::Closed)) {
                        return Err(InvitationFailure::MembershipUnconfirmed);
                    }
                    // Lagged receivers deliberately re-read the full live map.
                }
                changed = roster_updates.changed() => {
                    if changed.is_err() { return Err(InvitationFailure::CallUnavailable); }
                }
            }
        }
        // Fetch custom state from the server: sliding sync can omit this event.
        // An unreadable marker fails quiet; it is never treated as disabled.
        if rtc
            .voice_space_marker(&self.info.room_id)
            .await
            .map_err(|_| InvitationFailure::VoiceSpaceCheck)?
            .is_some_and(|marker| marker.enabled)
        {
            return Ok(InvitationOutcome::Suppressed(Suppression::VoiceSpace));
        }
        if let Some(reason) = self.blocked() {
            return Ok(InvitationOutcome::Suppressed(reason));
        }
        // Resolve recipients after membership observation and the shared-marker
        // request, so a long join wait does not preserve stale permissions or
        // DM membership. Ambiguous DM metadata never falls back to @room.
        let is_direct = room
            .is_direct()
            .await
            .map_err(|_| InvitationFailure::TargetCheck)?;
        let direct_metadata = room.direct_targets();
        let targets = if is_direct {
            let members = room
                .members(RoomMemberships::JOIN)
                .await
                .map_err(|_| InvitationFailure::TargetCheck)?;
            if let Some(reason) = self.blocked() {
                return Ok(InvitationOutcome::Suppressed(reason));
            }
            let direct_metadata = room.direct_targets();
            direct_targets(
                &self.account.user_id,
                members.iter().map(|m| m.user_id().as_str()),
                direct_metadata.iter().map(|target| target.as_str()),
            )?
        } else if !direct_metadata.is_empty() {
            return Err(InvitationFailure::TargetCheck);
        } else {
            let powers = room
                .power_levels()
                .await
                .map_err(|_| InvitationFailure::TargetCheck)?;
            room_targets(powers.user_can_trigger_room_notification(&user))?
        };
        if let Some(reason) = self.blocked() {
            return Ok(InvitationOutcome::Suppressed(reason));
        }
        let roster = rtc
            .participants(&self.info.room_id)
            .map_err(|_| InvitationFailure::CallUnavailable)?;
        match occupancy(&roster, self.account, &self.info.membership_id) {
            Occupancy::Occupied => return Ok(InvitationOutcome::Suppressed(Suppression::Occupied)),
            Occupancy::Pending => return Err(InvitationFailure::MembershipUnconfirmed),
            Occupancy::OwnOnly => {}
        }
        let membership = own_membership(&room, self.account, self.info, &dialect, self.mode)?
            .ok_or(InvitationFailure::MembershipUnconfirmed)?;
        let sender_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|value| u64::try_from(value.as_millis()).ok())
            .ok_or(InvitationFailure::Clock)?;
        let transaction_id = TransactionId::new();
        let content = dialect.render(notification_content(
            &self.account.device_id,
            &membership,
            &targets,
            sender_ts,
        ));
        let expires_at = Instant::now() + RING_LIFETIME;
        if let Some(reason) = self.blocked() {
            return Ok(InvitationOutcome::Suppressed(reason));
        }
        // There is no atomic roster-check-and-send operation in Matrix. A peer
        // can join immediately after the last snapshot; do not claim otherwise.
        started.store(true, Ordering::SeqCst);
        match room
            .send_raw(dialect.event_type(), content)
            .with_transaction_id(&transaction_id)
            .with_sticky_duration(STICKY_DURATION)
            .with_request_config(RequestConfig::new().retry_limit(0).timeout(REQUEST_TIMEOUT))
            .await
        {
            Ok(result) => Ok(InvitationOutcome::Sent(InvitationReceipt {
                room_id,
                event_id: result.response.event_id,
                _transaction_id: transaction_id,
                retraction_transaction_id: TransactionId::new(),
                user_id: self.account.user_id.clone(),
                device_id: self.account.device_id.clone(),
                expires_at,
            })),
            // Even an SDK error can follow an accepted request with a lost
            // response. No new transaction or immediate retry is offered.
            Err(_) => Ok(InvitationOutcome::Unconfirmed),
        }
    }
}

fn guard_blocked(
    selection_current: bool,
    token: u64,
    active_token: u64,
    stopped: bool,
    voice: bool,
) -> Option<Suppression> {
    if !selection_current || token == 0 || token != active_token || stopped {
        Some(Suppression::Cancelled)
    } else if voice {
        Some(Suppression::VoiceSpace)
    } else {
        None
    }
}

fn interrupted(started: bool, reason: Option<Suppression>) -> InvitationOutcome {
    if started {
        InvitationOutcome::Unconfirmed
    } else if let Some(reason) = reason {
        InvitationOutcome::Suppressed(reason)
    } else {
        InvitationOutcome::Failed(InvitationFailure::TimedOut)
    }
}

struct Targets {
    users: Vec<String>,
    room: bool,
}

fn direct_targets<'a>(
    own_user: &str,
    joined: impl IntoIterator<Item = &'a str>,
    metadata: impl IntoIterator<Item = &'a str>,
) -> Result<Targets, InvitationFailure> {
    let mut users = BTreeSet::new();
    let mut saw_self = false;
    for user in joined {
        UserId::parse(user).map_err(|_| InvitationFailure::TargetCheck)?;
        if user == own_user {
            saw_self = true;
        } else {
            users.insert(user.to_owned());
            if users.len() > MAX_TARGETS {
                return Err(InvitationFailure::TooManyTargets);
            }
        }
    }
    if users.is_empty() {
        return Err(InvitationFailure::NoTargets);
    }
    let mut declared = BTreeSet::new();
    for user in metadata {
        // A third-party identifier cannot authorize a Matrix notification.
        UserId::parse(user).map_err(|_| InvitationFailure::TargetCheck)?;
        if user != own_user {
            declared.insert(user.to_owned());
        }
        if declared.len() > MAX_TARGETS {
            return Err(InvitationFailure::TooManyTargets);
        }
    }
    if !saw_self || users != declared {
        return Err(InvitationFailure::TargetCheck);
    }
    Ok(Targets {
        users: users.into_iter().collect(),
        room: false,
    })
}

fn room_targets(permitted: bool) -> Result<Targets, InvitationFailure> {
    if !permitted {
        return Err(InvitationFailure::RoomMentionForbidden);
    }
    Ok(Targets {
        users: vec![],
        room: true,
    })
}

#[derive(Debug, Eq, PartialEq)]
enum Occupancy {
    Pending,
    OwnOnly,
    Occupied,
}

fn occupancy(roster: &[ParticipantInfo], own: &AccountInfo, membership_id: &str) -> Occupancy {
    let mut saw_current = false;
    for member in roster {
        // Another own device and unknown-device entries are real participants.
        // Reachability does not decide whether a call is already occupied.
        if member.user_id != own.user_id
            || member.device_id.as_deref() != Some(own.device_id.as_str())
            || member.member_id != membership_id
            || !member.is_local
        {
            return Occupancy::Occupied;
        }
        saw_current = true;
    }
    if saw_current {
        Occupancy::OwnOnly
    } else {
        Occupancy::Pending
    }
}

struct MembershipEvidence<'a> {
    sender: &'a str,
    device: Option<&'a str>,
    verified: bool,
    sticky_key: &'a str,
    content: &'a Value,
}

fn matches_own_membership(
    evidence: MembershipEvidence<'_>,
    own: &AccountInfo,
    info: &CallInfo,
    wire_slot: &str,
    mode: InteropMode,
) -> bool {
    let content = evidence.content;
    if !evidence.verified
        || evidence.sender != own.user_id
        || evidence.device != Some(own.device_id.as_str())
        || evidence.sticky_key != info.membership_id
        || content.get("slot_id").and_then(Value::as_str) != Some(wire_slot)
        || content.pointer("/application/type").and_then(Value::as_str) != Some("m.call")
        || content.pointer("/member/id").and_then(Value::as_str)
            != Some(info.membership_id.as_str())
    {
        return false;
    }
    let key = content.get("sticky_key");
    let unstable = content.get("msc4354_sticky_key");
    if key.is_none() && unstable.is_none() {
        return false;
    }
    if [key, unstable]
        .into_iter()
        .flatten()
        .any(|value| value.as_str() != Some(info.membership_id.as_str()))
    {
        return false;
    }
    let membership = content.pointer("/member/membership");
    match mode {
        InteropMode::Current if membership.and_then(Value::as_str) == Some("join") => {}
        InteropMode::Sticky2025
            if membership.is_none() || membership.and_then(Value::as_str) == Some("join") => {}
        _ => return false,
    }
    // Current content does not require these redundant claims. If present,
    // neither dialect may contradict the verified sending device/user.
    [
        ("/member/user_id", own.user_id.as_str()),
        ("/member/device_id", own.device_id.as_str()),
    ]
    .into_iter()
    .all(|(path, expected)| {
        content
            .pointer(path)
            .is_none_or(|value| value.as_str() == Some(expected))
    })
}

fn own_membership(
    room: &Room,
    own: &AccountInfo,
    info: &CallInfo,
    dialect: &NotificationDialect,
    mode: InteropMode,
) -> Result<Option<OwnedEventId>, InvitationFailure> {
    let mut result = None;
    for entry in room.sticky_events().live() {
        if entry.key.sender.as_str() != own.user_id || entry.key.sticky_key != info.membership_id {
            continue;
        }
        if !matches!(
            entry.key.event_type.to_string().as_str(),
            "m.rtc.member" | "org.matrix.msc4143.rtc.member"
        ) {
            continue;
        }
        if entry.raw().json().get().len() > MAX_MEMBERSHIP_BYTES {
            continue;
        }
        let Some(encryption) = entry.encryption_info() else {
            continue;
        };
        if encryption.sender != entry.key.sender {
            continue;
        }
        let Ok(Some(content)) = entry.raw().get_field::<Value>("content") else {
            continue;
        };
        if !matches_own_membership(
            MembershipEvidence {
                sender: encryption.sender.as_str(),
                device: encryption.sender_device.as_deref().map(|id| id.as_str()),
                verified: encryption.verification_state == VerificationState::Verified,
                sticky_key: &entry.key.sticky_key,
                content: &content,
            },
            own,
            info,
            dialect.membership_slot(),
            mode,
        ) {
            continue;
        }
        if result.as_ref().is_some_and(|old| old != &entry.event_id) {
            return Err(InvitationFailure::MembershipUnconfirmed);
        }
        result = Some(entry.event_id);
    }
    Ok(result)
}

fn notification_content(
    device: &str,
    membership: &OwnedEventId,
    targets: &Targets,
    sender_ts: u64,
) -> Value {
    // Dual nested/deployed fields match the pinned library's notification
    // contract. The adapter handles Matrix2 flattening; never add a sticky key.
    json!({
        "application": {"type": "m.call", "notification_type": "ring",
            "device_id": device, "sender_ts": sender_ts, "lifetime": 30000,
            "m.call.intent": "audio"},
        "notification_type": "ring", "sender_ts": sender_ts, "lifetime": 30000,
        "m.call.intent": "audio", "m.text": [{"body": "Call invitation"}],
        "m.mentions": {"user_ids": targets.users, "room": targets.room},
        "m.relates_to": {"rel_type": "m.reference", "event_id": membership}
    })
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::EventId;
    use morpheus_rtc::ringing::{
        CallParticipation, ResolvedJoinedCall, RingContext, RingIneligible, RingNotification,
        RoomMentionPermission,
    };

    use super::*;

    fn account() -> AccountInfo {
        AccountInfo {
            id: "local-account".into(),
            user_id: "@self:example.org".into(),
            device_id: "DEVICE".into(),
            homeserver: "https://example.org".into(),
        }
    }

    fn info() -> CallInfo {
        CallInfo {
            room_id: "!room:example.org".into(),
            slot_id: "m.call#room".into(),
            membership_id: "current-join".into(),
            local_identity: "opaque-identity".into(),
        }
    }

    fn member() -> ParticipantInfo {
        ParticipantInfo {
            member_id: "current-join".into(),
            user_id: "@self:example.org".into(),
            device_id: Some("DEVICE".into()),
            is_local: true,
            reachable: true,
            streams: vec![],
            hand_raised_at_ms: None,
            joined_at_ms: None,
        }
    }

    fn membership() -> Value {
        json!({"slot_id": "m.call#room", "application": {"type": "m.call"},
            "member": {"id": "current-join", "membership": "join"},
            "msc4354_sticky_key": "current-join"})
    }

    fn matches(
        content: &Value,
        mode: InteropMode,
        verified: bool,
        sender: &str,
        device: Option<&str>,
    ) -> bool {
        let account = account();
        let info = info();
        let dialect = NotificationDialect::new(
            mode,
            &account.user_id,
            &account.device_id,
            &info.room_id,
            &info.slot_id,
        );
        let Some(dialect) = dialect else {
            return false;
        };
        matches_own_membership(
            MembershipEvidence {
                sender,
                device,
                verified,
                sticky_key: "current-join",
                content,
            },
            &account,
            &info,
            dialect.membership_slot(),
            mode,
        )
    }

    #[test]
    fn only_this_verified_device_and_logical_join_can_supply_the_reference() {
        let content = membership();
        assert!(matches(
            &content,
            InteropMode::Current,
            true,
            "@self:example.org",
            Some("DEVICE")
        ));
        for (verified, sender, device) in [
            (false, "@self:example.org", Some("DEVICE")),
            (true, "@other:example.org", Some("DEVICE")),
            (true, "@self:example.org", Some("OTHER")),
            (true, "@self:example.org", None),
        ] {
            assert!(!matches(
                &content,
                InteropMode::Current,
                verified,
                sender,
                device
            ));
        }
        for (path, replacement) in [
            ("/member/id", json!("previous-join")),
            ("/member/membership", json!("leave")),
            ("/slot_id", json!("m.call#other")),
            ("/application/type", json!("other.app")),
            ("/msc4354_sticky_key", json!("previous-join")),
        ] {
            let mut bad = content.clone();
            *bad.pointer_mut(path).unwrap() = replacement;
            assert!(!matches(
                &bad,
                InteropMode::Current,
                true,
                "@self:example.org",
                Some("DEVICE")
            ));
        }
    }

    #[test]
    fn matrix2_shape_is_explicit_and_conflicting_claims_never_match() {
        let mut content = membership();
        content["slot_id"] = json!("m.call#ROOM");
        content["member"]
            .as_object_mut()
            .unwrap()
            .remove("membership");
        content["member"]["user_id"] = json!("@self:example.org");
        content["member"]["device_id"] = json!("DEVICE");
        assert!(matches(
            &content,
            InteropMode::Sticky2025,
            true,
            "@self:example.org",
            Some("DEVICE")
        ));
        assert!(!matches(
            &content,
            InteropMode::Current,
            true,
            "@self:example.org",
            Some("DEVICE")
        ));
        for (field, value) in [
            ("user_id", "@other:example.org"),
            ("device_id", "OTHER"),
            ("membership", "leave"),
        ] {
            let mut bad = content.clone();
            bad["member"][field] = json!(value);
            assert!(!matches(
                &bad,
                InteropMode::Sticky2025,
                true,
                "@self:example.org",
                Some("DEVICE")
            ));
        }
        content["sticky_key"] = json!("conflicting-join");
        assert!(!matches(
            &content,
            InteropMode::Sticky2025,
            true,
            "@self:example.org",
            Some("DEVICE")
        ));
        assert!(!matches(
            &content,
            InteropMode::RoomState,
            true,
            "@self:example.org",
            Some("DEVICE")
        ));
    }

    #[test]
    fn occupied_includes_unreachable_peers_other_own_devices_and_unknown_devices() {
        let account = account();
        assert_eq!(occupancy(&[], &account, "current-join"), Occupancy::Pending);
        assert_eq!(
            occupancy(&[member()], &account, "current-join"),
            Occupancy::OwnOnly
        );
        let mut earlier = member();
        earlier.member_id = "previous-join".into();
        assert_eq!(
            occupancy(&[earlier.clone()], &account, "current-join"),
            Occupancy::Occupied
        );
        assert_eq!(
            occupancy(&[earlier, member()], &account, "current-join"),
            Occupancy::Occupied
        );
        for (user, device) in [
            ("@peer:example.org", Some("DEVICE")),
            ("@self:example.org", Some("OTHER")),
            ("@self:example.org", None),
        ] {
            let mut peer = member();
            peer.user_id = user.into();
            peer.device_id = device.map(str::to_owned);
            peer.is_local = false;
            peer.reachable = false;
            assert_eq!(
                occupancy(&[member(), peer], &account, "current-join"),
                Occupancy::Occupied
            );
        }
    }

    #[test]
    fn direct_targets_use_joined_users_exclude_self_deduplicate_and_bound() {
        let targets = direct_targets(
            "@self:example.org",
            [
                "@self:example.org",
                "@peer:example.org",
                "@peer:example.org",
            ],
            ["@peer:example.org"],
        )
        .unwrap();
        assert!(!targets.room);
        assert_eq!(targets.users, ["@peer:example.org"]);
        assert!(matches!(
            direct_targets(
                "@self:example.org",
                ["@self:example.org"],
                ["@peer:example.org"]
            ),
            Err(InvitationFailure::NoTargets)
        ));
        let many: Vec<_> = (0..=MAX_TARGETS)
            .map(|i| format!("@u{i}:example.org"))
            .collect();
        assert!(matches!(
            direct_targets(
                "@self:example.org",
                many.iter().map(String::as_str),
                many.iter().map(String::as_str)
            ),
            Err(InvitationFailure::TooManyTargets)
        ));
        assert!(matches!(
            room_targets(false),
            Err(InvitationFailure::RoomMentionForbidden)
        ));
        let targets = room_targets(true).unwrap();
        assert!(targets.room && targets.users.is_empty());
    }

    #[test]
    fn stale_or_ambiguous_dm_metadata_fails_without_a_room_mention_fallback() {
        let joined = ["@self:example.org", "@peer:example.org"];
        for declared in [
            vec![],
            vec!["@departed:example.org"],
            vec!["peer@example.org"],
            vec!["@peer:example.org", "@departed:example.org"],
        ] {
            assert!(matches!(
                direct_targets("@self:example.org", joined, declared),
                Err(InvitationFailure::TargetCheck)
            ));
        }
        assert!(matches!(
            direct_targets(
                "@self:example.org",
                ["@peer:example.org"],
                ["@peer:example.org"]
            ),
            Err(InvitationFailure::TargetCheck)
        ));
    }

    #[test]
    fn stale_selection_call_generation_stop_and_voice_changes_block_send() {
        assert_eq!(guard_blocked(true, 2, 2, false, false), None);
        for (current, token, active, stopped) in [
            (false, 2, 2, false), // Revoked selection stays revoked across A→B→A.
            (true, 2, 3, false),
            (true, 0, 0, false),
            (true, 2, 2, true),
        ] {
            assert_eq!(
                guard_blocked(current, token, active, stopped, false),
                Some(Suppression::Cancelled)
            );
        }
        assert_eq!(
            guard_blocked(true, 2, 2, false, true),
            Some(Suppression::VoiceSpace)
        );
        assert!(matches!(
            interrupted(false, Some(Suppression::Cancelled)),
            InvitationOutcome::Suppressed(Suppression::Cancelled)
        ));
        for reason in [
            None,
            Some(Suppression::Cancelled),
            Some(Suppression::VoiceSpace),
        ] {
            assert!(matches!(
                interrupted(true, reason),
                InvitationOutcome::Unconfirmed
            ));
        }
    }

    #[test]
    fn both_dialects_reference_the_real_membership_and_expire_without_extension() {
        let targets = direct_targets(
            "@self:example.org",
            ["@self:example.org", "@peer:example.org"],
            ["@peer:example.org"],
        )
        .unwrap();
        let reference = EventId::parse("$actual-membership-receipt").unwrap();
        for mode in [InteropMode::Current, InteropMode::Sticky2025] {
            let dialect = NotificationDialect::new(
                mode,
                "@self:example.org",
                "DEVICE",
                "!room:example.org",
                "m.call#room",
            )
            .unwrap();
            let content =
                dialect.render(notification_content("DEVICE", &reference, &targets, 100000));
            assert!(content.get("sticky_key").is_none());
            assert!(content.get("msc4354_sticky_key").is_none());
            let envelope = json!({"type": dialect.event_type(), "event_id": "$notification", "sender": "@self:example.org",
                "origin_server_ts": 100000, "content": content});
            let parsed = RingNotification::parse(&envelope, "!room:example.org").unwrap();
            assert_eq!(parsed.membership_event_id(), reference.as_str());
            let participation = CallParticipation {
                room_id: "!room:example.org",
                sender: "@self:example.org",
                logical_membership_id: "current-join",
                slot_id: "m.call#room",
                application: "m.call",
                device_id: Some("DEVICE"),
            };
            let context = RingContext {
                recipient_user_id: "@peer:example.org",
                room_mention_permission: RoomMentionPermission::Denied,
                call: ResolvedJoinedCall {
                    referenced_event_id: reference.as_str(),
                    referenced: participation,
                    current: participation,
                },
            };
            assert_eq!(
                parsed.validate_for(&context, 100000).unwrap().remaining_ms,
                30000
            );
            assert_eq!(
                parsed.validate_for(&context, 130001),
                Err(RingIneligible::Expired)
            );
        }
    }

    #[test]
    fn receipt_debug_and_status_never_reveal_event_transaction_or_identity() {
        let receipt = InvitationReceipt {
            room_id: RoomId::parse("!private:example.org").unwrap(),
            event_id: EventId::parse("$private-event").unwrap(),
            _transaction_id: "private-txn".into(),
            retraction_transaction_id: "private-retract".into(),
            user_id: "@private:example.org".into(),
            device_id: "PRIVATE_DEVICE".into(),
            expires_at: Instant::now() + RING_LIFETIME,
        };
        assert_eq!(receipt._transaction_id.as_str(), "private-txn");
        assert!(receipt.expires_at() > Instant::now());
        let outcome = InvitationOutcome::Sent(receipt);
        assert_eq!(format!("{outcome:?}"), "Sent(InvitationReceipt { .. })");
        assert!(!outcome.status_text().contains("private"));
        assert!(
            InvitationOutcome::Unconfirmed
                .status_text()
                .contains("Do not retry")
        );
    }
}
