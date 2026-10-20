//! Member moderation and redaction for one account selection and one room:
//! invite, kick, ban, unban, one member's power level, and one redaction.
//!
//! Each action is one typed request: `POST /invite`, `/kick`, `/ban` or
//! `/unban`, `PUT /redact/{eventId}/{txnId}`, or one `m.room.power_levels`
//! state `PUT`. Each is sent at most once: no HTTP retry, no automatic
//! authentication refresh and replay (`disable_auth_refresh`, STAGED from
//! root's SDK checkpoint d4c5d08, so this compiles only with it), a timeout
//! and a 64 KiB answer, on the account's no-redirect transport. No lock is
//! held across a request.
//!
//! Before it, a fresh bounded read of the room's create event and power
//! levels (and, for a redaction, the event's sender) decides whether this
//! account may act, with the creators its room version names. After the
//! last await the final checks run, with nothing awaited before the
//! hand-off: the caller has not cancelled, the exact selection still owns its
//! account, the room is joined on this device, and the app's admission (its
//! security gate) allows it. Every wait races the cancellation first, then
//! `AccountSelection::guard` (root's d56ba8b lets a ready revocation win
//! before the work is polled). Until the hand-off a stop sends nothing.
//! After it, a stop, a timeout or any answer but a 4xx is `Unknown` (it may
//! or may not have applied), never resent; a 4xx is a fixed category.
//!
//! A power-level change rewrites the stored content as raw JSON text, never
//! through a parsed value: every other field and every other user's entry
//! keeps its exact text and order, numbers included; only the one user's
//! entry is set. The power levels are read again immediately before the
//! write and compared as the exact bytes returned; any difference sends
//! nothing. Matrix has no conditional state write, so a change landing
//! between that read and the write can still be overwritten. The write is
//! then read back. Errors use fixed text: no server text, nothing logged.

use std::{
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::future::BoxFuture;
use matrix_sdk::{
    Client, HttpError, RoomState,
    config::RequestConfig,
    ruma::{
        EventId, Int, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, TransactionId, UserId,
        api::{
            client::{
                membership::{
                    ban_user,
                    invite_user::{
                        self,
                        v3::{InvitationRecipient, InviteUserId},
                    },
                    kick_user, unban_user,
                },
                redact::redact_event,
                room::get_room_event,
                state::{get_state_event_for_key, send_state_event},
            },
            error::ErrorKind,
        },
        events::{
            StateEventType,
            room::{
                create::RoomCreateEventContent,
                power_levels::{
                    RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
                    UserPowerLevel,
                },
            },
        },
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{sync::Notify, time};

use crate::{AccountManager, AccountSelection, account_access};

/// Largest state content or event read; larger is refused, not cut. Matrix
/// events are at most 64 KiB.
const MAX_READ_BYTES: u64 = 64 * 1024;
/// Most bytes accepted in a write's answer: an event ID or an error.
const MAX_WRITE_RESPONSE_BYTES: u64 = 64 * 1024;
/// Largest power-level content written.
const MAX_POWER_LEVELS_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on each guarded read, including the SDK's version check.
const READ_BOUND: Duration = Duration::from_secs(30);
/// Bound on the hand-off and its answer.
const SEND_BOUND: Duration = Duration::from_secs(20);
/// Room creators considered; more is refused as unreadable.
const MAX_CREATORS: usize = 64;

/// The request, boxed so every action's request type goes through the same
/// final checks; it is not polled until the hand-off.
macro_rules! write_once {
    ($client:expr, $request:expr) => {{
        let client: &Client = $client;
        let request = $request;
        let send: BoxFuture<'_, std::result::Result<(), HttpError>> = Box::pin(async move {
            client
                .send(request)
                .with_request_config(write_config())
                .await
                .map(|_| ())
        });
        send
    }};
}

/// Why an action did not go ahead, or its result is not known. Fixed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModerationError {
    #[error("This action was cancelled before anything was sent")]
    Cancelled,
    #[error("The selected account changed before anything was sent")]
    AccountChanged,
    #[error("That is not a valid Matrix user ID")]
    InvalidUser,
    #[error("That is not a valid Matrix room or event ID")]
    InvalidTarget,
    #[error("Power levels are whole numbers within Matrix's safe range")]
    InvalidPowerLevel,
    #[error("Join this room before managing it")]
    NotJoined,
    #[error("Your current room permissions do not allow this; nothing was sent")]
    NotAllowed,
    #[error("Finish the account or security step in progress first; nothing was sent")]
    SecurityBusy,
    #[error("The room's current permissions could not be read; nothing was sent")]
    Unavailable,
    #[error("The room's power levels cannot be changed safely here; nothing was sent")]
    Unreadable,
    #[error("The power levels changed on the server; nothing was sent. Refresh and try again")]
    BaselineChanged,
    #[error("The request timed out before anything was sent")]
    TimedOut,
    #[error("The homeserver is limiting requests; nothing was changed. Wait, then try again")]
    RateLimited,
    #[error("This sign-in needs renewing; the homeserver did not apply the change")]
    SignInExpired,
    #[error("The homeserver refused this action; nothing was changed")]
    Refused,
    #[error("No answer came back; the action may or may not have been applied")]
    Unknown,
}

pub type Result<T> = std::result::Result<T, ModerationError>;

/// What became of an action the homeserver was asked to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModerationOutcome {
    /// The homeserver applied it (for a power level, the room shows it).
    Done,
    /// The power level already was as asked; nothing was sent.
    AlreadyDone,
    /// The homeserver accepted the power level; reading it back did not
    /// show it yet. Refresh to check.
    AcceptedUnconfirmed,
}

/// A membership action on one member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberAction {
    Invite,
    Kick { reason: Option<String> },
    Ban { reason: Option<String> },
    Unban { reason: Option<String> },
}

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Ends one action. Before its request is handed over nothing is sent;
/// after that, the result becomes `Unknown`.
#[derive(Clone, Default)]
pub struct ModerationCancellation(Arc<CancellationState>);

impl ModerationCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Completes once cancelled, at once when that already happened.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before checking: notify_waiters keeps no permit.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for ModerationCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModerationCancellation { .. }")
    }
}

/// What one action runs under: the exact account selection, the caller's
/// cancellation, and the app's admission (its security gate), asked right
/// before the request is handed over; false refuses it with nothing sent.
pub struct ModerationScope<'a> {
    pub selection: &'a AccountSelection,
    pub cancellation: &'a ModerationCancellation,
    pub admission: &'a (dyn Fn() -> bool + Send + Sync),
}

impl AccountManager {
    /// Invite, kick, ban or unban `user_id` in `room_id` for the scope's
    /// exact selection: one request, after a fresh permission read (see the
    /// module notes).
    pub async fn moderate_member_selected(
        &self,
        scope: &ModerationScope<'_>,
        room_id: &str,
        user_id: &str,
        action: MemberAction,
    ) -> Result<ModerationOutcome> {
        let room_id = RoomId::parse(room_id).map_err(|_| ModerationError::InvalidTarget)?;
        let target = UserId::parse(user_id).map_err(|_| ModerationError::InvalidUser)?;
        let (client, own) = self.moderation_client(scope, &room_id).await?;
        let (levels, _) = read_levels(scope, &client, &room_id).await?;
        let allowed = match &action {
            MemberAction::Invite => levels.user_can_invite(&own),
            MemberAction::Kick { .. } => levels.user_can_kick_user(&own, &target),
            MemberAction::Ban { .. } => levels.user_can_ban_user(&own, &target),
            MemberAction::Unban { .. } => levels.user_can_unban_user(&own, &target),
        };
        if !allowed {
            return Err(ModerationError::NotAllowed);
        }
        let send = member_request(&client, room_id.clone(), target, action);
        self.dispatch(scope, &client, &room_id, send).await?;
        Ok(ModerationOutcome::Done)
    }

    /// Set `user_id`'s power level in `room_id` to `level` for the scope's
    /// exact selection: a fresh read, a re-read compared byte for byte, one
    /// state write changing only that entry, and a read-back (see the module
    /// notes).
    pub async fn set_member_power_level_selected(
        &self,
        scope: &ModerationScope<'_>,
        room_id: &str,
        user_id: &str,
        level: i64,
    ) -> Result<ModerationOutcome> {
        let room_id = RoomId::parse(room_id).map_err(|_| ModerationError::InvalidTarget)?;
        let target = UserId::parse(user_id).map_err(|_| ModerationError::InvalidUser)?;
        let level = Int::try_from(level).map_err(|_| ModerationError::InvalidPowerLevel)?;
        let (client, own) = self.moderation_client(scope, &room_id).await?;
        let (levels, first) = read_levels(scope, &client, &room_id).await?;
        // The new level may not exceed this account's own.
        let allowed = levels.user_can_change_user_power_level(&own, &target)
            && levels.for_user(&own) >= UserPowerLevel::Int(level);
        if !allowed {
            return Err(ModerationError::NotAllowed);
        }
        if levels.for_user(&target) == UserPowerLevel::Int(level) {
            return Ok(ModerationOutcome::AlreadyDone);
        }
        let merged = with_user_level(first.get(), &target, level)?;
        // Immediately before the write, compared as the exact bytes
        // returned: any change at all refuses it.
        let second = guarded(scope, read_power_levels(&client, &room_id)).await?;
        if second.as_deref().map(RawValue::get) != Some(first.get()) {
            return Err(ModerationError::BaselineChanged);
        }
        let merged = RawValue::from_string(merged).map_err(|_| ModerationError::Unreadable)?;
        let request = send_state_event::v3::Request::new_raw(
            room_id.clone(),
            StateEventType::RoomPowerLevels,
            String::new(),
            Raw::from_json(merged),
        );
        let send = write_once!(&client, request);
        self.dispatch(scope, &client, &room_id, send).await?;
        let back = guarded(scope, read_power_levels(&client, &room_id)).await;
        let shown = back
            .ok()
            .flatten()
            .is_some_and(|raw| has_level(raw.get(), &target, level));
        Ok(if shown {
            ModerationOutcome::Done
        } else {
            ModerationOutcome::AcceptedUnconfirmed
        })
    }

    /// Redact `event_id` in `room_id` for the scope's exact selection: the
    /// event's sender and the power levels are read fresh, then one redaction
    /// request is sent.
    pub async fn redact_event_selected(
        &self,
        scope: &ModerationScope<'_>,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<ModerationOutcome> {
        let room_id = RoomId::parse(room_id).map_err(|_| ModerationError::InvalidTarget)?;
        let event_id = EventId::parse(event_id).map_err(|_| ModerationError::InvalidTarget)?;
        let (client, own) = self.moderation_client(scope, &room_id).await?;
        let (levels, _) = read_levels(scope, &client, &room_id).await?;
        let sender = guarded(scope, event_sender(&client, &room_id, &event_id)).await?;
        let allowed = if sender == own {
            levels.user_can_redact_own_event(&own)
        } else {
            levels.user_can_redact_event_of_other(&own)
        };
        if !allowed {
            return Err(ModerationError::NotAllowed);
        }
        let mut request =
            redact_event::v3::Request::new(room_id.clone(), event_id, TransactionId::new());
        request.reason = reason.map(str::to_owned);
        let send = write_once!(&client, request);
        self.dispatch(scope, &client, &room_id, send).await?;
        Ok(ModerationOutcome::Done)
    }

    /// The exact selection's client and own user ID, while the selection
    /// owns its account and the room is joined on this device. The state lock
    /// is released before anything is sent.
    async fn moderation_client(
        &self,
        scope: &ModerationScope<'_>,
        room_id: &RoomId,
    ) -> Result<(Client, OwnedUserId)> {
        scope_check(scope)?;
        if !self.owns_selection(scope.selection) {
            return Err(ModerationError::AccountChanged);
        }
        // Bounded like every wait: the cancellation first, then the shared
        // selection guard, then the scope again once the lock is held.
        let state = guarded(scope, async { Ok(self.state.lock().await) }).await?;
        if !self.owns_selection(scope.selection) {
            return Err(ModerationError::AccountChanged);
        }
        let account = scope.selection.account_id();
        account_access::ensure_eligible(&state, account)
            .map_err(|_| ModerationError::AccountChanged)?;
        let loaded = crate::selected_loaded_for(&state, account)
            .map_err(|_| ModerationError::AccountChanged)?;
        let client = loaded.client.clone();
        drop(state);
        let own = client
            .user_id()
            .ok_or(ModerationError::Unavailable)?
            .to_owned();
        joined(&client, room_id)?;
        Ok((client, own))
    }

    /// The final checks and the one hand-off; nothing is awaited between
    /// them. From the hand-off on, a stop or a failure other than a 4xx is
    /// `Unknown`.
    async fn dispatch(
        &self,
        scope: &ModerationScope<'_>,
        client: &Client,
        room_id: &RoomId,
        send: BoxFuture<'_, std::result::Result<(), HttpError>>,
    ) -> Result<()> {
        scope_check(scope)?;
        if !self.owns_selection(scope.selection) {
            return Err(ModerationError::AccountChanged);
        }
        joined(client, room_id)?;
        if !(scope.admission)() {
            return Err(ModerationError::SecurityBusy);
        }
        let sent = tokio::select! {
            biased;
            () = scope.cancellation.cancelled() => return Err(ModerationError::Unknown),
            sent = scope.selection.guard(time::timeout(SEND_BOUND, send)) => sent,
        };
        match sent {
            Some(Ok(Ok(()))) => Ok(()),
            Some(Ok(Err(error))) => Err(refusal(&error).unwrap_or(ModerationError::Unknown)),
            // Revoked once handed over, or no answer in time.
            None | Some(Err(_)) => Err(ModerationError::Unknown),
        }
    }
}

fn scope_check(scope: &ModerationScope<'_>) -> Result<()> {
    if scope.cancellation.is_cancelled() {
        return Err(ModerationError::Cancelled);
    }
    if !scope.selection.is_current() {
        return Err(ModerationError::AccountChanged);
    }
    Ok(())
}

fn joined(client: &Client, room_id: &RoomId) -> Result<()> {
    let joined = client
        .get_room(room_id)
        .is_some_and(|room| room.state() == RoomState::Joined);
    if !joined {
        return Err(ModerationError::NotJoined);
    }
    Ok(())
}

/// One read under the scope: the cancellation first, then the shared
/// selection guard, in bounded time. Nothing has been sent.
async fn guarded<T>(
    scope: &ModerationScope<'_>,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    scope_check(scope)?;
    let result = tokio::select! {
        biased;
        () = scope.cancellation.cancelled() => Err(ModerationError::Cancelled),
        result = scope.selection.guard(time::timeout(READ_BOUND, work)) => match result {
            Some(result) => result.unwrap_or(Err(ModerationError::TimedOut)),
            None => Err(ModerationError::AccountChanged),
        },
    };
    let result = result?;
    scope_check(scope)?;
    Ok(result)
}

/// Reads: no HTTP retry, no authentication replay, bounded time and size.
fn read_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(REQUEST_TIMEOUT)
        .max_response_bytes(MAX_READ_BYTES)
}

/// One attempt on the wire for an action: no HTTP retry, no automatic
/// authentication refresh and replay (STAGED: root's d4c5d08), a timeout
/// and a small answer.
fn write_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(REQUEST_TIMEOUT)
        .max_response_bytes(MAX_WRITE_RESPONSE_BYTES)
}

fn member_request(
    client: &Client,
    room_id: OwnedRoomId,
    target: OwnedUserId,
    action: MemberAction,
) -> BoxFuture<'_, std::result::Result<(), HttpError>> {
    match action {
        MemberAction::Invite => {
            let recipient = InvitationRecipient::from(InviteUserId::new(target));
            write_once!(client, invite_user::v3::Request::new(room_id, recipient))
        }
        MemberAction::Kick { reason } => {
            let mut request = kick_user::v3::Request::new(room_id, target);
            request.reason = reason;
            write_once!(client, request)
        }
        MemberAction::Ban { reason } => {
            let mut request = ban_user::v3::Request::new(room_id, target);
            request.reason = reason;
            write_once!(client, request)
        }
        MemberAction::Unban { reason } => {
            let mut request = unban_user::v3::Request::new(room_id, target);
            request.reason = reason;
            write_once!(client, request)
        }
    }
}

/// A definite 4xx answer, which applied nothing, as a fixed category.
fn refusal(error: &HttpError) -> Option<ModerationError> {
    let answer = error.as_client_api_error()?;
    if !answer.status_code.is_client_error() {
        return None;
    }
    Some(match answer.error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => ModerationError::RateLimited,
        _ if answer.status_code.as_u16() == 429 => ModerationError::RateLimited,
        Some(ErrorKind::UnknownToken(_)) => ModerationError::SignInExpired,
        _ => ModerationError::Refused,
    })
}

/// A read that failed: nothing was sent, whatever the cause.
fn read_failure(error: &HttpError) -> ModerationError {
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        return ModerationError::Unreadable;
    }
    match refusal(error) {
        Some(ModerationError::RateLimited) => ModerationError::RateLimited,
        Some(ModerationError::SignInExpired) => ModerationError::SignInExpired,
        _ => ModerationError::Unavailable,
    }
}

fn not_found(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|answer| {
        answer.status_code.as_u16() == 404 && answer.error_kind() == Some(&ErrorKind::NotFound)
    })
}

/// The room's create event, as the fields a permission decision needs.
#[derive(Deserialize)]
struct CreateEvent {
    room_id: OwnedRoomId,
    #[serde(rename = "type")]
    kind: String,
    state_key: String,
    sender: OwnedUserId,
    content: RoomCreateEventContent,
}

/// The stored power-level content as the exact text returned. `None`: the
/// room has none (404 M_NOT_FOUND only).
async fn read_power_levels(client: &Client, room_id: &RoomId) -> Result<Option<Box<RawValue>>> {
    let request = get_state_event_for_key::v3::Request::new(
        room_id.to_owned(),
        StateEventType::RoomPowerLevels,
        String::new(),
    );
    match client
        .send(request)
        .with_request_config(read_config())
        .await
    {
        Ok(response) => Ok(Some(response.event_or_content)),
        Err(error) if not_found(&error) => Ok(None),
        Err(error) => Err(read_failure(&error)),
    }
}

async fn read_create(client: &Client, room_id: &RoomId) -> Result<CreateEvent> {
    let mut request = get_state_event_for_key::v3::Request::new(
        room_id.to_owned(),
        StateEventType::RoomCreate,
        String::new(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    let response = client
        .send(request)
        .with_request_config(read_config())
        .await
        .map_err(|error| read_failure(&error))?;
    let event: CreateEvent = serde_json::from_str(response.event_or_content.get())
        .map_err(|_| ModerationError::Unreadable)?;
    let exact = event.room_id.as_str() == room_id.as_str();
    if !exact || event.kind != "m.room.create" || !event.state_key.is_empty() {
        return Err(ModerationError::Unreadable);
    }
    Ok(event)
}

/// The sender of `event_id` in the room, from one bounded read.
async fn event_sender(
    client: &Client,
    room_id: &RoomId,
    event_id: &EventId,
) -> Result<OwnedUserId> {
    #[derive(Deserialize)]
    struct Sent {
        event_id: OwnedEventId,
        room_id: Option<OwnedRoomId>,
        sender: OwnedUserId,
    }
    let request = get_room_event::v3::Request::new(room_id.to_owned(), event_id.to_owned());
    let response = client
        .send(request)
        .with_request_config(read_config())
        .await
        .map_err(|error| read_failure(&error))?;
    let sent: Sent = serde_json::from_str(response.event.json().get())
        .map_err(|_| ModerationError::Unreadable)?;
    let elsewhere = sent
        .room_id
        .is_some_and(|room| room.as_str() != room_id.as_str());
    if sent.event_id.as_str() != event_id.as_str() || elsewhere {
        return Err(ModerationError::Unreadable);
    }
    Ok(sent.sender)
}

/// The room's power levels, fresh, with the creators its room version names,
/// and the stored power-level content as the exact text returned. A room
/// without power levels, or of a room version these rules do not cover, is
/// refused: nothing is granted by guessing.
async fn read_levels(
    scope: &ModerationScope<'_>,
    client: &Client,
    room_id: &RoomId,
) -> Result<(RoomPowerLevels, Box<RawValue>)> {
    let create = guarded(scope, read_create(client, room_id)).await?;
    let power = guarded(scope, read_power_levels(client, room_id)).await?;
    let power = power.ok_or(ModerationError::Unreadable)?;
    Ok((power_levels(&create, power.get())?, power))
}

fn power_levels(create: &CreateEvent, power: &str) -> Result<RoomPowerLevels> {
    let rules = create.content.room_version.rules();
    let rules = rules.ok_or(ModerationError::Unreadable)?;
    let mut creators = if rules.authorization.use_room_create_sender {
        vec![create.sender.clone()]
    } else {
        #[allow(deprecated)]
        let creator = create.content.creator.clone();
        vec![creator.ok_or(ModerationError::Unreadable)?]
    };
    if rules.authorization.additional_room_creators {
        creators.extend(create.content.additional_creators.clone());
    }
    if creators.len() > MAX_CREATORS {
        return Err(ModerationError::Unreadable);
    }
    let content: RoomPowerLevelsEventContent =
        serde_json::from_str(power).map_err(|_| ModerationError::Unreadable)?;
    let source = RoomPowerLevelsSource::Original(content);
    Ok(RoomPowerLevels::new(source, &rules.authorization, creators))
}

/// Whether the power-level content gives `user` exactly `level`.
fn has_level(content: &str, user: &UserId, level: Int) -> bool {
    let content = serde_json::from_str::<RoomPowerLevelsEventContent>(content);
    content.is_ok_and(|content| content.users.get(user) == Some(&level))
}

/// A JSON object's fields as raw text, in their stored order. A normal map
/// deserializer silently keeps one of two duplicate names; such an object
/// is refused instead.
struct RawFields(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for RawFields {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = RawFields;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields: Vec<(String, Box<RawValue>)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.iter().any(|(seen, _)| *seen == key) {
                        return Err(serde::de::Error::custom("duplicate field"));
                    }
                    fields.push((key, value));
                }
                Ok(RawFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

fn raw_fields(text: &str) -> Result<Vec<(String, Box<RawValue>)>> {
    let fields = serde_json::from_str::<RawFields>(text);
    fields
        .map(|fields| fields.0)
        .map_err(|_| ModerationError::Unreadable)
}

fn object_text(fields: &[(String, Box<RawValue>)]) -> Result<String> {
    let mut text = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        let key = serde_json::to_string(key).map_err(|_| ModerationError::Unreadable)?;
        text.push_str(&key);
        text.push(':');
        text.push_str(value.get());
    }
    text.push('}');
    Ok(text)
}

/// The power-level content `stored` (raw JSON text) with `user`'s entry in
/// `users` set to `level`, as raw JSON text. Every other field and every
/// other user's entry keeps its exact text and order; a missing `users` is
/// added last. Content that is not an object, has duplicate fields or whose
/// `users` is not such an object is refused.
pub(crate) fn with_user_level(stored: &str, user: &UserId, level: Int) -> Result<String> {
    let unreadable = |_| ModerationError::Unreadable;
    let mut fields = raw_fields(stored)?;
    let level = RawValue::from_string(level.to_string()).map_err(unreadable)?;
    let mut users = match fields.iter().find(|(key, _)| key == "users") {
        Some((_, users)) => raw_fields(users.get())?,
        None => Vec::new(),
    };
    match users.iter_mut().find(|(key, _)| key == user.as_str()) {
        Some((_, entry)) => *entry = level,
        None => users.push((user.to_string(), level)),
    }
    let users = RawValue::from_string(object_text(&users)?).map_err(unreadable)?;
    match fields.iter_mut().find(|(key, _)| key == "users") {
        Some((_, entry)) => *entry = users,
        None => fields.push(("users".to_owned(), users)),
    }
    let merged = object_text(&fields)?;
    if merged.len() > MAX_POWER_LEVELS_BYTES {
        return Err(ModerationError::Unreadable);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::tests::{fixture, runtime};
    use std::{
        collections::{BTreeMap, VecDeque},
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    const ROOM: &str = "!moderated:example.org";
    const ME: &str = "@casey:example.org";
    const BIG: &str = "123456789012345678901234567890";

    /// Stored power levels with numbers a parsed value would change, an
    /// unknown field, and fields in a non-alphabetical order.
    fn stored_levels() -> String {
        [
            r#"{"users":{"@casey:example.org":100,"@erin:example.org":100,"#,
            r#""@dana:example.org":0},"org.example.policy":{"big":"#,
            BIG,
            r#","decimal":1.10,"exp":2.5E-3},"users_default":0,"#,
            r#""events":{"m.room.name":50},"state_default":50,"kick":50,"ban":50,"#,
            r#""invite":0,"redact":50}"#,
        ]
        .concat()
    }

    #[test]
    fn a_power_level_change_keeps_every_other_byte_and_refuses_odd_content() {
        let dana = UserId::parse("@dana:example.org").unwrap();
        let merged = with_user_level(&stored_levels(), &dana, Int::from(50)).unwrap();
        let expected =
            stored_levels().replace(r#""@dana:example.org":0"#, r#""@dana:example.org":50"#);
        assert_eq!(merged, expected);
        // A member not yet listed is added last; a missing `users` too.
        let frank = UserId::parse("@frank:example.org").unwrap();
        let added = with_user_level(&stored_levels(), &frank, Int::from(10)).unwrap();
        assert!(added.contains(r#""@dana:example.org":0,"@frank:example.org":10}"#));
        assert!(added.contains(&format!(r#""big":{BIG},"decimal":1.10,"exp":2.5E-3"#)));
        let bare = with_user_level(r#"{"ban":50}"#, &frank, Int::from(10)).unwrap();
        assert_eq!(bare, r#"{"ban":50,"users":{"@frank:example.org":10}}"#);
        // Odd content is refused, never rewritten.
        for odd in [
            "[]",
            r#""text""#,
            r#"{"users":[]}"#,
            r#"{"ban":50,"ban":0}"#,
            r#"{"users":{"@dana:example.org":0,"@dana:example.org":1}}"#,
        ] {
            assert_eq!(
                with_user_level(odd, &dana, Int::from(50)),
                Err(ModerationError::Unreadable),
                "{odd}"
            );
        }
    }

    #[derive(Clone, Default)]
    struct Served {
        state: Arc<StdMutex<BTreeMap<String, String>>>,
        syncs: Arc<StdMutex<VecDeque<String>>>,
        writes: Arc<StdMutex<Vec<(String, String)>>>,
        write_reply: Arc<StdMutex<Option<(u16, String)>>>,
        power_reads: Arc<AtomicUsize>,
        hold_power: Arc<AtomicBool>,
        hold_writes: Arc<AtomicBool>,
        /// The power-level read with this number sees other bytes.
        change_at_read: Arc<AtomicUsize>,
        stopped: Arc<AtomicBool>,
    }

    /// A loopback homeserver for one joined room of one account. It answers
    /// the version check, queued syncs, state reads (content as stored text,
    /// or the whole create event), one message event, and every action
    /// (recorded, then applied for a state write unless a reply is set),
    /// each connection on its own thread. Power-level reads can be held; an
    /// action taken while actions are held is never answered or applied.
    struct ModerationServer {
        url: String,
        served: Served,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl ModerationServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let served = Served::default();
            {
                let mut state = served.state.lock().unwrap();
                state.insert("m.room.power_levels".into(), stored_levels());
                let create = format!(r#"{{"creator":"{ME}","room_version":"10"}}"#);
                state.insert("m.room.create".into(), create);
            }
            let serving = served.clone();
            let worker = thread::spawn(move || {
                while !serving.stopped.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let serving = serving.clone();
                            thread::spawn(move || answer(stream, &serving));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                served,
                worker: Some(worker),
            }
        }

        fn writes(&self) -> Vec<(String, String)> {
            self.served.writes.lock().unwrap().clone()
        }

        fn power_reads(&self) -> usize {
            self.served.power_reads.load(Ordering::SeqCst)
        }

        async fn wait_for_writes(&self, count: usize) {
            while self.writes().len() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        }

        async fn wait_for_power_reads(&self, count: usize) {
            while self.power_reads() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    impl Drop for ModerationServer {
        fn drop(&mut self) {
            self.served.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn event(kind: &str, sender: &str, id: &str, content: &str) -> String {
        let fields = format!(r#""type":"{kind}","state_key":"","sender":"{sender}""#);
        let place = format!(r#""event_id":"{id}","room_id":"{ROOM}","origin_server_ts":1"#);
        format!(r#"{{{fields},{place},"content":{content}}}"#)
    }

    /// The first sync: the account is joined to the room.
    fn joined_room() -> String {
        let create = format!(r#"{{"creator":"{ME}","room_version":"10"}}"#);
        let create = event("m.room.create", ME, "$create", &create);
        let member = event("m.room.member", ME, "$join", r#"{"membership":"join"}"#)
            .replace(r#""state_key":"""#, &format!(r#""state_key":"{ME}""#));
        let state = format!(r#"{{"events":[{create},{member}]}}"#);
        let timeline = r#"{"events":[],"limited":false}"#;
        let room = format!(r#"{{"state":{state},"timeline":{timeline}}}"#);
        format!(r#"{{"next_batch":"s1","rooms":{{"join":{{"{ROOM}":{room}}}}}}}"#)
    }

    fn answer(mut stream: TcpStream, served: &Served) {
        stream.set_nonblocking(false).unwrap();
        let timeout = Some(Duration::from_secs(2));
        stream.set_read_timeout(timeout).unwrap();
        let (line, body) = read_request(&mut stream);
        let mut parts = line.split(' ');
        let method = parts.next().unwrap_or("").to_owned();
        let target = parts.next().unwrap_or("").to_owned();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let missing = r#"{"errcode":"M_NOT_FOUND","error":"x"}"#.to_owned();
        let (status, reply) = if path.starts_with("/_matrix/client/versions") {
            (200, r#"{"versions":["v1.15"]}"#.to_owned())
        } else if path.contains("/sync") {
            let next = served.syncs.lock().unwrap().pop_front();
            (
                200,
                next.unwrap_or_else(|| r#"{"next_batch":"idle"}"#.to_owned()),
            )
        } else if method == "GET" && path.contains("/state/") {
            let kind = path.split("/state/").nth(1).unwrap_or("");
            let kind = kind.trim_end_matches('/').to_owned();
            if kind == "m.room.power_levels" {
                let read = served.power_reads.fetch_add(1, Ordering::SeqCst) + 1;
                while served.hold_power.load(Ordering::SeqCst) {
                    if served.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if read == served.change_at_read.load(Ordering::SeqCst) {
                    // Another client changed only a number's spelling.
                    let mut state = served.state.lock().unwrap();
                    let levels = state.get(&kind).cloned().unwrap_or_default();
                    state.insert(kind.clone(), levels.replace("1.10", "1.1"));
                }
            }
            match served.state.lock().unwrap().get(&kind).cloned() {
                Some(content) if query.contains("format=event") => {
                    (200, event(&kind, ME, "$state", &content))
                }
                Some(content) => (200, content),
                None => (404, missing),
            }
        } else if method == "GET" && path.contains("/event/") {
            let content = r#"{"msgtype":"m.text","body":"hello"}"#;
            (
                200,
                event("m.room.message", "@dana:example.org", "$msg", content),
            )
        } else if method == "POST" || method == "PUT" {
            let label = if path.contains("/redact/") {
                "redact".to_owned()
            } else if let Some(kind) = path.split("/state/").nth(1) {
                kind.trim_end_matches('/').to_owned()
            } else {
                path.rsplit('/').next().unwrap_or("").to_owned()
            };
            let write = (label.clone(), body.clone());
            served.writes.lock().unwrap().push(write);
            if served.hold_writes.load(Ordering::SeqCst) {
                while !served.stopped.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                return;
            }
            let reply = served.write_reply.lock().unwrap().clone();
            reply.unwrap_or_else(|| {
                if label.starts_with("m.room.") {
                    served.state.lock().unwrap().insert(label, body);
                }
                (200, r#"{"event_id":"$done"}"#.to_owned())
            })
        } else {
            (
                404,
                r#"{"errcode":"M_UNRECOGNIZED","error":"x"}"#.to_owned(),
            )
        };
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }

    /// The request line and body of one HTTP/1.1 request.
    fn read_request(stream: &mut TcpStream) -> (String, String) {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(size) = stream.read(&mut chunk) {
            if size == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..size]);
            let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_string();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    let length = name.eq_ignore_ascii_case("content-length");
                    length.then(|| value.trim().parse::<usize>().ok()).flatten()
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                let line = headers.lines().next().unwrap_or("").to_owned();
                let body = String::from_utf8_lossy(&request[end + 4..end + 4 + length]);
                return (line, body.to_string());
            }
        }
        (String::new(), String::new())
    }

    /// A fresh manager whose selected account is signed in with a synthetic
    /// session against `server` and synced once into the joined room.
    async fn signed_in(
        server: &ModerationServer,
    ) -> (std::path::PathBuf, Arc<AccountManager>, crate::AccountInfo) {
        let account = crate::AccountInfo {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: ME.into(),
            device_id: "MODERATION-FIXTURE".into(),
            homeserver: server.url.clone(),
        };
        let (directory, manager, account) = fixture(Some(account)).await;
        let client = {
            let state = manager.state.lock().await;
            state.loaded[&account.id].client.clone()
        };
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: UserId::parse(ME).unwrap(),
                device_id: "MODERATION-FIXTURE".into(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: "synthetic access token".into(),
                refresh_token: None,
            },
        };
        let restored = client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await;
        restored.unwrap();
        server.served.syncs.lock().unwrap().push_back(joined_room());
        let settings = matrix_sdk::config::SyncSettings::default();
        client.sync_once(settings).await.unwrap();
        (directory, manager, account)
    }

    fn kick(reason: &str) -> MemberAction {
        MemberAction::Kick {
            reason: Some(reason.to_owned()),
        }
    }

    #[test]
    fn every_action_is_one_bounded_request_and_a_level_change_keeps_other_bytes() {
        for config in [read_config(), write_config()] {
            let config = format!("{config:?}");
            assert!(config.contains("retry_limit: 0"), "{config}");
            assert!(config.contains("max_response_bytes: 65536"), "{config}");
            // STAGED: root's d4c5d08.
            assert!(config.contains("disable_auth_refresh: true"), "{config}");
        }
        runtime().block_on(async {
            let server = ModerationServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let cancellation = ModerationCancellation::new();
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &|| true,
            };
            // The endpoint returns $msg: it cannot authorize a different
            // event's redaction, even when the sender could be read.
            let mismatched = manager
                .redact_event_selected(&scope, ROOM, "$different", None)
                .await;
            assert_eq!(mismatched, Err(ModerationError::Unreadable));
            assert!(server.writes().is_empty());
            let dana = "@dana:example.org";
            let actions = [
                MemberAction::Invite,
                kick("disposable"),
                MemberAction::Ban { reason: None },
                MemberAction::Unban { reason: None },
            ];
            for action in actions {
                let done = manager
                    .moderate_member_selected(&scope, ROOM, dana, action)
                    .await;
                assert_eq!(done, Ok(ModerationOutcome::Done));
            }
            let redacted = manager
                .redact_event_selected(&scope, ROOM, "$msg", None)
                .await;
            assert_eq!(redacted, Ok(ModerationOutcome::Done));
            let raised = manager
                .set_member_power_level_selected(&scope, ROOM, dana, 50)
                .await;
            assert_eq!(raised, Ok(ModerationOutcome::Done));
            let writes = server.writes();
            let labels: Vec<_> = writes.iter().map(|(label, _)| label.as_str()).collect();
            assert_eq!(
                labels,
                [
                    "invite",
                    "kick",
                    "ban",
                    "unban",
                    "redact",
                    "m.room.power_levels"
                ]
            );
            assert!(writes[1].1.contains(r#""reason":"disposable""#));
            // Only Dana's entry changed; every other byte was kept.
            let expected =
                stored_levels().replace(r#""@dana:example.org":0"#, r#""@dana:example.org":50"#);
            assert_eq!(writes[5].1, expected);
            // Already as asked: nothing more is sent.
            let again = manager
                .set_member_power_level_selected(&scope, ROOM, dana, 50)
                .await;
            assert_eq!(again, Ok(ModerationOutcome::AlreadyDone));
            assert_eq!(server.writes().len(), 6);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_refusal_a_closed_gate_or_a_changed_account_before_the_hand_off_sends_nothing() {
        runtime().block_on(async {
            let server = ModerationServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let cancellation = ModerationCancellation::new();
            let open = Arc::new(AtomicBool::new(true));
            let gate = open.clone();
            let admitted = move || gate.load(Ordering::SeqCst);
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &admitted,
            };
            // Erin has this account's level: not allowed, decided locally.
            let erin = "@erin:example.org";
            let refused = manager
                .moderate_member_selected(&scope, ROOM, erin, kick("x"))
                .await;
            assert_eq!(refused, Err(ModerationError::NotAllowed));
            let above = manager
                .set_member_power_level_selected(&scope, ROOM, ME, 101)
                .await;
            assert_eq!(above, Err(ModerationError::NotAllowed));
            // The security gate closes during the fresh permission read.
            let dana = "@dana:example.org";
            server.served.hold_power.store(true, Ordering::SeqCst);
            let reads = server.power_reads();
            let close = async {
                server.wait_for_power_reads(reads + 1).await;
                open.store(false, Ordering::SeqCst);
                server.served.hold_power.store(false, Ordering::SeqCst);
            };
            let ban = MemberAction::Ban { reason: None };
            let action = manager.moderate_member_selected(&scope, ROOM, dana, ban);
            let (result, ()) = tokio::join!(action, close);
            assert_eq!(result, Err(ModerationError::SecurityBusy));
            // The account changes during the fresh permission read.
            open.store(true, Ordering::SeqCst);
            server.served.hold_power.store(true, Ordering::SeqCst);
            let reads = server.power_reads();
            let switch = async {
                server.wait_for_power_reads(reads + 1).await;
                manager.publish_active_account(None);
                server.served.hold_power.store(false, Ordering::SeqCst);
            };
            let action = manager.redact_event_selected(&scope, ROOM, "$msg", None);
            let (result, ()) = tokio::join!(action, switch);
            assert_eq!(result, Err(ModerationError::AccountChanged));
            assert!(server.writes().is_empty());
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_held_account_lock_ends_with_the_cancellation_or_the_selection() {
        runtime().block_on(async {
            let server = ModerationServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let dana = "@dana:example.org";
            // Another holder keeps the account state locked.
            let held = manager.state.lock().await;
            let selection = manager.account_selection(&account.id).unwrap();
            let cancellation = ModerationCancellation::new();
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &|| true,
            };
            let close = async {
                time::sleep(Duration::from_millis(50)).await;
                cancellation.cancel();
            };
            let action = manager.moderate_member_selected(&scope, ROOM, dana, kick("x"));
            let bounded = time::timeout(Duration::from_secs(10), action);
            let (result, ()) = tokio::join!(bounded, close);
            assert_eq!(result.unwrap(), Err(ModerationError::Cancelled));
            // The account changes while the lock is still held.
            let cancellation = ModerationCancellation::new();
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &|| true,
            };
            let switch = async {
                time::sleep(Duration::from_millis(50)).await;
                manager.publish_active_account(None);
            };
            let action = manager.redact_event_selected(&scope, ROOM, "$msg", None);
            let bounded = time::timeout(Duration::from_secs(10), action);
            let (result, ()) = tokio::join!(bounded, switch);
            assert_eq!(result.unwrap(), Err(ModerationError::AccountChanged));
            drop(held);
            assert!(server.writes().is_empty());
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn after_the_hand_off_a_stop_or_failure_is_unknown_and_a_changed_baseline_sends_nothing() {
        runtime().block_on(async {
            let server = ModerationServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let dana = "@dana:example.org";
            server.served.hold_writes.store(true, Ordering::SeqCst);
            // The caller cancels while the kick is with the homeserver.
            let selection = manager.account_selection(&account.id).unwrap();
            let cancellation = ModerationCancellation::new();
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &|| true,
            };
            let close = async {
                server.wait_for_writes(1).await;
                cancellation.cancel();
            };
            let action = manager.moderate_member_selected(&scope, ROOM, dana, kick("x"));
            let (result, ()) = tokio::join!(action, close);
            assert_eq!(result, Err(ModerationError::Unknown));
            // The account changes while the power write is with it.
            let cancellation = ModerationCancellation::new();
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &|| true,
            };
            let switch = async {
                server.wait_for_writes(2).await;
                manager.publish_active_account(None);
            };
            let action = manager.set_member_power_level_selected(&scope, ROOM, dana, 25);
            let (result, ()) = tokio::join!(action, switch);
            assert_eq!(result, Err(ModerationError::Unknown));
            // Neither was sent again.
            assert_eq!(server.writes().len(), 2);
            manager.publish_active_account(Some(account.clone()));
            server.served.hold_writes.store(false, Ordering::SeqCst);
            let selection = manager.account_selection(&account.id).unwrap();
            let cancellation = ModerationCancellation::new();
            let scope = ModerationScope {
                selection: &selection,
                cancellation: &cancellation,
                admission: &|| true,
            };
            // Answers: a 4xx changed nothing; anything else may have.
            let error = |code: &str| format!(r#"{{"errcode":"{code}","error":"homeserver text"}}"#);
            // The unknown-token answer comes last: it marks the token expired.
            let answers = [
                (429, error("M_LIMIT_EXCEEDED"), ModerationError::RateLimited),
                (403, error("M_FORBIDDEN"), ModerationError::Refused),
                (500, error("M_UNKNOWN"), ModerationError::Unknown),
                (502, "not json".to_owned(), ModerationError::Unknown),
                (
                    401,
                    error("M_UNKNOWN_TOKEN"),
                    ModerationError::SignInExpired,
                ),
            ];
            for (sent, (status, body, expected)) in answers.into_iter().enumerate() {
                if status == 401 {
                    // Another client respells a number between the fresh
                    // read and the re-read: nothing is sent.
                    let next = server.power_reads() + 2;
                    server.served.change_at_read.store(next, Ordering::SeqCst);
                    let changed = manager.set_member_power_level_selected(&scope, ROOM, dana, 10);
                    assert_eq!(changed.await, Err(ModerationError::BaselineChanged));
                    assert_eq!(server.writes().len(), 2 + sent);
                }
                *server.served.write_reply.lock().unwrap() = Some((status, body));
                let ban = MemberAction::Ban { reason: None };
                let result = manager
                    .moderate_member_selected(&scope, ROOM, dana, ban)
                    .await;
                assert_eq!(result, Err(expected), "status {status}");
                assert_eq!(server.writes().len(), 2 + sent + 1, "status {status}");
                assert!(!expected.to_string().contains("homeserver text"));
            }
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }
}
