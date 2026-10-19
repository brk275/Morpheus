//! Presentation ordering over the already account/Space-filtered room models.
//! This module never selects an account, changes membership, or opens a room.

use slint::{Model, ModelRc, VecModel};
use std::collections::HashSet;

use crate::{AppWindow, NavigationRoomItem, RoomItem};

struct RoomOrder {
    channels: Vec<RoomItem>,
    direct_messages: Vec<RoomItem>,
    combined: Vec<NavigationRoomItem>,
}

fn room_order(
    mut channels: Vec<RoomItem>,
    mut direct_messages: Vec<RoomItem>,
    unread_first: bool,
) -> RoomOrder {
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
    }
}

pub(crate) fn apply_room_order(ui: &AppWindow) {
    let order = room_order(
        ui.get_unsorted_channels().iter().collect(),
        ui.get_unsorted_direct_messages().iter().collect(),
        ui.get_sort_unread_first(),
    );
    ui.set_channels(ModelRc::new(VecModel::from(order.channels)));
    ui.set_direct_messages(ModelRc::new(VecModel::from(order.direct_messages)));
    ui.set_combined_rooms(ModelRc::new(VecModel::from(order.combined)));
}

/// Keyboard navigation follows the same displayed list order; duplicate IDs
/// are ignored for traversal without conflating equal display names.
pub(crate) fn visible_rooms(ui: &AppWindow) -> Vec<RoomItem> {
    let rooms = if ui.get_group_direct_messages() {
        let (channels, direct) = (ui.get_channels(), ui.get_direct_messages());
        // The grouped sections are shown in either order.
        let (first, second) = if ui.get_direct_messages_first() {
            (direct, channels)
        } else {
            (channels, direct)
        };
        first.iter().chain(second.iter()).collect::<Vec<_>>()
    } else {
        ui.get_combined_rooms()
            .iter()
            .map(|entry| entry.room)
            .collect::<Vec<_>>()
    };
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
        let order = room_order(channels, directs, false);
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
        let order = room_order(channels.clone(), directs.clone(), true);
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
        let restored = room_order(channels, directs, false);
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
        assert!(room_order(Vec::new(), Vec::new(), true).combined.is_empty());
    }
}
