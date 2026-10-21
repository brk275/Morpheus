//! The thread roots of one room, a page at a time, for the native thread
//! browser. A page asks the selected account's own homeserver, through that
//! account's pinned client, for one bounded page of `/threads`: sent once,
//! with no retry and no automatic sign-in renewal and replay, a timeout and
//! a response size cap (account clients follow no redirect). Nothing is
//! fetched for a room the user did not open the browser in, and every read
//! runs under the exact selection that asked.
//!
//! Roots are decrypted with this device's keys (`Room::decrypt_event`, as
//! `Room::list_threads` does) and projected exactly like pinned messages:
//! bounded single-line plain text, an undecryptable root shows no body and
//! never its ciphertext, and a sender its encryption does not establish
//! carries a warning. The latest-reply line comes only from the server's
//! own thread summary (time, sender and reply count), never from a reply's
//! body, and no unread count is computed. Nothing is logged, stored or
//! downloaded; the text exists only in memory. Whether the room is
//! encrypted is read from this device's room state only (unknown counts as
//! encrypted), so a page is the one request.
//!
//! A homeserver without `/threads` gets nothing more sent: the browser can
//! then show the thread roots this device already holds, read from the
//! room's local event cache with no request at all
//! (`cached_thread_roots_for`), and labelled as such.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    config::RequestConfig,
    deserialized_responses::TimelineEvent,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
        api::{
            client::threads::get_threads::{self, v1::IncludeThreads},
            error::ErrorKind,
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent, SyncMessageLikeEvent,
        },
        serde::Raw,
    },
};
use serde::Deserialize;
use tokio::time::{self, Instant};

use crate::{
    AccountManager, AccountSelection, MatrixCoreError,
    pinned_messages::{PinnedMessage, PinnedMessageKind, classify, plain_text},
    space_explorer::valid_token,
};

/// Thread roots one page asks for.
pub const THREAD_PAGE_LIMIT: u32 = 25;
/// Characters of a root's text kept per row.
pub const MAX_THREAD_EXCERPT_CHARS: usize = 280;
/// Characters of a display name kept per row.
pub const MAX_THREAD_SENDER_CHARS: usize = 80;
/// Thread roots read from this device's event cache, at most.
pub const MAX_CACHED_THREAD_ROOTS: usize = 200;
/// Cached events one fallback read looks at, newest first, at most.
pub const MAX_CACHED_EXAMINED: usize = 2_000;
// Bytes of root events one fallback read copies, at most.
const MAX_CACHED_BYTES: usize = 4 * 1024 * 1024;
const MAX_SENDER_ID_CHARS: usize = 255;
// One root larger than this is left out unread.
const MAX_ROOT_BYTES: usize = 512 * 1024;
const MAX_PAGE_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
// The whole page: the request, decryption and local display names.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
// Local display names only; a slow store leaves user IDs.
const NAMES_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadListError {
    AccountChanged,
    InvalidRoom,
    InvalidToken,
    NotJoined,
    Forbidden,
    RateLimited,
    TooLarge,
    TimedOut,
    Unsupported,
    Unavailable,
}

impl ThreadListError {
    /// A fixed sentence: server text is never shown.
    pub fn text(self) -> &'static str {
        match self {
            Self::AccountChanged => "The account changed before this room's threads were read.",
            Self::InvalidRoom => "This is not a room Morpheus can list threads for.",
            Self::InvalidToken => {
                "The next page of threads could not be requested. Close and open the list again."
            }
            Self::NotJoined => "Join this room to browse its threads.",
            Self::Forbidden => {
                "Your homeserver does not let this account list this room's threads."
            }
            Self::RateLimited => "Your homeserver is busy. Try again shortly.",
            Self::TooLarge => "This page of threads is too large to show.",
            Self::TimedOut => "Reading this room's threads timed out. Try again.",
            Self::Unsupported => {
                "Your homeserver does not list threads. Open a thread from its message instead."
            }
            Self::Unavailable => "This room's threads could not be read. Try again.",
        }
    }

    /// Whether asking again for the same page may succeed.
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::TimedOut | Self::Unavailable)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRootKind {
    /// Text, a notice, an emote, a sticker or an attachment description.
    Message,
    /// The root was redacted; only its sender and time remain.
    Redacted,
    /// This device has no key for the root. No body is shown.
    Undecryptable,
    /// A root Morpheus does not display (state, polls and other types).
    Unsupported,
}

/// The newest reply, as the server's thread summary reports it. Only the
/// envelope is read: a reply's body is never taken from the summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadLatestReply {
    /// 0 when the summary gives no usable time.
    pub timestamp_ms: u64,
    pub sender_id: String,
    /// The room display name when cached, else the user ID.
    pub sender_name: String,
}

/// One thread root. All strings are bounded single-line plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRootSummary {
    /// The root's stable event ID: the thread to open.
    pub root_id: String,
    pub kind: ThreadRootKind,
    pub sender_id: String,
    /// The room display name when cached, else the user ID.
    pub sender_name: String,
    /// 0 when unknown.
    pub timestamp_ms: u64,
    /// Empty unless `kind` is `Message`.
    pub excerpt: String,
    /// "Image", "Video", "Audio", "File" or "Sticker" with a display-only
    /// basename. Nothing is downloaded for it.
    pub attachment: Option<String>,
    /// The server reports a later edit; this row shows the original text.
    pub edited: bool,
    pub authenticity_warning: bool,
    /// Present only when the server's summary names the newest reply.
    pub latest: Option<ThreadLatestReply>,
    /// The server's reply count, when its summary carries one.
    pub replies: Option<u64>,
    /// The server's summary says this account took part in the thread: it
    /// sent the root or a reply (the summary does not say which).
    pub participated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRootPage {
    pub room_id: String,
    /// Each root once, in the server's order (most recent activity first).
    pub roots: Vec<ThreadRootSummary>,
    /// A usable continuation token; `None` when the server sent none or an
    /// unusable one (then `truncated` says so).
    pub next_batch: Option<String>,
    /// From the server: it sent more than one page or an unusable token.
    /// From this device's cache: the bounded scan (events looked at, roots,
    /// bytes) ended before the in-memory cache did, so older loaded events
    /// were not looked at. It does not say that more roots exist.
    pub truncated: bool,
    /// Events left out: not identifiable as roots of this room, too large,
    /// or not readable at all.
    pub skipped: usize,
}

impl AccountManager {
    /// One page of the thread roots of `room_id` for the exact selection.
    /// Pass the previous page's `next_batch` to continue. The request is
    /// sent once, with a bounded response, under the selection's guard.
    pub async fn thread_roots_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        from: Option<&str>,
    ) -> std::result::Result<ThreadRootPage, ThreadListError> {
        let room_id = RoomId::parse(room_id).map_err(|_| ThreadListError::InvalidRoom)?;
        if from.is_some_and(|token| !valid_token(token)) {
            return Err(ThreadListError::InvalidToken);
        }
        let read = async {
            let (client, room) = self.thread_room(selection, &room_id).await?;
            let mut request = get_threads::v1::Request::new(room_id.clone());
            request.from = from.map(str::to_owned);
            request.include = IncludeThreads::All;
            request.limit = Some(UInt::from(THREAD_PAGE_LIMIT));
            let config = page_config(client.request_config());
            let response = selection
                .guard(async {
                    if !self.owns_selection(selection) {
                        return Err(ThreadListError::AccountChanged);
                    }
                    client
                        .send(request)
                        .with_request_config(config)
                        .await
                        .map_err(|error| thread_failure(&error, from.is_some()))
                })
                .await
                .ok_or(ThreadListError::AccountChanged)??;
            if !self.owns_selection(selection) {
                return Err(ThreadListError::AccountChanged);
            }
            let bounded = bound_page(&room_id, response.chunk, response.next_batch);
            let page = selection
                .guard(project_page(&room, bounded))
                .await
                .ok_or(ThreadListError::AccountChanged)?;
            // Left, kicked or banned while the page was read or decrypted:
            // nothing of it is shown.
            ensure_joined(&room)?;
            if !self.owns_selection(selection) {
                return Err(ThreadListError::AccountChanged);
            }
            Ok(page)
        };
        time::timeout(LOAD_TIMEOUT, read)
            .await
            .map_err(|_| ThreadListError::TimedOut)?
    }

    /// The thread roots this device already holds for `room_id`: events of
    /// the room's in-memory event cache that carry a thread summary, newest
    /// first. Nothing is requested from the homeserver. For a homeserver
    /// that does not list threads: these are only roots of messages loaded
    /// on this device, and no reply count or latest reply is given for them
    /// (the cache's may be partial).
    ///
    /// The cache is walked newest first by reference, through
    /// `RoomEventCache::rfind_map_event_in_memory_by`: at most
    /// [`MAX_CACHED_EXAMINED`] events are looked at, none is parsed, and only
    /// thread roots within [`MAX_CACHED_THREAD_ROOTS`] and a byte bound are
    /// copied (see [`CachedScan`]). The rest of the cache is never copied.
    pub async fn cached_thread_roots_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> std::result::Result<ThreadRootPage, ThreadListError> {
        let room_id = RoomId::parse(room_id).map_err(|_| ThreadListError::InvalidRoom)?;
        let read = async {
            let (_, room) = self.thread_room(selection, &room_id).await?;
            let scan = selection
                .guard(async {
                    // The handles keep the room's cache alive while it is read.
                    let (cache, _handles) = room.event_cache().await.ok()?;
                    let mut scan = CachedScan::default();
                    cache
                        .rfind_map_event_in_memory_by(|event| scan.visit(event).then_some(()))
                        .await
                        .ok()?;
                    Some(scan)
                })
                .await
                .ok_or(ThreadListError::AccountChanged)?
                .ok_or(ThreadListError::Unavailable)?;
            if !self.owns_selection(selection) {
                return Err(ThreadListError::AccountChanged);
            }
            let mut page = cached_page(&room_id, scan, room_encrypted(&room));
            selection
                .guard(fill_names(&room, &mut page.roots))
                .await
                .ok_or(ThreadListError::AccountChanged)?;
            // Left, kicked or banned while it was read: nothing is shown.
            ensure_joined(&room)?;
            if !self.owns_selection(selection) {
                return Err(ThreadListError::AccountChanged);
            }
            Ok(page)
        };
        time::timeout(LOAD_TIMEOUT, read)
            .await
            .map_err(|_| ThreadListError::TimedOut)?
    }

    /// The pinned client of the exact selection, and the room, joined.
    async fn thread_room(
        &self,
        selection: &AccountSelection,
        room_id: &RoomId,
    ) -> std::result::Result<(Client, Room), ThreadListError> {
        let client = self
            .hierarchy_client_for(selection)
            .await
            .map_err(|error| match error {
                MatrixCoreError::SelectedAccountChanged => ThreadListError::AccountChanged,
                _ => ThreadListError::Unavailable,
            })?;
        let room = client.get_room(room_id).ok_or(ThreadListError::NotJoined)?;
        ensure_joined(&room)?;
        Ok((client, room))
    }
}

/// This account is still joined to the room: checked before a read and
/// again after it, before anything read is returned.
fn ensure_joined(room: &Room) -> std::result::Result<(), ThreadListError> {
    if room.state() == RoomState::Joined {
        Ok(())
    } else {
        Err(ThreadListError::NotJoined)
    }
}

/// Whether to treat the room as encrypted, from this device's room state
/// only: nothing is fetched, and an unknown state counts as encrypted, so an
/// unencrypted root is never shown as authenticated by mistake.
fn room_encrypted(room: &Room) -> bool {
    let state = room.encryption_state();
    state.is_unknown() || state.is_encrypted()
}

/// One bounded pass over this device's cached events, newest first, each
/// seen by reference. Looking at an event reads only fields the cache
/// already holds (its thread summary, its cached ID and its JSON length):
/// nothing is parsed, and nothing but a thread root within the bounds is
/// copied. The pass ends at the first bound reached, and says so.
#[derive(Debug, Default)]
pub(crate) struct CachedScan {
    /// Copies of the roots kept, newest first.
    pub(crate) roots: Vec<TimelineEvent>,
    seen: HashSet<OwnedEventId>,
    pub(crate) examined: usize,
    bytes: usize,
    /// A bound ended the pass before the cache did.
    pub(crate) truncated: bool,
    /// Roots left out: too large, or without an event ID.
    pub(crate) skipped: usize,
}

impl CachedScan {
    /// Look at one event; `true` ends the pass.
    pub(crate) fn visit(&mut self, event: &TimelineEvent) -> bool {
        if self.examined >= MAX_CACHED_EXAMINED {
            self.truncated = true;
            return true;
        }
        self.examined += 1;
        if event.thread_summary.summary().is_none() {
            return false;
        }
        let size = event.raw().json().get().len();
        let Some(id) = event.event_id() else {
            self.skipped += 1;
            return false;
        };
        if size > MAX_ROOT_BYTES {
            self.skipped += 1;
            return false;
        }
        if self.roots.len() >= MAX_CACHED_THREAD_ROOTS
            || self.bytes.saturating_add(size) > MAX_CACHED_BYTES
        {
            self.truncated = true;
            return true;
        }
        if !self.seen.insert(id.to_owned()) {
            return false;
        }
        self.bytes += size;
        self.roots.push(event.clone());
        false
    }
}

/// The roots a [`CachedScan`] kept, projected like a page. A cached event is
/// already decrypted when this device could; one it could not stays
/// undecryptable.
pub(crate) fn cached_page(room_id: &RoomId, scan: CachedScan, encrypted: bool) -> ThreadRootPage {
    let mut roots = Vec::with_capacity(scan.roots.len());
    let mut skipped = scan.skipped;
    for event in &scan.roots {
        let Some(id) = event.event_id().map(ToOwned::to_owned) else {
            skipped += 1;
            continue;
        };
        match root_summary(classify(&id, event, encrypted), None) {
            Some(root) => roots.push(root),
            None => skipped += 1,
        }
    }
    ThreadRootPage {
        room_id: room_id.to_string(),
        roots,
        next_batch: None,
        truncated: scan.truncated,
        skipped,
    }
}

/// One response before decryption: at most a page of events, each root of
/// this room once, and a continuation token only when it is usable.
#[derive(Debug)]
pub(crate) struct BoundedPage {
    pub(crate) room_id: OwnedRoomId,
    pub(crate) roots: Vec<(OwnedEventId, Raw<AnyTimelineEvent>)>,
    pub(crate) next_batch: Option<String>,
    pub(crate) truncated: bool,
    pub(crate) skipped: usize,
}

pub(crate) fn bound_page(
    room_id: &RoomId,
    chunk: Vec<Raw<AnyTimelineEvent>>,
    next_batch: Option<String>,
) -> BoundedPage {
    let limit = THREAD_PAGE_LIMIT as usize;
    let truncated = chunk.len() > limit
        || next_batch
            .as_deref()
            .is_some_and(|token| !valid_token(token));
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    let mut skipped = 0;
    for raw in chunk.into_iter().take(limit) {
        match root_id(&raw, room_id) {
            // The same root twice is one thread, not a lost one.
            Some(id) if !seen.insert(id.clone()) => {}
            Some(id) => roots.push((id, raw)),
            None => skipped += 1,
        }
    }
    BoundedPage {
        room_id: room_id.to_owned(),
        roots,
        next_batch: next_batch.filter(|token| valid_token(token)),
        truncated,
        skipped,
    }
}

/// The root's event ID when the event is a bounded event of `room_id`.
fn root_id(raw: &Raw<AnyTimelineEvent>, room_id: &RoomId) -> Option<OwnedEventId> {
    if raw.json().get().len() > MAX_ROOT_BYTES {
        return None;
    }
    let room = raw.get_field::<OwnedRoomId>("room_id").ok().flatten()?;
    if room.as_str() != room_id.as_str() {
        return None;
    }
    raw.get_field::<OwnedEventId>("event_id").ok().flatten()
}

/// The server's bundled thread summary, envelope fields only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ServerSummary {
    pub(crate) latest: Option<(u64, OwnedUserId)>,
    pub(crate) replies: Option<u64>,
    pub(crate) participated: bool,
}

#[derive(Deserialize)]
struct Unsigned {
    #[serde(rename = "m.relations", default)]
    relations: Option<Relations>,
}
#[derive(Deserialize)]
struct Relations {
    #[serde(rename = "m.thread", default)]
    thread: Option<Summary>,
}
#[derive(Deserialize)]
struct Summary {
    #[serde(default)]
    latest_event: Option<LatestEnvelope>,
    #[serde(default)]
    count: Option<UInt>,
    #[serde(default)]
    current_user_participated: Option<bool>,
}
// Only the envelope: `content` is never read, encrypted or not.
#[derive(Deserialize)]
struct LatestEnvelope {
    sender: OwnedUserId,
    #[serde(default)]
    origin_server_ts: Option<UInt>,
}

/// Read from the event as the server sent it, before decryption: the
/// summary is the server's cleartext `unsigned` data either way.
pub(crate) fn server_summary(raw: &Raw<AnyTimelineEvent>) -> Option<ServerSummary> {
    let unsigned = raw.get_field::<Unsigned>("unsigned").ok().flatten()?;
    let summary = unsigned.relations?.thread?;
    Some(ServerSummary {
        latest: summary
            .latest_event
            .map(|latest| (latest.origin_server_ts.map_or(0, u64::from), latest.sender)),
        replies: summary.count.map(u64::from),
        participated: summary.current_user_participated.unwrap_or(false),
    })
}

async fn project_page(room: &Room, page: BoundedPage) -> ThreadRootPage {
    let encrypted = room_encrypted(room);
    let mut roots = Vec::with_capacity(page.roots.len());
    let mut skipped = page.skipped;
    for (id, raw) in page.roots {
        let summary = server_summary(&raw);
        let event = decrypt(room, raw).await;
        match root_summary(classify(&id, &event, encrypted), summary) {
            Some(root) => roots.push(root),
            None => skipped += 1,
        }
    }
    fill_names(room, &mut roots).await;
    ThreadRootPage {
        room_id: page.room_id.to_string(),
        roots,
        next_batch: page.next_batch,
        truncated: page.truncated,
        skipped,
    }
}

// The same decryption `Room::list_threads` applies, without push actions. A
// root this device cannot decrypt stays the encrypted event, which the
// projection shows as undecryptable, never as text.
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

/// A root row from the pinned-message projection. An event whose ID does
/// not match what it names is left out.
pub(crate) fn root_summary(
    row: PinnedMessage,
    summary: Option<ServerSummary>,
) -> Option<ThreadRootSummary> {
    let kind = match row.kind {
        PinnedMessageKind::Message => ThreadRootKind::Message,
        PinnedMessageKind::Redacted => ThreadRootKind::Redacted,
        PinnedMessageKind::Undecryptable => ThreadRootKind::Undecryptable,
        PinnedMessageKind::Unsupported => ThreadRootKind::Unsupported,
        PinnedMessageKind::Unavailable => return None,
    };
    let message = kind == ThreadRootKind::Message;
    let summary = summary.unwrap_or_default();
    Some(ThreadRootSummary {
        root_id: row.event_id,
        kind,
        sender_name: row.sender_id.clone(),
        sender_id: row.sender_id,
        timestamp_ms: row.timestamp_ms,
        excerpt: if message {
            plain_text(&row.text, MAX_THREAD_EXCERPT_CHARS)
        } else {
            String::new()
        },
        attachment: row.attachment.filter(|_| message),
        edited: message && row.edited,
        authenticity_warning: message && row.authenticity_warning,
        latest: summary.latest.map(|(timestamp_ms, sender)| {
            let sender_id = plain_text(sender.as_str(), MAX_SENDER_ID_CHARS);
            ThreadLatestReply {
                timestamp_ms,
                sender_name: sender_id.clone(),
                sender_id,
            }
        }),
        replies: summary.replies,
        participated: summary.participated,
    })
}

/// Display names from this device's member store only, within a short
/// budget; a sender without one keeps the user ID.
async fn fill_names(room: &Room, roots: &mut [ThreadRootSummary]) {
    let deadline = Instant::now() + NAMES_TIMEOUT;
    let mut names = HashMap::<String, Option<String>>::new();
    for root in roots.iter_mut() {
        if let Some(name) = cached_name(room, &mut names, &root.sender_id, deadline).await {
            root.sender_name = name;
        }
        if let Some(latest) = root.latest.as_mut()
            && let Some(name) = cached_name(room, &mut names, &latest.sender_id, deadline).await
        {
            latest.sender_name = name;
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
                .map(|name| plain_text(name, MAX_THREAD_SENDER_CHARS))
                .filter(|name| !name.is_empty()),
            _ => None,
        },
        Err(_) => None,
    };
    names.insert(sender_id.to_owned(), name.clone());
    name
}

/// One `/threads` request on the wire: `base` (the account client's own
/// configuration) with no HTTP retry, no automatic authentication refresh
/// and replay (an expired token returns its own 401, a fixed category), the
/// page timeout and the response cap.
fn page_config(base: RequestConfig) -> RequestConfig {
    base.retry_limit(0)
        .timeout(PAGE_TIMEOUT)
        .max_response_bytes(MAX_PAGE_RESPONSE_BYTES)
        .disable_auth_refresh()
}

// A fixed category only; never reflected server text. A homeserver without
// `/threads` answers M_UNRECOGNIZED, or a bare 404/405 without an error code;
// a room it does not know answers with its own error code instead.
fn thread_failure(error: &HttpError, paging: bool) -> ThreadListError {
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        return ThreadListError::TooLarge;
    }
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => return ThreadListError::RateLimited,
        Some(ErrorKind::Unrecognized) => return ThreadListError::Unsupported,
        Some(ErrorKind::InvalidParam) if paging => return ThreadListError::InvalidToken,
        _ => {}
    }
    let Some(error) = error.as_client_api_error() else {
        return ThreadListError::Unavailable;
    };
    match error.status_code.as_u16() {
        403 => ThreadListError::Forbidden,
        429 => ThreadListError::RateLimited,
        404 | 405 if error.error_kind().is_none() => ThreadListError::Unsupported,
        _ => ThreadListError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{event_id, room_id};
    use serde_json::json;

    fn raw(value: serde_json::Value) -> Raw<AnyTimelineEvent> {
        Raw::from_json(serde_json::value::to_raw_value(&value).unwrap())
    }

    fn text_root(id: &str, room: &str, body: &str) -> serde_json::Value {
        json!({
            "type": "m.room.message",
            "event_id": id,
            "room_id": room,
            "sender": "@ana:example.org",
            "origin_server_ts": 1_700_000_000_000_u64,
            "content": { "msgtype": "m.text", "body": body },
        })
    }

    fn project(value: serde_json::Value, encrypted: bool) -> Option<ThreadRootSummary> {
        let raw = raw(value);
        let id = raw.get_field::<OwnedEventId>("event_id").unwrap().unwrap();
        let summary = server_summary(&raw);
        let event = TimelineEvent::from_plaintext(raw.cast());
        root_summary(classify(&id, &event, encrypted), summary)
    }

    #[test]
    fn a_page_keeps_one_page_of_this_rooms_roots_once_each() {
        let room = room_id!("!room:example.org");
        let mut chunk = Vec::new();
        for index in 0..(THREAD_PAGE_LIMIT as usize + 5) {
            chunk.push(raw(text_root(
                &format!("$root{index}:example.org"),
                room.as_str(),
                "hello",
            )));
        }
        // Inside the page: a duplicate, another room's event, no event ID.
        chunk[1] = raw(text_root("$root0:example.org", room.as_str(), "again"));
        chunk[2] = raw(text_root(
            "$elsewhere:example.org",
            "!other:example.org",
            "x",
        ));
        chunk[3] = raw(json!({ "type": "m.room.message", "room_id": room.as_str() }));
        let page = bound_page(room, chunk, Some("next-token".into()));
        assert!(page.truncated, "more events than a page");
        assert_eq!(page.skipped, 2);
        assert_eq!(page.roots.len(), THREAD_PAGE_LIMIT as usize - 3);
        let ids: HashSet<_> = page.roots.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(ids.len(), page.roots.len());
        assert_eq!(page.next_batch.as_deref(), Some("next-token"));
    }

    #[test]
    fn an_unusable_token_ends_paging_and_is_reported() {
        let room = room_id!("!room:example.org");
        for token in ["", "line\nbreak", &"x".repeat(5000)] {
            let page = bound_page(room, Vec::new(), Some(token.to_owned()));
            assert_eq!(page.next_batch, None);
            assert!(page.truncated);
        }
        let page = bound_page(room, Vec::new(), None);
        assert_eq!(page.next_batch, None);
        assert!(!page.truncated, "no token is the end, not truncation");
    }

    #[test]
    fn an_oversized_root_is_skipped_unread() {
        let room = room_id!("!room:example.org");
        let big = "x".repeat(MAX_ROOT_BYTES);
        let page = bound_page(
            room,
            vec![raw(text_root("$big:example.org", room.as_str(), &big))],
            None,
        );
        assert!(page.roots.is_empty());
        assert_eq!(page.skipped, 1);
    }

    #[test]
    fn root_text_is_bounded_single_line_plain_text() {
        let body = format!("first\u{202e}line\n\u{200b}second {}", "y".repeat(1000));
        let root = project(text_root("$a:example.org", "!r:example.org", &body), false).unwrap();
        assert_eq!(root.kind, ThreadRootKind::Message);
        assert!(root.excerpt.starts_with("first line second"));
        assert!(root.excerpt.chars().count() <= MAX_THREAD_EXCERPT_CHARS);
        assert!(!root.excerpt.contains(['\n', '\u{202e}', '\u{200b}']));
        assert!(!root.authenticity_warning, "unencrypted room, no claim");
    }

    #[test]
    fn an_undecryptable_root_never_shows_its_ciphertext() {
        let encrypted = json!({
            "type": "m.room.encrypted",
            "event_id": "$secret:example.org",
            "room_id": "!r:example.org",
            "sender": "@ana:example.org",
            "origin_server_ts": 1_700_000_000_000_u64,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AwgAEpAB-CIPHERTEXT-MUST-NOT-SHOW",
                "device_id": "DEVICE",
                "sender_key": "c2VuZGVyS2V5c2VuZGVyS2V5c2VuZGVyS2V5c2VuZA",
                "session_id": "c2Vzc2lvbklkc2Vzc2lvbklkc2Vzc2lvbklkc2Vzcw",
            },
        });
        let root = project(encrypted, true).unwrap();
        assert_eq!(root.kind, ThreadRootKind::Undecryptable);
        assert_eq!(root.excerpt, "");
        assert_eq!(root.attachment, None);
        assert!(!format!("{root:?}").contains("CIPHERTEXT"));
    }

    #[test]
    fn a_plaintext_root_in_an_encrypted_room_carries_a_warning() {
        let root = project(text_root("$a:example.org", "!r:example.org", "hi"), true).unwrap();
        assert!(root.authenticity_warning);
    }

    #[test]
    fn latest_reply_comes_only_from_the_servers_summary_envelope() {
        let mut value = text_root("$a:example.org", "!r:example.org", "root");
        value["unsigned"] = json!({
            "m.relations": { "m.thread": {
                "latest_event": {
                    "type": "m.room.encrypted",
                    "event_id": "$reply:example.org",
                    "sender": "@bo:example.org",
                    "origin_server_ts": 1_700_000_100_000_u64,
                    "content": { "ciphertext": "REPLY-CIPHERTEXT", "body": "REPLY-BODY" },
                },
                "count": 7,
                "current_user_participated": true,
            } },
        });
        let root = project(value, false).unwrap();
        let latest = root.latest.clone().unwrap();
        assert_eq!(latest.timestamp_ms, 1_700_000_100_000);
        assert_eq!(latest.sender_id, "@bo:example.org");
        assert_eq!(root.replies, Some(7));
        assert!(root.participated);
        let shown = format!("{root:?}");
        assert!(!shown.contains("REPLY-CIPHERTEXT") && !shown.contains("REPLY-BODY"));
    }

    #[test]
    fn without_a_summary_nothing_about_replies_is_invented() {
        let root = project(text_root("$a:example.org", "!r:example.org", "root"), false).unwrap();
        assert_eq!(root.latest, None);
        assert_eq!(root.replies, None);
        assert!(!root.participated);
        // A malformed summary is the same as none.
        let mut value = text_root("$b:example.org", "!r:example.org", "root");
        value["unsigned"] = json!({ "m.relations": { "m.thread": {
            "latest_event": { "sender": "not a user id" }, "count": 3,
        } } });
        let root = project(value, false).unwrap();
        assert_eq!((root.latest, root.replies), (None, None));
    }

    #[test]
    fn a_redacted_root_keeps_only_its_sender_and_time() {
        let value = json!({
            "type": "m.room.message",
            "event_id": "$gone:example.org",
            "room_id": "!r:example.org",
            "sender": "@ana:example.org",
            "origin_server_ts": 1_700_000_000_000_u64,
            "content": {},
            "unsigned": { "redacted_because": {
                "type": "m.room.redaction",
                "event_id": "$redaction:example.org",
                "sender": "@ana:example.org",
                "origin_server_ts": 1_700_000_000_001_u64,
                "redacts": "$gone:example.org",
                "content": {},
            } },
        });
        let root = project(value, false).unwrap();
        assert_eq!(root.kind, ThreadRootKind::Redacted);
        assert_eq!(root.excerpt, "");
        assert_eq!(root.root_id, event_id!("$gone:example.org").as_str());
    }

    fn api_error(status: u16, errcode: &str) -> HttpError {
        use matrix_sdk::ruma::api::{
            client::uiaa::UiaaResponse,
            error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
        };
        let body: StandardErrorBody = serde_json::from_value(json!({
            "errcode": errcode, "error": "private reflected response"
        }))
        .unwrap();
        HttpError::Api(Box::new(FromHttpResponseError::Server(
            UiaaResponse::MatrixError(Error::new(
                reqwest::StatusCode::from_u16(status).unwrap(),
                ErrorBody::Standard(body),
            )),
        )))
    }

    /// The page is sent once: no HTTP retry, no automatic authentication
    /// refresh and replay, the page timeout and the 4 MiB cap. An expired
    /// token's 401 (returned as is, with the replay refused) and the other
    /// answers map to fixed categories that never carry the server's text.
    #[test]
    fn a_page_request_is_one_bounded_attempt_with_no_auth_replay() {
        let config = format!("{:?}", page_config(RequestConfig::new()));
        assert!(config.contains("retry_limit: 0"), "{config}");
        assert!(
            config.contains(&format!("max_response_bytes: {MAX_PAGE_RESPONSE_BYTES}")),
            "{config}"
        );
        assert!(config.contains("disable_auth_refresh: true"), "{config}");
        assert!(
            config.contains(&format!("timeout: Some({PAGE_TIMEOUT:?})")),
            "{config}"
        );
        for (status, errcode, paging, expected) in [
            (401, "M_UNKNOWN_TOKEN", false, ThreadListError::Unavailable),
            (401, "M_UNKNOWN_TOKEN", true, ThreadListError::Unavailable),
            (403, "M_FORBIDDEN", false, ThreadListError::Forbidden),
            (429, "M_LIMIT_EXCEEDED", false, ThreadListError::RateLimited),
            (404, "M_UNRECOGNIZED", false, ThreadListError::Unsupported),
            (400, "M_INVALID_PARAM", true, ThreadListError::InvalidToken),
            (500, "M_UNKNOWN", false, ThreadListError::Unavailable),
        ] {
            let failure = thread_failure(&api_error(status, errcode), paging);
            assert_eq!(failure, expected, "{status} {errcode}");
            assert!(!failure.text().contains("private"));
        }
        let too_large = HttpError::ResponseTooLarge {
            limit: MAX_PAGE_RESPONSE_BYTES,
        };
        assert_eq!(thread_failure(&too_large, false), ThreadListError::TooLarge);
    }

    #[test]
    fn errors_are_fixed_sentences_and_only_transient_ones_retry() {
        use ThreadListError::*;
        for error in [
            AccountChanged,
            InvalidRoom,
            InvalidToken,
            NotJoined,
            Forbidden,
            RateLimited,
            TooLarge,
            TimedOut,
            Unsupported,
            Unavailable,
        ] {
            assert!(!error.text().is_empty());
            assert_eq!(
                error.retryable(),
                matches!(error, RateLimited | TimedOut | Unavailable)
            );
        }
        assert!(!Unsupported.text().contains("complete"));
    }

    #[test]
    fn cached_roots_are_thread_roots_newest_first_bounded_without_reply_claims() {
        use matrix_sdk::deserialized_responses::{ThreadSummary, ThreadSummaryStatus};
        let room = room_id!("!r:example.org");
        let event = |id: &str, root: bool| {
            let mut event =
                TimelineEvent::from_plaintext(raw(text_root(id, room.as_str(), "hi")).cast());
            if root {
                event.thread_summary = ThreadSummaryStatus::Some(ThreadSummary {
                    latest_reply: None,
                    num_replies: 4,
                });
            }
            event
        };
        // The pass `rfind_map_event_in_memory_by` makes: newest first, by
        // reference, ending at the first `true`.
        let pass = |newest_first: &[TimelineEvent]| {
            let mut scan = CachedScan::default();
            let ended = newest_first.iter().any(|event| scan.visit(event));
            (scan, ended)
        };
        let events = [
            event("$new:example.org", true),
            event("$plain:example.org", false),
            event("$new:example.org", true),
            event("$old:example.org", true),
        ];
        let (scan, ended) = pass(&events);
        assert!(!ended && !scan.truncated);
        assert_eq!(scan.examined, 4);
        assert_eq!(scan.roots.len(), 2, "roots only, each once");
        let page = cached_page(room, scan, false);
        let ids: Vec<_> = page
            .roots
            .iter()
            .map(|root| root.root_id.as_str())
            .collect();
        assert_eq!(ids, ["$new:example.org", "$old:example.org"]);
        // The cache's counts may be partial: nothing about replies is said.
        assert!(
            page.roots
                .iter()
                .all(|root| root.latest.is_none() && root.replies.is_none() && !root.participated)
        );
        assert_eq!(page.next_batch, None);
        assert!(!page.truncated);

        // Looked-at events are bounded before anything is copied: a cache
        // of plain messages ends the pass at the bound with nothing copied.
        let plain: Vec<_> = (0..MAX_CACHED_EXAMINED + 5)
            .map(|n| event(&format!("$p{n}:example.org"), false))
            .collect();
        let (scan, ended) = pass(&plain);
        assert!(ended && scan.truncated);
        assert_eq!(scan.examined, MAX_CACHED_EXAMINED);
        assert!(scan.roots.is_empty());

        // So are the roots copied...
        let many: Vec<_> = (0..MAX_CACHED_THREAD_ROOTS + 3)
            .map(|n| event(&format!("$r{n}:example.org"), true))
            .collect();
        let (scan, ended) = pass(&many);
        assert!(ended && scan.truncated);
        assert_eq!(scan.roots.len(), MAX_CACHED_THREAD_ROOTS);
        assert_eq!(
            cached_page(room, scan, false).roots.len(),
            MAX_CACHED_THREAD_ROOTS
        );

        // ...and their bytes: large roots stop the pass at the byte bound,
        // and a root over the per-root bound is skipped uncopied.
        let big = |id: &str, chars: usize| {
            let mut event = TimelineEvent::from_plaintext(
                raw(text_root(id, room.as_str(), &"x".repeat(chars))).cast(),
            );
            event.thread_summary = ThreadSummaryStatus::Some(ThreadSummary {
                latest_reply: None,
                num_replies: 1,
            });
            event
        };
        let heavy: Vec<_> = (0..20)
            .map(|n| big(&format!("$b{n}:example.org"), 400 * 1024))
            .collect();
        let (scan, ended) = pass(&heavy);
        assert!(ended && scan.truncated);
        assert!(scan.roots.len() < 20);
        let copied: usize = scan
            .roots
            .iter()
            .map(|root| root.raw().json().get().len())
            .sum();
        assert!(copied <= MAX_CACHED_BYTES);
        let (scan, _) = pass(&[big("$huge:example.org", MAX_ROOT_BYTES)]);
        assert_eq!((scan.roots.len(), scan.skipped), (0, 1));
    }
}
