//! Native Matrix pinned messages (`m.room.pinned_events`).
//!
//! Every read is an authoritative server GET, never the SDK's cached pinned
//! list: sliding sync does not request this state, so a local copy can be
//! stale or absent. A change rereads the state immediately before writing,
//! edits only the `pinned` array (every other content field and every other
//! list entry is kept byte for byte), sends one unretried request and then
//! rereads the result. Matrix state has no compare-and-swap, so a concurrent
//! change landing between that reread and the write can still be replaced by
//! the homeserver's last-writer-wins.
//!
//! Pinned events are fetched one by one through this account's SDK client,
//! which decrypts them. Nothing here downloads media, and no message text is
//! logged or stored; the returned text exists only in memory.

use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{StreamExt, stream};
use matrix_sdk::{
    HttpError, Room, RoomState,
    config::RequestConfig,
    deserialized_responses::{EncryptionInfo, TimelineEvent, VerificationLevel, VerificationState},
    ruma::{
        EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
        api::{
            client::state::{get_state_event_for_key, send_state_event},
            error::ErrorKind,
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, StateEventType, SyncMessageLikeEvent,
            room::{
                create::RoomCreateEventContent,
                member::{MembershipState, RoomMemberEventContent},
                message::{MessageType, Relation},
                power_levels::{
                    RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
                },
            },
        },
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{
    sync::{Mutex, Notify},
    time::{self, Instant},
};

use crate::{AccountManager, AccountSelection, account_room};

/// Pins shown and fetched per panel load, newest pin first.
pub const MAX_PINNED_DISPLAY: usize = 50;
/// Morpheus refuses to add a pin to a list already holding this many.
/// Unpinning is always allowed, because it only shrinks the list.
pub const MAX_PINNED_EVENTS: usize = 256;
/// Characters of message text kept per pinned row.
pub const MAX_PINNED_TEXT_CHARS: usize = 500;
/// Characters of sender display name kept per pinned row.
pub const MAX_PINNED_SENDER_CHARS: usize = 80;
/// Concurrent single-event requests during a panel load.
pub const PINNED_FETCH_CONCURRENCY: usize = 4;

// A client-format state event may carry `unsigned.prev_content`, so the read
// cap is larger than the content a write may send.
const MAX_STATE_RESPONSE_BYTES: usize = 160 * 1024;
// Stays below the 64 KiB Matrix event limit once the homeserver adds the
// event envelope, hashes and signatures.
const MAX_CONTENT_BYTES: usize = 60 * 1024;
// Lists longer than this are shown but not edited.
const MAX_LIST_ENTRIES: usize = 2048;
const MAX_EVENT_BYTES: usize = 512 * 1024;
const MAX_CREATORS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT: Duration = Duration::from_secs(40);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

// One pinned-list change at a time in this process. Changes are explicit,
// rare user actions; serializing them keeps one write from racing another
// write's reread.
static WRITE_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PinnedMessagesError {
    #[error("Pinned messages for this room were closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Join this room to see its pinned messages")]
    NotJoined,
    #[error("Pinned messages are unavailable; refresh to try again")]
    Unavailable,
    #[error("The homeserver is limiting requests; wait and refresh")]
    RateLimited,
    #[error("The request timed out; refresh before trying again")]
    TimedOut,
    #[error("The server returned invalid pinned-message state")]
    InvalidState,
    #[error("This room's pinned list has a format Morpheus will not change")]
    Unsupported,
    #[error("Your current room permissions do not allow changing pins")]
    PermissionDenied,
    #[error("This room already has the maximum number of pins")]
    TooMany,
    #[error("The pinned list would be too large to save")]
    TooLarge,
}
type Result<T> = std::result::Result<T, PinnedMessagesError>;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}
/// Revokes a panel load, state read or pending change. Cancelling does not
/// undo a write the homeserver already received.
#[derive(Clone, Default)]
pub struct PinnedMessagesCancellation(Arc<CancellationState>);
impl PinnedMessagesCancellation {
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
    /// Resolves once any clone is cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// The authoritative pinned list of one room for one account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinnedState {
    /// Valid pinned event IDs without duplicates, in the room's list order
    /// (normally oldest pin first).
    pub event_ids: Vec<String>,
    /// List entries that are not event IDs. Every change keeps them.
    pub unsupported_entries: usize,
    /// The room's authoritative power levels let this account send the list.
    pub permitted: bool,
    /// The list has a shape Morpheus changes without losing anything.
    pub editable: bool,
}
impl PinnedState {
    pub fn can_change(&self) -> bool {
        self.permitted && self.editable
    }
    pub fn contains(&self, event_id: &str) -> bool {
        self.event_ids.iter().any(|id| id == event_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinnedMessageKind {
    /// Text, a notice, an emote, a sticker or an attachment description.
    Message,
    /// The pinned event was redacted; only its sender and time remain.
    Redacted,
    /// This device has no key for the pinned event.
    Undecryptable,
    /// A pinned event Morpheus does not display (state, polls, edits and
    /// other event types).
    Unsupported,
    /// The homeserver did not return the event in time, or at all.
    Unavailable,
}

/// One display row. All strings are bounded and free of control,
/// bidirectional-override and zero-width characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedMessage {
    pub event_id: String,
    pub kind: PinnedMessageKind,
    /// Empty when unknown.
    pub sender_id: String,
    /// The room display name when cached, else the user ID; empty when unknown.
    pub sender_name: String,
    /// 0 when unknown.
    pub timestamp_ms: u64,
    pub text: String,
    /// "Image", "Video", "Audio", "File" or "Sticker" with a display-only
    /// basename. Nothing is downloaded for it.
    pub attachment: Option<String>,
    /// The thread root when the pinned message belongs to a thread.
    pub thread_root: Option<String>,
    /// The server reports a later edit; this row shows the original text.
    pub edited: bool,
    /// The event's encryption does not establish its sender: sent without
    /// encryption in an encrypted room, by an unsigned or unknown device,
    /// after an identity change, or with a mismatched sender.
    pub authenticity_warning: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedMessages {
    pub state: PinnedState,
    /// Newest pin first; at most [`MAX_PINNED_DISPLAY`] rows.
    pub messages: Vec<PinnedMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedChange {
    /// False when the list already had the requested state; nothing was sent.
    pub changed: bool,
    /// The list reread after the change. None when the change was accepted
    /// but the reread failed; refresh to see the list.
    pub state: Option<PinnedState>,
}

/// What a sync batch means for one room's pinned view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinnedRoomChange {
    /// The pinned list, the power levels or this account's membership may
    /// have changed: reread the state.
    pub state: bool,
    /// A shown pin may have been redacted: an open panel should reload its
    /// rows.
    pub content: bool,
}

/// Classify a sync batch for `room_id` as seen by `own_user_id`, whose
/// current pins are `pinned`. State can arrive before or after the timeline,
/// and older servers or sliding sync can put it in the timeline. Sliding-sync
/// room lists carry only the newest timeline event and do not request the
/// pinned list, so a timeline gap counts as a possible change of both.
pub fn room_updates_affect_pinned(
    updates: &matrix_sdk::sync::RoomUpdates,
    room_id: &str,
    own_user_id: &str,
    pinned: &[String],
) -> PinnedRoomChange {
    fn field<T>(event: &Raw<T>, name: &str) -> Option<String> {
        event.get_field::<String>(name).ok().flatten()
    }
    fn relevant_state<T>(event: &Raw<T>, own: &str) -> bool {
        let (Some(kind), Some(key)) = (field(event, "type"), field(event, "state_key")) else {
            return false;
        };
        match kind.as_str() {
            "m.room.pinned_events" | "m.room.power_levels" | "m.room.create" => key.is_empty(),
            // Only this account's membership changes what it may see or do.
            "m.room.member" => key == own,
            _ => false,
        }
    }
    // Room versions before 11 put the target at the top level, later ones in
    // the content.
    fn redaction_target<T>(event: &Raw<T>) -> Option<String> {
        field(event, "redacts").or_else(|| {
            event
                .get_field::<serde_json::Value>("content")
                .ok()
                .flatten()?
                .get("redacts")?
                .as_str()
                .map(str::to_owned)
        })
    }
    let Ok(room) = RoomId::parse(room_id) else {
        return PinnedRoomChange::default();
    };
    let room: &RoomId = &room;
    if updates.left.contains_key(room)
        || updates.invited.contains_key(room)
        || updates.knocked.contains_key(room)
    {
        return PinnedRoomChange {
            state: true,
            content: true,
        };
    }
    let Some(joined) = updates.joined.get(room) else {
        return PinnedRoomChange::default();
    };
    let mut change = PinnedRoomChange::default();
    if joined.timeline.limited {
        change.state = true;
        change.content = true;
    }
    let state_events = match &joined.state {
        matrix_sdk::sync::State::Before(events) | matrix_sdk::sync::State::After(events) => events,
    };
    if state_events
        .iter()
        .any(|event| relevant_state(event, own_user_id))
    {
        change.state = true;
    }
    for event in &joined.timeline.events {
        let raw = event.raw();
        if field(raw, "type").as_deref() == Some("m.room.redaction") {
            // Redacting the pinned-events state itself empties the list.
            change.state = true;
            match redaction_target(raw) {
                Some(target) => change.content |= pinned.contains(&target),
                None => change.content = true,
            }
        } else if relevant_state(raw, own_user_id) {
            change.state = true;
        }
    }
    change
}

impl AccountManager {
    /// Authoritative pinned list and permission for one room of this exact
    /// account selection. Fetches no events.
    pub async fn pinned_state_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &PinnedMessagesCancellation,
    ) -> Result<PinnedState> {
        let (room, own) = self.pinned_room(selection, room_id, cancellation).await?;
        let current = guarded(
            selection,
            cancellation,
            READ_TIMEOUT,
            read_authoritative(&room, &own),
        )
        .await?;
        Ok(current.snapshot())
    }

    /// The authoritative pinned list plus display rows for its newest
    /// [`MAX_PINNED_DISPLAY`] pins. Events that fail or time out become
    /// [`PinnedMessageKind::Unavailable`] rows instead of failing the load.
    pub async fn pinned_messages_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &PinnedMessagesCancellation,
    ) -> Result<PinnedMessages> {
        let (room, own) = self.pinned_room(selection, room_id, cancellation).await?;
        let current = guarded(
            selection,
            cancellation,
            READ_TIMEOUT,
            read_authoritative(&room, &own),
        )
        .await?;
        let state = current.snapshot();
        let ids: Vec<OwnedEventId> = current
            .content
            .ids
            .iter()
            .rev()
            .take(MAX_PINNED_DISPLAY)
            .cloned()
            .collect();
        let messages = guarded(
            selection,
            cancellation,
            LOAD_TIMEOUT + REQUEST_TIMEOUT,
            fetch_messages(&room, ids),
        )
        .await?;
        Ok(PinnedMessages { state, messages })
    }

    /// Pin (`pinned == true`) or unpin one event after rereading the
    /// authoritative list. Unpinning removes every occurrence of the ID.
    /// Nothing is sent when the list already has the requested state.
    pub async fn set_message_pinned_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        event_id: &str,
        pinned: bool,
        cancellation: &PinnedMessagesCancellation,
    ) -> Result<PinnedChange> {
        let event_id = EventId::parse(event_id).map_err(|_| PinnedMessagesError::Unsupported)?;
        let (room, own) = self.pinned_room(selection, room_id, cancellation).await?;
        guarded(selection, cancellation, WRITE_TIMEOUT, async {
            let _write = WRITE_LOCK.lock().await;
            ensure_scope(selection, cancellation)?;
            let current = read_authoritative(&room, &own).await?;
            ensure_scope(selection, cancellation)?;
            if !current.permitted {
                return Err(PinnedMessagesError::PermissionDenied);
            }
            let Some(body) = current.content.with_pin(&event_id, pinned)? else {
                return Ok(PinnedChange {
                    changed: false,
                    state: Some(current.snapshot()),
                });
            };
            ensure_scope(selection, cancellation)?;
            send_pinned(&room, body).await?;
            // The change was accepted. A failed reread must not turn that into
            // an error the user might retry.
            let state = match read_authoritative(&room, &own).await {
                Ok(after) => Some(after.snapshot()),
                Err(_) => None,
            };
            Ok(PinnedChange {
                changed: true,
                state,
            })
        })
        .await
    }

    async fn pinned_room(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &PinnedMessagesCancellation,
    ) -> Result<(Room, OwnedUserId)> {
        guarded(selection, cancellation, REQUEST_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self
                .account_selection(selection.account_id())
                .ok_or(PinnedMessagesError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(PinnedMessagesError::AccountChanged);
            }
            ensure_scope(selection, cancellation)?;
            let room = account_room(&state, selection.account_id(), room_id)
                .map_err(|_| PinnedMessagesError::Unavailable)?;
            ensure_joined(&room)?;
            let own = room
                .client()
                .user_id()
                .ok_or(PinnedMessagesError::Unavailable)?
                .to_owned();
            Ok((room, own))
        })
        .await
    }
}

fn ensure_scope(
    selection: &AccountSelection,
    cancellation: &PinnedMessagesCancellation,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(PinnedMessagesError::Cancelled);
    }
    if !selection.is_current() {
        return Err(PinnedMessagesError::AccountChanged);
    }
    Ok(())
}
fn ensure_joined(room: &Room) -> Result<()> {
    if room.state() != RoomState::Joined {
        return Err(PinnedMessagesError::NotJoined);
    }
    Ok(())
}
async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &PinnedMessagesCancellation,
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    ensure_scope(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(PinnedMessagesError::Cancelled),
        _ = selection.revoked() => Err(PinnedMessagesError::AccountChanged),
        result = time::timeout(limit, work) => {
            result.map_err(|_| PinnedMessagesError::TimedOut)?
        }
    }?;
    ensure_scope(selection, cancellation)?;
    Ok(result)
}

#[derive(Deserialize)]
struct Envelope {
    room_id: OwnedRoomId,
    sender: OwnedUserId,
    #[serde(rename = "type")]
    event_type: String,
    state_key: String,
    content: Box<RawValue>,
}
fn parse_state(raw: &str, room: &RoomId, kind: &StateEventType, key: &str) -> Result<Envelope> {
    if raw.len() > MAX_STATE_RESPONSE_BYTES {
        return Err(PinnedMessagesError::InvalidState);
    }
    let event: Envelope =
        serde_json::from_str(raw).map_err(|_| PinnedMessagesError::InvalidState)?;
    if event.room_id.as_str() != room.as_str()
        || event.event_type != kind.to_string()
        || event.state_key != key
        || !event.content.get().trim_start().starts_with('{')
    {
        return Err(PinnedMessagesError::InvalidState);
    }
    Ok(event)
}
fn missing(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|e| {
        e.status_code.as_u16() == 404 && e.error_kind() == Some(&ErrorKind::NotFound)
    })
}
// Keep only a fixed category. Never retain reflected response bodies, URLs or
// server-provided Retry-After text.
fn request_failure(error: HttpError) -> PinnedMessagesError {
    let Some(error) = error.as_client_api_error() else {
        return PinnedMessagesError::Unavailable;
    };
    match error.status_code.as_u16() {
        403 => PinnedMessagesError::PermissionDenied,
        413 => PinnedMessagesError::TooLarge,
        429 => PinnedMessagesError::RateLimited,
        _ if matches!(error.error_kind(), Some(ErrorKind::LimitExceeded(_))) => {
            PinnedMessagesError::RateLimited
        }
        _ => PinnedMessagesError::Unavailable,
    }
}
async fn state(room: &Room, kind: StateEventType, key: &str) -> Result<Option<Envelope>> {
    let mut request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        kind.clone(),
        key.into(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    match room
        .client()
        .send(request)
        .with_request_config(RequestConfig::new().retry_limit(0).timeout(REQUEST_TIMEOUT))
        .await
    {
        Ok(response) => {
            parse_state(response.event_or_content.get(), room.room_id(), &kind, key).map(Some)
        }
        Err(error) if missing(&error) => Ok(None),
        // A read never needs the permission category.
        Err(error) => Err(match request_failure(error) {
            PinnedMessagesError::RateLimited => PinnedMessagesError::RateLimited,
            _ => PinnedMessagesError::Unavailable,
        }),
    }
}
fn typed<T: serde::de::DeserializeOwned>(content: &RawValue) -> Result<T> {
    serde_json::from_str(content.get()).map_err(|_| PinnedMessagesError::InvalidState)
}
// The pinned client has encrypted-state events disabled; the pinned list is
// shared room state. One bounded attempt: a transparent retry could reapply a
// list whose baseline already changed.
async fn send_pinned(room: &Room, body: Box<RawValue>) -> Result<()> {
    ensure_joined(room)?;
    let request = send_state_event::v3::Request::new_raw(
        room.room_id().to_owned(),
        StateEventType::RoomPinnedEvents,
        String::new(),
        Raw::from_json(body),
    );
    room.client()
        .send(request)
        .with_request_config(RequestConfig::new().retry_limit(0).timeout(REQUEST_TIMEOUT))
        .await
        .map_err(request_failure)?;
    Ok(())
}

struct Authoritative {
    content: PinnedContent,
    permitted: bool,
}
impl Authoritative {
    fn snapshot(&self) -> PinnedState {
        PinnedState {
            event_ids: self.content.ids.iter().map(ToString::to_string).collect(),
            unsupported_entries: self.content.invalid,
            permitted: self.permitted,
            editable: self.content.editable,
        }
    }
}
async fn read_authoritative(room: &Room, own: &UserId) -> Result<Authoritative> {
    ensure_joined(room)?;
    let (create, member, power, pinned) = tokio::try_join!(
        state(room, StateEventType::RoomCreate, ""),
        state(room, StateEventType::RoomMember, own.as_str()),
        state(room, StateEventType::RoomPowerLevels, ""),
        state(room, StateEventType::RoomPinnedEvents, ""),
    )?;
    let create = create.ok_or(PinnedMessagesError::InvalidState)?;
    let member = member.ok_or(PinnedMessagesError::NotJoined)?;
    let member: RoomMemberEventContent = typed(&member.content)?;
    if member.membership != MembershipState::Join {
        return Err(PinnedMessagesError::NotJoined);
    }
    let permitted = may_send_pinned(own, &create, power.as_ref())?;
    let content = PinnedContent::parse(pinned.as_ref().map(|event| event.content.as_ref()))?;
    ensure_joined(room)?;
    Ok(Authoritative { content, permitted })
}
// Fails closed: an unknown room version grants nothing.
fn may_send_pinned(own: &UserId, create: &Envelope, power: Option<&Envelope>) -> Result<bool> {
    let creation: RoomCreateEventContent = typed(&create.content)?;
    let Some(rules) = creation.room_version.rules() else {
        return Ok(false);
    };
    let mut creators = if rules.authorization.use_room_create_sender {
        vec![create.sender.clone()]
    } else {
        #[allow(deprecated)]
        vec![
            creation
                .creator
                .clone()
                .ok_or(PinnedMessagesError::InvalidState)?,
        ]
    };
    if rules.authorization.additional_room_creators {
        creators.extend(creation.additional_creators.clone());
    }
    if creators.len() > MAX_CREATORS {
        return Err(PinnedMessagesError::InvalidState);
    }
    let power = match power {
        Some(event) => {
            RoomPowerLevelsSource::Original(typed::<RoomPowerLevelsEventContent>(&event.content)?)
        }
        None => RoomPowerLevelsSource::None,
    };
    let levels = RoomPowerLevels::new(power, &rules.authorization, creators);
    Ok(levels.user_can_send_state(own, StateEventType::RoomPinnedEvents))
}

// A normal map deserializer silently keeps only the last duplicate key.
// Refuse ambiguous objects rather than dropping an unknown field on write.
struct UniqueFields(BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = UniqueFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate state content field"));
                    }
                }
                Ok(UniqueFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// The pinned-events content as the server sent it. `fields` keeps every
/// top-level value as raw JSON and `entries` keeps every list entry as raw
/// JSON, so an edit reproduces everything it does not change.
struct PinnedContent {
    fields: BTreeMap<String, Box<RawValue>>,
    entries: Vec<Box<RawValue>>,
    ids: Vec<OwnedEventId>,
    invalid: usize,
    editable: bool,
}
impl PinnedContent {
    fn parse(content: Option<&RawValue>) -> Result<Self> {
        let mut parsed = Self {
            fields: BTreeMap::new(),
            entries: Vec::new(),
            ids: Vec::new(),
            invalid: 0,
            editable: true,
        };
        let Some(content) = content else {
            return Ok(parsed);
        };
        let fields: UniqueFields =
            serde_json::from_str(content.get()).map_err(|_| PinnedMessagesError::InvalidState)?;
        parsed.fields = fields.0;
        // A redacted list keeps no `pinned` field. Adding one loses nothing.
        let Some(list) = parsed.fields.get("pinned") else {
            return Ok(parsed);
        };
        let Ok(entries) = serde_json::from_str::<Vec<Box<RawValue>>>(list.get()) else {
            // Show nothing from a list that is not an array, and never
            // replace it.
            parsed.editable = false;
            return Ok(parsed);
        };
        if entries.len() > MAX_LIST_ENTRIES {
            parsed.editable = false;
        }
        let mut seen = HashSet::new();
        for entry in &entries {
            match entry_id(entry) {
                Some(id) if seen.insert(id.clone()) => parsed.ids.push(id),
                Some(_) => {}
                None => parsed.invalid += 1,
            }
        }
        parsed.entries = entries;
        Ok(parsed)
    }

    /// The content to send, or None when the list already has that state.
    fn with_pin(&self, event_id: &EventId, pinned: bool) -> Result<Option<Box<RawValue>>> {
        if !self.editable {
            return Err(PinnedMessagesError::Unsupported);
        }
        let present = self.ids.iter().any(|id| id == event_id);
        if present == pinned {
            return Ok(None);
        }
        let mut entries: Vec<Box<RawValue>> = Vec::with_capacity(self.entries.len() + 1);
        if pinned {
            if self.ids.len() >= MAX_PINNED_EVENTS {
                return Err(PinnedMessagesError::TooMany);
            }
            entries.extend(self.entries.iter().cloned());
            entries.push(
                serde_json::value::to_raw_value(event_id.as_str())
                    .map_err(|_| PinnedMessagesError::InvalidState)?,
            );
        } else {
            entries.extend(
                self.entries
                    .iter()
                    .filter(|entry| entry_id(entry).as_deref() != Some(event_id))
                    .cloned(),
            );
        }
        let mut fields = self.fields.clone();
        fields.insert(
            "pinned".to_owned(),
            serde_json::value::to_raw_value(&entries)
                .map_err(|_| PinnedMessagesError::InvalidState)?,
        );
        let body = serde_json::value::to_raw_value(&fields)
            .map_err(|_| PinnedMessagesError::InvalidState)?;
        if body.get().len() > MAX_CONTENT_BYTES {
            return Err(PinnedMessagesError::TooLarge);
        }
        Ok(Some(body))
    }
}
fn entry_id(entry: &RawValue) -> Option<OwnedEventId> {
    let text: String = serde_json::from_str(entry.get()).ok()?;
    EventId::parse(text).ok()
}

async fn fetch_messages(room: &Room, ids: Vec<OwnedEventId>) -> Result<Vec<PinnedMessage>> {
    let deadline = Instant::now() + LOAD_TIMEOUT;
    // An unknown encryption state is treated as encrypted, so an unencrypted
    // event is never presented as authenticated by mistake.
    let encrypted = match time::timeout_at(deadline, room.latest_encryption_state()).await {
        Ok(Ok(state)) if !state.is_unknown() => state.is_encrypted(),
        _ => true,
    };
    let rows = stream::iter(ids)
        .map(|id| load_one(room, id, encrypted, deadline))
        .buffered(PINNED_FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    ensure_joined(room)?;
    Ok(rows)
}
async fn load_one(
    room: &Room,
    id: OwnedEventId,
    encrypted: bool,
    deadline: Instant,
) -> PinnedMessage {
    let limit = deadline.min(Instant::now() + EVENT_TIMEOUT);
    let config = RequestConfig::new().retry_limit(0).timeout(EVENT_TIMEOUT);
    let event = match time::timeout_at(limit, room.event(&id, Some(config))).await {
        Ok(Ok(event)) => event,
        _ => return PinnedMessage::placeholder(id, PinnedMessageKind::Unavailable),
    };
    let mut row = classify(&id, &event, encrypted);
    if !row.sender_id.is_empty()
        && let Ok(sender) = UserId::parse(row.sender_id.as_str())
        && let Ok(Ok(Some(member))) =
            time::timeout_at(limit, room.get_member_no_sync(&sender)).await
        && let Some(name) = member.display_name()
    {
        let name = plain_text(name, MAX_PINNED_SENDER_CHARS);
        if !name.is_empty() {
            row.sender_name = name;
        }
    }
    row
}

impl PinnedMessage {
    fn placeholder(id: OwnedEventId, kind: PinnedMessageKind) -> Self {
        Self {
            event_id: id.to_string(),
            kind,
            sender_id: String::new(),
            sender_name: String::new(),
            timestamp_ms: 0,
            text: String::new(),
            attachment: None,
            thread_root: None,
            edited: false,
            authenticity_warning: false,
        }
    }
}

fn classify(id: &EventId, event: &TimelineEvent, encrypted: bool) -> PinnedMessage {
    let raw = event.raw();
    if raw.json().get().len() > MAX_EVENT_BYTES {
        return PinnedMessage::placeholder(id.to_owned(), PinnedMessageKind::Unsupported);
    }
    let Ok(parsed) = raw.deserialize() else {
        return PinnedMessage::placeholder(id.to_owned(), PinnedMessageKind::Unsupported);
    };
    if parsed.event_id() != id {
        return PinnedMessage::placeholder(id.to_owned(), PinnedMessageKind::Unavailable);
    }
    let sender = parsed.sender().to_owned();
    let mut row = PinnedMessage::placeholder(id.to_owned(), PinnedMessageKind::Unsupported);
    row.sender_id = plain_text(sender.as_str(), 255);
    row.sender_name = row.sender_id.clone();
    row.timestamp_ms = u64::from(parsed.origin_server_ts().get());
    let AnySyncTimelineEvent::MessageLike(message) = &parsed else {
        return row;
    };
    if message.original_content().is_none() {
        row.kind = PinnedMessageKind::Redacted;
        return row;
    }
    match message {
        AnySyncMessageLikeEvent::RoomEncrypted(_) => {
            row.kind = PinnedMessageKind::Undecryptable;
        }
        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(original)) => {
            let content = &original.content;
            match &content.relates_to {
                // A pinned edit event is not a message of its own.
                Some(Relation::Replacement(_)) => return row,
                Some(Relation::Thread(thread)) => {
                    row.thread_root = Some(thread.event_id.to_string());
                }
                _ => {}
            }
            let reply = matches!(content.relates_to, Some(Relation::Reply(_)));
            let (text, attachment) = message_text(&content.msgtype, reply);
            row.kind = PinnedMessageKind::Message;
            row.text = text;
            row.attachment = attachment;
        }
        AnySyncMessageLikeEvent::Sticker(SyncMessageLikeEvent::Original(sticker)) => {
            row.kind = PinnedMessageKind::Message;
            row.attachment = Some("Sticker".to_owned());
            row.text = plain_text(&sticker.content.body, MAX_PINNED_TEXT_CHARS);
        }
        _ => return row,
    }
    if row.kind == PinnedMessageKind::Message {
        row.edited = has_replacement(raw);
        row.authenticity_warning = !authenticated(
            encrypted,
            &sender,
            event.encryption_info().map(|info| info.as_ref()),
        );
    }
    row
}
fn message_text(msgtype: &MessageType, reply: bool) -> (String, Option<String>) {
    let category = match msgtype {
        MessageType::Image(_) => Some("Image"),
        MessageType::Video(_) => Some("Video"),
        MessageType::Audio(_) => Some("Audio"),
        MessageType::File(_) => Some("File"),
        _ => None,
    };
    if let Some(category) = category {
        let attachment = match crate::media::attachment_name(msgtype) {
            Some(name) => {
                let name = plain_text(&name, 120);
                if name.is_empty() {
                    category.to_owned()
                } else {
                    format!("{category}: {name}")
                }
            }
            None => category.to_owned(),
        };
        return (String::new(), Some(attachment));
    }
    if let MessageType::Location(_) = msgtype {
        return ("Shared a location".to_owned(), None);
    }
    let body = msgtype.body();
    let body = if reply {
        strip_reply_fallback(body)
    } else {
        body.to_owned()
    };
    (plain_text(&body, MAX_PINNED_TEXT_CHARS), None)
}
// Older clients quote the replied-to message in `body`. Show only the reply.
fn strip_reply_fallback(body: &str) -> String {
    let mut lines = body.split('\n').peekable();
    let mut quoted = false;
    while lines.peek().is_some_and(|line| line.starts_with('>')) {
        lines.next();
        quoted = true;
    }
    if !quoted {
        return body.to_owned();
    }
    if lines.peek().is_some_and(|line| line.trim().is_empty()) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n")
}
fn has_replacement(raw: &Raw<AnySyncTimelineEvent>) -> bool {
    raw.get_field::<serde_json::Value>("unsigned")
        .ok()
        .flatten()
        .and_then(|unsigned| unsigned.get("m.relations")?.get("m.replace").cloned())
        .is_some()
}
// Unverified identities are the ordinary case and carry no warning, as in
// the timeline. Anything that fails to tie the event to its sender does.
pub(crate) fn authenticated(
    encrypted_room: bool,
    sender: &UserId,
    info: Option<&EncryptionInfo>,
) -> bool {
    let Some(info) = info else {
        return !encrypted_room;
    };
    info.sender.as_str() == sender.as_str()
        && match &info.verification_state {
            VerificationState::Verified => true,
            VerificationState::Unverified(level) => {
                matches!(level, VerificationLevel::UnverifiedIdentity)
            }
        }
}

/// Bounded single-line display text: control, bidirectional-override and
/// zero-width characters become spaces, whitespace runs collapse, and an
/// ellipsis marks truncation.
pub(crate) fn plain_text(source: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut space = false;
    for ch in source.chars() {
        let hidden = ch.is_control()
            || matches!(ch,
                '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}' | '\u{feff}');
        if hidden || ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        let needed = if space { 2 } else { 1 };
        if count + needed > max_chars {
            if max_chars > 0 {
                while count >= max_chars {
                    out.pop();
                    count -= 1;
                }
                out.push('…');
            }
            return out;
        }
        if space {
            out.push(' ');
            count += 1;
            space = false;
        }
        out.push(ch);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{
        deserialized_responses::AlgorithmInfo,
        ruma::{event_id, room_id, user_id},
    };
    use serde_json::json;

    fn raw(value: serde_json::Value) -> Box<RawValue> {
        serde_json::value::to_raw_value(&value).unwrap()
    }
    fn parse(text: &str) -> PinnedContent {
        let raw = RawValue::from_string(text.to_owned()).unwrap();
        PinnedContent::parse(Some(&raw)).unwrap()
    }

    #[test]
    fn pin_preserves_unknown_fields_and_entries_byte_for_byte() {
        let content = parse(
            r#"{"pinned":["$a:example.org",17,"not an id","$a:example.org"],"org.example.big":12345678901234567890,"z":{"nested":[1.50,"x"]}}"#,
        );
        assert_eq!(content.ids.len(), 1);
        assert_eq!(content.invalid, 2);
        let body = content
            .with_pin(event_id!("$b:example.org"), true)
            .unwrap()
            .unwrap();
        assert_eq!(
            body.get(),
            r#"{"org.example.big":12345678901234567890,"pinned":["$a:example.org",17,"not an id","$a:example.org","$b:example.org"],"z":{"nested":[1.50,"x"]}}"#
        );
    }

    #[test]
    fn duplicate_content_fields_are_refused_instead_of_lost_on_write() {
        for json in [
            r#"{"pinned":[],"pinned":["$other:example.org"]}"#,
            r#"{"pinned":[],"org.example.extra":1,"org.example.extra":2}"#,
            r#"{"pinned":[],"org.example.extra":1,"org.example.\u0065xtra":2}"#,
        ] {
            let raw = RawValue::from_string(json.to_owned()).unwrap();
            assert!(matches!(
                PinnedContent::parse(Some(&raw)),
                Err(PinnedMessagesError::InvalidState)
            ));
        }
    }

    #[test]
    fn unpin_removes_every_occurrence_and_keeps_the_rest() {
        let content = parse(
            r#"{"pinned":["$a:example.org",false,"$b:example.org","$a:example.org"],"extra":"kept"}"#,
        );
        let body = content
            .with_pin(event_id!("$a:example.org"), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            body.get(),
            r#"{"extra":"kept","pinned":[false,"$b:example.org"]}"#
        );
    }

    #[test]
    fn requested_state_already_present_sends_nothing() {
        let content = parse(r#"{"pinned":["$a:example.org"]}"#);
        assert!(
            content
                .with_pin(event_id!("$a:example.org"), true)
                .unwrap()
                .is_none()
        );
        assert!(
            content
                .with_pin(event_id!("$b:example.org"), false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn absent_or_redacted_list_gains_only_the_pinned_field() {
        let empty = PinnedContent::parse(None).unwrap();
        let body = empty
            .with_pin(event_id!("$a:example.org"), true)
            .unwrap()
            .unwrap();
        assert_eq!(body.get(), r#"{"pinned":["$a:example.org"]}"#);
        let redacted = parse(r#"{"custom":true}"#);
        let body = redacted
            .with_pin(event_id!("$a:example.org"), true)
            .unwrap()
            .unwrap();
        assert_eq!(body.get(), r#"{"custom":true,"pinned":["$a:example.org"]}"#);
    }

    #[test]
    fn a_list_that_is_not_an_array_is_never_replaced() {
        let content = parse(r#"{"pinned":{"$a:example.org":true}}"#);
        assert!(!content.editable);
        assert!(content.ids.is_empty());
        assert_eq!(
            content.with_pin(event_id!("$a:example.org"), true).err(),
            Some(PinnedMessagesError::Unsupported)
        );
        assert_eq!(
            content.with_pin(event_id!("$a:example.org"), false).err(),
            Some(PinnedMessagesError::Unsupported)
        );
        let invalid = RawValue::from_string("[1]".to_owned()).unwrap();
        assert!(PinnedContent::parse(Some(&invalid)).is_err());
    }

    #[test]
    fn adding_is_bounded_but_removing_is_not() {
        let ids: Vec<String> = (0..MAX_PINNED_EVENTS)
            .map(|i| format!("$e{i}:example.org"))
            .collect();
        let content = parse(&json!({ "pinned": ids }).to_string());
        assert_eq!(
            content.with_pin(event_id!("$new:example.org"), true).err(),
            Some(PinnedMessagesError::TooMany)
        );
        assert!(
            content
                .with_pin(event_id!("$e0:example.org"), false)
                .unwrap()
                .is_some()
        );
        let long = format!("${}:example.org", "x".repeat(200));
        let big: Vec<String> = (0..300).map(|i| format!("{long}{i}")).collect();
        let content = parse(&json!({ "pinned": [], "pad": big }).to_string());
        assert_eq!(
            content.with_pin(event_id!("$a:example.org"), true).err(),
            Some(PinnedMessagesError::TooLarge)
        );
    }

    #[test]
    fn state_envelope_must_match_the_exact_request() {
        let room = room_id!("!r:example.org");
        let good = json!({
            "event_id": "$s:example.org", "room_id": "!r:example.org",
            "sender": "@a:example.org", "type": "m.room.pinned_events",
            "state_key": "", "content": {"pinned": []},
        })
        .to_string();
        assert!(parse_state(&good, room, &StateEventType::RoomPinnedEvents, "").is_ok());
        assert!(
            parse_state(
                &good,
                room_id!("!o:example.org"),
                &StateEventType::RoomPinnedEvents,
                ""
            )
            .is_err()
        );
        assert!(parse_state(&good, room, &StateEventType::RoomPowerLevels, "").is_err());
        assert!(parse_state(&good, room, &StateEventType::RoomPinnedEvents, "x").is_err());
        let not_object = good.replace(r#"{"pinned":[]}"#, "[]");
        assert!(parse_state(&not_object, room, &StateEventType::RoomPinnedEvents, "").is_err());
        let huge = format!("{}{}", " ".repeat(MAX_STATE_RESPONSE_BYTES), good);
        assert!(parse_state(&huge, room, &StateEventType::RoomPinnedEvents, "").is_err());
    }

    fn envelope(sender: &str, content: serde_json::Value) -> Envelope {
        Envelope {
            room_id: room_id!("!r:example.org").to_owned(),
            sender: UserId::parse(sender).unwrap(),
            event_type: "m.room.create".into(),
            state_key: String::new(),
            content: raw(content),
        }
    }

    #[test]
    fn permission_follows_authoritative_power_levels() {
        let create = envelope(
            "@creator:example.org",
            json!({"room_version": "11", "creator": "@creator:example.org"}),
        );
        let power = envelope(
            "@creator:example.org",
            json!({
                "users": {"@creator:example.org": 100, "@mod:example.org": 50},
                "events": {"m.room.pinned_events": 50},
                "state_default": 100,
            }),
        );
        let can = |user: &str, power: Option<&Envelope>| {
            may_send_pinned(&UserId::parse(user).unwrap(), &create, power).unwrap()
        };
        assert!(can("@mod:example.org", Some(&power)));
        assert!(can("@creator:example.org", Some(&power)));
        assert!(!can("@user:example.org", Some(&power)));
        // Without power levels only the creator may send state.
        assert!(can("@creator:example.org", None));
        assert!(!can("@user:example.org", None));
        let unknown = envelope(
            "@creator:example.org",
            json!({"room_version": "org.example.custom", "creator": "@creator:example.org"}),
        );
        assert!(!may_send_pinned(user_id!("@creator:example.org"), &unknown, None).unwrap());
    }

    fn timeline(value: serde_json::Value) -> TimelineEvent {
        TimelineEvent::from_plaintext(Raw::from_json(raw(value)))
    }
    fn message(id: &str, content: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "m.room.message", "event_id": id, "sender": "@a:example.org",
            "origin_server_ts": 1_700_000_000_000u64, "content": content,
        })
    }

    #[test]
    fn rows_report_redacted_undecryptable_and_unsupported_events_honestly() {
        let id = event_id!("$m:example.org");
        let text = classify(
            id,
            &timeline(message(
                "$m:example.org",
                json!({"msgtype": "m.text", "body": "hi\u{202e}\nthere"}),
            )),
            false,
        );
        assert_eq!(text.kind, PinnedMessageKind::Message);
        assert_eq!(text.text, "hi there");
        assert_eq!(text.timestamp_ms, 1_700_000_000_000);
        assert!(!text.authenticity_warning);

        let redacted = classify(
            id,
            &timeline(json!({
                "type": "m.room.message", "event_id": "$m:example.org",
                "sender": "@a:example.org", "origin_server_ts": 1, "content": {},
                "unsigned": {"redacted_because": {
                    "type": "m.room.redaction", "event_id": "$r:example.org",
                    "sender": "@a:example.org", "origin_server_ts": 2,
                    "redacts": "$m:example.org", "content": {"redacts": "$m:example.org"},
                }},
            })),
            false,
        );
        assert_eq!(redacted.kind, PinnedMessageKind::Redacted);
        assert!(redacted.text.is_empty());

        let encrypted = classify(
            id,
            &timeline(json!({
                "type": "m.room.encrypted", "event_id": "$m:example.org",
                "sender": "@a:example.org", "origin_server_ts": 1,
                "content": {
                    "algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "AAAA",
                    "sender_key": "", "device_id": "D", "session_id": "S",
                },
            })),
            true,
        );
        assert_eq!(encrypted.kind, PinnedMessageKind::Undecryptable);

        let edit = classify(
            id,
            &timeline(message(
                "$m:example.org",
                json!({
                    "msgtype": "m.text", "body": "* new",
                    "m.new_content": {"msgtype": "m.text", "body": "new"},
                    "m.relates_to": {"rel_type": "m.replace", "event_id": "$o:example.org"},
                }),
            )),
            false,
        );
        assert_eq!(edit.kind, PinnedMessageKind::Unsupported);

        let other = classify(
            id,
            &timeline(message(
                "$x:example.org",
                json!({"msgtype": "m.text", "body": "x"}),
            )),
            false,
        );
        assert_eq!(other.kind, PinnedMessageKind::Unavailable);
    }

    #[test]
    fn attachments_threads_replies_and_edits_are_described_without_media() {
        let id = event_id!("$m:example.org");
        let image = classify(
            id,
            &timeline(message(
                "$m:example.org",
                json!({"msgtype": "m.image", "body": "../cat\u{200f}.png", "url": "mxc://example.org/x"}),
            )),
            false,
        );
        assert_eq!(image.kind, PinnedMessageKind::Message);
        assert!(image.text.is_empty());
        assert!(image.attachment.as_deref().unwrap().starts_with("Image: "));
        assert!(!image.attachment.as_deref().unwrap().contains('/'));

        let threaded = timeline(json!({
            "type": "m.room.message", "event_id": "$m:example.org", "sender": "@a:example.org",
            "origin_server_ts": 1,
            "content": {
                "msgtype": "m.text", "body": "in thread",
                "m.relates_to": {"rel_type": "m.thread", "event_id": "$root:example.org",
                    "is_falling_back": true, "m.in_reply_to": {"event_id": "$root:example.org"}},
            },
            "unsigned": {"m.relations": {"m.replace": {"event_id": "$e:example.org"}}},
        }));
        let threaded = classify(id, &threaded, false);
        assert_eq!(threaded.thread_root.as_deref(), Some("$root:example.org"));
        assert!(threaded.edited);

        let reply = classify(
            id,
            &timeline(message(
                "$m:example.org",
                json!({
                    "msgtype": "m.text", "body": "> <@b:example.org> quoted\n\nanswer",
                    "m.relates_to": {"m.in_reply_to": {"event_id": "$q:example.org"}},
                }),
            )),
            false,
        );
        assert_eq!(reply.text, "answer");
    }

    #[test]
    fn plaintext_in_an_encrypted_room_and_device_problems_are_flagged() {
        let id = event_id!("$m:example.org");
        let plain = classify(
            id,
            &timeline(message(
                "$m:example.org",
                json!({"msgtype": "m.text", "body": "x"}),
            )),
            true,
        );
        assert!(plain.authenticity_warning);

        let sender = user_id!("@a:example.org");
        let info = |state| EncryptionInfo {
            sender: sender.to_owned(),
            sender_device: None,
            forwarder: None,
            algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key: String::new(),
                sender_claimed_keys: Default::default(),
                session_id: None,
            },
            verification_state: state,
        };
        assert!(authenticated(
            true,
            sender,
            Some(&info(VerificationState::Verified))
        ));
        assert!(authenticated(
            true,
            sender,
            Some(&info(VerificationState::Unverified(
                VerificationLevel::UnverifiedIdentity
            )))
        ));
        assert!(!authenticated(
            true,
            sender,
            Some(&info(VerificationState::Unverified(
                VerificationLevel::UnsignedDevice
            )))
        ));
        assert!(!authenticated(
            true,
            user_id!("@b:example.org"),
            Some(&info(VerificationState::Verified))
        ));
    }

    #[test]
    fn display_text_is_bounded_and_single_line() {
        assert_eq!(plain_text("  a\n\tb\u{200b}c  ", 20), "a b c");
        assert_eq!(plain_text("abcdef", 4), "abc…");
        assert_eq!(plain_text("ab cd", 3), "ab…");
        assert_eq!(plain_text("\u{202e}\u{2066}", 10), "");
        let bounded = plain_text(&"x".repeat(10_000), MAX_PINNED_TEXT_CHARS);
        assert!(bounded.chars().count() <= MAX_PINNED_TEXT_CHARS);
    }

    #[test]
    fn sync_batches_select_only_relevant_room_changes() {
        use matrix_sdk::sync::{JoinedRoomUpdate, RoomUpdates};
        let room = room_id!("!r:example.org");
        let own = "@own:example.org";
        let pinned = vec!["$pin:example.org".to_owned()];
        let affect = |updates: &RoomUpdates, room: &str| {
            room_updates_affect_pinned(updates, room, own, &pinned)
        };
        let state_event = |kind: &str, key: &str| {
            timeline(json!({
                "type": kind, "event_id": "$s:example.org", "sender": "@a:example.org",
                "origin_server_ts": 1, "state_key": key, "content": {},
            }))
        };
        let redaction = |target: &str| {
            timeline(json!({
                "type": "m.room.redaction", "event_id": "$r:example.org",
                "sender": "@a:example.org", "origin_server_ts": 1,
                "redacts": target, "content": {"redacts": target},
            }))
        };
        let batch = |events: Vec<TimelineEvent>, limited: bool| {
            let mut joined = JoinedRoomUpdate::default();
            joined.timeline.events = events;
            joined.timeline.limited = limited;
            let mut updates = RoomUpdates::default();
            updates.joined.insert(room.to_owned(), joined);
            updates
        };
        let none = PinnedRoomChange::default();
        assert_eq!(affect(&RoomUpdates::default(), room.as_str()), none);
        let text = timeline(message(
            "$m:example.org",
            json!({"msgtype": "m.text", "body": "x"}),
        ));
        assert_eq!(
            affect(&batch(vec![text.clone()], false), room.as_str()),
            none
        );
        // Another member's join or profile change says nothing about pins.
        let other = state_event("m.room.member", "@other:example.org");
        assert_eq!(affect(&batch(vec![other], false), room.as_str()), none);
        let mine = state_event("m.room.member", own);
        assert!(affect(&batch(vec![mine], false), room.as_str()).state);
        let list = state_event("m.room.pinned_events", "");
        let changed = affect(&batch(vec![list], false), room.as_str());
        assert!(changed.state && !changed.content);
        // A gap may hide a pin change or a redaction.
        let gap = affect(&batch(vec![text], true), room.as_str());
        assert!(gap.state && gap.content);
        // Every redaction may have emptied the list; only a pinned target
        // changes the shown rows.
        let unrelated = affect(
            &batch(vec![redaction("$x:example.org")], false),
            room.as_str(),
        );
        assert!(unrelated.state && !unrelated.content);
        let shown = affect(
            &batch(vec![redaction("$pin:example.org")], false),
            room.as_str(),
        );
        assert!(shown.state && shown.content);
        assert_eq!(
            affect(
                &batch(vec![redaction("$pin:example.org")], false),
                "!other:example.org"
            ),
            none
        );
    }
}
