//! Presentation ordering over the already account/Space-filtered room models.
//! This module never selects an account, changes membership, or opens a room.
//!
//! Rooms tagged `m.favourite` or `m.lowpriority` move out of the room and
//! direct-message lists into the Favorites and Low priority sections. Those
//! sections keep their tag order (rooms without an order follow by name) and
//! are not resorted by unread state; unread-first applies to the other lists
//! as before. Tags never widen what the current Home or Space view shows.

use slint::{Model, ModelRc, VecModel};
use std::collections::HashSet;

use crate::{AppWindow, NavigationRoomItem, RoomItem, room_organization::RoomTags};

struct RoomOrder {
    channels: Vec<RoomItem>,
    direct_messages: Vec<RoomItem>,
    combined: Vec<NavigationRoomItem>,
    favourites: Vec<NavigationRoomItem>,
    low_priority: Vec<NavigationRoomItem>,
}

fn room_order(
    channels: Vec<RoomItem>,
    direct_messages: Vec<RoomItem>,
    unread_first: bool,
    tags_of: impl Fn(&str) -> RoomTags,
) -> RoomOrder {
    let mut favourites = Vec::new();
    let mut low_priority = Vec::new();
    // Every row carries the tags its menu acts on; a room tagged both ways
    // is listed under Favorites.
    let mut keep = |rooms: Vec<RoomItem>, direct: bool| {
        let mut kept = Vec::with_capacity(rooms.len());
        for mut room in rooms {
            let tags = tags_of(room.id.as_str());
            room.favourite = tags.favourite;
            room.low_priority = tags.low_priority;
            if tags.favourite {
                favourites.push(NavigationRoomItem { room, direct });
            } else if tags.low_priority {
                low_priority.push(NavigationRoomItem { room, direct });
            } else {
                kept.push(room);
            }
        }
        kept
    };
    let mut channels = keep(channels, false);
    let mut direct_messages = keep(direct_messages, true);
    // Ordered rooms first by their order, then the rest by name; identity
    // breaks ties, never the display name alone.
    let tag_key = |entry: &NavigationRoomItem| {
        let order = tags_of(entry.room.id.as_str()).order;
        (
            order.is_none(),
            order,
            entry.room.name.to_lowercase(),
            entry.room.id.clone(),
        )
    };
    favourites.sort_by_cached_key(tag_key);
    low_priority.sort_by_cached_key(tag_key);
    let mut combined = channels
        .iter()
        .cloned()
        .map(|room| NavigationRoomItem {
            room,
            direct: false,
        })
        .chain(
            direct_messages
                .iter()
                .cloned()
                .map(|room| NavigationRoomItem { room, direct: true }),
        )
        .collect::<Vec<_>>();
    // Reconstruct the Matrix room snapshot's name/ID order across both types.
    // Identity and type never depend on the name, which may be duplicated.
    combined.sort_by_cached_key(|entry| (entry.room.name.to_lowercase(), entry.room.id.clone()));
    if unread_first {
        // Stable partitions preserve the established order within each group.
        channels.sort_by_key(|room| room.unread == 0);
        direct_messages.sort_by_key(|room| room.unread == 0);
        combined.sort_by_key(|entry| entry.room.unread == 0);
    }
    RoomOrder {
        channels,
        direct_messages,
        combined,
        favourites,
        low_priority,
    }
}

pub(crate) fn apply_room_order(ui: &AppWindow) {
    let order = room_order(
        ui.get_unsorted_channels().iter().collect(),
        ui.get_unsorted_direct_messages().iter().collect(),
        ui.get_sort_unread_first(),
        crate::room_organization::tags,
    );
    let same_room = |a: &RoomItem, b: &RoomItem| a.id == b.id;
    let same_entry = |a: &NavigationRoomItem, b: &NavigationRoomItem| a.room.id == b.room.id;
    sync_model(ui.get_channels(), order.channels, same_room, |model| ui.set_channels(model));
    sync_model(ui.get_direct_messages(), order.direct_messages, same_room, |model| {
        ui.set_direct_messages(model)
    });
    sync_model(ui.get_combined_rooms(), order.combined, same_entry, |model| {
        ui.set_combined_rooms(model)
    });
    sync_model(ui.get_favourite_rooms(), order.favourites, same_entry, |model| {
        ui.set_favourite_rooms(model)
    });
    sync_model(ui.get_low_priority_rooms(), order.low_priority, same_entry, |model| {
        ui.set_low_priority_rooms(model)
    });
    crate::room_organization::publish_active(ui);
}

/// Replace a room list's model only when its rooms or their order change.
/// The same rooms in the same order are updated row by row, so each row, its
/// keyboard focus and an open organization menu survive unread and tag
/// updates from sync. A row is never rewritten with a different room, so an
/// open menu cannot end up acting on another room.
fn sync_model<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    rows: Vec<T>,
    same_room: impl Fn(&T, &T) -> bool,
    replace: impl FnOnce(ModelRc<T>),
) {
    if let Some(model) = current.as_any().downcast_ref::<VecModel<T>>()
        && model.row_count() == rows.len()
        && model.iter().zip(&rows).all(|(old, new)| same_room(&old, new))
    {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
        return;
    }
    replace(ModelRc::new(VecModel::from(rows)));
}

/// Keyboard navigation follows the same displayed list order; duplicate IDs
/// are ignored for traversal without conflating equal display names.
pub(crate) fn visible_rooms(ui: &AppWindow) -> Vec<RoomItem> {
    // Favorites, then the room lists, then Low priority, as displayed. A
    // collapsed section keeps its place so its rooms stay reachable.
    let middle = if ui.get_group_direct_messages() {
        ui.get_channels()
            .iter()
            .chain(ui.get_direct_messages().iter())
            .collect::<Vec<_>>()
    } else {
        ui.get_combined_rooms()
            .iter()
            .map(|entry| entry.room)
            .collect::<Vec<_>>()
    };
    let rooms = ui
        .get_favourite_rooms()
        .iter()
        .map(|entry| entry.room)
        .chain(middle)
        .chain(ui.get_low_priority_rooms().iter().map(|entry| entry.room))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    rooms
        .into_iter()
        .filter(|room| seen.insert(room.id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, name: &str, unread: i32, voice: bool) -> RoomItem {
        RoomItem {
            id: id.into(),
            name: name.into(),
            unread,
            voice_space: voice,
            voice_local: voice,
            ..Default::default()
        }
    }

    #[test]
    fn merged_names_do_not_conflate_room_dm_or_voice_identity() {
        let channels = vec![room("!z", "same", 0, false), room("!c", "Zebra", 1, true)];
        let directs = vec![room("!a", "SAME", 0, true), room("!b", "Aster", 0, false)];
        let order = room_order(channels, directs, false, |_| RoomTags::default());
        let identities = order
            .combined
            .iter()
            .map(|item| {
                (
                    item.room.id.as_str(),
                    item.direct,
                    item.room.voice_space,
                    item.room.voice_local,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            [
                ("!b", true, false, false),
                ("!a", true, true, true),
                ("!z", false, false, false),
                ("!c", false, true, true),
            ]
        );
        assert_eq!(
            order
                .channels
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["!z", "!c"]
        );
        assert_eq!(
            order
                .direct_messages
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["!a", "!b"]
        );
    }

    #[test]
    fn unread_partition_spans_types_and_reverting_restores_filtered_order() {
        let channels = vec![room("!c", "Cedar", 0, false), room("!a", "Aster", 2, false)];
        let directs = vec![room("!d", "Delta", 3, false), room("!b", "Birch", 0, false)];
        let order = room_order(channels.clone(), directs.clone(), true, |_| RoomTags::default());
        assert_eq!(
            order
                .combined
                .iter()
                .map(|r| r.room.id.as_str())
                .collect::<Vec<_>>(),
            ["!a", "!d", "!b", "!c"]
        );
        assert_eq!(order.channels[0].id, "!a");
        assert_eq!(order.direct_messages[0].id, "!d");
        let restored = room_order(channels, directs, false, |_| RoomTags::default());
        assert_eq!(restored.channels[0].id, "!c");
        assert_eq!(restored.direct_messages[0].id, "!d");
        assert_eq!(
            restored
                .combined
                .iter()
                .map(|r| r.room.id.as_str())
                .collect::<Vec<_>>(),
            ["!a", "!b", "!c", "!d"]
        );
        assert!(
            room_order(Vec::new(), Vec::new(), true, |_| RoomTags::default())
                .combined
                .is_empty()
        );
    }

    #[test]
    fn tagged_rooms_form_ordered_sections_without_changing_their_identity() {
        use morpheus_matrix::TagOrder;

        let channels = vec![
            room("!f2", "Beta", 0, false),
            room("!n", "Normal", 4, false),
            room("!l", "Later", 9, false),
            room("!f3", "Alpha", 1, false),
        ];
        let directs = vec![room("!f1", "Zed", 0, false), room("!both", "Both", 0, false)];
        let tags = |id: &str| {
            let (favourite, low_priority, order) = match id {
                "!f1" => (true, false, TagOrder::new(0.1)),
                "!f2" => (true, false, TagOrder::new(0.2)),
                "!f3" => (true, false, None),
                // Another client may set both tags; Favorites wins.
                "!both" => (true, true, None),
                "!l" => (false, true, None),
                _ => (false, false, None),
            };
            RoomTags {
                favourite,
                low_priority,
                order,
            }
        };
        let order = room_order(channels, directs, true, tags);
        let ids = |entries: &[NavigationRoomItem]| {
            entries
                .iter()
                .map(|entry| (entry.room.id.to_string(), entry.direct))
                .collect::<Vec<_>>()
        };
        // Ordered first by order, then unordered by name; unread state does
        // not reorder a tagged section.
        assert_eq!(
            ids(&order.favourites),
            [
                ("!f1".to_owned(), true),
                ("!f2".to_owned(), false),
                ("!f3".to_owned(), false),
                ("!both".to_owned(), true),
            ]
        );
        // Rows carry the tags their menus act on.
        let both = &order.favourites[3].room;
        assert!(both.favourite && both.low_priority);
        assert_eq!(ids(&order.low_priority), [("!l".to_owned(), false)]);
        assert!(order.low_priority[0].room.low_priority);
        assert_eq!(
            order
                .channels
                .iter()
                .map(|room| (room.id.as_str(), room.favourite, room.low_priority))
                .collect::<Vec<_>>(),
            [("!n", false, false)]
        );
        assert!(order.direct_messages.is_empty());
        assert_eq!(order.combined.len(), 1);
    }

    #[test]
    fn unchanged_room_sequences_update_rows_in_place() {
        let first = ModelRc::new(VecModel::from(vec![
            room("!a", "A", 0, false),
            room("!b", "B", 0, false),
        ]));
        let mut replaced = None;
        // Same rooms, new unread count: the model is kept, the row updated.
        sync_model(
            first.clone(),
            vec![room("!a", "A", 3, false), room("!b", "B", 0, false)],
            |a, b| a.id == b.id,
            |model| replaced = Some(model),
        );
        assert!(replaced.is_none());
        assert_eq!(first.row_data(0).map(|row| row.unread), Some(3));
        // Another order, or another room set, replaces the model.
        sync_model(
            first.clone(),
            vec![room("!b", "B", 0, false), room("!a", "A", 3, false)],
            |a, b| a.id == b.id,
            |model| replaced = Some(model),
        );
        let replaced = replaced.expect("a reordered list is a new model");
        assert_eq!(replaced.row_data(0).map(|row| row.id), Some("!b".into()));
        assert_eq!(first.row_data(0).map(|row| row.id), Some("!a".into()));
    }
}
