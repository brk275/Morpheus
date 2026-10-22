//! Account-scoped local message search. The SDK indexes decrypted events in
//! each room store; the query and filters never leave this device and no
//! homeserver search endpoint is used. Loading a matched event the local
//! store lacks may still fetch that event, by its ID, from the account's
//! homeserver over the authenticated session.
//! Nothing here logs a query, a result or an account identifier.

use std::collections::{BinaryHeap, HashMap, VecDeque};

use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    message_search::SearchFilter as IndexFilter,
    room::Room,
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, UInt, UserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            room::message::{MessageType, Relation, SyncRoomMessageEvent},
        },
    },
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineEventFocusThreadMode, TimelineFocus};
use serde::{Deserialize, Serialize};

use crate::{
    AccountManager, AccountSelection, MatrixCoreError, MessageInfo, Result, selected_loaded_for,
};

const MAX_SEARCH_BYTES: usize = 256;
const MAX_RESULT_BODY_CHARS: usize = 500;
/// Most hits one page may examine; the UI asks for fewer.
const MAX_SEARCH_PAGE: usize = 50;
/// Deepest index cursor a page may start from or advance to.
const MAX_SEARCH_OFFSET: usize = 10_000;

/// Most bytes of a sender filter: a full Matrix user ID.
const MAX_SENDER_BYTES: usize = 255;
/// Index hits a filtered page may examine to fill itself. Filters are
/// checked against each loaded hit, so a narrow filter reads further into
/// the index per page, always within this bound and the cursor cap.
const MAX_FILTERED_PAGE: usize = 200;
/// The latest date the index can hold (tantivy keeps nanoseconds in an
/// i64; indexing clamps to the same limit). A later `since` is clamped to it
/// for the index, never dropped; loaded hits are still checked exactly.
const MAX_INDEXED_MILLIS: u64 = (i64::MAX / 1_000_000).cast_unsigned();

/// Whether a search keeps messages with an attachment. The index holds an
/// attachment's file name and caption, never the file's contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AttachmentFilter {
    #[default]
    Any,
    /// Only image, video, audio and file messages.
    With,
    /// Only messages without an attachment.
    Without,
}

/// Optional narrowing of a search. The sender and date bounds also narrow the
/// index itself through typed queries (see index_filter); every filter is
/// then checked against each loaded hit on this device. The index schema is
/// unchanged and the filter values never leave the device (loading a hit the
/// store lacks may fetch that event by ID from the homeserver). Debug output
/// is redacted, since a filter can name a person or a time.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    /// Only messages from this exact Matrix user ID.
    pub sender: Option<String>,
    /// Only messages sent at or after this time, in milliseconds since the
    /// Unix epoch.
    pub since_ms: Option<u64>,
    /// Only messages sent at or before this time.
    pub until_ms: Option<u64>,
    pub attachment: AttachmentFilter,
}

impl std::fmt::Debug for SearchFilters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SearchFilters([redacted])")
    }
}

impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        self.sender.is_none()
            && self.since_ms.is_none()
            && self.until_ms.is_none()
            && self.attachment == AttachmentFilter::Any
    }

    /// A sender or date is set, so a search without words can be driven by
    /// the index. Attachment presence is known only once a hit is loaded.
    pub fn indexed(&self) -> bool {
        self.sender.is_some() || self.since_ms.is_some() || self.until_ms.is_some()
    }

    /// The index applies the sender and both date bounds as typed queries.
    /// Replacements use their original's normalized timestamp, including
    /// edits older than the original. Loaded hits still enforce millisecond
    /// bounds and attachment presence. The app's persistence cohort rejects
    /// stores from builds using the previous edit-date indexing semantics.
    fn index_filter(&self) -> IndexFilter {
        let sender = self
            .sender
            .as_deref()
            .and_then(|sender| UserId::parse(sender).ok());
        let since = self
            .since_ms
            .map(|since| since.min(MAX_INDEXED_MILLIS))
            .and_then(UInt::new)
            .map(MilliSecondsSinceUnixEpoch);
        IndexFilter {
            sender,
            since,
            until: self
                .until_ms
                .map(|until| until.min(MAX_INDEXED_MILLIS))
                .and_then(UInt::new)
                .map(MilliSecondsSinceUnixEpoch),
        }
    }

    /// A malformed filter is refused before any index work, never treated
    /// as matching nothing.
    pub fn validate(&self) -> Result<()> {
        if let Some(sender) = &self.sender {
            if sender.len() > MAX_SENDER_BYTES || UserId::parse(sender.as_str()).is_err() {
                return Err(MatrixCoreError::InvalidSearchQuery(
                    "sender must be a full Matrix user ID".into(),
                ));
            }
        }
        if let (Some(since), Some(until)) = (self.since_ms, self.until_ms) {
            if since > until {
                return Err(MatrixCoreError::InvalidSearchQuery(
                    "the date range ends before it starts".into(),
                ));
            }
        }
        Ok(())
    }

    fn admits(&self, result: &MessageSearchResult) -> bool {
        let attachment = match self.attachment {
            AttachmentFilter::Any => true,
            AttachmentFilter::With => result.has_attachment,
            AttachmentFilter::Without => !result.has_attachment,
        };
        let (sender, since, until) = (self.sender.as_deref(), self.since_ms, self.until_ms);
        attachment
            && sender.is_none_or(|sender| sender == result.sender)
            && since.is_none_or(|since| result.timestamp_ms >= since)
            && until.is_none_or(|until| result.timestamp_ms <= until)
    }

    /// Index hits one page may examine for `wanted` results.
    fn page_allowance(&self, wanted: usize) -> usize {
        if self.is_empty() {
            wanted
        } else {
            MAX_FILTERED_PAGE
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageSearchResult {
    pub room_id: String,
    /// The room's display name when the hit was loaded. Presentation only.
    #[serde(default)]
    pub room_name: String,
    pub event_id: String,
    /// Root event of the thread this message belongs to, when it is a thread
    /// reply. The UI opens that thread to show the hit in context.
    #[serde(default)]
    pub thread_root: Option<String>,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub relevance_score: f32,
    /// An image, video, audio or file message (matched by its file name or
    /// caption).
    #[serde(default)]
    pub has_attachment: bool,
}

/// One bounded page of hits. `next_offset` is the index cursor for the next
/// page, counting every indexed hit examined, including ones that are not
/// shown (redacted, non-message or unloadable events), so pages never repeat
/// or skip a hit. It is `None` when the index holds no further hits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSearchPage {
    pub results: Vec<MessageSearchResult>,
    pub next_offset: Option<usize>,
    /// Hits on this page whose event could not be loaded from the store or
    /// the homeserver. They are counted, never silently claimed as no match.
    pub unavailable: usize,
    /// Hits on this page that were loaded but did not match the filters.
    pub filtered: usize,
}

impl AccountManager {
    /// Search decrypted events already indexed in one joined room. Older
    /// history becomes searchable when the client loads or paginates it.
    /// Results from a previously selected account are discarded.
    pub async fn search_messages_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MessageSearchResult>> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let page = self
            .search_messages_page_selected(&selection, room_id, query, limit, offset)
            .await?;
        Ok(page.results)
    }

    /// Search locally indexed messages across the selected account's joined
    /// rooms. No query text is sent to the homeserver. The room ID in each
    /// result lets the UI open its conversation context.
    pub async fn search_all_messages_for(
        &self,
        expected_account_id: &str,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MessageSearchResult>> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let page = self
            .search_all_messages_page_selected(&selection, query, limit, offset)
            .await?;
        Ok(page.results)
    }

    /// One page of one room's hits under the exact account selection the
    /// caller captured. Switching, removal or stop revokes the wait, even when
    /// the same local account is selected again (A -> B -> A).
    pub async fn search_messages_page_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<MessageSearchPage> {
        let filters = SearchFilters::default();
        self.search_messages_filtered_selected(selection, room_id, query, &filters, limit, offset)
            .await
    }

    /// As search_messages_page_selected, keeping only hits that match
    /// `filters`. Malformed filters are refused before any index work.
    pub async fn search_messages_filtered_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
        offset: usize,
    ) -> Result<MessageSearchPage> {
        filters.validate()?;
        let query = validate_terms(query, filters)?;
        let room = {
            let state = selection
                .guard(self.state.lock())
                .await
                .ok_or(MatrixCoreError::SelectedAccountChanged)?;
            self.ensure_search_owner(selection)?;
            selected_loaded_for(&state, selection.account_id())?;
            super::account_room(&state, selection.account_id(), room_id)?
        };
        let page = selection
            .guard(room_page(&room, query.as_deref(), filters, limit, offset))
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)??;
        self.ensure_search_owner(selection)?;
        Ok(page)
    }

    /// One page of hits across every joined room of the captured selection,
    /// ordered by relevance across rooms.
    pub async fn search_all_messages_page_selected(
        &self,
        selection: &AccountSelection,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<MessageSearchPage> {
        let filters = SearchFilters::default();
        self.search_all_messages_filtered_selected(selection, query, &filters, limit, offset)
            .await
    }

    /// As search_all_messages_page_selected, keeping only hits that match
    /// `filters`. Malformed filters are refused before any index work.
    pub async fn search_all_messages_filtered_selected(
        &self,
        selection: &AccountSelection,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
        offset: usize,
    ) -> Result<MessageSearchPage> {
        filters.validate()?;
        let query = validate_terms(query, filters)?;
        let client = {
            let state = selection
                .guard(self.state.lock())
                .await
                .ok_or(MatrixCoreError::SelectedAccountChanged)?;
            self.ensure_search_owner(selection)?;
            selected_loaded_for(&state, selection.account_id())?
                .client
                .clone()
        };
        let search = global_page(&client, query.as_deref(), filters, limit, offset);
        let page = selection
            .guard(search)
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)??;
        self.ensure_search_owner(selection)?;
        Ok(page)
    }

    /// Load a bounded event-focused snapshot around a search result, including
    /// older history not present in the live timeline. The UI can show this
    /// context before it supports an exact scroll position in its live list.
    pub async fn search_result_context_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
        num_context_events: u16,
    ) -> Result<Vec<MessageInfo>> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        self.search_result_context_selected(&selection, room_id, event_id, num_context_events)
            .await
    }

    /// The event-focused snapshot for one hit under the captured selection.
    /// A threaded hit yields its thread's events around the hit.
    pub async fn search_result_context_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        event_id: &str,
        num_context_events: u16,
    ) -> Result<Vec<MessageInfo>> {
        let target = super::parse_event_id(event_id)?;
        let room = {
            let state = selection
                .guard(self.state.lock())
                .await
                .ok_or(MatrixCoreError::SelectedAccountChanged)?;
            self.ensure_search_owner(selection)?;
            selected_loaded_for(&state, selection.account_id())?;
            super::account_room(&state, selection.account_id(), room_id)?
        };
        let context = selection
            .guard(async move {
                let timeline = room
                    .timeline_builder()
                    .with_focus(TimelineFocus::Event {
                        target,
                        num_context_events: num_context_events.min(20),
                        thread_mode: TimelineEventFocusThreadMode::Automatic {
                            hide_threaded_events: true,
                        },
                    })
                    .build()
                    .await
                    .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
                let (items, _) = timeline.subscribe().await;
                let context = items
                    .iter()
                    .filter_map(|item| super::message_info(item.as_ref()))
                    .collect::<Vec<_>>();
                Ok::<_, MatrixCoreError>(context)
            })
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)??;
        self.ensure_search_owner(selection)?;
        Ok(context)
    }

    /// The captured selection is still this manager's current selection
    /// epoch. Matching account IDs alone never revive a revoked request.
    fn ensure_search_owner(&self, selection: &AccountSelection) -> Result<()> {
        if selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| selection.same_epoch(&current))
        {
            Ok(())
        } else {
            Err(MatrixCoreError::SelectedAccountChanged)
        }
    }
}

fn validate_query(query: &str) -> Result<&str> {
    let query = query.trim();
    if query.is_empty() || query.len() > MAX_SEARCH_BYTES || query.chars().any(char::is_control) {
        Err(MatrixCoreError::InvalidSearchQuery(
            "search must be 1–256 bytes of plain text".into(),
        ))
    } else {
        Ok(query)
    }
}

/// The words of a search, or none for a filters-only search, which needs a
/// sender or date filter the index can apply.
fn validate_terms(query: &str, filters: &SearchFilters) -> Result<Option<String>> {
    if query.trim().is_empty() && filters.indexed() {
        Ok(None)
    } else {
        validate_query(query).map(|query| Some(query.to_owned()))
    }
}

fn index_error(error: impl std::fmt::Display) -> MatrixCoreError {
    MatrixCoreError::Sdk(error.to_string())
}

/// One page of a room's hits. `query` is `None` for a filters-only search.
/// Sender and both date bounds narrow the index through typed queries (never
/// query text). Loaded hits enforce exact millisecond bounds and attachment
/// presence against the original message's normalized timestamp.
async fn room_page(
    room: &Room,
    query: Option<&str>,
    filters: &SearchFilters,
    limit: usize,
    offset: usize,
) -> Result<MessageSearchPage> {
    let offset = offset.min(MAX_SEARCH_OFFSET);
    let wanted = limit.clamp(1, MAX_SEARCH_PAGE);
    // No page examines a hit at or beyond the advertised cursor cap.
    let allowance = filters
        .page_allowance(wanted)
        .min(MAX_SEARCH_OFFSET - offset);
    if allowance == 0 {
        return Ok(MessageSearchPage::default());
    }
    // One extra hit tells whether another page exists without showing it.
    let fetch = allowance + 1;
    let index_filter = filters.index_filter();
    let searched = match query {
        Some(query) if index_filter.is_empty() => room.search(query, fetch, Some(offset)).await,
        _ => {
            room.search_filtered(query, &index_filter, fetch, Some(offset))
                .await
        }
    };
    let hits = searched.map_err(index_error)?.events;
    let fetched = hits.len();
    let mut page = PageBuilder::default();
    let mut examined = 0;
    for (relevance_score, event_id) in hits.into_iter().take(allowance) {
        // A filtered page stops once full; its cursor resumes after the
        // last hit it examined.
        if page.results.len() == wanted {
            break;
        }
        examined += 1;
        page.add(room, &event_id, relevance_score, filters).await;
    }
    let next = offset + examined;
    let next_offset = (fetched > examined && next < MAX_SEARCH_OFFSET).then_some(next);
    Ok(page.finish(next_offset))
}

async fn global_page(
    client: &Client,
    query: Option<&str>,
    filters: &SearchFilters,
    limit: usize,
    offset: usize,
) -> Result<MessageSearchPage> {
    // Without words, a typed filters-only search merges rooms newest first.
    let Some(query) = query else {
        return newest_page(client, filters, limit, offset).await;
    };
    let wanted = limit.clamp(1, MAX_SEARCH_PAGE);
    let allowance = filters.page_allowance(wanted);
    let offset = offset.min(MAX_SEARCH_OFFSET);
    // The cursor cap is exhausted: no hit at or beyond it is examined.
    if offset == MAX_SEARCH_OFFSET {
        return Ok(MessageSearchPage::default());
    }
    let mut pages = Box::pin(client.search_messages(query.to_owned()).build());
    let mut page = PageBuilder::default();
    // Hits consumed from the start of the merged, relevance-ordered stream.
    let mut position = 0;
    let mut examined = 0;
    let mut more = false;
    'pages: while let Some(hits) = pages.next().await {
        for (room_id, relevance_score, event_id) in hits.map_err(index_error)? {
            if position < offset {
                position += 1;
                continue;
            }
            if examined == allowance
                || page.results.len() == wanted
                || position >= MAX_SEARCH_OFFSET
            {
                more = position < MAX_SEARCH_OFFSET;
                break 'pages;
            }
            position += 1;
            examined += 1;
            if let Some(room) = client.get_room(&room_id) {
                page.add(&room, &event_id, relevance_score, filters).await;
            }
        }
    }
    Ok(page.finish(more.then_some(position)))
}

/// Events buffered across all rooms while merging rooms newest first. Each
/// room reads windows of TOTAL_WINDOW_BUDGET / rooms events, clamped to
/// 1..=MAX_ROOM_WINDOW, so at most max(rooms, TOTAL_WINDOW_BUDGET) events are
/// buffered, plus one heap head per room. No room is ever left out.
const TOTAL_WINDOW_BUDGET: usize = 512;
const MAX_ROOM_WINDOW: usize = 32;
/// Rooms initialized, or merged hits skipped, between cooperative yields,
/// so a revoked selection's guard can drop the page promptly.
const YIELD_EVERY: usize = 32;

/// Events one room reads per window when `rooms` rooms are merged.
fn window_size(rooms: usize) -> usize {
    (TOTAL_WINDOW_BUDGET / rooms.max(1)).clamp(1, MAX_ROOM_WINDOW)
}

/// One page of a filters-only search across every joined room. Each room's
/// index yields typed, newest-first windows of (indexed date, event). One
/// live head per room sits in a max heap ordered by date, equal dates going
/// to the lower room ID, so every page merges in the same order. The cursor
/// counts merged hits from the newest, like global_page: each page re-merges
/// from the start and never passes the cursor cap. Every joined room takes
/// part; none is dropped. Every filter is then checked again on each loaded
/// hit.
async fn newest_page(
    client: &Client,
    filters: &SearchFilters,
    limit: usize,
    offset: usize,
) -> Result<MessageSearchPage> {
    let wanted = limit.clamp(1, MAX_SEARCH_PAGE);
    let allowance = filters.page_allowance(wanted);
    let offset = offset.min(MAX_SEARCH_OFFSET);
    if offset == MAX_SEARCH_OFFSET {
        return Ok(MessageSearchPage::default());
    }
    let index_filter = filters.index_filter();
    let mut rooms = client.joined_rooms();
    rooms.sort_by(|a, b| a.room_id().cmp(b.room_id()));
    let size = window_size(rooms.len());
    let windows = rooms.into_iter().map(|room| RoomWindow::new(room, size));
    let mut windows = windows.collect::<Vec<_>>();
    let mut heads = BinaryHeap::with_capacity(windows.len());
    for (room, window) in windows.iter_mut().enumerate() {
        if let Some(date) = window.head(&index_filter).await? {
            heads.push(Head { date, room });
        }
        if room % YIELD_EVERY == YIELD_EVERY - 1 {
            tokio::task::yield_now().await;
        }
    }
    let mut page = PageBuilder::default();
    let mut position = 0;
    let mut examined = 0;
    let mut more = false;
    while let Some(Head { room, .. }) = heads.pop() {
        let window = &mut windows[room];
        let Some((_, event_id)) = window.events.pop_front() else {
            continue;
        };
        if let Some(date) = window.head(&index_filter).await? {
            heads.push(Head { date, room });
        }
        if position < offset {
            position += 1;
            if position % YIELD_EVERY == 0 {
                tokio::task::yield_now().await;
            }
            continue;
        }
        if examined == allowance || page.results.len() == wanted || position >= MAX_SEARCH_OFFSET {
            more = position < MAX_SEARCH_OFFSET;
            break;
        }
        position += 1;
        examined += 1;
        let room = windows[room].room.clone();
        page.add(&room, &event_id, 0.0, filters).await;
    }
    Ok(page.finish(more.then_some(position)))
}

/// One room's live head in the merge: the max heap pops the newest date,
/// and on equal dates the lower room position (sorted room ID).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Head {
    date: MilliSecondsSinceUnixEpoch,
    room: usize,
}

impl Ord for Head {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| other.room.cmp(&self.room))
    }
}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// One room's place in a newest-first merge: a bounded window of its typed
/// index hits and where its next window starts.
struct RoomWindow {
    room: Room,
    events: VecDeque<(MilliSecondsSinceUnixEpoch, OwnedEventId)>,
    next: usize,
    /// Events read per window (see window_size).
    size: usize,
    done: bool,
}

impl RoomWindow {
    fn new(room: Room, size: usize) -> Self {
        Self {
            room,
            events: VecDeque::new(),
            next: 0,
            size,
            done: false,
        }
    }

    /// The room's newest unmerged indexed date, reading its next window
    /// when the current one is used up.
    async fn head(&mut self, filter: &IndexFilter) -> Result<Option<MilliSecondsSinceUnixEpoch>> {
        if self.events.is_empty() && !self.done && self.next < MAX_SEARCH_OFFSET {
            let window = self
                .room
                .search_newest(filter, self.size, Some(self.next))
                .await
                .map_err(index_error)?;
            self.done = window.len() < self.size;
            self.next += window.len();
            self.events.extend(window);
        }
        Ok(self.events.front().map(|(date, _)| *date))
    }
}

/// Index failures end a page with an error before this builder is used. An
/// event that cannot be loaded only counts as unavailable, so the page keeps
/// its cursor and later hits stay reachable. A loaded hit that the filters
/// reject only counts as filtered.
#[derive(Default)]
struct PageBuilder {
    results: Vec<MessageSearchResult>,
    room_names: HashMap<String, String>,
    unavailable: usize,
    filtered: usize,
}

impl PageBuilder {
    async fn add(
        &mut self,
        room: &Room,
        event_id: &EventId,
        relevance_score: f32,
        filters: &SearchFilters,
    ) {
        match load_search_result(room, event_id, relevance_score).await {
            Ok(Some(result)) if !filters.admits(&result) => self.filtered += 1,
            Ok(Some(mut result)) => {
                result.room_name = self.room_name(room).await;
                self.results.push(result);
            }
            Ok(None) => {}
            Err(_) => self.unavailable += 1,
        }
    }

    async fn room_name(&mut self, room: &Room) -> String {
        let room_id = room.room_id().as_str();
        if let Some(name) = self.room_names.get(room_id) {
            return name.clone();
        }
        let name = room
            .display_name()
            .await
            .map(|name| name.to_string())
            .unwrap_or_else(|_| room_id.to_owned());
        self.room_names.insert(room_id.to_owned(), name.clone());
        name
    }

    fn finish(self, next_offset: Option<usize>) -> MessageSearchPage {
        MessageSearchPage {
            results: self.results,
            next_offset,
            unavailable: self.unavailable,
            filtered: self.filtered,
        }
    }
}

fn thread_root_of<C>(relation: Option<&Relation<C>>) -> Option<String> {
    match relation {
        Some(Relation::Thread(thread)) => Some(thread.event_id.to_string()),
        _ => None,
    }
}

async fn load_search_result(
    room: &Room,
    event_id: &EventId,
    relevance_score: f32,
) -> Result<Option<MessageSearchResult>> {
    let event = room
        .load_or_fetch_event(event_id, None)
        .await
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let event_timestamp_ms = event
        .timestamp()
        .map(|time| u64::from(time.get()))
        .unwrap_or(0);
    let raw = event
        .raw()
        .deserialize()
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncRoomMessageEvent::Original(event),
    )) = raw
    else {
        return Ok(None);
    };
    // An edit's attachment is the one its new content carries.
    let msgtype = match &event.content.relates_to {
        Some(Relation::Replacement(replacement)) => &replacement.new_content.msgtype,
        _ => &event.content.msgtype,
    };
    let has_attachment = is_attachment(msgtype);
    // The SDK indexes a replacement under the edit's event ID, but the
    // timeline presents the original event with the new content applied.
    // Return that visible event ID so opening/highlighting the hit works.
    let (visible_event_id, thread_root, sender, timestamp_ms, body) =
        if let Some(Relation::Replacement(replacement)) = &event.content.relates_to {
            let original = room
                .load_or_fetch_event(&replacement.event_id, None)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            let original_timestamp_ms = original
                .timestamp()
                .map(|time| u64::from(time.get()))
                .unwrap_or(0);
            let original_raw = original
                .raw()
                .deserialize()
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncRoomMessageEvent::Original(original),
            )) = original_raw
            else {
                return Ok(None);
            };
            if original.sender != event.sender {
                return Ok(None);
            }
            (
                replacement.event_id.to_string(),
                // An edit keeps the thread of the message it replaces.
                thread_root_of(original.content.relates_to.as_ref()),
                original.sender.to_string(),
                original_timestamp_ms,
                replacement.new_content.msgtype.body(),
            )
        } else {
            (
                event.event_id.to_string(),
                thread_root_of(event.content.relates_to.as_ref()),
                event.sender.to_string(),
                event_timestamp_ms,
                event.content.msgtype.body(),
            )
        };
    let body = body.chars().take(MAX_RESULT_BODY_CHARS).collect();
    Ok(Some(MessageSearchResult {
        room_id: room.room_id().to_string(),
        room_name: String::new(),
        event_id: visible_event_id,
        thread_root,
        sender,
        body,
        timestamp_ms,
        relevance_score,
        has_attachment,
    }))
}

/// Image, video, audio and file messages carry an attachment.
fn is_attachment(msgtype: &MessageType) -> bool {
    match msgtype {
        MessageType::Image(_) | MessageType::Video(_) => true,
        MessageType::Audio(_) | MessageType::File(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(sender: &str, timestamp_ms: u64, has_attachment: bool) -> MessageSearchResult {
        MessageSearchResult {
            room_id: "!room:example.org".into(),
            room_name: String::new(),
            event_id: "$event:example.org".into(),
            thread_root: None,
            sender: sender.into(),
            body: "plans".into(),
            timestamp_ms,
            relevance_score: 1.0,
            has_attachment,
        }
    }

    #[test]
    fn filters_are_bounded_validated_and_never_printed() {
        let sender = "@casey:example.org";
        let filters = SearchFilters {
            sender: Some(sender.into()),
            since_ms: Some(1_000),
            until_ms: Some(2_000),
            attachment: AttachmentFilter::With,
        };
        filters.validate().unwrap();
        assert_eq!(format!("{filters:?}"), "SearchFilters([redacted])");
        assert!(filters.admits(&hit(sender, 1_000, true)));
        assert!(filters.admits(&hit(sender, 2_000, true)));
        assert!(!filters.admits(&hit(sender, 999, true)));
        assert!(!filters.admits(&hit(sender, 2_001, true)));
        assert!(!filters.admits(&hit(sender, 1_500, false)));
        assert!(!filters.admits(&hit("@casey:example.com", 1_500, true)));
        let without = SearchFilters {
            attachment: AttachmentFilter::Without,
            ..SearchFilters::default()
        };
        assert!(without.admits(&hit(sender, 1, false)));
        assert!(!without.admits(&hit(sender, 1, true)));

        // A malformed sender or a reversed range is refused, not matched.
        let long = format!("@{}:example.org", "x".repeat(255));
        for sender in ["casey", "@casey", long.as_str()] {
            let filters = SearchFilters {
                sender: Some(sender.to_owned()),
                ..SearchFilters::default()
            };
            assert!(matches!(
                filters.validate(),
                Err(MatrixCoreError::InvalidSearchQuery(_))
            ));
        }
        let reversed = SearchFilters {
            since_ms: Some(2),
            until_ms: Some(1),
            ..SearchFilters::default()
        };
        assert!(reversed.validate().is_err());

        // Unfiltered pages examine exactly what they show; filtered pages
        // read further, within a fixed bound.
        let empty = SearchFilters::default();
        assert!(empty.is_empty() && empty.admits(&hit(sender, 1, true)));
        assert_eq!(empty.page_allowance(25), 25);
        assert_eq!(filters.page_allowance(25), MAX_FILTERED_PAGE);
    }

    #[test]
    fn filters_only_searches_need_an_indexed_filter_and_until_stays_on_loaded_hits() {
        let sender = SearchFilters {
            sender: Some("@casey:example.org".into()),
            ..SearchFilters::default()
        };
        assert_eq!(validate_terms("  ", &sender).unwrap(), None);
        let words = validate_terms(" plans ", &sender).unwrap();
        assert_eq!(words.as_deref(), Some("plans"));
        // Attachment presence is only known after loading: it cannot drive
        // an index search on its own, and nothing at all is no search.
        let attachment_only = SearchFilters {
            attachment: AttachmentFilter::With,
            ..SearchFilters::default()
        };
        assert!(validate_terms("", &attachment_only).is_err());
        assert!(validate_terms("", &SearchFilters::default()).is_err());

        // Both date bounds are typed index restrictions using the original
        // message's normalized date, then checked to the exact millisecond.
        let dated = SearchFilters {
            since_ms: Some(1_500),
            until_ms: Some(9_000),
            ..sender.clone()
        };
        let index = dated.index_filter();
        let indexed_sender = index.sender.as_ref().map(|sender| sender.as_str());
        assert_eq!(indexed_sender, Some("@casey:example.org"));
        let since = index.since.map(|since| u64::from(since.get()));
        assert_eq!(since, Some(1_500));
        assert_eq!(index.until.map(|until| u64::from(until.get())), Some(9_000));
        assert!(SearchFilters::default().index_filter().is_empty());
        let until_only = SearchFilters {
            until_ms: Some(9_000),
            ..SearchFilters::default()
        };
        assert!(until_only.indexed() && !until_only.index_filter().is_empty());
        // A `since` past the latest indexable date is clamped, never dropped.
        let far = SearchFilters {
            since_ms: Some(u64::MAX),
            ..SearchFilters::default()
        };
        let since = far.index_filter().since.map(|since| u64::from(since.get()));
        assert_eq!(since, Some(MAX_INDEXED_MILLIS));
        let far_until = SearchFilters {
            until_ms: Some(u64::MAX),
            ..SearchFilters::default()
        };
        assert_eq!(
            far_until
                .index_filter()
                .until
                .map(|until| u64::from(until.get())),
            Some(MAX_INDEXED_MILLIS)
        );
    }

    #[test]
    fn rooms_merge_newest_first_with_a_stable_tie_order() {
        let at = |millis: u64| MilliSecondsSinceUnixEpoch(UInt::new(millis).unwrap());
        let mut heads = BinaryHeap::new();
        for (room, millis) in [(0, 1_000), (1, 5_000), (2, 3_000), (3, 5_000)] {
            heads.push(Head {
                date: at(millis),
                room,
            });
        }
        // Newest first; equal dates go to the lower room position (sorted
        // room ID), so every page of a filters-only all-room search merges
        // in the same order.
        let order = std::iter::from_fn(|| heads.pop().map(|head| head.room)).collect::<Vec<_>>();
        assert_eq!(order, [1, 3, 2, 0]);
        // Window sizes share one budget: at most max(rooms, budget) events are
        // buffered, every room keeps at least one, none gets more than 32.
        assert_eq!(window_size(0), MAX_ROOM_WINDOW);
        assert_eq!(window_size(16), MAX_ROOM_WINDOW);
        assert_eq!(window_size(64), 8);
        assert_eq!(window_size(TOTAL_WINDOW_BUDGET), 1);
        assert_eq!(window_size(10_000), 1);
    }
}
