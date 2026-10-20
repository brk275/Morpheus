//! Standard Matrix room organization: the `m.favourite` and `m.lowpriority`
//! room tags and their `order` metadata.
//!
//! Tags live in the account's per-room `m.tag` account data. Each tag is
//! written with its own request, so tags this module does not touch, such as
//! user `u.*` tags or other apps' tags, are never rewritten. Favourite and low
//! priority are kept mutually exclusive, as the pinned SDK does: adding one
//! removes the other. Adding a tag the room already has leaves it as stored.
//! A favourite tag whose stored info carries fields other than `order` is
//! never reordered, because the typed request would drop them; such a
//! reorder is refused before anything is sent.
//!
//! Every entry point is bound to the caller's `AccountSelection`; the room
//! must be joined when the request starts. A request already handed to the
//! homeserver cannot be recalled by a later switch; the caller only loses
//! the result, and sync brings the stored state back.
//!
//! Writes are decided from a fresh, bounded read of the room's `m.tag` from
//! the homeserver, not the synced copy, and each write is preceded by a
//! re-read that must still equal that baseline (with this change's own
//! accepted writes applied); otherwise nothing more is sent. Every request
//! is one dispatch: no HTTP retry, a capped response, and (STAGED on root's
//! d4c5d08 SDK request API) no replay after a token refresh. The manager
//! lock is never held across a request. A write whose answer never came, or
//! whose wait ended after it was handed to the SDK, is reported as Unknown,
//! never as not saved.

use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

use matrix_sdk::{
    Client, HttpError,
    config::RequestConfig,
    ruma::{
        OwnedRoomId, OwnedUserId, RoomId,
        api::{
            client::{
                config::get_room_account_data,
                tag::{create_tag, delete_tag},
            },
            error::ErrorKind,
        },
        events::{RoomAccountDataEventType, tag::TagInfo},
    },
};
use serde_json::{Map, Value};

use super::*;

/// Favourites reordered in one request at most.
pub const MAX_TAG_ORDER_WRITES: usize = 100;
/// Bound on one change; a reorder adds `ORDER_WRITE_TIMEOUT` per room.
const TAG_TIMEOUT: Duration = Duration::from_secs(30);
const ORDER_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// One request's own bound, inside the change's overall bound.
const TAG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest `m.tag` content read back; larger is refused, not cut.
const MAX_TAG_CONTENT_BYTES: u64 = 64 * 1024;
/// A tag write's answer is an empty object.
const MAX_TAG_WRITE_RESPONSE_BYTES: u64 = 16 * 1024;
const FAVOURITE: &str = "m.favourite";
const LOW_PRIORITY: &str = "m.lowpriority";

/// A tag `order` in `0.0..=1.0`, compared numerically.
///
/// Stored as the bit pattern of the non-negative value, whose integer order
/// is the numeric order, so room models stay `Eq` without losing precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TagOrder(u64);

impl<'de> Deserialize<'de> for TagOrder {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Keep the existing serialized bit representation, but enforce the
        // same invariant as construction before it enters a room model.
        let bits = u64::deserialize(deserializer)?;
        Self::new(f64::from_bits(bits)).ok_or_else(|| {
            serde::de::Error::custom("room tag order must be finite and between 0 and 1")
        })
    }
}

impl TagOrder {
    /// `None` unless `value` is finite and within `0.0..=1.0`.
    pub fn new(value: f64) -> Option<Self> {
        // Adding +0.0 turns -0.0 into +0.0, so both share one bit pattern.
        (value.is_finite() && (0.0..=1.0).contains(&value)).then(|| Self((value + 0.0).to_bits()))
    }

    pub fn value(self) -> f64 {
        f64::from_bits(self.0)
    }
}

/// One explicit organization change for a room. The order applies only to
/// a newly added tag; an existing server tag keeps its metadata and order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomTagChange {
    /// Add `m.favourite` with this order and remove `m.lowpriority`.
    Favourite(Option<TagOrder>),
    /// Add `m.lowpriority` with this order and remove `m.favourite`.
    LowPriority(Option<TagOrder>),
    /// Remove `m.favourite`.
    RemoveFavourite,
    /// Remove `m.lowpriority`.
    RemoveLowPriority,
}

/// Cached display knowledge while sync lags behind a caller's own writes.
/// These hints never replace the fresh server read used for mutations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TagKnowledge {
    /// Nothing beyond the synced display state.
    #[default]
    Stored,
    /// A write the homeserver confirmed left the tag present.
    Present,
    /// A write the homeserver confirmed left the tag absent.
    Absent,
    /// A failed request may have written the tag. Its current server state
    /// must be read before deciding another mutation.
    Uncertain,
}

/// The caller's cached display knowledge. A fresh
/// homeserver read, never this hint, decides every mutation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfirmedTags {
    pub favourite: TagKnowledge,
    pub low_priority: TagKnowledge,
}

/// One favourite's new order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FavouriteOrder {
    pub room_id: String,
    pub order: TagOrder,
    /// A legacy display hint. Reordering still requires the favourite tag
    /// to exist in the fresh homeserver read.
    pub confirmed_favourite: bool,
}

/// The authoritative baseline with this operation's accepted writes applied.
/// A no-op carries the server's actual tags and orders, not the UI's plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomTagState {
    pub room_id: String,
    pub favourite: bool,
    pub low_priority: bool,
    pub favourite_order: Option<TagOrder>,
    pub low_priority_order: Option<TagOrder>,
}

impl RoomTagState {
    fn from_tags(room: &RoomId, tags: &Map<String, Value>) -> Self {
        Self {
            room_id: room.to_string(),
            favourite: tags.contains_key(FAVOURITE),
            low_priority: tags.contains_key(LOW_PRIORITY),
            favourite_order: tags.get(FAVOURITE).and_then(tag_order),
            low_priority_order: tags.get(LOW_PRIORITY).and_then(tag_order),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomTagError {
    #[error("The selected account changed")]
    AccountChanged,
    #[error("This room is not joined")]
    RoomUnavailable,
    #[error("This room's tag carries data from another app and was left unchanged")]
    UnsupportedTagData,
    #[error("This room is no longer a favorite")]
    NotFavourite,
    #[error("Too many favorites to reorder at once")]
    TooMany,
    #[error("The homeserver did not confirm the change")]
    NotConfirmed,
    #[error("Only part of the change was saved")]
    PartlySaved,
    #[error("This room's tags changed elsewhere; nothing was changed")]
    Changed,
    #[error("The homeserver refused the change")]
    Refused,
    #[error("The change may or may not have been saved; it was not retried")]
    Unknown,
}

/// The organization of one room as the SDK last stored it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TagSummary {
    pub(crate) favourite: bool,
    pub(crate) low_priority: bool,
    /// Each tag's own stored order, for a tag the room has.
    pub(crate) favourite_order: Option<TagOrder>,
    pub(crate) low_priority_order: Option<TagOrder>,
}

/// Cached tag flags; the stored tags are read only for tagged rooms.
pub(crate) async fn tag_summary(room: &Room) -> TagSummary {
    let favourite = room.is_favourite();
    let low_priority = room.is_low_priority();
    if !favourite && !low_priority {
        return TagSummary::default();
    }
    let tags = stored_tags(room).await.unwrap_or_default();
    // Both orders are kept: a room tagged both ways keeps its low-priority
    // order when it leaves Favorites.
    let order = |name: &str| tags.get(name).and_then(tag_order);
    TagSummary {
        favourite,
        low_priority,
        favourite_order: order(FAVOURITE).filter(|_| favourite),
        low_priority_order: order(LOW_PRIORITY).filter(|_| low_priority),
    }
}

/// The room's stored `m.tag` `tags` object, exactly as synced.
async fn stored_tags(room: &Room) -> std::result::Result<Map<String, Value>, RoomTagError> {
    let raw = room
        .account_data(RoomAccountDataEventType::Tag)
        .await
        .map_err(|_| RoomTagError::NotConfirmed)?;
    let Some(raw) = raw else {
        return Ok(Map::new());
    };
    let content: Option<Value> = raw
        .get_field("content")
        .map_err(|_| RoomTagError::UnsupportedTagData)?;
    Ok(content
        .as_ref()
        .and_then(|content| content.get("tags"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

/// A stored order: a number, or a numeric string as some clients write it.
fn tag_order(info: &Value) -> Option<TagOrder> {
    let order = info.get("order")?;
    let value = order.as_f64().or_else(|| {
        order
            .as_str()
            .and_then(|text| text.trim().parse::<f64>().ok())
    })?;
    TagOrder::new(value)
}

/// Rewriting this tag through the typed request would drop its other fields.
fn carries_other_fields(tags: &Map<String, Value>, name: &str) -> bool {
    tags.get(name).is_some_and(|info| match info.as_object() {
        Some(info) => info.keys().any(|key| key != "order"),
        // Not an object: nothing Morpheus can rewrite faithfully.
        None => true,
    })
}

type TagWrites = (
    Option<(&'static str, Option<TagOrder>)>,
    Option<&'static str>,
);

/// Decide solely from the fresh server read. Display hints cannot recreate
/// a removed tag or overwrite an existing tag's order and foreign metadata.
fn tag_writes(
    change: RoomTagChange,
    _confirmed: ConfirmedTags,
    tags: &Map<String, Value>,
) -> TagWrites {
    let (add, order, remove) = match change {
        RoomTagChange::Favourite(order) => (Some(FAVOURITE), order, Some(LOW_PRIORITY)),
        RoomTagChange::LowPriority(order) => (Some(LOW_PRIORITY), order, Some(FAVOURITE)),
        RoomTagChange::RemoveFavourite => (None, None, Some(FAVOURITE)),
        RoomTagChange::RemoveLowPriority => (None, None, Some(LOW_PRIORITY)),
    };
    let add = add.filter(|&name| !tags.contains_key(name));
    let remove = remove.filter(|&name| tags.contains_key(name));
    (add.map(|name| (name, order)), remove)
}

/// Validate the complete change before its first request. A typed PUT must
/// never discard another client's fields, just as reordering must not.
fn checked_tag_writes(
    change: RoomTagChange,
    confirmed: ConfirmedTags,
    tags: &Map<String, Value>,
) -> std::result::Result<TagWrites, RoomTagError> {
    let writes = tag_writes(change, confirmed, tags);
    if writes
        .0
        .is_some_and(|(name, _)| carries_other_fields(tags, name))
    {
        return Err(RoomTagError::UnsupportedTagData);
    }
    Ok(writes)
}

fn tag_info(order: Option<TagOrder>) -> TagInfo {
    let mut info = TagInfo::new();
    info.order = order.map(TagOrder::value);
    info
}

fn read_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(TAG_REQUEST_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_TAG_CONTENT_BYTES)
        // STAGED: root's d4c5d08 SDK request API.
        .disable_auth_refresh()
}

fn write_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(TAG_REQUEST_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_TAG_WRITE_RESPONSE_BYTES)
        // STAGED: root's d4c5d08 SDK request API.
        .disable_auth_refresh()
}

/// The homeserver answered 404 `M_NOT_FOUND`: the room has no tags.
fn not_found(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|answer| {
        answer.status_code.as_u16() == 404 && answer.error_kind() == Some(&ErrorKind::NotFound)
    })
}

/// A client error: the homeserver answered and did not apply the write.
fn refused_by_server(error: &HttpError) -> bool {
    error
        .as_client_api_error()
        .is_some_and(|answer| answer.status_code.is_client_error())
}

/// The `tags` object of raw `m.tag` content. Content that is not an object,
/// or `tags` that is not one, is refused rather than read as no tags, so a
/// write never goes ahead on a list Morpheus cannot read.
fn content_tags(raw: &str) -> std::result::Result<Map<String, Value>, RoomTagError> {
    let content: Value = serde_json::from_str(raw).map_err(|_| RoomTagError::UnsupportedTagData)?;
    let content = content
        .as_object()
        .ok_or(RoomTagError::UnsupportedTagData)?;
    match content.get("tags") {
        None => Ok(Map::new()),
        Some(Value::Object(tags)) => Ok(tags.clone()),
        Some(_) => Err(RoomTagError::UnsupportedTagData),
    }
}

/// The tags one accepted write leaves: what the next re-read must find.
fn after_write(
    tags: &mut Map<String, Value>,
    name: &str,
    added: Option<Option<TagOrder>>,
) -> std::result::Result<(), RoomTagError> {
    match added {
        Some(order) => {
            let info = serde_json::to_value(tag_info(order)).map_err(|_| RoomTagError::Unknown)?;
            tags.insert(name.to_owned(), info);
        }
        None => {
            tags.remove(name);
        }
    }
    Ok(())
}

/// How far one change got, for a truthful result when its wait ends early.
#[derive(Default)]
struct TagProgress {
    /// Writes the homeserver accepted.
    written: AtomicUsize,
    /// A write was handed to the SDK and its answer has not come back.
    in_flight: AtomicBool,
}

impl TagProgress {
    /// The result for a change that stopped with no write in flight.
    fn stopped(&self, nothing_saved: RoomTagError) -> RoomTagError {
        if self.in_flight.load(Ordering::Acquire) {
            RoomTagError::Unknown
        } else if self.written.load(Ordering::Acquire) > 0 {
            RoomTagError::PartlySaved
        } else {
            nothing_saved
        }
    }
}

/// One room's tag reads and writes for the account `user`.
struct TagWriter<'a> {
    client: &'a Client,
    user: OwnedUserId,
    progress: &'a TagProgress,
}

impl TagWriter<'_> {
    /// One fresh, bounded read of the room's tags from the homeserver.
    async fn read(&self, room: &RoomId) -> std::result::Result<Map<String, Value>, RoomTagError> {
        let request = get_room_account_data::v3::Request::new(
            self.user.clone(),
            room.to_owned(),
            RoomAccountDataEventType::Tag,
        );
        match self
            .client
            .send(request)
            .with_request_config(read_config())
            .await
        {
            Ok(response) => content_tags(response.account_data.json().get())
                .map_err(|error| self.progress.stopped(error)),
            Err(error) if not_found(&error) => Ok(Map::new()),
            Err(_) => Err(self.progress.stopped(RoomTagError::NotConfirmed)),
        }
    }

    /// Re-read immediately before a write: the room must still have exactly
    /// `expected`, or nothing more is sent.
    async fn unchanged(
        &self,
        room: &RoomId,
        expected: &Map<String, Value>,
    ) -> std::result::Result<(), RoomTagError> {
        if self.read(room).await? != *expected {
            return Err(self.progress.stopped(RoomTagError::Changed));
        }
        Ok(())
    }

    /// One write, sent once: `Some(order)` adds the tag, `None` removes it.
    async fn write(
        &self,
        room: &RoomId,
        name: &str,
        added: Option<Option<TagOrder>>,
    ) -> std::result::Result<(), RoomTagError> {
        let (user, room_id) = (self.user.clone(), room.to_owned());
        self.progress.in_flight.store(true, Ordering::Release);
        let sent = match added {
            Some(order) => {
                let request =
                    create_tag::v3::Request::new(user, room_id, name.to_owned(), tag_info(order));
                let sent = self
                    .client
                    .send(request)
                    .with_request_config(write_config());
                sent.await.map(|_| ())
            }
            None => {
                let request = delete_tag::v3::Request::new(user, room_id, name.to_owned());
                let sent = self
                    .client
                    .send(request)
                    .with_request_config(write_config());
                sent.await.map(|_| ())
            }
        };
        self.progress.in_flight.store(false, Ordering::Release);
        match sent {
            Ok(()) => {
                self.progress.written.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
            Err(error) if refused_by_server(&error) => {
                Err(self.progress.stopped(RoomTagError::Refused))
            }
            Err(_) => Err(RoomTagError::Unknown),
        }
    }
}

/// One organization change on the homeserver's tags: a fresh read decides
/// the writes, and each write is preceded by a re-read that must still find
/// the baseline with this change's accepted writes applied.
async fn apply_change(
    writer: &TagWriter<'_>,
    room: &RoomId,
    change: RoomTagChange,
    confirmed: ConfirmedTags,
) -> std::result::Result<RoomTagState, RoomTagError> {
    let mut expected = writer.read(room).await?;
    let (add, remove) = checked_tag_writes(change, confirmed, &expected)?;
    if let Some((name, order)) = add {
        writer.unchanged(room, &expected).await?;
        writer.write(room, name, Some(order)).await?;
        after_write(&mut expected, name, Some(order))?;
    }
    if let Some(name) = remove {
        writer.unchanged(room, &expected).await?;
        writer.write(room, name, None).await?;
        after_write(&mut expected, name, None)?;
    }
    Ok(RoomTagState::from_tags(room, &expected))
}

/// New favourite orders: every room is checked on the homeserver's tags
/// before the first write, and each baseline is re-read right before its
/// write.
async fn apply_orders(
    writer: &TagWriter<'_>,
    rooms: &[(OwnedRoomId, TagOrder, bool)],
) -> std::result::Result<Vec<RoomTagState>, RoomTagError> {
    let mut baselines = Vec::with_capacity(rooms.len());
    for (room, _, _) in rooms {
        let tags = writer.read(room).await?;
        if !tags.contains_key(FAVOURITE) {
            return Err(RoomTagError::NotFavourite);
        }
        if carries_other_fields(&tags, FAVOURITE) {
            return Err(RoomTagError::UnsupportedTagData);
        }
        baselines.push(tags);
    }
    let mut applied = Vec::with_capacity(rooms.len());
    for ((room, order, _), baseline) in rooms.iter().zip(&mut baselines) {
        writer.unchanged(room, baseline).await?;
        writer.write(room, FAVOURITE, Some(Some(*order))).await?;
        after_write(baseline, FAVOURITE, Some(Some(*order)))?;
        applied.push(RoomTagState::from_tags(room, baseline));
    }
    Ok(applied)
}

impl AccountManager {
    fn owns_tag_selection(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| selection.same_epoch(&current))
    }

    /// The selected account's client while `selection` still owns it. The
    /// manager lock is released before any homeserver request.
    async fn tag_client(
        &self,
        selection: &AccountSelection,
    ) -> std::result::Result<Client, RoomTagError> {
        if !self.owns_tag_selection(selection) {
            return Err(RoomTagError::AccountChanged);
        }
        let state = selection
            .guard(self.state.lock())
            .await
            .ok_or(RoomTagError::AccountChanged)?;
        if !self.owns_tag_selection(selection) {
            return Err(RoomTagError::AccountChanged);
        }
        super::account_access::ensure_eligible(&state, selection.account_id())
            .map_err(|_| RoomTagError::AccountChanged)?;
        Ok(selected_loaded_for(&state, selection.account_id())
            .map_err(|_| RoomTagError::AccountChanged)?
            .client
            .clone())
    }

    fn joined_room(client: &Client, room_id: &str) -> std::result::Result<Room, RoomTagError> {
        let id = RoomId::parse(room_id).map_err(|_| RoomTagError::RoomUnavailable)?;
        client
            .get_room(&id)
            .filter(|room| matches!(room.state(), RoomState::Joined) && !room.is_space())
            .ok_or(RoomTagError::RoomUnavailable)
    }

    /// Run one bounded change under `selection`; revocation or a timeout
    /// ends the wait and its result is never returned. `progress` makes the
    /// result truthful: a write still in flight is Unknown, an accepted one
    /// with nothing in flight is PartlySaved, and only a change that saved
    /// nothing reports AccountChanged or NotConfirmed.
    async fn under_tag_selection<T>(
        &self,
        selection: &AccountSelection,
        timeout: Duration,
        progress: &TagProgress,
        request: impl std::future::Future<Output = std::result::Result<T, RoomTagError>>,
    ) -> std::result::Result<T, RoomTagError> {
        let result = selection
            .guard(tokio::time::timeout(timeout, request))
            .await;
        let result = match result {
            None => return Err(progress.stopped(RoomTagError::AccountChanged)),
            Some(Err(_)) => return Err(progress.stopped(RoomTagError::NotConfirmed)),
            Some(Ok(result)) => result?,
        };
        if !self.owns_tag_selection(selection) {
            return Err(progress.stopped(RoomTagError::AccountChanged));
        }
        Ok(result)
    }

    /// Apply one explicit organization change to a joined room. Unrelated
    /// tags are untouched, and a tag the server has is not rewritten. The
    /// returned state includes preserved server orders even when no write
    /// was necessary; cached `confirmed` hints cannot override the read.
    pub async fn change_room_tags_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        change: RoomTagChange,
        confirmed: ConfirmedTags,
    ) -> std::result::Result<Vec<RoomTagState>, RoomTagError> {
        let client = self.tag_client(selection).await?;
        let room = Self::joined_room(&client, room_id)?;
        let user = client
            .user_id()
            .ok_or(RoomTagError::AccountChanged)?
            .to_owned();
        let progress = TagProgress::default();
        let writer = TagWriter {
            client: &client,
            user,
            progress: &progress,
        };
        // Decided from the homeserver's tags, not the lagging store.
        let request = apply_change(&writer, room.room_id(), change, confirmed);
        self.under_tag_selection(selection, TAG_TIMEOUT, &progress, request)
            .await
            .map(|state| vec![state])
    }

    /// Write new favourite orders, at most `MAX_TAG_ORDER_WRITES` rooms.
    /// Every room is checked first (joined, a favourite on the homeserver,
    /// no other fields in its favourite tag); nothing is written if
    /// any check fails.
    pub async fn set_favourite_orders_selected(
        &self,
        selection: &AccountSelection,
        orders: Vec<FavouriteOrder>,
    ) -> std::result::Result<Vec<RoomTagState>, RoomTagError> {
        if orders.len() > MAX_TAG_ORDER_WRITES {
            return Err(RoomTagError::TooMany);
        }
        let client = self.tag_client(selection).await?;
        let rooms = orders
            .into_iter()
            .map(|write| {
                let room = Self::joined_room(&client, &write.room_id)?;
                Ok((
                    room.room_id().to_owned(),
                    write.order,
                    write.confirmed_favourite,
                ))
            })
            .collect::<std::result::Result<Vec<_>, RoomTagError>>()?;
        let timeout = TAG_TIMEOUT + ORDER_WRITE_TIMEOUT * rooms.len() as u32;
        let user = client
            .user_id()
            .ok_or(RoomTagError::AccountChanged)?
            .to_owned();
        let progress = TagProgress::default();
        let writer = TagWriter {
            client: &client,
            user,
            progress: &progress,
        };
        let request = apply_orders(&writer, &rooms);
        self.under_tag_selection(selection, timeout, &progress, request)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use uuid::Uuid;

    use super::*;
    use crate::{AccountInfo, LoadedAccount};

    #[test]
    fn orders_are_bounded_and_compare_numerically() {
        assert!(TagOrder::new(-0.1).is_none());
        assert!(TagOrder::new(1.1).is_none());
        assert!(TagOrder::new(f64::NAN).is_none());
        assert!(TagOrder::new(f64::INFINITY).is_none());
        assert_eq!(TagOrder::new(-0.0), TagOrder::new(0.0));
        let values = [0.0, 1e-300, 0.25, 0.250_000_000_000_001, 0.5, 1.0];
        let orders = values.map(|value| TagOrder::new(value).unwrap());
        assert!(orders.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(orders[3].value(), 0.250_000_000_000_001);
    }

    #[test]
    fn deserialized_orders_cannot_bypass_numeric_bounds() {
        for value in [0.0_f64, 0.5, 1.0] {
            let order = TagOrder::new(value).unwrap();
            let encoded = serde_json::to_string(&order).unwrap();
            assert_eq!(serde_json::from_str::<TagOrder>(&encoded).unwrap(), order);
        }
        for value in [-0.1_f64, 1.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let encoded = value.to_bits().to_string();
            assert!(serde_json::from_str::<TagOrder>(&encoded).is_err());
        }
        let negative_zero = (-0.0_f64).to_bits().to_string();
        assert_eq!(
            serde_json::from_str::<TagOrder>(&negative_zero).unwrap(),
            TagOrder::new(0.0).unwrap()
        );
    }

    #[test]
    fn fresh_tags_govern_writes_despite_every_cached_hint() {
        let stored = serde_json::json!({"m.lowpriority": {"order": 0.3}, "u.work": {}});
        let stored = stored.as_object().unwrap();
        let order = TagOrder::new(0.5);
        // The low-priority order the caller shows.
        let low = RoomTagChange::LowPriority(TagOrder::new(0.3));
        for knowledge in [
            TagKnowledge::Stored,
            TagKnowledge::Present,
            TagKnowledge::Absent,
            TagKnowledge::Uncertain,
        ] {
            let hint = ConfirmedTags {
                favourite: knowledge,
                low_priority: knowledge,
            };
            assert_eq!(
                tag_writes(RoomTagChange::Favourite(order), hint, stored),
                (Some((FAVOURITE, order)), Some(LOW_PRIORITY))
            );
            assert_eq!(tag_writes(low, hint, stored), (None, None));
            assert_eq!(
                tag_writes(RoomTagChange::RemoveLowPriority, hint, stored),
                (None, Some(LOW_PRIORITY))
            );
            assert_eq!(
                tag_writes(RoomTagChange::RemoveLowPriority, hint, &Map::new()),
                (None, None)
            );
        }
    }

    #[test]
    fn retrying_uncertain_tags_never_discards_other_clients_metadata() {
        for (name, change) in [
            (FAVOURITE, RoomTagChange::Favourite(TagOrder::new(0.5))),
            (LOW_PRIORITY, RoomTagChange::LowPriority(TagOrder::new(0.5))),
        ] {
            for info in [
                serde_json::json!({"order": 0.25, "colour": "mint"}),
                serde_json::json!("another client's representation"),
            ] {
                let mut tags = Map::new();
                tags.insert(name.to_owned(), info);
                tags.insert("u.work".to_owned(), serde_json::json!({"colour": "blue"}));
                let before = tags.clone();
                // A failed PUT or earlier DELETE cannot overrule a fresh
                // server tag, regardless of the caller's display hint.
                for knowledge in [TagKnowledge::Uncertain, TagKnowledge::Absent] {
                    let confirmed = ConfirmedTags {
                        favourite: knowledge,
                        low_priority: knowledge,
                    };
                    assert!(
                        checked_tag_writes(change, confirmed, &tags)
                            .unwrap()
                            .0
                            .is_none()
                    );
                    assert_eq!(tags, before);
                }
                // Keeping an existing tag does not rewrite its metadata.
                let writes = checked_tag_writes(change, ConfirmedTags::default(), &tags).unwrap();
                assert!(writes.0.is_none());
                assert_eq!(tags, before);
            }
            // Existing ordinary tags keep their server order too.
            let mut tags = Map::new();
            tags.insert(name.to_owned(), serde_json::json!({"order": 0.25}));
            tags.insert("u.work".to_owned(), serde_json::json!({"colour": "blue"}));
            let confirmed = ConfirmedTags {
                favourite: TagKnowledge::Uncertain,
                low_priority: TagKnowledge::Uncertain,
            };
            assert!(
                checked_tag_writes(change, confirmed, &tags)
                    .unwrap()
                    .0
                    .is_none()
            );
        }
    }

    #[test]
    fn stored_orders_and_foreign_fields_are_read_faithfully() {
        let tags = serde_json::json!({
            "m.favourite": {"order": 0.25},
            "m.lowpriority": {"order": "0.75"},
            "u.work": {"order": 0.1, "colour": "blue"},
            "m.server_notice": {}
        });
        let tags = tags.as_object().unwrap().clone();
        assert_eq!(tags.get(FAVOURITE).and_then(tag_order), TagOrder::new(0.25));
        assert_eq!(
            tags.get(LOW_PRIORITY).and_then(tag_order),
            TagOrder::new(0.75)
        );
        assert!(!carries_other_fields(&tags, FAVOURITE));
        assert!(carries_other_fields(&tags, "u.work"));
        assert!(!carries_other_fields(&tags, "m.server_notice"));
        assert!(!carries_other_fields(&tags, "absent"));
        let odd = serde_json::json!({"m.favourite": "pinned"});
        assert!(carries_other_fields(odd.as_object().unwrap(), FAVOURITE));
        assert_eq!(tag_info(TagOrder::new(0.5)).order, Some(0.5));
        assert_eq!(tag_info(None).order, None);
    }

    /// A change captured before A -> B -> A is refused before any request,
    /// and the current selection still reaches only joined rooms and
    /// bounded reorders. No homeserver is contacted.
    #[test]
    fn a_superseded_selection_changes_no_tags() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = std::env::temp_dir().join(format!("morpheus-room-tags-{}", Uuid::new_v4()));
            let manager = AccountManager::open(&root).unwrap();
            let homeserver = "http://127.0.0.1:9";
            let account = AccountInfo {
                id: Uuid::new_v4().to_string(),
                user_id: "@alice:localhost".into(),
                device_id: "ALICE".into(),
                homeserver: homeserver.into(),
            };
            let client = Client::builder()
                .homeserver_url(homeserver)
                .build()
                .await
                .unwrap();
            let loaded = Arc::new(LoadedAccount::new(
                client,
                account.id.clone(),
                String::new(),
                manager._access_lock.clone(),
            ));
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![account.clone()];
                state.active_account_id = Some(account.id.clone());
                state.loaded.insert(account.id.clone(), loaded);
            }
            manager.selections.replace(Some(&account.id));
            let stale = manager.account_selection(&account.id).unwrap();
            manager.selections.replace(None);
            manager.selections.replace(Some(&account.id));
            let order = TagOrder::new(0.5).unwrap();
            let room = "!room:localhost";
            let write = |room_id: String| FavouriteOrder {
                room_id,
                order,
                confirmed_favourite: false,
            };
            assert_eq!(
                manager
                    .change_room_tags_selected(
                        &stale,
                        room,
                        RoomTagChange::LowPriority(None),
                        ConfirmedTags::default(),
                    )
                    .await,
                Err(RoomTagError::AccountChanged)
            );
            assert_eq!(
                manager
                    .set_favourite_orders_selected(&stale, vec![write(room.to_owned())])
                    .await,
                Err(RoomTagError::AccountChanged)
            );
            let current = manager.account_selection(&account.id).unwrap();
            assert_eq!(
                manager
                    .change_room_tags_selected(
                        &current,
                        room,
                        RoomTagChange::LowPriority(None),
                        ConfirmedTags::default(),
                    )
                    .await,
                Err(RoomTagError::RoomUnavailable)
            );
            let too_many = (0..=MAX_TAG_ORDER_WRITES)
                .map(|index| write(format!("!{index}:localhost")))
                .collect();
            assert_eq!(
                manager
                    .set_favourite_orders_selected(&current, too_many)
                    .await,
                Err(RoomTagError::TooMany)
            );
            manager.selections.replace(None);
            drop(manager);
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn tag_requests_are_bounded_single_attempts() {
        for config in [read_config(), write_config()] {
            let shown = format!("{config:?}");
            assert!(shown.contains("retry_limit: 0") || shown.contains("retry_limit: Some(0)"));
            assert!(shown.contains("force_auth: true"));
        }
        assert!(format!("{:?}", read_config()).contains(&MAX_TAG_CONTENT_BYTES.to_string()));
        let write = format!("{:?}", write_config());
        assert!(write.contains(&MAX_TAG_WRITE_RESPONSE_BYTES.to_string()));
    }

    #[test]
    fn unreadable_content_is_refused_and_results_stay_truthful() {
        let tags = content_tags(r#"{"tags":{"m.favourite":{"order":0.5}},"x":1}"#).unwrap();
        assert!(tags.contains_key(FAVOURITE));
        assert_eq!(content_tags(r#"{"x":1}"#), Ok(Map::new()));
        for odd in ["[]", r#"{"tags":[]}"#, r#"{"tags":"x"}"#, "not json"] {
            assert_eq!(content_tags(odd), Err(RoomTagError::UnsupportedTagData));
        }
        let mut expected = tags.clone();
        after_write(&mut expected, LOW_PRIORITY, Some(TagOrder::new(0.25))).unwrap();
        assert_eq!(expected[LOW_PRIORITY], serde_json::json!({"order": 0.25}));
        after_write(&mut expected, FAVOURITE, None).unwrap();
        assert!(!expected.contains_key(FAVOURITE));
        after_write(&mut expected, FAVOURITE, Some(None)).unwrap();
        assert_eq!(expected[FAVOURITE], serde_json::json!({}));
        let progress = TagProgress::default();
        assert_eq!(
            progress.stopped(RoomTagError::Changed),
            RoomTagError::Changed
        );
        progress.written.store(1, Ordering::Release);
        assert_eq!(
            progress.stopped(RoomTagError::Changed),
            RoomTagError::PartlySaved
        );
        progress.in_flight.store(true, Ordering::Release);
        assert_eq!(
            progress.stopped(RoomTagError::Changed),
            RoomTagError::Unknown
        );
    }

    /// A loopback homeserver holding per-room `m.tag` content. It answers
    /// the version check, serves the content (404 M_NOT_FOUND when absent),
    /// records every tag PUT and DELETE, and applies each accepted one.
    /// `answers` overrides the next writes' statuses; `edit_at_read` makes
    /// another client change one room when the given read arrives.
    struct TagServer {
        url: String,
        served: Arc<std::sync::Mutex<Served>>,
        stopped: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    #[derive(Default)]
    struct Served {
        contents: std::collections::HashMap<String, Value>,
        writes: Vec<(String, String, String, String)>,
        reads: usize,
        edit_at_read: Option<(usize, String, Value)>,
        answers: std::collections::VecDeque<u16>,
    }

    impl TagServer {
        fn start(contents: &[(&str, Value)]) -> Self {
            use std::io::Write;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let served = Arc::new(std::sync::Mutex::new(Served {
                contents: contents
                    .iter()
                    .map(|(room, content)| ((*room).to_owned(), content.clone()))
                    .collect(),
                ..Served::default()
            }));
            let stopped = Arc::new(AtomicBool::new(false));
            let (shared, stop) = (served.clone(), stopped.clone());
            let worker = std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let (line, body) = read_request(&mut stream);
                    let (status, reply) = answer(&mut shared.lock().unwrap(), &line, &body);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self {
                url,
                served,
                stopped,
                worker: Some(worker),
            }
        }

        fn writes(&self) -> Vec<(String, String, String, String)> {
            self.served.lock().unwrap().writes.clone()
        }
    }

    impl Drop for TagServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// The room key (`one`, `two`) a request line names.
    fn room_key(line: &str) -> Option<String> {
        ["one", "two"]
            .into_iter()
            .find(|room| line.contains(&format!("%21{room}%3A")))
            .map(str::to_owned)
    }

    fn answer(served: &mut Served, line: &str, body: &str) -> (u16, String) {
        let missing = serde_json::json!({"errcode": "M_NOT_FOUND", "error": "x"}).to_string();
        if line.contains("/_matrix/client/versions") {
            return (200, serde_json::json!({"versions": ["v1.15"]}).to_string());
        }
        let Some(room) = room_key(line) else {
            return (404, missing);
        };
        if line.starts_with("GET ") && line.contains("/account_data/m.tag") {
            served.reads += 1;
            if let Some((at, edited, content)) = served.edit_at_read.clone()
                && at == served.reads
            {
                served.contents.insert(edited, content);
            }
            return match served.contents.get(&room) {
                Some(content) => (200, content.to_string()),
                None => (404, missing),
            };
        }
        let Some(tag) = line
            .split("/tags/")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
        else {
            return (404, missing);
        };
        let method = line.split(' ').next().unwrap_or("").to_owned();
        served.writes.push((
            method.clone(),
            room.clone(),
            tag.to_owned(),
            body.to_owned(),
        ));
        let status = served.answers.pop_front().unwrap_or(200);
        if status != 200 {
            let errcode = if status < 500 {
                "M_FORBIDDEN"
            } else {
                "M_UNKNOWN"
            };
            return (
                status,
                serde_json::json!({"errcode": errcode, "error": "x"}).to_string(),
            );
        }
        let content = served
            .contents
            .entry(room)
            .or_insert_with(|| serde_json::json!({"tags": {}}));
        let tags = content["tags"].as_object_mut().unwrap();
        if method == "PUT" {
            tags.insert(tag.to_owned(), serde_json::from_str(body).unwrap());
        } else {
            tags.remove(tag);
        }
        (200, "{}".to_owned())
    }

    /// The request line and body of one HTTP/1.1 request.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
        use std::io::Read;
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

    /// A client signed in with a synthetic session against `server`.
    async fn signed_in(server: &TagServer) -> Client {
        let client = Client::builder()
            .homeserver_url(&server.url)
            .build()
            .await
            .unwrap();
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: matrix_sdk::ruma::UserId::parse("@alice:localhost").unwrap(),
                device_id: "TAGS-FIXTURE".into(),
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
        client
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn room(key: &str) -> OwnedRoomId {
        RoomId::parse(format!("!{key}:localhost")).unwrap()
    }

    /// One change: a fresh read decides, each write is re-checked first and
    /// sent once, unrelated tags are never written, and every failure says
    /// truthfully what was saved.
    #[test]
    fn a_change_writes_each_tag_once_against_a_fresh_unchanged_baseline() {
        runtime().block_on(async {
            let stored = serde_json::json!({"tags": {
                "m.lowpriority": {"order": 0.3},
                "u.work": {"colour": "blue"},
            }, "com.example.extra": true});
            let favourite = RoomTagChange::Favourite(TagOrder::new(0.5));
            let nothing = ConfirmedTags::default();
            let server = TagServer::start(&[("one", stored.clone())]);
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let user = client.user_id().unwrap().to_owned();
            let writer = TagWriter {
                client: &client,
                user,
                progress: &progress,
            };
            apply_change(&writer, &room("one"), favourite, nothing)
                .await
                .unwrap();
            let writes = server.writes();
            assert_eq!(writes.len(), 2);
            assert_eq!(
                (writes[0].0.as_str(), writes[0].2.as_str()),
                ("PUT", FAVOURITE)
            );
            let body: Value = serde_json::from_str(&writes[0].3).unwrap();
            assert_eq!(body, serde_json::json!({"order": 0.5}));
            assert_eq!(
                (writes[1].0.as_str(), writes[1].2.as_str()),
                ("DELETE", LOW_PRIORITY)
            );
            // Already as asked: read, nothing written.
            apply_change(&writer, &room("one"), favourite, nothing)
                .await
                .unwrap();
            assert_eq!(server.writes().len(), 2);
            drop(server);

            // Another client changes the tags between the read and the
            // first write: refused, nothing written.
            let server = TagServer::start(&[("one", stored.clone())]);
            let edited = serde_json::json!({"tags": {"u.work": {}}});
            server.served.lock().unwrap().edit_at_read = Some((2, "one".into(), edited));
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            let changed = apply_change(&writer, &room("one"), favourite, nothing).await;
            assert_eq!(changed, Err(RoomTagError::Changed));
            assert!(server.writes().is_empty());
            drop(server);

            // ... or between the two writes: the first stays, partly saved.
            let server = TagServer::start(&[("one", stored.clone())]);
            let edited = serde_json::json!({"tags": {"m.favourite": {"order": 0.9}}});
            server.served.lock().unwrap().edit_at_read = Some((3, "one".into(), edited));
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            let partly = apply_change(&writer, &room("one"), favourite, nothing).await;
            assert_eq!(partly, Err(RoomTagError::PartlySaved));
            assert_eq!(server.writes().len(), 1);
            drop(server);

            // Refused first write: nothing saved. Refused second: partly.
            // A server error: unknown, and never sent again.
            for (answers, expected, sent) in [
                (vec![403], RoomTagError::Refused, 1),
                (vec![200, 403], RoomTagError::PartlySaved, 2),
                (vec![500], RoomTagError::Unknown, 1),
                (vec![200, 502], RoomTagError::Unknown, 2),
            ] {
                let server = TagServer::start(&[("one", stored.clone())]);
                server.served.lock().unwrap().answers = answers.into();
                let client = signed_in(&server).await;
                let progress = TagProgress::default();
                let writer = TagWriter {
                    client: &client,
                    user: client.user_id().unwrap().to_owned(),
                    progress: &progress,
                };
                let result = apply_change(&writer, &room("one"), favourite, nothing).await;
                assert_eq!(result, Err(expected));
                assert_eq!(server.writes().len(), sent);
            }

            // No tags stored yet (404): the tag is added. Tags Morpheus
            // cannot read are refused before anything is sent.
            let server = TagServer::start(&[("two", serde_json::json!({"tags": []}))]);
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            let low = RoomTagChange::LowPriority(None);
            let unreadable = apply_change(&writer, &room("two"), low, nothing).await;
            assert_eq!(unreadable, Err(RoomTagError::UnsupportedTagData));
            assert!(server.writes().is_empty());
            server.served.lock().unwrap().contents.clear();
            apply_change(&writer, &room("two"), low, nothing)
                .await
                .unwrap();
            let writes = server.writes();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].3, "{}");
        });
    }

    #[test]
    fn fresh_server_tags_and_orders_survive_stale_confirmation_hints() {
        runtime().block_on(async {
            let stored = serde_json::json!({"tags": {
                "m.favourite": {"order": 0.8, "colour": "mint"},
                "u.work": {"colour": "blue"}
            }});
            let server = TagServer::start(&[("one", stored.clone())]);
            let client = signed_in(&server).await;
            for knowledge in [
                TagKnowledge::Stored,
                TagKnowledge::Present,
                TagKnowledge::Absent,
                TagKnowledge::Uncertain,
            ] {
                let progress = TagProgress::default();
                let writer = TagWriter {
                    client: &client,
                    user: client.user_id().unwrap().to_owned(),
                    progress: &progress,
                };
                let hint = ConfirmedTags {
                    favourite: knowledge,
                    low_priority: knowledge,
                };
                let actual = apply_change(
                    &writer,
                    &room("one"),
                    RoomTagChange::Favourite(TagOrder::new(0.5)),
                    hint,
                )
                .await
                .unwrap();
                assert!(actual.favourite);
                assert_eq!(actual.favourite_order, TagOrder::new(0.8));
                assert!(server.writes().is_empty());
                assert_eq!(server.served.lock().unwrap().contents["one"], stored);
            }

            // A stale confirmed favourite never authorizes recreating one
            // that another client removed before this fresh server read.
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            let orders = [(room("two"), TagOrder::new(0.25).unwrap(), true)];
            assert_eq!(
                apply_orders(&writer, &orders).await,
                Err(RoomTagError::NotFavourite)
            );
            assert!(server.writes().is_empty());

            // Present/absent hints cannot suppress an explicit add/remove
            // when the fresh server state says it is needed.
            for (change, knowledge, expected) in [
                (
                    RoomTagChange::Favourite(TagOrder::new(0.4)),
                    TagKnowledge::Present,
                    true,
                ),
                (RoomTagChange::RemoveFavourite, TagKnowledge::Absent, false),
            ] {
                let progress = TagProgress::default();
                let writer = TagWriter {
                    client: &client,
                    user: client.user_id().unwrap().to_owned(),
                    progress: &progress,
                };
                let hint = ConfirmedTags {
                    favourite: knowledge,
                    low_priority: knowledge,
                };
                let actual = apply_change(&writer, &room("two"), change, hint)
                    .await
                    .unwrap();
                assert_eq!(actual.favourite, expected);
                assert_eq!(
                    actual.favourite_order,
                    TagOrder::new(0.4).filter(|_| expected)
                );
            }
            assert_eq!(server.writes().len(), 2);
        });
    }

    #[test]
    fn malformed_rereads_after_accepted_writes_report_partial_changes() {
        runtime().block_on(async {
            // The first PUT succeeds; malformed content before the second
            // write must not become "left unchanged" or authorize DELETE.
            let low = serde_json::json!({"tags": {"m.lowpriority": {"order": 0.3}}});
            let server = TagServer::start(&[("one", low)]);
            server.served.lock().unwrap().edit_at_read =
                Some((3, "one".into(), serde_json::json!({"tags": []})));
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            let result = apply_change(
                &writer,
                &room("one"),
                RoomTagChange::Favourite(TagOrder::new(0.5)),
                ConfirmedTags::default(),
            )
            .await;
            assert_eq!(result, Err(RoomTagError::PartlySaved));
            let writes = server.writes();
            assert_eq!(writes.len(), 1);
            assert_eq!(
                (writes[0].0.as_str(), writes[0].2.as_str()),
                ("PUT", FAVOURITE)
            );
            drop(server);

            // The same rule holds after an earlier room in a reorder was
            // accepted. No second room is written on an unreadable baseline.
            let favourite = serde_json::json!({"tags": {"m.favourite": {"order": 0.1}}});
            let server = TagServer::start(&[("one", favourite.clone()), ("two", favourite)]);
            server.served.lock().unwrap().edit_at_read =
                Some((4, "two".into(), serde_json::json!({"tags": "unreadable"})));
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            let orders = [
                (room("one"), TagOrder::new(0.2).unwrap(), false),
                (room("two"), TagOrder::new(0.4).unwrap(), false),
            ];
            assert_eq!(
                apply_orders(&writer, &orders).await,
                Err(RoomTagError::PartlySaved)
            );
            let writes = server.writes();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].1, "one");
        });
    }

    /// Reorders check every room on the homeserver before the first write,
    /// and re-check each baseline right before its own write.
    #[test]
    fn reorders_check_every_room_first_and_each_baseline_before_its_write() {
        runtime().block_on(async {
            let favourite = serde_json::json!({"tags": {"m.favourite": {"order": 0.1}}});
            let plain = serde_json::json!({"tags": {}});
            let orders = vec![
                (room("one"), TagOrder::new(0.2).unwrap(), false),
                (room("two"), TagOrder::new(0.4).unwrap(), false),
            ];
            let server = TagServer::start(&[("one", favourite.clone()), ("two", plain.clone())]);
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            // A room that is not a favourite: refused before any write.
            assert_eq!(
                apply_orders(&writer, &orders).await,
                Err(RoomTagError::NotFavourite)
            );
            assert!(server.writes().is_empty());
            drop(server);

            let both = [("one", favourite.clone()), ("two", favourite.clone())];
            let server = TagServer::start(&both);
            // The second room changes after the checks (reads 1-2) and the
            // first room's re-read (3): its re-read (4) refuses it.
            let edited = serde_json::json!({"tags": {"m.favourite": {"order": 0.7}}});
            server.served.lock().unwrap().edit_at_read = Some((4, "two".into(), edited));
            let client = signed_in(&server).await;
            let progress = TagProgress::default();
            let writer = TagWriter {
                client: &client,
                user: client.user_id().unwrap().to_owned(),
                progress: &progress,
            };
            assert_eq!(
                apply_orders(&writer, &orders).await,
                Err(RoomTagError::PartlySaved)
            );
            let writes = server.writes();
            assert_eq!(writes.len(), 1);
            assert_eq!(
                (writes[0].1.as_str(), writes[0].2.as_str()),
                ("one", FAVOURITE)
            );
        });
    }

    /// The selection runner reports what a stopped change saved: a write in
    /// flight is Unknown, an accepted one PartlySaved, otherwise the stop.
    #[test]
    fn a_stopped_change_reports_truthfully_what_it_saved() {
        runtime().block_on(async {
            let root = std::env::temp_dir().join(format!("morpheus-room-tags-{}", Uuid::new_v4()));
            let manager = AccountManager::open(&root).unwrap();
            let id = Uuid::new_v4().to_string();
            manager.selections.replace(Some(&id));
            for (written, in_flight, expected) in [
                (0, false, RoomTagError::AccountChanged),
                (1, false, RoomTagError::PartlySaved),
                (0, true, RoomTagError::Unknown),
                (1, true, RoomTagError::Unknown),
            ] {
                manager.selections.replace(Some(&id));
                let selection = manager.account_selection(&id).unwrap();
                let progress = TagProgress::default();
                progress.written.store(written, Ordering::Release);
                progress.in_flight.store(in_flight, Ordering::Release);
                let pending = std::future::pending::<std::result::Result<(), RoomTagError>>();
                let run = manager.under_tag_selection(&selection, TAG_TIMEOUT, &progress, pending);
                let revoke = async { manager.selections.replace(None) };
                let (result, ()) = tokio::join!(run, revoke);
                assert_eq!(result, Err(expected));
                // A timeout is the same, with NotConfirmed for nothing saved.
                manager.selections.replace(Some(&id));
                let selection = manager.account_selection(&id).unwrap();
                let pending = std::future::pending::<std::result::Result<(), RoomTagError>>();
                let short = Duration::from_millis(10);
                let timed = manager
                    .under_tag_selection(&selection, short, &progress, pending)
                    .await;
                let expected = match expected {
                    RoomTagError::AccountChanged => RoomTagError::NotConfirmed,
                    other => other,
                };
                assert_eq!(timed, Err(expected));
            }
            manager.selections.replace(None);
            drop(manager);
            let _ = std::fs::remove_dir_all(&root);
        });
    }
}
