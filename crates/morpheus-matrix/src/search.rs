//! Account-scoped local message search. The SDK indexes decrypted events in
//! each room store; no message text is sent to a homeserver search endpoint.

use futures_util::StreamExt;
use matrix_sdk::ruma::{
    EventId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        room::message::{Relation, SyncRoomMessageEvent},
    },
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineEventFocusThreadMode, TimelineFocus};
use serde::{Deserialize, Serialize};

use crate::{AccountManager, MatrixCoreError, MessageInfo, Result, selected_loaded_for};

const MAX_SEARCH_BYTES: usize = 256;
const MAX_RESULT_BODY_CHARS: usize = 500;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageSearchResult {
    pub room_id: String,
    pub event_id: String,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub relevance_score: f32,
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
        let query = validate_query(query)?;
        let room = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
            super::account_room(&state, expected_account_id, room_id)?
        };
        let page = room
            .search(query, limit.clamp(1, 50), Some(offset.min(10_000)))
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        let mut results = Vec::with_capacity(page.events.len());
        for (relevance_score, event_id) in page.events {
            if let Some(result) = load_search_result(&room, &event_id, relevance_score).await? {
                results.push(result);
            }
        }
        {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
        }
        Ok(results)
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
        let query = validate_query(query)?;
        let client = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?
                .client
                .clone()
        };
        let mut pages = Box::pin(client.search_messages(query.to_owned()).build());
        let mut results = Vec::new();
        let mut skipped = 0;
        let limit = limit.clamp(1, 50);
        let offset = offset.min(10_000);
        while let Some(page) = pages.next().await {
            for (room_id, relevance_score, event_id) in
                page.map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
            {
                if skipped < offset {
                    skipped += 1;
                    continue;
                }
                let Some(room) = client.get_room(&room_id) else {
                    continue;
                };
                if let Some(result) = load_search_result(&room, &event_id, relevance_score).await? {
                    results.push(result);
                }
                if results.len() >= limit {
                    break;
                }
            }
            if results.len() >= limit {
                break;
            }
        }
        {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
        }
        Ok(results)
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
        let target = super::parse_event_id(event_id)?;
        let room = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
            super::account_room(&state, expected_account_id, room_id)?
        };
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
            .collect();
        {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
        }
        Ok(context)
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

async fn load_search_result(
    room: &matrix_sdk::room::Room,
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
    let (visible_event_id, sender, timestamp_ms, body) =
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
                original.sender.to_string(),
                original.origin_server_ts.get().into(),
                replacement.new_content.msgtype.body(),
            )
        } else {
            (
                event.event_id.to_string(),
                event.sender.to_string(),
                event.origin_server_ts.get().into(),
                event.content.msgtype.body(),
            )
        };
    let body = body.chars().take(MAX_RESULT_BODY_CHARS).collect();
    Ok(Some(MessageSearchResult {
        room_id: room.room_id().to_string(),
        event_id: visible_event_id,
        sender,
        body,
        timestamp_ms,
        relevance_score,
    }))
}
