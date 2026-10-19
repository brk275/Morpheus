//! Standard Matrix room organization: the `m.favourite` and `m.lowpriority`
//! room tags and their `order` metadata.
//!
//! Tags live in the account's per-room `m.tag` account data. Each tag is
//! written with its own request, so tags this module does not touch, such as
//! user `u.*` tags or other apps' tags, are never rewritten. Favourite and low
//! priority are kept mutually exclusive, as the pinned SDK does: adding one
//! removes the other. A tag whose stored info carries fields other than
//! `order` is never rewritten, because the typed request would drop them; a
//! change that would do so is refused before anything is sent.
//!
//! Every entry point is bound to the caller's `AccountSelection`; the room
//! must be joined when the request starts. A request already handed to the
//! homeserver cannot be recalled by a later switch; the caller only loses
//! the result, and sync brings the stored state back.

use std::time::Duration;

use matrix_sdk::{
    Client,
    ruma::{
        RoomId,
        events::{
            RoomAccountDataEventType,
            tag::{TagInfo, TagName},
        },
    },
};
use serde_json::{Map, Value};

use super::*;

/// Favourites reordered in one request at most.
pub const MAX_TAG_ORDER_WRITES: usize = 100;
const TAG_TIMEOUT: Duration = Duration::from_secs(60);
const FAVOURITE: &str = "m.favourite";
const LOW_PRIORITY: &str = "m.lowpriority";

/// A tag `order` in `0.0..=1.0`, compared numerically.
///
/// Stored as the bit pattern of the non-negative value, whose integer order
/// is the numeric order, so room models stay `Eq` without losing precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TagOrder(u64);

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

/// One explicit organization change for a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomTagChange {
    /// Add `m.favourite` with this order and remove `m.lowpriority`.
    Favourite(Option<TagOrder>),
    /// Add `m.lowpriority` and remove `m.favourite`.
    LowPriority,
    /// Remove `m.favourite`.
    RemoveFavourite,
    /// Remove `m.lowpriority`.
    RemoveLowPriority,
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
    #[error("Only part of the change was saved; the room list shows what the homeserver stored")]
    PartlySaved,
}

/// The organization of one room as the SDK last stored it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TagSummary {
    pub(crate) favourite: bool,
    pub(crate) low_priority: bool,
    /// Order of the favourite tag, else of the low-priority tag.
    pub(crate) order: Option<TagOrder>,
}

/// Cached tag flags; the stored tags are read only for tagged rooms.
pub(crate) async fn tag_summary(room: &Room) -> TagSummary {
    let favourite = room.is_favourite();
    let low_priority = room.is_low_priority();
    if !favourite && !low_priority {
        return TagSummary::default();
    }
    let tags = stored_tags(room).await.unwrap_or_default();
    let name = if favourite { FAVOURITE } else { LOW_PRIORITY };
    TagSummary {
        favourite,
        low_priority,
        order: tags.get(name).and_then(tag_order),
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
    let value = order
        .as_f64()
        .or_else(|| order.as_str().and_then(|text| text.trim().parse::<f64>().ok()))?;
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

fn tag_info(order: Option<TagOrder>) -> TagInfo {
    let mut info = TagInfo::new();
    info.order = order.map(TagOrder::value);
    info
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

    /// Run one bounded request under `selection`; revocation or a timeout
    /// ends the wait and its result is never returned.
    async fn under_tag_selection<T>(
        &self,
        selection: &AccountSelection,
        request: impl std::future::Future<Output = std::result::Result<T, RoomTagError>>,
    ) -> std::result::Result<T, RoomTagError> {
        let result = selection
            .guard(tokio::time::timeout(TAG_TIMEOUT, request))
            .await
            .ok_or(RoomTagError::AccountChanged)?
            .map_err(|_| RoomTagError::NotConfirmed)??;
        if !self.owns_tag_selection(selection) {
            return Err(RoomTagError::AccountChanged);
        }
        Ok(result)
    }

    /// Apply one explicit organization change to a joined room. Unrelated
    /// tags are untouched. Adding a tag the room already has, where the
    /// stored tag carries other fields, is refused before any request.
    pub async fn change_room_tags_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        change: RoomTagChange,
    ) -> std::result::Result<(), RoomTagError> {
        let client = self.tag_client(selection).await?;
        let room = Self::joined_room(&client, room_id)?;
        self.under_tag_selection(selection, async move {
            let tags = stored_tags(&room).await?;
            let (add, add_info, remove) = match change {
                RoomTagChange::Favourite(order) => {
                    (Some(TagName::Favorite), tag_info(order), Some(LOW_PRIORITY))
                }
                RoomTagChange::LowPriority => {
                    // Keep a stored low-priority order when re-adding.
                    let order = tags.get(LOW_PRIORITY).and_then(tag_order);
                    (Some(TagName::LowPriority), tag_info(order), Some(FAVOURITE))
                }
                RoomTagChange::RemoveFavourite => (None, TagInfo::new(), Some(FAVOURITE)),
                RoomTagChange::RemoveLowPriority => (None, TagInfo::new(), Some(LOW_PRIORITY)),
            };
            if let Some(name) = &add
                && carries_other_fields(&tags, name.as_ref())
            {
                return Err(RoomTagError::UnsupportedTagData);
            }
            let mut written = false;
            if let Some(name) = add {
                room.set_tag(name, add_info)
                    .await
                    .map_err(|_| RoomTagError::NotConfirmed)?;
                written = true;
            }
            if let Some(name) = remove
                && tags.contains_key(name)
            {
                room.remove_tag(TagName::from(name)).await.map_err(|_| {
                    if written {
                        RoomTagError::PartlySaved
                    } else {
                        RoomTagError::NotConfirmed
                    }
                })?;
            }
            Ok(())
        })
        .await
    }

    /// Write new favourite orders, at most `MAX_TAG_ORDER_WRITES` rooms.
    /// Every room is checked first (joined, a favourite, no other fields in
    /// its favourite tag); nothing is written if any check fails.
    pub async fn set_favourite_orders_selected(
        &self,
        selection: &AccountSelection,
        orders: Vec<(String, TagOrder)>,
    ) -> std::result::Result<(), RoomTagError> {
        if orders.len() > MAX_TAG_ORDER_WRITES {
            return Err(RoomTagError::TooMany);
        }
        let client = self.tag_client(selection).await?;
        let rooms = orders
            .into_iter()
            .map(|(room_id, order)| Ok((Self::joined_room(&client, &room_id)?, order)))
            .collect::<std::result::Result<Vec<_>, RoomTagError>>()?;
        self.under_tag_selection(selection, async move {
            for (room, _) in &rooms {
                let tags = stored_tags(room).await?;
                if !tags.contains_key(FAVOURITE) {
                    return Err(RoomTagError::NotFavourite);
                }
                if carries_other_fields(&tags, FAVOURITE) {
                    return Err(RoomTagError::UnsupportedTagData);
                }
            }
            for (written, (room, order)) in rooms.iter().enumerate() {
                room.set_tag(TagName::Favorite, tag_info(Some(*order)))
                    .await
                    .map_err(|_| {
                        if written > 0 {
                            RoomTagError::PartlySaved
                        } else {
                            RoomTagError::NotConfirmed
                        }
                    })?;
            }
            Ok(())
        })
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
    fn stored_orders_and_foreign_fields_are_read_faithfully() {
        let tags = serde_json::json!({
            "m.favourite": {"order": 0.25},
            "m.lowpriority": {"order": "0.75"},
            "u.work": {"order": 0.1, "colour": "blue"},
            "m.server_notice": {}
        });
        let tags = tags.as_object().unwrap().clone();
        assert_eq!(tags.get(FAVOURITE).and_then(tag_order), TagOrder::new(0.25));
        assert_eq!(tags.get(LOW_PRIORITY).and_then(tag_order), TagOrder::new(0.75));
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
            assert_eq!(
                manager
                    .change_room_tags_selected(&stale, room, RoomTagChange::LowPriority)
                    .await,
                Err(RoomTagError::AccountChanged)
            );
            assert_eq!(
                manager
                    .set_favourite_orders_selected(&stale, vec![(room.to_owned(), order)])
                    .await,
                Err(RoomTagError::AccountChanged)
            );
            let current = manager.account_selection(&account.id).unwrap();
            assert_eq!(
                manager
                    .change_room_tags_selected(&current, room, RoomTagChange::LowPriority)
                    .await,
                Err(RoomTagError::RoomUnavailable)
            );
            let too_many = (0..=MAX_TAG_ORDER_WRITES)
                .map(|index| (format!("!{index}:localhost"), order))
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
}
