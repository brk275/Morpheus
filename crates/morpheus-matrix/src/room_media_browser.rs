//! The shared media of one room, newest first, for the native media
//! browser: images, videos, audio and files sent as room messages.
//!
//! A load asks the selected account's own homeserver, through that
//! account's pinned client, for at most [`MEDIA_LOAD_REQUESTS`] pages of
//! `/messages` going back from a point. Each page is sent once, with no
//! retry, a timeout and a response size cap (account clients follow no
//! redirect), and the load stops early once it found [`MEDIA_LOAD_TARGET`]
//! items. Every wait races the caller's cancellation first, then runs under
//! the exact selection that asked; the manager's lock is held only to clone
//! the client, never across a request.
//!
//! In a room this device knows to be unencrypted the server is asked for
//! messages with a media address only. Otherwise messages and encrypted
//! messages are read, and encrypted ones are decrypted with this device's
//! keys exactly as `Room::messages` does. A message this device cannot
//! decrypt is counted, never shown: its ciphertext, and whether it holds
//! media at all, stay unknown. Of a media message only the sender's
//! declared file name (a display-only basename), MIME type and size, the
//! time, the sender and its thread are kept, as bounded single-line plain
//! text. Its body or caption and its media source (address and file key)
//! never leave this module. Nothing is downloaded, logged or stored.
//!
//! A load says what it covered: how many events it looked at, whether the
//! server reported the start of the history this account can see, and
//! whether paging cannot continue. Only the start being reached means that
//! nothing older exists.

use std::{
    collections::{HashMap, HashSet},
    future::{Future, IntoFuture},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    deserialized_responses::TimelineEvent,
    ruma::{
        EventId, OwnedEventId, OwnedRoomId, RoomId, UInt, UserId,
        api::{
            client::{
                filter::{RoomEventFilter, UrlFilter},
                message::get_message_events,
            },
            error::ErrorKind,
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent, SyncMessageLikeEvent,
            room::message::{MessageType, Relation},
        },
        serde::Raw,
    },
};
use tokio::{
    sync::Notify,
    time::{self, Instant},
};

use crate::{
    AccountManager, AccountSelection, MatrixCoreError,
    pinned_messages::{authenticated, plain_text},
    space_explorer::valid_token,
};

/// Events one `/messages` request asks for.
pub const MEDIA_PAGE_EVENTS: u32 = 50;
/// Requests one load sends, at most.
pub const MEDIA_LOAD_REQUESTS: usize = 6;
/// A load ends after the request in which it reached this many items.
pub const MEDIA_LOAD_TARGET: usize = 40;
/// Characters of a file name kept per item.
pub const MAX_MEDIA_NAME_CHARS: usize = 160;
/// Characters of a display name kept per item.
pub const MAX_MEDIA_SENDER_CHARS: usize = 80;
const MAX_MEDIA_TYPE_CHARS: usize = 100;
const MAX_SENDER_ID_CHARS: usize = 255;
// Matrix caps an event at 64 KiB; the server's `unsigned` data adds a little.
// A larger event is left out unread.
const MAX_EVENT_BYTES: usize = 96 * 1024;
const MAX_PAGE_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
// The whole load: its requests, decryption and local display names.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
// Local display names only; a slow store leaves user IDs.
const NAMES_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Ends one browser's loads. Cancelling any clone ends every load using it
/// at its next wait; a request already with the homeserver is dropped
/// unread.
#[derive(Clone, Default)]
pub struct RoomMediaCancellation(Arc<CancelState>);

impl RoomMediaCancellation {
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

    async fn cancelled(&self) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomMediaError {
    AccountChanged,
    Cancelled,
    InvalidRoom,
    InvalidToken,
    NotJoined,
    Forbidden,
    RateLimited,
    TooLarge,
    ReadLimit,
    TimedOut,
    Unavailable,
}

impl RoomMediaError {
    /// A fixed sentence: server text is never shown.
    pub fn text(self) -> &'static str {
        match self {
            Self::AccountChanged => "The account changed before this room's media was read.",
            Self::Cancelled => "Reading this room's media was stopped.",
            Self::InvalidRoom => "This is not a room Morpheus can list media for.",
            Self::InvalidToken => {
                "Older media could not be requested from here. Close the list and open it again."
            }
            Self::NotJoined => "Join this room to browse its media.",
            Self::Forbidden => {
                "Your homeserver does not let this account read this room's history."
            }
            Self::RateLimited => "Your homeserver is busy. Try again shortly.",
            Self::TooLarge => "This part of the room's history is too large to read here.",
            Self::ReadLimit => {
                "This media list reached its reading limit. Close it and open it again."
            }
            Self::TimedOut => "Reading this room's media timed out. Try again.",
            Self::Unavailable => "This room's media could not be read. Try again.",
        }
    }

    /// Whether asking again from the same point may succeed.
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::TimedOut | Self::Unavailable)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomMediaKind {
    Image,
    Video,
    Audio,
    File,
}

/// One media message. All strings are bounded single-line plain text; the
/// file name, type and size are what the sender declared, not checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomMediaItem {
    /// The message's stable event ID: what Open, View and Save act on.
    pub event_id: String,
    pub kind: RoomMediaKind,
    /// A display-only basename, never empty. Nothing is downloaded for it.
    pub filename: String,
    /// `type/subtype` when declared and valid, else empty.
    pub mime_type: String,
    pub size: Option<u64>,
    /// 0 when unknown.
    pub timestamp_ms: u64,
    pub sender_id: String,
    /// The room display name when cached, else the user ID.
    pub sender_name: String,
    /// The thread root when the message belongs to a thread.
    pub thread_root: Option<String>,
    /// The message was end-to-end encrypted.
    pub encrypted: bool,
    /// The message's encryption does not establish its sender.
    pub authenticity_warning: bool,
}

/// What one load found, newest first, and how far it got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoomMediaPage {
    pub room_id: String,
    /// Each event once, in the server's order (newest first).
    pub items: Vec<RoomMediaItem>,
    /// Where an older load continues; `None` once the start was reached or
    /// paging cannot continue.
    pub next: Option<String>,
    /// The server reported the start of the history this account can see.
    pub reached_start: bool,
    /// Paging cannot continue from here (an unusable or repeated token, or
    /// a longer page than asked for): older media may exist but is not
    /// read. It never means that nothing older exists.
    pub truncated: bool,
    /// Events this load looked at.
    pub examined: usize,
    /// Encrypted messages this device could not decrypt; any media in them
    /// is not listed.
    pub undecryptable: usize,
    /// Events left out unread: too large, malformed or not of this room.
    pub skipped: usize,
    /// The room is encrypted, or this device does not know that it is not.
    pub room_encrypted: bool,
}

impl AccountManager {
    /// One load of `room_id`'s media for the exact selection, going back
    /// from `from` (the previous load's `next`), or from the newest message
    /// when `None`. Each request is sent once, under the selection's guard,
    /// and `cancellation` ends the load at its next wait.
    pub async fn room_media_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        from: Option<&str>,
        cancellation: &RoomMediaCancellation,
    ) -> Result<RoomMediaPage, RoomMediaError> {
        self.room_media_for_budget(
            selection,
            room_id,
            from,
            MEDIA_LOAD_REQUESTS * MEDIA_PAGE_EVENTS as usize,
            cancellation,
        )
        .await
    }

    /// A load within the caller's remaining event budget. The core still
    /// applies its per-load request/event limits; zero sends nothing.
    pub async fn room_media_for_budget(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        from: Option<&str>,
        remaining_events: usize,
        cancellation: &RoomMediaCancellation,
    ) -> Result<RoomMediaPage, RoomMediaError> {
        let room_id = RoomId::parse(room_id).map_err(|_| RoomMediaError::InvalidRoom)?;
        if from.is_some_and(|token| !valid_token(token)) {
            return Err(RoomMediaError::InvalidToken);
        }
        self.media_scope(selection, cancellation)?;
        if remaining_events == 0 {
            return Err(RoomMediaError::ReadLimit);
        }
        let budget = remaining_events.min(MEDIA_LOAD_REQUESTS * MEDIA_PAGE_EVENTS as usize);
        let load = self.load_media(selection, &room_id, from, budget, cancellation);
        time::timeout(LOAD_TIMEOUT, load)
            .await
            .map_err(|_| RoomMediaError::TimedOut)?
    }

    async fn load_media(
        &self,
        selection: &AccountSelection,
        room_id: &RoomId,
        from: Option<&str>,
        budget: usize,
        cancellation: &RoomMediaCancellation,
    ) -> Result<RoomMediaPage, RoomMediaError> {
        let opened = self.media_room(selection, room_id);
        let (client, room) = self.within(selection, cancellation, opened).await??;
        let encrypted = room_encrypted(&room);
        let config = client
            .request_config()
            .retry_limit(0)
            .disable_auth_refresh()
            .timeout(PAGE_TIMEOUT)
            .max_response_bytes(MAX_PAGE_RESPONSE_BYTES);
        let mut page = RoomMediaPage {
            room_id: room_id.to_string(),
            room_encrypted: encrypted,
            ..RoomMediaPage::default()
        };
        let mut seen = HashSet::new();
        let mut from = from.map(str::to_owned);
        for _ in 0..MEDIA_LOAD_REQUESTS {
            let limit = (budget - page.examined).min(MEDIA_PAGE_EVENTS as usize);
            if limit == 0 {
                break;
            }
            // The prior decryption/store awaits may have outlived joined
            // membership. This helper rechecks before every new page.
            let request = page_request(&room, from.clone(), encrypted, limit)?;
            let paging = from.is_some();
            let send = client.send(request).with_request_config(config);
            let response = self
                .within(selection, cancellation, send.into_future())
                .await?
                .map_err(|error| media_failure(&error, paging))?;
            // Left, kicked or banned meanwhile: nothing read is returned.
            ensure_joined(&room)?;
            let admitted = admit_chunk(room_id, response.chunk, limit, &mut seen, &mut page);
            for (id, raw) in admitted {
                ensure_joined(&room)?;
                let event = self
                    .within(selection, cancellation, decrypt(&room, raw))
                    .await?;
                ensure_joined(&room)?;
                match project(&id, &event, encrypted) {
                    Seen::Media(item) => page.items.push(item),
                    Seen::Undecryptable => page.undecryptable += 1,
                    Seen::Skipped => page.skipped += 1,
                    Seen::Other => {}
                }
            }
            if page.truncated {
                // The server's end is beyond an unread tail. Continuing
                // from it would lose media and could falsely claim the
                // whole history later, even when this response has no end.
                from = None;
                break;
            }
            match next_step(from.as_deref(), response.end) {
                Next::Start => {
                    page.reached_start = true;
                    from = None;
                    break;
                }
                Next::Stuck => {
                    page.truncated = true;
                    from = None;
                    break;
                }
                Next::Continue(end) => from = Some(end),
            }
            if page.items.len() >= MEDIA_LOAD_TARGET {
                break;
            }
        }
        page.next = from;
        let names = fill_names(&room, &mut page.items);
        self.within(selection, cancellation, names).await?;
        ensure_joined(&room)?;
        Ok(page)
    }

    /// The pinned client of the exact selection, and the room, joined. The
    /// manager's lock is released before this returns.
    async fn media_room(
        &self,
        selection: &AccountSelection,
        room_id: &RoomId,
    ) -> Result<(Client, Room), RoomMediaError> {
        let client = self
            .hierarchy_client_for(selection)
            .await
            .map_err(|error| match error {
                MatrixCoreError::SelectedAccountChanged => RoomMediaError::AccountChanged,
                _ => RoomMediaError::Unavailable,
            })?;
        let room = client.get_room(room_id).ok_or(RoomMediaError::NotJoined)?;
        ensure_joined(&room)?;
        Ok((client, room))
    }

    /// `work` raced against the cancellation first, then run under the
    /// selection's guard; both are checked again after it.
    async fn within<T>(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomMediaCancellation,
        work: impl Future<Output = T>,
    ) -> Result<T, RoomMediaError> {
        self.media_scope(selection, cancellation)?;
        let output = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(RoomMediaError::Cancelled),
            output = selection.guard(work) => output.ok_or(RoomMediaError::AccountChanged)?,
        };
        self.media_scope(selection, cancellation)?;
        Ok(output)
    }

    fn media_scope(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomMediaCancellation,
    ) -> Result<(), RoomMediaError> {
        if cancellation.is_cancelled() {
            return Err(RoomMediaError::Cancelled);
        }
        if !self.owns_selection(selection) {
            return Err(RoomMediaError::AccountChanged);
        }
        Ok(())
    }
}

/// This account is still joined to the room: checked before a read and
/// again after it, before anything read is returned.
fn ensure_joined(room: &Room) -> Result<(), RoomMediaError> {
    if room.state() == RoomState::Joined {
        Ok(())
    } else {
        Err(RoomMediaError::NotJoined)
    }
}

/// Construct one page only while still joined, after prior asynchronous
/// work. `limit` is bounded by the remaining load budget and page size.
fn page_request(
    room: &Room,
    from: Option<String>,
    encrypted: bool,
    limit: usize,
) -> Result<get_message_events::v3::Request, RoomMediaError> {
    ensure_joined(room)?;
    let mut request = get_message_events::v3::Request::backward(room.room_id().to_owned());
    request.from = from;
    request.limit = UInt::from(limit as u32);
    request.filter = media_filter(encrypted);
    Ok(request)
}

/// Whether to treat the room as encrypted, from this device's room state
/// only: nothing is fetched, and an unknown state counts as encrypted, so
/// encrypted messages are never filtered out by a media-address filter.
fn room_encrypted(room: &Room) -> bool {
    let state = room.encryption_state();
    state.is_unknown() || state.is_encrypted()
}

/// What the server is asked for. An encrypted message's media address is
/// inside its ciphertext, so the address filter applies only to a room this
/// device knows to be unencrypted.
pub(crate) fn media_filter(encrypted: bool) -> RoomEventFilter {
    let mut filter = RoomEventFilter::default();
    let mut types = vec!["m.room.message".to_owned()];
    if encrypted {
        types.push("m.room.encrypted".to_owned());
    } else {
        filter.url_filter = Some(UrlFilter::EventsWithUrl);
    }
    filter.types = Some(types);
    filter
}

/// One response's events, bounded before anything is decrypted: at most a
/// page, each event of this room once. Counts what it looked at and left
/// out into `page`.
pub(crate) fn admit_chunk(
    room_id: &RoomId,
    chunk: Vec<Raw<AnyTimelineEvent>>,
    limit: usize,
    seen: &mut HashSet<OwnedEventId>,
    page: &mut RoomMediaPage,
) -> Vec<(OwnedEventId, Raw<AnyTimelineEvent>)> {
    let limit = limit.min(MEDIA_PAGE_EVENTS as usize);
    if chunk.len() > limit {
        page.truncated = true;
    }
    let mut admitted = Vec::new();
    for raw in chunk.into_iter().take(limit) {
        page.examined += 1;
        match event_id(&raw, room_id) {
            // The same event twice is one message, not a lost one.
            Some(id) if !seen.insert(id.clone()) => {}
            Some(id) => admitted.push((id, raw)),
            None => page.skipped += 1,
        }
    }
    admitted
}

/// The event's ID when it is a bounded event of `room_id`.
fn event_id(raw: &Raw<AnyTimelineEvent>, room_id: &RoomId) -> Option<OwnedEventId> {
    if raw.json().get().len() > MAX_EVENT_BYTES {
        return None;
    }
    let room = raw.get_field::<OwnedRoomId>("room_id").ok().flatten()?;
    if room.as_str() != room_id.as_str() {
        return None;
    }
    raw.get_field::<OwnedEventId>("event_id").ok().flatten()
}

/// Where the next request starts, from a response's `end`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Continue(String),
    /// No `end`: the start of the history this account can see.
    Start,
    /// An unusable token, or the one this request started from.
    Stuck,
}

pub(crate) fn next_step(from: Option<&str>, end: Option<String>) -> Next {
    match end {
        None => Next::Start,
        Some(end) if !valid_token(&end) || from == Some(end.as_str()) => Next::Stuck,
        Some(end) => Next::Continue(end),
    }
}

// The same decryption `Room::messages` applies, without push actions. A
// message this device cannot decrypt stays the encrypted event, which the
// projection counts as undecryptable, never as media.
async fn decrypt(room: &Room, raw: Raw<AnyTimelineEvent>) -> TimelineEvent {
    if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
        SyncMessageLikeEvent::Original(_),
    ))) = raw.deserialize_as::<AnySyncTimelineEvent>()
        && let Ok(event) = room.decrypt_event(raw.cast_ref_unchecked(), None).await
    {
        return event;
    }
    TimelineEvent::from_plaintext(raw.cast())
}

/// What one event is to the browser.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    Media(RoomMediaItem),
    /// An encrypted message this device cannot decrypt: nothing of it is
    /// known, so it is counted, never shown.
    Undecryptable,
    /// Anything else: text, state, edits, stickers and removed messages.
    Other,
    /// Unreadable, or not the event it was admitted as.
    Skipped,
}

/// A media item from a (decrypted) event. Only declared metadata is read;
/// the body, caption and media source are never copied.
pub(crate) fn project(id: &EventId, event: &TimelineEvent, encrypted_room: bool) -> Seen {
    let raw = event.raw();
    if raw.json().get().len() > MAX_EVENT_BYTES {
        return Seen::Skipped;
    }
    let Ok(parsed) = raw.deserialize() else {
        return Seen::Skipped;
    };
    if parsed.event_id() != id {
        return Seen::Skipped;
    }
    let AnySyncTimelineEvent::MessageLike(message) = &parsed else {
        return Seen::Other;
    };
    let original = match message {
        AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(_)) => {
            return Seen::Undecryptable;
        }
        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(original)) => original,
        _ => return Seen::Other,
    };
    let content = &original.content;
    let thread_root = match &content.relates_to {
        // An edit is not media of its own; the original stays listed.
        Some(Relation::Replacement(_)) => return Seen::Other,
        Some(Relation::Thread(thread)) => Some(thread.event_id.to_string()),
        _ => None,
    };
    let Some((kind, mime_type, size)) = declared(&content.msgtype) else {
        return Seen::Other;
    };
    let sender = parsed.sender();
    let sender_id = plain_text(sender.as_str(), MAX_SENDER_ID_CHARS);
    let info = event.encryption_info().map(|info| info.as_ref());
    Seen::Media(RoomMediaItem {
        event_id: id.to_string(),
        kind,
        filename: display_name(&content.msgtype),
        mime_type: mime_type.map(media_type).unwrap_or_default(),
        size: size.map(u64::from),
        timestamp_ms: u64::from(parsed.origin_server_ts().get()),
        sender_name: sender_id.clone(),
        sender_id,
        thread_root,
        encrypted: info.is_some(),
        authenticity_warning: !authenticated(encrypted_room, sender, info),
    })
}

/// The kind, declared MIME type and declared size of a media message.
fn declared(message: &MessageType) -> Option<(RoomMediaKind, Option<&str>, Option<UInt>)> {
    Some(match message {
        MessageType::Image(content) => {
            let info = content.info.as_deref();
            let mime = info.and_then(|info| info.mimetype.as_deref());
            (RoomMediaKind::Image, mime, info.and_then(|info| info.size))
        }
        MessageType::Video(content) => {
            let info = content.info.as_deref();
            let mime = info.and_then(|info| info.mimetype.as_deref());
            (RoomMediaKind::Video, mime, info.and_then(|info| info.size))
        }
        MessageType::Audio(content) => {
            let info = content.info.as_deref();
            let mime = info.and_then(|info| info.mimetype.as_deref());
            (RoomMediaKind::Audio, mime, info.and_then(|info| info.size))
        }
        MessageType::File(content) => {
            let info = content.info.as_deref();
            let mime = info.and_then(|info| info.mimetype.as_deref());
            (RoomMediaKind::File, mime, info.and_then(|info| info.size))
        }
        _ => return None,
    })
}

/// The declared file name as a display-only basename (the same one the
/// timeline and Save suggest), bounded; never empty.
fn display_name(message: &MessageType) -> String {
    let name = crate::media::attachment_name(message).unwrap_or_default();
    let name = plain_text(&name, MAX_MEDIA_NAME_CHARS);
    if name.is_empty() {
        "attachment".to_owned()
    } else {
        name
    }
}

/// A declared MIME type as `type/subtype`, lowercase; empty when invalid.
fn media_type(declared: &str) -> String {
    if declared.len() > MAX_MEDIA_TYPE_CHARS {
        return String::new();
    }
    match declared.parse::<mime::Mime>() {
        Ok(parsed) => parsed.essence_str().to_ascii_lowercase(),
        Err(_) => String::new(),
    }
}

/// Display names from this device's member store only, within a short
/// budget; a sender without one keeps the user ID.
async fn fill_names(room: &Room, items: &mut [RoomMediaItem]) {
    let deadline = Instant::now() + NAMES_TIMEOUT;
    let mut names = HashMap::<String, Option<String>>::new();
    for item in items.iter_mut() {
        if let Some(name) = cached_name(room, &mut names, &item.sender_id, deadline).await {
            item.sender_name = name;
        }
    }
}

async fn cached_name(
    room: &Room,
    names: &mut HashMap<String, Option<String>>,
    sender_id: &str,
    deadline: Instant,
) -> Option<String> {
    if let Some(name) = names.get(sender_id) {
        return name.clone();
    }
    let name = match UserId::parse(sender_id) {
        Ok(user) => match time::timeout_at(deadline, room.get_member_no_sync(&user)).await {
            Ok(Ok(Some(member))) => member
                .display_name()
                .map(|name| plain_text(name, MAX_MEDIA_SENDER_CHARS))
                .filter(|name| !name.is_empty()),
            _ => None,
        },
        Err(_) => None,
    };
    names.insert(sender_id.to_owned(), name.clone());
    name
}

// A fixed category only; never reflected server text.
fn media_failure(error: &HttpError, paging: bool) -> RoomMediaError {
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        return RoomMediaError::TooLarge;
    }
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => return RoomMediaError::RateLimited,
        Some(ErrorKind::InvalidParam) if paging => return RoomMediaError::InvalidToken,
        _ => {}
    }
    let Some(error) = error.as_client_api_error() else {
        return RoomMediaError::Unavailable;
    };
    match error.status_code.as_u16() {
        403 => RoomMediaError::Forbidden,
        429 => RoomMediaError::RateLimited,
        _ => RoomMediaError::Unavailable,
    }
}

#[cfg(test)]
mod tests;
