//! Unread navigation for one exact account selection: where this account's
//! fully-read marker is, a bounded jump back to it, and the room's
//! `m.marked_unread` flag.
//!
//! Reading the marker and the flag is local: `Room::fully_read_event_id`
//! and `Room::is_marked_unread`, as the last sync stored them. Nothing here
//! sends a read receipt, moves the fully-read marker, sends typing or joins
//! a room; opening, painting or focusing a room changes nothing.
//!
//! Where the marker falls among the conversation's rows
//! (`unread_position_selected`) comes from exact order only: this
//! selection's own live timeline (the window of items the conversation's
//! feed shows; never built here) and the room's local event cache (memory,
//! and this device's storage only to recognise a reply in a thread). Never
//! from times or counts, and no request is made. The position is the first
//! shown later message that somebody else sent: the marker may name an
//! event that is not a row (a state event, a redacted or undecryptable
//! message, a call event, an edit or a reaction), and its place is then
//! that event's own place in the cache's order. A reply in a thread (which
//! the main list does not show) is placed at its thread's first message
//! when that is loaded, and has no place in the main list otherwise.
//!
//! The jump places the marker first, and pages back through the room's live
//! timeline only while the marker is not loaded at all (at most
//! `MAX_JUMP_PAGES` pages of `JUMP_PAGE_EVENTS`; never for a recognised
//! reply in a thread), placing it again after each page once the list shows
//! what the page loaded. It is raced against the caller's cancellation and
//! the selection, and bounded in time.
//!
//! Marking a room unread (or clearing that mark) is one bounded write of the
//! room's `m.marked_unread` account data, after one bounded read of the
//! stored content. The stored content is merged lexically by the shared
//! `direct_account_data::merged_field`: every other field keeps its exact
//! raw text, and content that is not an object or names a field twice is
//! refused, never rewritten. Both requests are single attempts (no HTTP
//! retry, no token-refresh replay, capped responses). The view, the
//! selection and the caller's admission fence are checked right before the
//! write is handed over; once it is, any stop or failure is `Unknown` and
//! nothing is resent. Matrix account data has no compare-and-swap. Error
//! text is fixed.

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use futures_util::StreamExt;
use matrix_sdk::{
    Client, HttpError,
    config::RequestConfig,
    ruma::{
        EventId, OwnedRoomId, OwnedUserId,
        api::{
            client::config::{get_room_account_data, set_room_account_data},
            error::ErrorKind,
        },
        events::RoomAccountDataEventType,
        serde::Raw,
    },
};
use matrix_sdk_ui::timeline::{Timeline, TimelineItem};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{
    AccountManager, AccountSelection, MatrixCoreError, account_room,
    direct_account_data::merged_field, message_info_for, parse_event_id,
};

/// History pages one jump reads at most (each `JUMP_PAGE_EVENTS` events).
pub const MAX_JUMP_PAGES: u8 = 8;
/// Events one jump page asks for (as many as a history reveal's page).
const JUMP_PAGE_EVENTS: u16 = 50;
/// How long a jump waits after each page for the conversation's list to
/// show what the page loaded, before placing the marker again.
const JUMP_SETTLE: Duration = Duration::from_millis(500);
const JUMP_TIMEOUT: Duration = Duration::from_secs(30);
const ROOM_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest stored `m.marked_unread` content read; larger is refused.
const MAX_FLAG_CONTENT_BYTES: u64 = 16 * 1024;
const MAX_WRITE_RESPONSE_BYTES: u64 = 16 * 1024;
/// Inspect one loaded event only within this bound (as for pinned events).
const MAX_POSITION_EVENT_BYTES: usize = 512 * 1024;
/// Unread navigation never retains or routes a larger root. This is local
/// recognition, independent of Matrix links' unchanged routing policy.
const MAX_POSITION_ROOT_BYTES: usize = 255;
/// A supported ASCII ID can be entirely JSON-escaped as `\uXXXX`.
const MAX_ESCAPED_ROOT_BYTES: usize = MAX_POSITION_ROOT_BYTES * 6 + 2;

/// What this device's store knows of a room's unread state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnreadState {
    /// The exact event this account's `m.fully_read` marker names, if any.
    pub fully_read: Option<String>,
    /// The room's `m.marked_unread` flag, as synced.
    pub marked_unread: bool,
}

/// Where this account's fully-read marker falls among the conversation's
/// shown rows, from exact order only (see the module notes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnreadPosition {
    /// The account has no fully-read marker in this room.
    NoMarker,
    /// The marker's place among the loaded rows is exact. `first` is the
    /// first shown later message that somebody else sent (the "New
    /// messages" divider goes above it); `None` when every later shown
    /// message is this account's own, or none is loaded.
    Placed { first: Option<String> },
    /// The marker names a reply in a thread whose first message is not
    /// loaded or whose root is unsupported: no place in the main conversation.
    ThreadNotLoaded,
    /// The marker is loaded, but its JSON cannot be inspected or placed
    /// within the local bounds. No history page can establish its position.
    Unplaceable,
    /// The marker's event is not among what this device has loaded.
    NotLoaded,
}

/// Where a jump to the fully-read marker got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnreadJump {
    /// The account has no fully-read marker in this room.
    NoMarker,
    /// The first shown message after the marker that somebody else sent is
    /// loaded in the live timeline.
    Found { first: String },
    /// The marker is placed, but no later shown message from somebody else
    /// is loaded.
    NothingNewer,
    /// The marker names a reply in a thread that cannot be placed in the
    /// main conversation. Nothing was paged for it.
    InThread,
    /// The marker is loaded but could not be placed within the local
    /// inspection bounds. Nothing was paged; no thread or absence is inferred.
    Unplaceable,
    /// The start of the room was reached without placing it (history not
    /// kept, or the marker names an event this device cannot load).
    NotInHistory,
    /// Not within the pages read; older history was not loaded further.
    NotReached,
}

/// Why a jump did not complete. Fixed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnreadJumpError {
    #[error("The jump was stopped")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Older messages could not be loaded. Try again.")]
    Unavailable,
}

/// What a flag write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkedUnreadOutcome {
    /// The homeserver accepted the write.
    Accepted,
    /// The stored content already said so; nothing was written.
    AlreadySet,
}

/// Why a flag write is not known to have happened. Fixed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MarkedUnreadError {
    #[error("Nothing was changed.")]
    NotSent,
    #[error("The view changed or a security step began before sending. Nothing was changed.")]
    SecurityBusy,
    #[error("The selected account changed. Nothing was changed.")]
    AccountChanged,
    #[error("The room's unread mark could not be read. Nothing was changed.")]
    Unreadable,
    #[error("Your homeserver refused the change. Nothing was changed.")]
    Refused,
    #[error("No answer came back. The unread mark may have changed; it was not sent again.")]
    Unknown,
}

/// Ends a flag write that has not been handed over yet.
pub trait UnreadWriteGuard {
    /// Whether the write must not start (view closed or cancelled).
    fn stopped(&self) -> bool;
    /// Resolves when the view closes or the caller cancels.
    fn wait_stopped(&self) -> impl Future<Output = ()> + Send;
}

fn read_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(REQUEST_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_FLAG_CONTENT_BYTES)
        .disable_auth_refresh()
}

fn write_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(REQUEST_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_WRITE_RESPONSE_BYTES)
        .disable_auth_refresh()
}

/// The raw text of the flag a write puts in place.
fn flag_text(unread: bool) -> &'static str {
    if unread { "true" } else { "false" }
}

impl AccountManager {
    /// The room and its client under the exact selection; the state lock is
    /// released before this returns, and the wait for it is bounded.
    async fn unread_room(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> std::result::Result<(Client, matrix_sdk::Room), MatrixCoreError> {
        let taken = selection.guard(tokio::time::timeout(ROOM_TIMEOUT, async {
            let state = self.state.lock().await;
            if state.active_account_id.as_deref() != Some(selection.account_id()) {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            let room = account_room(&state, selection.account_id(), room_id)?;
            let client = state
                .loaded
                .get(selection.account_id())
                .map(|loaded| loaded.client.clone())
                .ok_or(MatrixCoreError::SelectedAccountChanged)?;
            Ok((client, room))
        }));
        let taken = taken.await.ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let taken = taken.map_err(|_| MatrixCoreError::Sdk("room lookup timed out".into()))??;
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(taken)
    }

    /// The room's fully-read marker and marked-unread flag from this
    /// device's store. Local; nothing is sent.
    pub async fn unread_state_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> std::result::Result<UnreadState, MatrixCoreError> {
        let (_, room) = self.unread_room(selection, room_id).await?;
        Ok(UnreadState {
            fully_read: room.fully_read_event_id().map(|id| id.to_string()),
            marked_unread: room.is_marked_unread(),
        })
    }

    /// The room's local read marker and unread flag, as sync stored them:
    /// at once when `seen` is `None` or already differs, otherwise as soon as
    /// a stored update makes them differ from `seen`. Local only; nothing is
    /// sent and no request is made. `Ok(None)` once `cancelled` resolves or
    /// the room's info stream ends; an account change is an error.
    pub async fn next_unread_state_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        seen: Option<&UnreadState>,
        cancelled: impl Future<Output = ()>,
    ) -> std::result::Result<Option<UnreadState>, MatrixCoreError> {
        let (_, room) = self.unread_room(selection, room_id).await?;
        let read = |room: &matrix_sdk::Room| UnreadState {
            fully_read: room.fully_read_event_id().map(|id| id.to_string()),
            marked_unread: room.is_marked_unread(),
        };
        // Subscribed before the first read, so no stored update is missed.
        let mut info = room.subscribe_info();
        let now = read(&room);
        if seen != Some(&now) {
            return Ok(Some(now));
        }
        tokio::pin!(cancelled);
        loop {
            tokio::select! {
                biased;
                () = &mut cancelled => return Ok(None),
                () = selection.revoked() => return Err(MatrixCoreError::SelectedAccountChanged),
                update = info.next() => {
                    if update.is_none() {
                        return Ok(None);
                    }
                    let now = read(&room);
                    if seen != Some(&now) {
                        return Ok(Some(now));
                    }
                }
            }
        }
    }

    /// This selection's own live timeline of the room as the conversation
    /// opened it, or `None`; never built here. The state lock is released
    /// before the timeline table is read, and the wait for both is bounded.
    async fn owned_timeline(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> std::result::Result<Option<Arc<Timeline>>, MatrixCoreError> {
        let taken = selection.guard(tokio::time::timeout(ROOM_TIMEOUT, async {
            let loaded = {
                let state = self.state.lock().await;
                if state.active_account_id.as_deref() != Some(selection.account_id()) {
                    return Err(MatrixCoreError::SelectedAccountChanged);
                }
                account_room(&state, selection.account_id(), room_id)?;
                state.loaded.get(selection.account_id()).cloned()
            };
            let loaded = loaded.ok_or(MatrixCoreError::SelectedAccountChanged)?;
            let timeline = loaded.timelines.lock().await.get(room_id).cloned();
            Ok(timeline)
        }));
        let taken = taken.await.ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let taken = taken.map_err(|_| MatrixCoreError::Sdk("room lookup timed out".into()))??;
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(taken)
    }

    /// Where the fully-read marker `marker` falls among the conversation's
    /// shown rows: the exact order of this selection's own live timeline
    /// and the room's local event cache only (see the module notes). Local:
    /// no request, no paging, nothing sent. A timeline the conversation has
    /// not opened is `NotLoaded`.
    pub async fn unread_position_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        marker: Option<&str>,
    ) -> std::result::Result<UnreadPosition, MatrixCoreError> {
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let Some(marker) = marker else {
            return Ok(UnreadPosition::NoMarker);
        };
        let marker = parse_event_id(marker)?;
        let Some(timeline) = self.owned_timeline(selection, room_id).await? else {
            return Ok(UnreadPosition::NotLoaded);
        };
        let placing = tokio::time::timeout(ROOM_TIMEOUT, position_in(&timeline, &marker));
        let placed = selection.guard(placing).await;
        let placed = placed.ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let placed = placed.map_err(|_| MatrixCoreError::Sdk("placing timed out".into()))?;
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(placed)
    }

    /// One page back through the room's live timeline, then a bounded wait
    /// (`JUMP_SETTLE`) until the conversation's list shows what it loaded:
    /// the SDK adds a page's items to the list after the page returns, and
    /// the marker is placed only among the list's items. A page of events
    /// that make no items changes nothing, and the wait ends at the bound.
    /// Returns whether the start of the room was reached.
    async fn jump_page(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> std::result::Result<bool, MatrixCoreError> {
        let timeline = self.owned_timeline(selection, room_id).await?;
        // Taken before the page, so the page's own update is not missed.
        let mut watched = None;
        if let Some(timeline) = &timeline {
            let (items, updates) = timeline.subscribe().await;
            watched = Some((front_of(items.front()), updates));
        }
        let reached = self
            .paginate_backwards_selected(selection, room_id, JUMP_PAGE_EVENTS)
            .await?;
        if let (Some(timeline), Some((before, updates))) = (timeline, watched) {
            tokio::pin!(updates);
            let deadline = tokio::time::Instant::now() + JUMP_SETTLE;
            loop {
                let (items, _) = timeline.subscribe().await;
                if front_of(items.front()) != before {
                    break;
                }
                match tokio::time::timeout_at(deadline, updates.next()).await {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        Ok(reached)
    }

    /// Place the fully-read marker among the conversation's rows; only
    /// while it is not loaded at all, page back one page at a time (at most
    /// `MAX_JUMP_PAGES`) and place it again. A recognised reply in a thread
    /// is never paged for. Sends no receipt and moves no marker.
    /// `cancelled` resolving stops it at once.
    pub async fn jump_to_fully_read_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancelled: impl Future<Output = ()>,
    ) -> std::result::Result<UnreadJump, UnreadJumpError> {
        let work = async {
            let state = self.unread_state_selected(selection, room_id).await;
            let state = state.map_err(jump_error)?;
            let Some(marker) = state.fully_read else {
                return Ok(UnreadJump::NoMarker);
            };
            let mut start_reached = false;
            for page in 0..=MAX_JUMP_PAGES {
                let position = self.unread_position_selected(selection, room_id, Some(&marker));
                match position.await.map_err(jump_error)? {
                    UnreadPosition::Placed { first: Some(first) } => {
                        return Ok(UnreadJump::Found { first });
                    }
                    UnreadPosition::Placed { first: None } => return Ok(UnreadJump::NothingNewer),
                    UnreadPosition::ThreadNotLoaded => return Ok(UnreadJump::InThread),
                    UnreadPosition::Unplaceable => return Ok(UnreadJump::Unplaceable),
                    UnreadPosition::NoMarker => return Ok(UnreadJump::NoMarker),
                    UnreadPosition::NotLoaded => {}
                }
                if start_reached {
                    return Ok(UnreadJump::NotInHistory);
                }
                if page == MAX_JUMP_PAGES {
                    break;
                }
                let paged = self.jump_page(selection, room_id).await;
                start_reached = paged.map_err(jump_error)?;
            }
            Ok(UnreadJump::NotReached)
        };
        let result = tokio::select! {
            biased;
            () = cancelled => return Err(UnreadJumpError::Cancelled),
            result = tokio::time::timeout(JUMP_TIMEOUT, work) => {
                result.unwrap_or(Err(UnreadJumpError::Unavailable))
            }
        };
        if !selection.is_current() {
            return Err(UnreadJumpError::AccountChanged);
        }
        result
    }

    /// Set (`unread` true) or clear the room's `m.marked_unread` flag: one
    /// bounded read of the stored content, then at most one bounded write
    /// of it with only `unread` changed. `admitted` (the app's navigation
    /// and security fence) and `guard` are checked right before the write
    /// is handed over; after that, any stop or failure is Unknown.
    pub async fn set_marked_unread_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        unread: bool,
        admitted: &(dyn Fn() -> bool + Send + Sync),
        guard: &impl UnreadWriteGuard,
    ) -> std::result::Result<MarkedUnreadOutcome, MarkedUnreadError> {
        // The caller's click-time fence is checked before and after every
        // wait and right before the hand-off; it is never re-captured.
        let fenced = || {
            if guard.stopped() {
                Err(MarkedUnreadError::NotSent)
            } else if !selection.is_current() {
                Err(MarkedUnreadError::AccountChanged)
            } else if !admitted() {
                Err(MarkedUnreadError::SecurityBusy)
            } else {
                Ok(())
            }
        };
        fenced()?;
        let lookup = self.unread_room(selection, room_id);
        let found = tokio::select! {
            biased;
            () = guard.wait_stopped() => return Err(MarkedUnreadError::NotSent),
            found = lookup => found,
        };
        let (client, room) = found.map_err(|error| match error {
            MatrixCoreError::SelectedAccountChanged => MarkedUnreadError::AccountChanged,
            _ => MarkedUnreadError::NotSent,
        })?;
        fenced()?;
        let user: OwnedUserId = client
            .user_id()
            .ok_or(MarkedUnreadError::NotSent)?
            .to_owned();
        let room_id: OwnedRoomId = room.room_id().to_owned();
        // One bounded read of what is stored now.
        let request = get_room_account_data::v3::Request::new(
            user.clone(),
            room_id.clone(),
            RoomAccountDataEventType::MarkedUnread,
        );
        let read = async {
            client
                .send(request)
                .with_request_config(read_config())
                .await
        };
        let read = tokio::select! {
            biased;
            () = guard.wait_stopped() => return Err(MarkedUnreadError::NotSent),
            read = read => read,
        };
        fenced()?;
        let stored = match read {
            Ok(response) => Some(response.account_data.json().get().to_owned()),
            Err(error) if not_found(&error) => None,
            Err(_) => return Err(MarkedUnreadError::Unreadable),
        };
        let merged = merged_field(stored.as_deref(), "unread", flag_text(unread))
            .map_err(|_| MarkedUnreadError::Unreadable)?;
        let Some(merged) = merged else {
            return Ok(MarkedUnreadOutcome::AlreadySet);
        };
        let raw = serde_json::value::RawValue::from_string(merged)
            .map_err(|_| MarkedUnreadError::Unreadable)?;
        let request = set_room_account_data::v3::Request::new_raw(
            user,
            room_id,
            RoomAccountDataEventType::MarkedUnread,
            Raw::from_json(raw),
        );
        // The last checks before the hand-off.
        fenced()?;
        let sent = async {
            client
                .send(request)
                .with_request_config(write_config())
                .await
        };
        // Once handed over, closing the view or the window no longer ends
        // the wait: the bounded request (15 s, no retry or replay) keeps its
        // own answer. Only an account change stops waiting, and then the
        // result is unknown and never resent.
        let sent = tokio::select! {
            biased;
            () = selection.revoked() => return Err(MarkedUnreadError::Unknown),
            sent = sent => sent,
        };
        match sent {
            Ok(_) => Ok(MarkedUnreadOutcome::Accepted),
            Err(error) if refused_by_server(&error) => Err(MarkedUnreadError::Refused),
            Err(_) => Err(MarkedUnreadError::Unknown),
        }
    }
}

fn not_found(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|api| {
        api.status_code.as_u16() == 404 && api.error_kind() == Some(&ErrorKind::NotFound)
    })
}

/// A client error: the server answered and did not apply the request.
fn refused_by_server(error: &HttpError) -> bool {
    error
        .as_client_api_error()
        .is_some_and(|api| api.status_code.is_client_error())
}

fn jump_error(error: MatrixCoreError) -> UnreadJumpError {
    match error {
        MatrixCoreError::SelectedAccountChanged => UnreadJumpError::AccountChanged,
        _ => UnreadJumpError::Unavailable,
    }
}

/// Keep a known thread with an unsupported root distinct from a nonthread
/// event. Unreadable loaded JSON is neither proof of a thread nor absence.
#[derive(Clone, Debug, PartialEq, Eq)]
enum UnreadThread {
    NonThread,
    Root(String),
    Unsupported,
    Unreadable,
}

fn checked_unread_root(root: &str) -> UnreadThread {
    if <&EventId>::try_from(root).is_err() {
        return UnreadThread::Unreadable;
    }
    if root.len() > MAX_POSITION_ROOT_BYTES {
        // Validate by borrowing; never clone or retain an unsupported ID.
        return UnreadThread::Unsupported;
    }
    UnreadThread::Root(root.to_owned())
}

/// Borrow the event, content and relation before decoding only bounded string
/// fields. Unknown fields (including large message bodies) are not cloned.
/// Ambiguous, malformed or over-bound JSON is loaded but unplaceable, so it
/// cannot become a false main-list boundary or provoke history paging.
fn unread_thread(event_json: &str) -> UnreadThread {
    #[derive(Deserialize)]
    struct Event<'a> {
        #[serde(borrow)]
        content: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct Content<'a> {
        #[serde(rename = "m.relates_to", borrow)]
        relates_to: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct Relation<'a> {
        #[serde(borrow)]
        rel_type: Option<&'a RawValue>,
        #[serde(borrow)]
        event_id: Option<&'a RawValue>,
    }
    if event_json.len() > MAX_POSITION_EVENT_BYTES {
        return UnreadThread::Unreadable;
    }
    let Ok(event) = serde_json::from_str::<Event<'_>>(event_json) else {
        return UnreadThread::Unreadable;
    };
    let Some(content) = event.content else {
        return UnreadThread::NonThread;
    };
    let Ok(content) = serde_json::from_str::<Content<'_>>(content.get()) else {
        return UnreadThread::Unreadable;
    };
    let Some(relation) = content.relates_to else {
        return UnreadThread::NonThread;
    };
    let Ok(relation) = serde_json::from_str::<Relation<'_>>(relation.get()) else {
        return UnreadThread::Unreadable;
    };
    let Some(kind) = relation.rel_type else {
        return UnreadThread::NonThread;
    };
    // Eight ASCII characters, even when entirely escaped, need at most 50
    // bytes including quotes. A longer token cannot be "m.thread".
    if kind.get().len() > 8 * 6 + 2 {
        return UnreadThread::NonThread;
    }
    let Ok(kind) = serde_json::from_str::<String>(kind.get()) else {
        return UnreadThread::Unreadable;
    };
    if kind != "m.thread" {
        return UnreadThread::NonThread;
    }
    let Some(root) = relation.event_id else {
        return UnreadThread::Unreadable;
    };
    let raw = root.get();
    if !raw.as_bytes().contains(&b'\\') {
        // Plain JSON strings can be borrowed even when the valid ID exceeds
        // our supported bound. This branch never allocates that larger ID.
        return serde_json::from_str::<&str>(raw)
            .map(checked_unread_root)
            .unwrap_or(UnreadThread::Unreadable);
    }
    // Escaping must not allocate a giant temporary String. Every supported
    // root fits this bound; a larger escaped token stays honestly unplaceable.
    if raw.len() > MAX_ESCAPED_ROOT_BYTES {
        return UnreadThread::Unreadable;
    }
    serde_json::from_str::<String>(raw)
        .map(|root| checked_unread_root(&root))
        .unwrap_or(UnreadThread::Unreadable)
}

/// The same classifier for a reply retained only in this device's store.
/// This is a cache lookup only; no request or timeline is created.
async fn stored_unread_thread(room: &matrix_sdk::Room, event: &EventId) -> UnreadThread {
    let Ok((cache, _handles)) = room.event_cache().await else {
        return UnreadThread::NonThread;
    };
    let Ok(Some(found)) = cache.find_event(event).await else {
        return UnreadThread::NonThread;
    };
    unread_thread(found.raw().json().get())
}

/// The first item's own ID: it changes when a page's items are added.
fn front_of(first: Option<&Arc<TimelineItem>>) -> Option<String> {
    first.map(|item| item.unique_id().0.clone())
}

/// What placing reads of one item of the conversation's list, in timeline
/// order.
#[derive(Clone, Copy, Debug)]
struct ItemRef<'a> {
    /// Its event, when it has one (not a virtual item or an unsent echo).
    event_id: Option<&'a str>,
    /// Sent by this account: its sender is exactly the selection's user ID.
    own: bool,
}

/// Where an event is, relative to the list's items.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Found {
    /// It is the item at this index.
    Item(usize),
    /// Loaded but not an item: the items from this index on come after it
    /// in the cache's exact order (`None`: no item does).
    Before(Option<usize>),
    /// A reply in the thread of this root.
    Thread(String),
    /// A known thread with a root outside the local navigation bounds.
    UnsupportedThread,
    /// Loaded JSON that could not be inspected or placed within the bounds.
    Unplaceable,
    /// Not loaded on this device, as far as the search went.
    Unknown,
}

/// Walks the room's loaded events newest first, up to `target`, noting the
/// nearest later event that is an item of the list.
struct Walk<'a> {
    target: &'a str,
    items: &'a HashMap<&'a str, usize>,
    next_item: Option<usize>,
}

impl Walk<'_> {
    /// One loaded event (its ID and raw JSON), newest first. `Some` at the
    /// target, classifying its relation without losing known unsupported
    /// threads; only the target's own JSON is parsed.
    fn step(&mut self, event_id: &str, json: &str) -> Option<UnreadThread> {
        if event_id == self.target {
            return Some(unread_thread(json));
        }
        if let Some(&index) = self.items.get(event_id) {
            self.next_item = Some(index);
        }
        None
    }
}

/// The first shown row at or after `from` that somebody else sent. `shown`
/// is asked only about others' items with an event, in order, until one is
/// a row.
fn first_of_others(
    items: &[ItemRef<'_>],
    from: usize,
    shown: impl Fn(usize) -> bool,
) -> Option<String> {
    items
        .iter()
        .enumerate()
        .skip(from)
        .find(|(index, item)| !item.own && item.event_id.is_some() && shown(*index))
        .and_then(|(_, item)| item.event_id.map(str::to_owned))
}

/// The first later row of others for an event that `found` places among the
/// items; `None` when it is not placed.
fn placed(
    items: &[ItemRef<'_>],
    found: &Found,
    shown: impl Fn(usize) -> bool,
) -> Option<Option<String>> {
    match found {
        Found::Item(at) => Some(first_of_others(items, at + 1, shown)),
        Found::Before(Some(at)) => Some(first_of_others(items, *at, shown)),
        Found::Before(None) => Some(None),
        Found::Thread(_) | Found::UnsupportedThread | Found::Unplaceable | Found::Unknown => None,
    }
}

/// Where `target` is relative to the list's items: an item itself, or an
/// event in the room's in-memory event cache, placed by the cache's exact
/// order (the list is built from that same order).
async fn find_loaded(room: &matrix_sdk::Room, items: &[ItemRef<'_>], target: &str) -> Found {
    if let Some(at) = items.iter().position(|item| item.event_id == Some(target)) {
        return Found::Item(at);
    }
    let Ok((cache, _handles)) = room.event_cache().await else {
        return Found::Unknown;
    };
    let ids: HashMap<&str, usize> = items
        .iter()
        .enumerate()
        .filter_map(|(at, item)| Some((item.event_id?, at)))
        .collect();
    let mut walk = Walk {
        target,
        items: &ids,
        next_item: None,
    };
    let hit = cache
        .rfind_map_event_in_memory_by(|event| {
            let id = event.event_id()?;
            walk.step(id.as_str(), event.raw().json().get())
        })
        .await;
    match hit {
        Ok(Some(UnreadThread::Root(root))) => Found::Thread(root),
        Ok(Some(UnreadThread::Unsupported)) => Found::UnsupportedThread,
        Ok(Some(UnreadThread::Unreadable)) => Found::Unplaceable,
        Ok(Some(UnreadThread::NonThread)) => Found::Before(walk.next_item),
        Ok(None) | Err(_) => Found::Unknown,
    }
}

/// The marker's position in `timeline` (see `UnreadPosition`). Local only:
/// no request and no paging.
async fn position_in(timeline: &Timeline, marker: &EventId) -> UnreadPosition {
    let room = timeline.room();
    let own = room.own_user_id();
    // Exactly the window of items the conversation's feed shows (older
    // items the SDK still skips are not rows yet); the stream is not kept.
    let (items, _) = timeline.subscribe().await;
    let refs: Vec<ItemRef<'_>> = items
        .iter()
        .map(|item| match item.as_event() {
            Some(event) => ItemRef {
                event_id: event.event_id().map(EventId::as_str),
                own: event.sender() == own,
            },
            None => ItemRef {
                event_id: None,
                own: false,
            },
        })
        .collect();
    // A row is exactly what the conversation shows (`message_info_for`).
    let shown = |index: usize| message_info_for(&items[index], Some(own)).is_some();
    let place = |found: &Found| {
        let first = placed(&refs, found, &shown)?;
        Some(UnreadPosition::Placed { first })
    };
    let root = match find_loaded(room, &refs, marker.as_str()).await {
        Found::Thread(root) => root,
        Found::UnsupportedThread => return UnreadPosition::ThreadNotLoaded,
        Found::Unplaceable => return UnreadPosition::Unplaceable,
        // Not in memory: this device's storage may still know it as a reply
        // in a thread, which no page of the main list would ever show.
        Found::Unknown => match stored_unread_thread(room, marker).await {
            UnreadThread::Root(root) => root,
            UnreadThread::Unsupported => return UnreadPosition::ThreadNotLoaded,
            UnreadThread::Unreadable => return UnreadPosition::Unplaceable,
            UnreadThread::NonThread => return UnreadPosition::NotLoaded,
        },
        found => return place(&found).unwrap_or(UnreadPosition::NotLoaded),
    };
    let root = find_loaded(room, &refs, &root).await;
    place(&root).unwrap_or(UnreadPosition::ThreadNotLoaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag is merged lexically: every other field keeps its exact raw
    /// text (numbers beyond u64, decimals, nested data), the flag gets
    /// exactly `true` or `false`, a matching flag writes nothing, and
    /// content Morpheus could not keep exactly is refused, not rewritten.
    #[test]
    fn the_flag_is_merged_without_touching_other_fields() {
        let stored = concat!(
            r#"{"org.example.n":123456789012345678901234567890,"#,
            r#""unread":false,"x":1.10}"#
        );
        let set = merged_field(Some(stored), "unread", flag_text(true))
            .unwrap()
            .unwrap();
        assert_eq!(
            set,
            r#"{"org.example.n":123456789012345678901234567890,"unread":true,"x":1.10}"#
        );
        assert_eq!(
            merged_field(Some(&set), "unread", flag_text(true)),
            Ok(None)
        );
        assert_eq!(
            merged_field(None, "unread", flag_text(true))
                .unwrap()
                .as_deref(),
            Some(r#"{"unread":true}"#)
        );
        // A string "true" is not the boolean: it is replaced by the flag.
        let stringy = r#"{"unread":"true"}"#;
        assert_eq!(
            merged_field(Some(stringy), "unread", flag_text(true))
                .unwrap()
                .as_deref(),
            Some(r#"{"unread":true}"#)
        );
        // Before sync the local flag may still say false after an accepted
        // mark: the clear's request is built from the server's stored
        // content, so it still writes exactly `false`, keeping other fields.
        let accepted = r#"{"unread":true,"org.example.n":1.10}"#;
        assert_eq!(
            merged_field(Some(accepted), "unread", flag_text(false))
                .unwrap()
                .as_deref(),
            Some(r#"{"org.example.n":1.10,"unread":false}"#)
        );
        let source = include_str!("unread_navigation.rs").replace("\r\n", "\n");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let (_, writer) = production
            .split_once("pub async fn set_marked_unread_selected(")
            .unwrap();
        let writer = writer.split("\n    }\n").next().unwrap();
        assert!(
            !writer.contains("is_marked_unread"),
            "the cached flag never decides a write"
        );
        for refused in [
            r#"{"unread":true,"unread":false}"#,
            "[]",
            r#""text""#,
            "not json",
        ] {
            assert!(merged_field(Some(refused), "unread", flag_text(false)).is_err());
        }
    }

    #[test]
    fn requests_are_bounded_single_attempts_without_auth_replay() {
        for config in [read_config(), write_config()] {
            let shown = format!("{config:?}");
            assert!(shown.contains("retry_limit: 0") || shown.contains("retry_limit: Some(0)"));
            assert!(shown.contains("force_auth: true"));
            assert!(shown.contains("max_response_bytes"));
            assert!(shown.contains("disable_auth_refresh: true"), "{shown}");
        }
        // After the hand-off only an account change ends the wait; closing
        // the view does not turn a sent write into Unknown.
        let source = include_str!("unread_navigation.rs").replace("\r\n", "\n");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let (_, handed) = production
            .split_once("// The last checks before the hand-off.")
            .unwrap();
        let handed = handed.split("match sent {").next().unwrap();
        assert!(!handed.contains("guard.wait_stopped()"));
        assert!(handed.contains("selection.revoked()"));
    }

    struct Guard(std::sync::atomic::AtomicBool);

    impl UnreadWriteGuard for Guard {
        fn stopped(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
            std::future::pending::<()>()
        }
    }

    /// Nothing is read or written for a superseded selection or a stopped
    /// view, and a jump for a superseded selection reports it. No
    /// homeserver is contacted.
    #[test]
    fn stopped_or_superseded_work_sends_nothing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let name = format!("morpheus-unread-{}", uuid::Uuid::new_v4());
            let root = std::env::temp_dir().join(name);
            let manager = AccountManager::open(&root).unwrap();
            let id = "account".to_owned();
            manager.selections.replace(Some(&id));
            let stale = manager.account_selection(&id).unwrap();
            manager.selections.replace(None);
            manager.selections.replace(Some(&id));
            let open = || true;
            let running = Guard(std::sync::atomic::AtomicBool::new(false));
            let stopped = Guard(std::sync::atomic::AtomicBool::new(true));
            let room = "!room:example.org";
            let refused = manager
                .set_marked_unread_selected(&stale, room, true, &open, &stopped)
                .await;
            assert_eq!(refused, Err(MarkedUnreadError::NotSent));
            let refused = manager
                .set_marked_unread_selected(&stale, room, true, &open, &running)
                .await;
            assert_eq!(refused, Err(MarkedUnreadError::AccountChanged));
            // A held navigation or security revocation (the click's fence no
            // longer holds) sends nothing, even for the current selection.
            let current = manager.account_selection(&id).unwrap();
            let revoked = || false;
            let refused = manager
                .set_marked_unread_selected(&current, room, false, &revoked, &running)
                .await;
            assert_eq!(refused, Err(MarkedUnreadError::SecurityBusy));
            // The local marker watch ends with the selection; it never waits.
            let watched = manager
                .next_unread_state_selected(&stale, room, None, std::future::pending())
                .await;
            assert!(matches!(
                watched,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            // Placing the marker ends with the selection too, marker or not.
            for marker in [Some("$marker:example.org"), None] {
                let position = manager.unread_position_selected(&stale, room, marker).await;
                assert!(matches!(
                    position,
                    Err(MatrixCoreError::SelectedAccountChanged)
                ));
            }
            let jump = manager
                .jump_to_fully_read_selected(&stale, room, std::future::pending())
                .await;
            assert_eq!(jump, Err(UnreadJumpError::AccountChanged));
            let jump = manager
                .jump_to_fully_read_selected(&stale, room, std::future::ready(()))
                .await;
            assert_eq!(jump, Err(UnreadJumpError::Cancelled));
            manager.selections.replace(None);
            drop(manager);
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// Items of a list for placing: an event ID (empty for a virtual item)
    /// and whether this account sent it.
    fn list<'a>(items: &[(&'a str, bool)]) -> Vec<ItemRef<'a>> {
        items
            .iter()
            .map(|&(event_id, own)| ItemRef {
                event_id: (!event_id.is_empty()).then_some(event_id),
                own,
            })
            .collect()
    }

    /// U6 and U3: the divider goes above the first shown later message that
    /// somebody else sent, wherever the marker is. This account's own rows,
    /// events that are not rows and virtual items are passed over; when
    /// every later shown message is this account's own there is none. Only
    /// others' items with an event are asked whether they are rows.
    #[test]
    fn the_divider_marks_the_first_later_row_somebody_else_sent() {
        let items = list(&[
            // 0: a row of others, where the marker is.
            ("$a:example.org", false),
            // 1: a virtual item (a date divider).
            ("", false),
            // 2: this account's own row.
            ("$mine:example.org", true),
            // 3: a state event of others: no row.
            ("$join:example.org", false),
            // 4: the first later row of others.
            ("$b:example.org", false),
            ("$c:example.org", false),
        ]);
        let rows = [0, 2, 4, 5];
        let asked = std::cell::RefCell::new(Vec::new());
        let shown = |index: usize| {
            asked.borrow_mut().push(index);
            rows.contains(&index)
        };
        let first = |found: Found| placed(&items, &found, &shown);
        let b = Some(Some("$b:example.org".to_owned()));
        assert_eq!(first(Found::Item(0)), b);
        assert_eq!(*asked.borrow(), [3, 4], "not own, virtual or earlier");
        // The marker on an event that is not a row: its own place.
        assert_eq!(first(Found::Item(3)), b);
        // Loaded but not an item (an edit, a reaction): the items after it.
        assert_eq!(first(Found::Before(Some(2))), b);
        let c = Some(Some("$c:example.org".to_owned()));
        assert_eq!(first(Found::Before(Some(5))), c);
        // No later item, or only this account's own rows after it.
        assert_eq!(first(Found::Before(None)), Some(None));
        assert_eq!(first(Found::Item(5)), Some(None));
        let own_after = list(&[
            ("$a:example.org", false),
            ("$mine:example.org", true),
            ("$again:example.org", true),
        ]);
        assert_eq!(placed(&own_after, &Found::Item(0), |_| true), Some(None));
        // A reply in a thread or an unknown event has no place by itself.
        assert_eq!(first(Found::Thread("$root:example.org".into())), None);
        assert_eq!(first(Found::Unknown), None);
    }

    /// Walks `loaded` (newest first) to `target` as the cache walk does,
    /// every event with the raw JSON `json`: what it found, and the nearest
    /// later item.
    fn walk_to(
        loaded: &[&str],
        items: &HashMap<&str, usize>,
        target: &str,
        json: &str,
    ) -> (Option<Option<String>>, Option<usize>) {
        let mut walk = Walk {
            target,
            items,
            next_item: None,
        };
        let hit = loaded
            .iter()
            .find_map(|id| walk.step(id, json))
            .map(|found| match found {
                UnreadThread::NonThread => None,
                UnreadThread::Root(root) => Some(root),
                UnreadThread::Unsupported | UnreadThread::Unreadable => {
                    panic!("this ordinary-order fixture must be placeable")
                }
            });
        (hit, walk.next_item)
    }

    /// U3: an event that is not an item (an edit, a reaction, a redaction) is
    /// placed by the room cache's exact order, walked newest first, never by
    /// its time: the nearest later item starts what comes after it. A reply
    /// in a thread names its root instead.
    #[test]
    fn a_marker_outside_the_list_is_placed_by_the_caches_order() {
        let items = HashMap::from([("$a:x.org", 0), ("$b:x.org", 1), ("$c:x.org", 2)]);
        let loaded = [
            "$reaction2:x.org",
            "$c:x.org",
            "$reaction:x.org",
            "$b:x.org",
            "$edit:x.org",
            "$reply:x.org",
            "$a:x.org",
        ];
        let event = |ts: u64, rel_type: &str| {
            let relation = format!(r#"{{"rel_type":"{rel_type}","event_id":"$a:x.org"}}"#);
            let content = format!(r#"{{"m.relates_to":{relation}}}"#);
            format!(r#"{{"origin_server_ts":{ts},"content":{content}}}"#)
        };
        let early = event(1, "m.replace");
        let late = event(9_999_999_999_999, "m.replace");
        let edit = walk_to(&loaded, &items, "$edit:x.org", &early);
        assert_eq!(edit, (Some(None), Some(1)));
        let later = walk_to(&loaded, &items, "$edit:x.org", &late);
        assert_eq!(later, edit, "not by time");
        let reply = event(1, "m.thread");
        let thread = walk_to(&loaded, &items, "$reply:x.org", &reply);
        assert_eq!(thread, (Some(Some("$a:x.org".to_owned())), Some(1)));
        // The newest event: nothing after it is an item.
        let newest = walk_to(&loaded, &items, "$reaction2:x.org", "{}");
        assert_eq!(newest, (Some(None), None));
        // Not in memory: nothing is found; the oldest item was passed last.
        let gone = walk_to(&loaded, &items, "$gone:x.org", "{}");
        assert_eq!(gone, (None, Some(0)));
    }

    /// A short valid reply ID can name a valid root beyond the routing bound.
    /// Keep that known thread out of the main list even though no bounded root
    /// can be retained. The memory walk and stored-only path share this exact
    /// borrowed classifier; ambiguous JSON stays unplaceable, not "in a thread".
    #[test]
    fn unsupported_thread_roots_do_not_become_main_list_boundaries() {
        let items = HashMap::from([("$before:x.org", 0), ("$after:x.org", 1)]);
        let reply = "$reply:x.org";
        assert!(<&EventId>::try_from(reply).is_ok());
        let root = format!("${}", "a".repeat(299));
        assert!(
            <&EventId>::try_from(root.as_str()).is_ok(),
            "valid SDK event ID"
        );
        assert!(root.len() > MAX_POSITION_ROOT_BYTES);
        let threaded = |root: &str| {
            format!(
                r#"{{"content":{{"m.relates_to":{{"rel_type":"m.thread","event_id":{root}}}}}}}"#
            )
        };
        let json = threaded(&format!("\"{root}\""));
        let mut walk = Walk {
            target: reply,
            items: &items,
            next_item: None,
        };
        assert_eq!(walk.step("$after:x.org", "{}"), None);
        assert_eq!(walk.next_item, Some(1));
        assert_eq!(walk.step(reply, &json), Some(UnreadThread::Unsupported));
        assert_eq!(
            unread_thread(&json),
            UnreadThread::Unsupported,
            "stored-only classifier"
        );
        assert_eq!(
            placed(
                &list(&[("$before:x.org", false), ("$after:x.org", false)]),
                &Found::UnsupportedThread,
                |_| true,
            ),
            None
        );
        // Escaped relation names and IDs are decoded within fixed bounds.
        let escaped = r#"{"content":{"m.relates_to":{"rel_type":"m\u002ethread","event_id":"\u0024before:x.org"}}}"#;
        assert_eq!(
            unread_thread(escaped),
            UnreadThread::Root("$before:x.org".into())
        );
        let escaped_long = threaded(&format!("\"\\u0024{}\"", "a".repeat(299)));
        assert_eq!(unread_thread(&escaped_long), UnreadThread::Unsupported);
        let boundary = format!("${}", "a".repeat(MAX_POSITION_ROOT_BYTES - 1));
        assert_eq!(
            unread_thread(&threaded(&format!("\"{boundary}\""))),
            UnreadThread::Root(boundary.clone())
        );
        let escaped_boundary = format!(
            "\"\\u0024{}\"",
            "\\u0061".repeat(MAX_POSITION_ROOT_BYTES - 1)
        );
        assert_eq!(
            unread_thread(&threaded(&escaped_boundary)),
            UnreadThread::Root(boundary)
        );
        for nonthread in [
            "{}",
            r#"{"content":{}}"#,
            r#"{"content":{"m.relates_to":null}}"#,
            r#"{"content":{"m.relates_to":{"event_id":"$before:x.org"}}}"#,
            r#"{"content":{"m.relates_to":{"rel_type":"m.replace","event_id":"$before:x.org"}}}"#,
        ] {
            assert_eq!(unread_thread(nonthread), UnreadThread::NonThread);
        }
        for ambiguous in [
            "not json",
            r#"{"content":[]}"#,
            r#"{"content":{"m.relates_to":[]}}"#,
            r#"{"content":{"m.relates_to":{"rel_type":"m.thread"}}}"#,
            r#"{"content":{"m.relates_to":{"rel_type":"m.thread","event_id":"not-an-id"}}}"#,
            r#"{"content":{"m.relates_to":{"rel_type":"m.thread","event_id":7}}}"#,
            r#"{"content":{"m.relates_to":{"rel_type":"m.thread","rel_type":"m.replace","event_id":"$before:x.org"}}}"#,
        ] {
            assert_eq!(unread_thread(ambiguous), UnreadThread::Unreadable);
        }
        let oversized = format!(
            r#"{{"content":{{"body":"{}"}}}}"#,
            "a".repeat(MAX_POSITION_EVENT_BYTES)
        );
        assert_eq!(unread_thread(&oversized), UnreadThread::Unreadable);
        let oversized_escape = threaded(&format!(
            "\"\\u0024{}\"",
            "\\u0061".repeat(MAX_POSITION_ROOT_BYTES)
        ));
        assert_eq!(unread_thread(&oversized_escape), UnreadThread::Unreadable);
        assert_eq!(
            placed(
                &list(&[("$after:x.org", false)]),
                &Found::Unplaceable,
                |_| true,
            ),
            None
        );
    }

    /// U3: placing is local (no page, request or new list) and own rows are
    /// the selection's exact user ID. The jump pages only while the marker is
    /// not loaded at all, never for a recognised reply in a thread, and no
    /// longer through the reveal that looked only for the marker's own item.
    #[test]
    fn placing_is_local_and_a_reply_in_a_thread_is_never_paged_for() {
        let source = include_str!("unread_navigation.rs").replace("\r\n", "\n");
        // Git's Windows checkout may use CRLF. Check the same production
        // boundaries and every assertion under both checkout representations.
        for source in [source.clone(), source.replace('\n', "\r\n")] {
            let source = source.replace("\r\n", "\n");
            let production = source.split("#[cfg(test)]").next().unwrap();
            let (_, placing) = production
                .split_once("/// The first item's own ID")
                .unwrap();
            let (_, position) = production
                .split_once("pub async fn unread_position_selected(")
                .unwrap();
            let position = position.split("\n    }\n").next().unwrap();
            let requests = [
                "paginate",
                ".send(",
                "timeline_builder",
                ".timeline(",
                "member",
                "keys",
            ];
            for text in [placing, position] {
                for request in requests {
                    assert!(!text.contains(request), "{request}");
                }
            }
            assert!(placing.contains("let own = room.own_user_id();"));
            assert!(placing.contains("own: event.sender() == own,"));
            let (_, jump) = production
                .split_once("pub async fn jump_to_fully_read_selected(")
                .unwrap();
            let jump = jump.split("\n    }\n").next().unwrap();
            assert!(!jump.contains("reveal_event_selected"));
            let page = jump.find("self.jump_page(").unwrap();
            for before in [
                "UnreadPosition::ThreadNotLoaded => return Ok(UnreadJump::InThread)",
                "UnreadPosition::Unplaceable => return Ok(UnreadJump::Unplaceable)",
                "UnreadPosition::Placed { first: None } => return Ok(UnreadJump::NothingNewer)",
                "if start_reached {",
            ] {
                assert!(jump.find(before).unwrap() < page, "{before}");
            }
        }
    }
}
