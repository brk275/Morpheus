//! Account-scoped local message search. The SDK indexes decrypted events in
//! each room store; no message text is sent to a homeserver search endpoint.
//! Nothing here logs a query, a result or an account identifier.

use std::collections::HashMap;

use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    room::Room,
    ruma::{
        EventId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            room::message::{Relation, SyncRoomMessageEvent},
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
        let query = validate_query(query)?.to_owned();
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
            .guard(room_page(&room, &query, limit, offset))
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
        let query = validate_query(query)?.to_owned();
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
        let page = selection
            .guard(global_page(&client, &query, limit, offset))
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

fn index_error(error: impl std::fmt::Display) -> MatrixCoreError {
    MatrixCoreError::Sdk(error.to_string())
}

async fn room_page(
    room: &Room,
    query: &str,
    limit: usize,
    offset: usize,
) -> Result<MessageSearchPage> {
    let offset = offset.min(MAX_SEARCH_OFFSET);
    // No page examines a hit at or beyond the advertised cursor cap.
    let allowance = limit
        .clamp(1, MAX_SEARCH_PAGE)
        .min(MAX_SEARCH_OFFSET - offset);
    if allowance == 0 {
        return Ok(MessageSearchPage::default());
    }
    // One extra hit tells whether another page exists without showing it.
    let hits = room
        .search(query, allowance + 1, Some(offset))
        .await
        .map_err(index_error)?
        .events;
    let more = hits.len() > allowance;
    let examined = hits.len().min(allowance);
    let mut page = PageBuilder::default();
    for (relevance_score, event_id) in hits.into_iter().take(allowance) {
        page.add(room, &event_id, relevance_score).await;
    }
    let next = offset + examined;
    let next_offset = (more && next < MAX_SEARCH_OFFSET).then_some(next);
    Ok(page.finish(next_offset))
}

async fn global_page(
    client: &Client,
    query: &str,
    limit: usize,
    offset: usize,
) -> Result<MessageSearchPage> {
    let limit = limit.clamp(1, MAX_SEARCH_PAGE);
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
            if examined == limit || position >= MAX_SEARCH_OFFSET {
                more = position < MAX_SEARCH_OFFSET;
                break 'pages;
            }
            position += 1;
            examined += 1;
            if let Some(room) = client.get_room(&room_id) {
                page.add(&room, &event_id, relevance_score).await;
            }
        }
    }
    Ok(page.finish(more.then_some(position)))
}

/// Index failures end a page with an error before this builder is used. An
/// event that cannot be loaded only counts as unavailable, so the page keeps
/// its cursor and later hits stay reachable.
#[derive(Default)]
struct PageBuilder {
    results: Vec<MessageSearchResult>,
    room_names: HashMap<String, String>,
    unavailable: usize,
}

impl PageBuilder {
    async fn add(&mut self, room: &Room, event_id: &EventId, relevance_score: f32) {
        match load_search_result(room, event_id, relevance_score).await {
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
    // The SDK indexes a replacement under the edit's event ID, but the
    // timeline presents the original event with the new content applied.
    // Return that visible event ID so opening/highlighting the hit works.
    let (visible_event_id, thread_root, sender, timestamp_ms, body) =
        if let Some(Relation::Replacement(replacement)) = &event.content.relates_to {
            let original = room
                .load_or_fetch_event(&replacement.event_id, None)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
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
                original.origin_server_ts.get().into(),
                replacement.new_content.msgtype.body(),
            )
        } else {
            (
                event.event_id.to_string(),
                thread_root_of(event.content.relates_to.as_ref()),
                event.sender.to_string(),
                event.origin_server_ts.get().into(),
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
    }))
}
