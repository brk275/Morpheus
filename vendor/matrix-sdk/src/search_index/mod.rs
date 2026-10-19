// Copyright 2025 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The search index is an abstraction layer in the matrix-sdk for the
//! matrix-sdk-search crate. It provides a [`SearchIndex`] which wraps
//! multiple [`RoomIndex`].

use std::{collections::hash_map::HashMap, path::PathBuf, sync::Arc};

use futures_util::future::join_all;
use matrix_sdk_base::{
    check_validity_of_replacement_events, deserialized_responses::TimelineEvent,
};
use matrix_sdk_search::{
    error::IndexError,
    index::{
        IndexableEvent, RoomIndex, RoomIndexOperation, SearchFilter, SearchResult,
        builder::RoomIndexBuilder,
    },
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        poll::{
            start::SyncPollStartEvent,
            unstable_start::{SyncUnstablePollStartEvent, UnstablePollStartEventContent},
        },
        room::{
            message::{MessageType, OriginalSyncRoomMessageEvent, Relation, SyncRoomMessageEvent},
            redaction::SyncRoomRedactionEvent,
        },
        sticker::SyncStickerEvent,
    },
    room_version_rules::RedactionRules,
};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, warn};

use crate::event_cache::RoomEventCache;

type Password = String;

/// Type of location to store [`RoomIndex`]
#[derive(Clone)]
pub enum SearchIndexStoreKind {
    /// Store unencrypted in file system folder
    UnencryptedDirectory(PathBuf),
    /// Store encrypted in file system folder
    EncryptedDirectory(PathBuf, Password),
    /// Store in memory
    InMemory,
}

impl std::fmt::Debug for SearchIndexStoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnencryptedDirectory(path) => {
                f.debug_tuple("UnencryptedDirectory").field(path).finish()
            }
            Self::EncryptedDirectory(path, _) => {
                f.debug_tuple("EncryptedDirectory").field(path).field(&"<redacted>").finish()
            }
            Self::InMemory => f.write_str("InMemory"),
        }
    }
}

/// Object that handles inteeraction with [`RoomIndex`]'s for search
#[derive(Clone, Debug)]
pub struct SearchIndex {
    /// HashMap that links each joined room to its RoomIndex
    room_indexes: Arc<Mutex<HashMap<OwnedRoomId, RoomIndex>>>,

    /// Base directory that stores the directories for each RoomIndex
    search_index_store_kind: SearchIndexStoreKind,
}

impl SearchIndex {
    /// Create a new [`SearchIndex`]
    pub fn new(
        room_indexes: Arc<Mutex<HashMap<OwnedRoomId, RoomIndex>>>,
        search_index_store_kind: SearchIndexStoreKind,
    ) -> Self {
        Self { room_indexes, search_index_store_kind }
    }

    /// Acquire [`SearchIndexGuard`] for this [`SearchIndex`].
    pub async fn lock(&self) -> SearchIndexGuard<'_> {
        SearchIndexGuard {
            index_map: self.room_indexes.lock().await,
            search_index_store_kind: &self.search_index_store_kind,
        }
    }
}

/// Object that represents an acquired [`SearchIndex`].
#[derive(Debug)]
pub struct SearchIndexGuard<'a> {
    /// Guard around the [`RoomIndex`] map
    index_map: MutexGuard<'a, HashMap<OwnedRoomId, RoomIndex>>,

    /// Base directory that stores the directories for each RoomIndex
    search_index_store_kind: &'a SearchIndexStoreKind,
}

impl SearchIndexGuard<'_> {
    fn create_index(&self, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let index = match self.search_index_store_kind {
            SearchIndexStoreKind::UnencryptedDirectory(path) => {
                RoomIndexBuilder::new_on_disk(path.to_path_buf(), room_id).unencrypted().build()?
            }
            SearchIndexStoreKind::EncryptedDirectory(path, password) => {
                RoomIndexBuilder::new_on_disk(path.to_path_buf(), room_id)
                    .encrypted(password)
                    .build()?
            }
            SearchIndexStoreKind::InMemory => RoomIndexBuilder::new_in_memory(room_id).build(),
        };
        Ok(index)
    }

    /// Handle a [`RoomIndexOperation`] in the [`RoomIndex`] of a given
    /// [`RoomId`]
    ///
    /// This which will add/remove/edit an event in the index based on the event
    /// type.
    ///
    /// Prefer [`SearchIndexGuard::bulk_execute`] for multiple operations.
    pub(crate) fn execute(
        &mut self,
        operation: RoomIndexOperation,
        room_id: &RoomId,
    ) -> Result<(), IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.execute(operation)
    }

    /// Handle a [`RoomIndexOperation`] in the [`RoomIndex`] of a given
    /// [`RoomId`]
    ///
    /// This which will add/remove/edit an event in the index based on the event
    /// type.
    pub(crate) fn bulk_execute(
        &mut self,
        operations: Vec<RoomIndexOperation>,
        room_id: &RoomId,
    ) -> Result<(), IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.bulk_execute(operations)
    }

    /// Search a [`Room`]'s index for the query and return at most
    /// max_number_of_results results.
    pub(crate) fn search(
        &mut self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
        room_id: &RoomId,
    ) -> Result<SearchResult, IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.search(query, max_number_of_results, pagination_offset)
    }

    /// Morpheus patch: search a [`Room`]'s index like
    /// [`SearchIndexGuard::search`], restricted by the typed `filter`
    /// (see [`RoomIndex::search_filtered`]). `query` may be `None` for a
    /// filters-only search, which returns hits newest first.
    pub fn search_filtered(
        &mut self,
        query: Option<&str>,
        filter: &SearchFilter,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
        room_id: &RoomId,
    ) -> Result<SearchResult, IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.search_filtered(query, filter, max_number_of_results, pagination_offset)
    }

    /// Morpheus patch: one newest-first window of a [`Room`]'s events
    /// matching the typed `filter`, with each event's indexed date (see
    /// [`RoomIndex::search_newest`]), for merging rooms newest first.
    pub fn search_newest(
        &mut self,
        filter: &SearchFilter,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
        room_id: &RoomId,
    ) -> Result<Vec<(MilliSecondsSinceUnixEpoch, OwnedEventId)>, IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.search_newest(filter, max_number_of_results, pagination_offset)
    }

    /// Given a [`TimelineEvent`] this function will derive a
    /// [`RoomIndexOperation`], if it should be handled, and execute it;
    /// returning the result.
    ///
    /// Prefer [`SearchIndexGuard::bulk_handle_timeline_event`] for multiple
    /// events.
    pub async fn handle_timeline_event(
        &mut self,
        event: TimelineEvent,
        room_cache: &RoomEventCache,
        room_id: &RoomId,
        redaction_rules: &RedactionRules,
    ) -> Result<(), IndexError> {
        let original = self.original_redaction_target(&event, room_id, redaction_rules)?;
        if let Some(index_operation) =
            parse_timeline_event(room_cache, event, redaction_rules, original).await
        {
            self.execute(index_operation, room_id)
        } else {
            Ok(())
        }
    }

    /// Run [`SearchIndexGuard::handle_timeline_event`] for multiple
    /// [`TimelineEvent`].
    pub async fn bulk_handle_timeline_event<T>(
        &mut self,
        events: T,
        room_cache: &RoomEventCache,
        room_id: &RoomId,
        redaction_rules: &RedactionRules,
    ) -> Result<(), IndexError>
    where
        T: Iterator<Item = TimelineEvent>,
    {
        let events = events
            .map(|event| {
                self.original_redaction_target(&event, room_id, redaction_rules)
                    .map(|original| (event, original))
            })
            .collect::<Result<Vec<_>, IndexError>>()?;
        let futures = events.into_iter().map(|(event, original)| {
            parse_timeline_event(room_cache, event, redaction_rules, original)
        });

        let operations: Vec<_> = join_all(futures).await.into_iter().flatten().collect();

        self.bulk_execute(operations, room_id)
    }

    fn original_redaction_target(
        &self,
        event: &TimelineEvent,
        room_id: &RoomId,
        rules: &RedactionRules,
    ) -> Result<Option<OwnedEventId>, IndexError> {
        let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(event))) =
            event.raw().deserialize()
        else {
            return Ok(None);
        };
        let Some(target) = event.redacts(rules) else { return Ok(None) };
        match self.index_map.get(room_id) {
            Some(index) => index.original_event_id(target),
            None => Ok(None),
        }
    }
}

/// Given an event id this function returns the most recent edit on said event
/// or the event itself if there are no edits, paired with the original's
/// normalized timestamp. Edits and redactions must never move a message across
/// date-filter boundaries or change its position in newest-first search.
async fn get_most_recent_edit(
    cache: &RoomEventCache,
    original: &EventId,
) -> Option<(OriginalSyncRoomMessageEvent, Option<MilliSecondsSinceUnixEpoch>)> {
    get_most_recent_edit_excluding(cache, original, None).await
}

async fn get_most_recent_edit_excluding(
    cache: &RoomEventCache,
    original: &EventId,
    excluded: Option<&EventId>,
) -> Option<(OriginalSyncRoomMessageEvent, Option<MilliSecondsSinceUnixEpoch>)> {
    use ruma::events::{AnySyncTimelineEvent, relation::RelationType};

    if excluded == Some(original) {
        return None;
    }
    let Ok(Some((original_ev, related))) =
        cache.find_event_with_relations(original, Some(vec![RelationType::Replacement])).await
    else {
        debug!("Couldn't find relations for {}", original);
        return None;
    };

    // Only index valid replacements (matching sender, type, etc.); otherwise
    // anyone could rewrite another user's indexed message. Fall back to the
    // original event when there is no valid edit.
    let latest = related
        .iter()
        .filter(|edit| {
            if edit.event_id().is_some_and(|id| excluded == Some(id)) {
                return false;
            }
            if !matches!(
                edit.raw().deserialize(),
                Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                    SyncRoomMessageEvent::Original(_)
                )))
            ) {
                return false;
            }
            check_validity_of_replacement_events(
                original_ev.raw(),
                original_ev.encryption_info().map(|info| &**info),
                edit.raw(),
                edit.encryption_info().map(|info| &**info),
            )
            .is_ok()
        })
        .max_by(|left, right| {
            left.timestamp()
                .cmp(&right.timestamp())
                .then_with(|| left.event_id().cmp(&right.event_id()))
        })
        .unwrap_or(&original_ev);

    match latest.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(latest))) => {
            latest.as_original().cloned().map(|event| (event, original_ev.timestamp()))
        }
        _ => None,
    }
}

/// Indexable text for a media message: its filename plus any caption.
fn media_body(filename: &str, caption: Option<&str>) -> String {
    match caption {
        Some(caption) => format!("{filename} {caption}"),
        None => filename.to_owned(),
    }
}

/// Extract the searchable text from a room message, or `None` if its type
/// carries no text to index.
fn room_message_body(msgtype: &MessageType) -> Option<String> {
    match msgtype {
        MessageType::Text(content) => Some(content.body.clone()),
        MessageType::Emote(content) => Some(content.body.clone()),
        MessageType::Notice(content) => Some(content.body.clone()),
        MessageType::ServerNotice(content) => Some(content.body.clone()),
        MessageType::Location(content) => Some(content.body.clone()),
        MessageType::Image(content) => Some(media_body(content.filename(), content.caption())),
        MessageType::Video(content) => Some(media_body(content.filename(), content.caption())),
        MessageType::Audio(content) => Some(media_body(content.filename(), content.caption())),
        MessageType::File(content) => Some(media_body(content.filename(), content.caption())),
        _ => None,
    }
}

/// Build an [`IndexableEvent`] from a room message, or `None` if its type
/// carries no searchable text.
fn indexable_from_room_message(
    event: &OriginalSyncRoomMessageEvent,
    timestamp: Option<MilliSecondsSinceUnixEpoch>,
) -> Option<IndexableEvent> {
    // The replacement's fallback body need not equal its new content. Index
    // exactly the text the timeline will display after the edit is applied.
    let msgtype = match &event.content.relates_to {
        Some(Relation::Replacement(replacement)) => &replacement.new_content.msgtype,
        _ => &event.content.msgtype,
    };
    let body = room_message_body(msgtype)?;
    let original_event_id = match &event.content.relates_to {
        Some(Relation::Replacement(replacement)) => replacement.event_id.clone(),
        _ => event.event_id.clone(),
    };

    Some(IndexableEvent::new(
        event.event_id.clone(),
        original_event_id,
        event.sender.clone(),
        timestamp,
        body,
    ))
}

/// If the given [`OriginalSyncRoomMessageEvent`] is an edit we make an
/// [`RoomIndexOperation::Edit`] with the new most recent version of the
/// original.
async fn handle_possible_edit(
    event: &OriginalSyncRoomMessageEvent,
    cache: &RoomEventCache,
) -> Option<RoomIndexOperation> {
    if let Some(Relation::Replacement(replacement_data)) = &event.content.relates_to {
        if let Some((recent, original_timestamp)) =
            get_most_recent_edit(cache, &replacement_data.event_id).await
        {
            return Some(
                indexable_from_room_message(&recent, original_timestamp).map_or(
                    RoomIndexOperation::Noop,
                    |indexable| {
                        RoomIndexOperation::Edit(replacement_data.event_id.clone(), indexable)
                    },
                ),
            );
        } else {
            return Some(RoomIndexOperation::Noop);
        }
    }
    None
}

/// Return a [`RoomIndexOperation::Edit`] or [`RoomIndexOperation::Add`]
/// depending on the message.
async fn handle_room_message(
    event: SyncRoomMessageEvent,
    cache: &RoomEventCache,
) -> Option<RoomIndexOperation> {
    if let Some(event) = event.as_original() {
        return handle_possible_edit(event, cache).await.or(get_most_recent_edit(
            cache,
            &event.event_id,
        )
        .await
        .and_then(|(recent, original_timestamp)| {
            indexable_from_room_message(&recent, original_timestamp).map(RoomIndexOperation::Add)
        }));
    }
    None
}

/// Return a [`RoomIndexOperation`] removing a redacted event from the index, or
/// re-adding the most recent remaining version if an edit was redacted.
async fn handle_room_redaction(
    event: SyncRoomRedactionEvent,
    cache: &RoomEventCache,
    rules: &RedactionRules,
    indexed_original: Option<OwnedEventId>,
) -> Option<RoomIndexOperation> {
    let redacted_event_id = event.redacts(rules)?;

    // Cache redaction removes m.relates_to. The encrypted index retains the
    // original ID so the surviving valid version can still be restored.
    if let Some(original) = indexed_original {
        let recent =
            get_most_recent_edit_excluding(cache, &original, Some(redacted_event_id)).await;
        return Some(
            match recent
                .and_then(|(event, timestamp)| indexable_from_room_message(&event, timestamp))
            {
                Some(event) => RoomIndexOperation::Edit(original, event),
                None => RoomIndexOperation::Remove(original),
            },
        );
    }

    // If the redacted event was a room message edit, re-add the most recent
    // remaining version instead of just removing it.
    if let Ok(Some(redacted_event)) = cache.find_event(redacted_event_id).await
        && let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            redacted_event,
        ))) = redacted_event.raw().deserialize()
        && let Some(redacted_event) = redacted_event.as_original()
        && let Some(operation) = handle_possible_edit(redacted_event, cache).await
    {
        return Some(operation);
    }

    // Otherwise remove the redacted event from the index. This covers plain
    // messages, stickers and polls.
    Some(RoomIndexOperation::Remove(redacted_event_id.to_owned()))
}

/// Return a [`RoomIndexOperation::Add`] indexing a sticker's descriptive text.
fn handle_sticker(
    event: SyncStickerEvent,
    timestamp: Option<MilliSecondsSinceUnixEpoch>,
) -> Option<RoomIndexOperation> {
    let event = event.as_original()?;

    Some(RoomIndexOperation::Add(IndexableEvent::new(
        event.event_id.clone(),
        event.event_id.clone(),
        event.sender.clone(),
        timestamp,
        event.content.body.clone(),
    )))
}

/// Return a [`RoomIndexOperation::Add`] indexing an unstable poll's question
/// and answers.
///
/// ponytail: only indexes the initial `New` poll — edits (`Replacement`) and
/// poll ends are ignored. Add edit handling if editing a poll needs to update
/// search results.
fn handle_unstable_poll_start(
    event: SyncUnstablePollStartEvent,
    timestamp: Option<MilliSecondsSinceUnixEpoch>,
) -> Option<RoomIndexOperation> {
    let event = event.as_original()?;

    let UnstablePollStartEventContent::New(content) = &event.content else {
        return None;
    };

    let block = &content.poll_start;
    let mut body = block.question.text.clone();
    for answer in block.answers.iter() {
        body.push(' ');
        body.push_str(&answer.text);
    }

    Some(RoomIndexOperation::Add(IndexableEvent::new(
        event.event_id.clone(),
        event.event_id.clone(),
        event.sender.clone(),
        timestamp,
        body,
    )))
}

/// Return a [`RoomIndexOperation::Add`] indexing a stable poll's question and
/// answers.
///
/// ponytail: like [`handle_unstable_poll_start`], edits and poll ends are
/// ignored — only the initial poll is indexed.
fn handle_poll_start(
    event: SyncPollStartEvent,
    timestamp: Option<MilliSecondsSinceUnixEpoch>,
) -> Option<RoomIndexOperation> {
    let event = event.as_original()?;

    // Skip poll edits, matching the unstable poll handling.
    if let Some(Relation::Replacement(_)) = &event.content.relates_to {
        return None;
    }

    let block = &event.content.poll;
    let mut body = block.question.text.find_plain()?.to_owned();
    for answer in block.answers.iter() {
        if let Some(text) = answer.text.find_plain() {
            body.push(' ');
            body.push_str(text);
        }
    }

    Some(RoomIndexOperation::Add(IndexableEvent::new(
        event.event_id.clone(),
        event.event_id.clone(),
        event.sender.clone(),
        timestamp,
        body,
    )))
}

/// Prepare a [`TimelineEvent`] into a [`RoomIndexOperation`] for search
/// indexing.
async fn parse_timeline_event(
    cache: &RoomEventCache,
    event: TimelineEvent,
    redaction_rules: &RedactionRules,
    redaction_original: Option<OwnedEventId>,
) -> Option<RoomIndexOperation> {
    use ruma::events::AnySyncTimelineEvent;

    if event.kind.is_utd() {
        return None;
    }

    let timestamp = event.timestamp();

    match event.raw().deserialize() {
        Ok(event) => match event {
            AnySyncTimelineEvent::MessageLike(event) => match event {
                AnySyncMessageLikeEvent::RoomMessage(event) => {
                    handle_room_message(event, cache).await
                }
                AnySyncMessageLikeEvent::RoomRedaction(event) => {
                    handle_room_redaction(event, cache, redaction_rules, redaction_original).await
                }
                AnySyncMessageLikeEvent::Sticker(event) => handle_sticker(event, timestamp),
                AnySyncMessageLikeEvent::PollStart(event) => handle_poll_start(event, timestamp),
                AnySyncMessageLikeEvent::UnstablePollStart(event) => {
                    handle_unstable_poll_start(event, timestamp)
                }
                _ => None,
            },
            AnySyncTimelineEvent::State(_) => None,
        },

        Err(e) => {
            warn!("failed to parse event: {e:?}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{
        event_id, events::room::message::RoomMessageEventContentWithoutRelation, room_id, user_id,
    };

    use crate::test_utils::mocks::MatrixMockServer;

    #[test]
    fn test_store_kind_debug_keeps_credentials_out_of_parent_diagnostics() {
        use std::{collections::HashMap, path::PathBuf, sync::Arc};

        use tokio::sync::Mutex;

        use super::{SearchIndex, SearchIndexStoreKind};

        let secret = "fixture-marker";
        let kind = SearchIndexStoreKind::EncryptedDirectory(
            PathBuf::from("synthetic-search"),
            secret.to_owned(),
        );
        let text = format!("{kind:?}");
        assert!(text.contains("EncryptedDirectory"));
        assert!(text.contains("<redacted>"));
        assert!(!text.contains(secret));
        let index = SearchIndex::new(Arc::new(Mutex::new(HashMap::new())), kind);
        assert!(!format!("{index:?}").contains(secret));
        let guard = index.room_indexes.try_lock().unwrap();
        let guarded = super::SearchIndexGuard {
            index_map: guard,
            search_index_store_kind: &index.search_index_store_kind,
        };
        assert!(!format!("{guarded:?}").contains(secret));
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_sync_message_is_indexed() {
        let mock_server = MatrixMockServer::new().await;
        let client = mock_server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room_id = room_id!("!room_id:localhost");
        let event_id = event_id!("$event_id:localost");
        let user_id = user_id!("@user_id:localost");

        let event_factory = EventFactory::new();
        let room = mock_server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                    event_factory
                        .text_msg("this is a sentence")
                        .event_id(event_id)
                        .sender(user_id)
                        .into_raw_sync(),
                ]),
            )
            .await;

        let response = room.search("this", 5, None).await.expect("search should have 1 result");

        assert_eq!(response.events.len(), 1, "unexpected numbers of responses: {response:?}");
        assert_eq!(response.events[0].1, event_id, "event id doesn't match: {response:?}");
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_sync_media_message_is_indexed() {
        use ruma::owned_mxc_uri;

        let mock_server = MatrixMockServer::new().await;
        let client = mock_server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room_id = room_id!("!room_id:localhost");
        let image_id = event_id!("$image_id:localhost");
        let file_id = event_id!("$file_id:localhost");
        let user_id = user_id!("@user_id:localhost");

        let f = EventFactory::new();
        let room = mock_server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                    f.image("holiday_beach.jpg".to_owned(), owned_mxc_uri!("mxc://localhost/1"))
                        .caption(Some("sunset over the ocean".to_owned()), None)
                        .event_id(image_id)
                        .sender(user_id)
                        .into_raw_sync(),
                    f.image("quarterly_report.pdf".to_owned(), owned_mxc_uri!("mxc://localhost/2"))
                        .event_id(file_id)
                        .sender(user_id)
                        .into_raw_sync(),
                ]),
            )
            .await;

        // The caption is indexed.
        let response = room.search("sunset", 5, None).await.unwrap();
        assert_eq!(response.events.len(), 1, "unexpected results for caption search: {response:?}");
        assert_eq!(response.events[0].1, image_id, "event id doesn't match: {response:?}");

        // The filename is indexed.
        let response = room.search("holiday_beach", 5, None).await.unwrap();
        assert_eq!(
            response.events.len(),
            1,
            "unexpected results for filename search: {response:?}"
        );
        assert_eq!(response.events[0].1, image_id, "event id doesn't match: {response:?}");

        // A media message without a caption still indexes its filename.
        let response = room.search("quarterly_report", 5, None).await.unwrap();
        assert_eq!(
            response.events.len(),
            1,
            "unexpected results for filename search: {response:?}"
        );
        assert_eq!(response.events[0].1, file_id, "event id doesn't match: {response:?}");
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_sync_sticker_and_poll_are_indexed() {
        use ruma::{events::room::ImageInfo, owned_mxc_uri};

        let mock_server = MatrixMockServer::new().await;
        let client = mock_server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room_id = room_id!("!room_id:localhost");
        let sticker_id = event_id!("$sticker_id:localhost");
        let poll_id = event_id!("$poll_id:localhost");
        let user_id = user_id!("@user_id:localhost");

        let f = EventFactory::new().room(room_id).sender(user_id);
        let room = mock_server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                    f.sticker(
                        "a waving cat",
                        ImageInfo::new(),
                        owned_mxc_uri!("mxc://localhost/1"),
                    )
                    .event_id(sticker_id)
                    .into_raw_sync(),
                    f.poll_start("fallback", "favourite cheese?", vec!["comté", "gruyère"])
                        .event_id(poll_id)
                        .into_raw_sync(),
                ]),
            )
            .await;

        // The sticker's description is indexed.
        let response = room.search("waving", 5, None).await.unwrap();
        assert_eq!(response.events.len(), 1, "unexpected results for sticker search: {response:?}");
        assert_eq!(response.events[0].1, sticker_id, "event id doesn't match: {response:?}");

        // The poll question is indexed.
        let response = room.search("cheese", 5, None).await.unwrap();
        assert_eq!(
            response.events.len(),
            1,
            "unexpected results for poll question search: {response:?}"
        );
        assert_eq!(response.events[0].1, poll_id, "event id doesn't match: {response:?}");

        // The poll answers are indexed.
        let response = room.search("gruyère", 5, None).await.unwrap();
        assert_eq!(
            response.events.len(),
            1,
            "unexpected results for poll answer search: {response:?}"
        );
        assert_eq!(response.events[0].1, poll_id, "event id doesn't match: {response:?}");
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_sync_stable_poll_is_indexed() {
        use ruma::events::{
            message::TextContentBlock,
            poll::start::{PollAnswer, PollAnswers, PollContentBlock, PollStartEventContent},
        };

        let mock_server = MatrixMockServer::new().await;
        let client = mock_server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room_id = room_id!("!room_id:localhost");
        let poll_id = event_id!("$stable_poll_id:localhost");
        let user_id = user_id!("@user_id:localhost");

        let answers: PollAnswers = vec![
            PollAnswer::new("0".to_owned(), TextContentBlock::plain("comté")),
            PollAnswer::new("1".to_owned(), TextContentBlock::plain("gruyère")),
        ]
        .try_into()
        .unwrap();
        let poll = PollContentBlock::new(TextContentBlock::plain("favourite cheese?"), answers);
        let content = PollStartEventContent::new(TextContentBlock::plain("fallback"), poll);

        let f = EventFactory::new().room(room_id).sender(user_id);
        let room = mock_server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_bulk(vec![f.event(content).event_id(poll_id).into_raw_sync()]),
            )
            .await;

        // The poll question is indexed.
        let response = room.search("cheese", 5, None).await.unwrap();
        assert_eq!(
            response.events.len(),
            1,
            "unexpected results for poll question search: {response:?}"
        );
        assert_eq!(response.events[0].1, poll_id, "event id doesn't match: {response:?}");

        // The poll answers are indexed.
        let response = room.search("gruyère", 5, None).await.unwrap();
        assert_eq!(
            response.events.len(),
            1,
            "unexpected results for poll answer search: {response:?}"
        );
        assert_eq!(response.events[0].1, poll_id, "event id doesn't match: {response:?}");
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_search_index_edit_ordering() {
        let room_id = room_id!("!room_id:localhost");
        let dummy_id = event_id!("$dummy");
        let edit1_id = event_id!("$edit1");
        let edit2_id = event_id!("$edit2");
        let edit3_id = event_id!("$edit3");
        let original_id = event_id!("$original");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room = server.sync_joined_room(&client, room_id).await;

        let f = EventFactory::new().room(room_id).sender(user_id!("@user_id:localhost"));

        // Indexable dummy message required because RoomIndex is initialised
        // lazily.
        let dummy = f.text_msg("dummy").event_id(dummy_id);

        let original = f.text_msg("This is a message").event_id(original_id);

        let edit1 = f
            .text_msg("* A new message")
            .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("A new message"))
            .event_id(edit1_id);

        let edit2 = f
            .text_msg("* An even newer message")
            .edit(
                original_id,
                RoomMessageEventContentWithoutRelation::text_plain("An even newer message"),
            )
            .event_id(edit2_id);

        let edit3 = f
            .text_msg("* The newest message")
            .edit(
                original_id,
                RoomMessageEventContentWithoutRelation::text_plain("The newest message"),
            )
            .event_id(edit3_id);

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(dummy)
                    .add_timeline_event(edit1)
                    .add_timeline_event(edit2),
            )
            .await;

        let results = room.search("message", 3, None).await.unwrap();

        assert_eq!(results.events.len(), 0, "Search should return 0 results, got {results:?}");

        // Adding the original after some pending edits should add the latest
        // edit instead of the original.
        server
            .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(original))
            .await;

        let results = room.search("message", 3, None).await.unwrap();

        assert_eq!(results.events.len(), 1, "Search should return 1 result, got {results:?}");
        assert_eq!(
            results.events[0].1, edit2_id,
            "Search should return latest edit, got {:?}",
            results.events[0].1
        );

        // Editing the original after it exists and there has been another edit
        // should delete the previous edits and add this one
        server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(edit3)).await;

        let results = room.search("message", 3, None).await.unwrap();

        assert_eq!(results.events.len(), 1, "Search should return 1 result, got {results:?}");
        assert_eq!(
            results.events[0].1, edit3_id,
            "Search should return latest edit, got {:?}",
            results.events[0].1
        );
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_search_index_replacements_keep_original_date_and_visible_body() {
        use matrix_sdk_search::index::SearchFilter;
        use ruma::{MilliSecondsSinceUnixEpoch, UInt};

        for edit_timestamp in [1_000_u64, 8_000_u64] {
            for original_first in [false, true] {
                let server = MatrixMockServer::new().await;
                let client = server.client_builder().build().await;
                client.event_cache().subscribe().unwrap();
                let room_id = room_id!("!date-fixture:localhost");
                let original_id = event_id!("$date-original");
                let edit_id = event_id!("$date-edit");
                let f = EventFactory::new().room(room_id).sender(user_id!("@fixture:localhost"));
                let original = f
                    .text_msg("baseline")
                    .event_id(original_id)
                    .server_ts(2_000_u64)
                    .into_raw_sync();
                // Fallback and replacement text deliberately disagree.
                let edit = f
                    .text_msg("unrelatedfallback")
                    .edit(
                        original_id,
                        RoomMessageEventContentWithoutRelation::text_plain("revised"),
                    )
                    .event_id(edit_id)
                    .server_ts(edit_timestamp)
                    .into_raw_sync();
                let events =
                    if original_first { vec![original, edit] } else { vec![edit, original] };
                server
                    .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_bulk(events))
                    .await;

                let at = |value| Some(MilliSecondsSinceUnixEpoch(UInt::new(value).unwrap()));
                let filter = SearchFilter { since: at(2_000), until: at(2_999), sender: None };
                {
                    let mut index = client.search_index().lock().await;
                    let revised =
                        index.search_filtered(Some("revised"), &filter, 10, None, room_id).unwrap();
                    assert_eq!(revised.events.len(), 1);
                    assert_eq!(revised.events[0].1, edit_id);
                    let fallback = index
                        .search_filtered(
                            Some("unrelatedfallback"),
                            &SearchFilter::default(),
                            10,
                            None,
                            room_id,
                        )
                        .unwrap();
                    assert!(fallback.events.is_empty());
                    let newest = index.search_newest(&filter, 10, None, room_id).unwrap();
                    assert_eq!(newest.len(), 1);
                    assert_eq!(newest[0].0, at(2_000).unwrap());
                }

                // Redacting the replacement restores the original's text/date;
                // neither the edit's date nor the redaction's date may leak in.
                let redaction = f
                    .redaction(edit_id)
                    .event_id(event_id!("$date-redaction"))
                    .server_ts(9_000_u64);
                server
                    .sync_room(
                        &client,
                        JoinedRoomBuilder::new(room_id).add_timeline_event(redaction),
                    )
                    .await;
                let mut index = client.search_index().lock().await;
                let restored =
                    index.search_filtered(Some("baseline"), &filter, 10, None, room_id).unwrap();
                assert_eq!(restored.events.len(), 1);
                assert_eq!(restored.events[0].1, original_id);
                let newest = index.search_newest(&filter, 10, None, room_id).unwrap();
                assert_eq!(newest.len(), 1);
                assert_eq!(newest[0].0, at(2_000).unwrap());
            }
        }
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_redactions_restore_the_surviving_edit_without_reviving_the_original() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        client.event_cache().subscribe().unwrap();
        let room_id = room_id!("!redaction-fixture:localhost");
        let original = event_id!("$repair-original");
        let older = event_id!("$repair-older");
        let newer = event_id!("$repair-newer");
        let f = EventFactory::new().room(room_id).sender(user_id!("@fixture:localhost"));
        let edit = |id, body, timestamp| {
            f.text_msg(body)
                .edit(original, RoomMessageEventContentWithoutRelation::text_plain(body))
                .event_id(id)
                .server_ts(timestamp)
                .into_raw_sync()
        };
        let room = server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                    f.text_msg("firsttext").event_id(original).server_ts(1_000_u64).into_raw_sync(),
                    edit(newer, "newestbody", 3_000_u64),
                    // Arrival order must not make an older edit win.
                    edit(older, "olderbody", 2_000_u64),
                ]),
            )
            .await;
        assert_eq!(room.search("newestbody", 10, None).await.unwrap().events.len(), 1);
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    f.redaction(newer).event_id(event_id!("$remove-newer")).server_ts(4_000_u64),
                ),
            )
            .await;
        assert_eq!(room.search("olderbody", 10, None).await.unwrap().events.len(), 1);
        assert!(room.search("newestbody", 10, None).await.unwrap().events.is_empty());
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    f.redaction(original)
                        .event_id(event_id!("$remove-original"))
                        .server_ts(5_000_u64),
                ),
            )
            .await;
        let mut index = client.search_index().lock().await;
        assert!(
            index
                .search_newest(
                    &matrix_sdk_search::index::SearchFilter::default(),
                    10,
                    None,
                    room_id
                )
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_search_index_ignores_cross_sender_edit() {
        let room_id = room_id!("!room_id:localhost");
        let original_id = event_id!("$original");
        let edit_id = event_id!("$edit");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room = server.sync_joined_room(&client, room_id).await;

        let f = EventFactory::new().room(room_id);

        let original =
            f.text_msg("original alpha").sender(user_id!("@alice:localhost")).event_id(original_id);

        // An edit from a different user than the original sender is not a valid
        // replacement and must be ignored.
        let malicious_edit = f
            .text_msg("* malicious beta")
            .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("malicious beta"))
            .sender(user_id!("@bob:localhost"))
            .event_id(edit_id);

        server
            .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(original))
            .await;

        // The original message is indexed.
        let results = room.search("alpha", 3, None).await.unwrap();
        assert_eq!(results.events.len(), 1, "Original should be indexed, got {results:?}");
        assert_eq!(results.events[0].1, original_id, "unexpected event id: {results:?}");

        server
            .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(malicious_edit))
            .await;

        // The forged edit's content must not be indexed.
        let results = room.search("beta", 3, None).await.unwrap();
        assert_eq!(results.events.len(), 0, "Cross-sender edit should be ignored, got {results:?}");

        // The original message stays indexed.
        let results = room.search("alpha", 3, None).await.unwrap();
        assert_eq!(results.events.len(), 1, "Original should stay indexed, got {results:?}");
        assert_eq!(results.events[0].1, original_id, "unexpected event id: {results:?}");
    }
}
