//! Native room organization: the Favorites and Low priority sections built
//! from the standard Matrix `m.favourite` and `m.lowpriority` tags, the room
//! context menu and the room header menu, and accessible favorite reordering.
//!
//! The tags are the account's server state (`morpheus_matrix::room_tags`);
//! nothing here persists them locally or claims a change before the
//! homeserver accepted it. Sections only partition the rooms the current
//! Home or Space view already shows, so Space membership, direct-message
//! grouping and the user's sorting are unchanged; see `crate::navigation`.
//! A change is one explicit choice for one room, one change at a time, fenced
//! by the account selection it started under.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use morpheus_matrix::{RoomInfo, RoomTagChange, RoomTagError, TagOrder};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use tokio::runtime::Handle;

use crate::{AppWindow, NavigationRoomItem, RoomOrganizationState, live::LiveUi};

/// Menu actions as the Slint menus report them.
const ADD_FAVOURITE: i32 = 0;
const REMOVE_FAVOURITE: i32 = 1;
const LOW_PRIORITY: i32 = 2;
const NORMAL_PRIORITY: i32 = 3;
const MOVE_UP: i32 = 4;
const MOVE_DOWN: i32 = 5;

/// Neighbouring orders closer than this are renumbered instead of split.
const MIN_ORDER_GAP: f64 = 1e-9;

thread_local! {
    /// Tag orders of the visible account's rooms from its latest room
    /// snapshot. UI thread only; replaced by every snapshot.
    static ORDERS: RefCell<HashMap<String, TagOrder>> = RefCell::new(HashMap::new());
}

/// Remember a room snapshot's tag orders. Called where the snapshot is
/// applied, before the room lists are ordered.
pub(crate) fn remember(rooms: &[RoomInfo]) {
    ORDERS.with(|orders| {
        let mut orders = orders.borrow_mut();
        orders.clear();
        orders.extend(
            rooms
                .iter()
                .filter_map(|room| Some((room.room_id.clone(), room.tag_order?))),
        );
    });
}

/// Forget the departed account's sections and orders.
pub(crate) fn clear(ui: &AppWindow) {
    remember(&[]);
    ui.set_favourite_rooms(ModelRc::new(VecModel::<NavigationRoomItem>::default()));
    ui.set_low_priority_rooms(ModelRc::new(VecModel::<NavigationRoomItem>::default()));
    let global = ui.global::<RoomOrganizationState>();
    global.set_pending_room("".into());
    publish_active(ui);
}

/// The remembered tag order of a visible room.
pub(crate) fn order_of(room_id: &str) -> Option<TagOrder> {
    ORDERS.with(|orders| orders.borrow().get(room_id).copied())
}

/// What one menu choice asks the homeserver for.
#[derive(Debug, PartialEq)]
enum Request {
    Change(RoomTagChange),
    Orders(Vec<(String, TagOrder)>),
}

/// Plan one menu choice for `room_id`. `favourites` is the displayed
/// Favorites section, in order, with each room's stored order.
fn plan(action: i32, room_id: &str, favourites: &[(String, Option<TagOrder>)]) -> Option<Request> {
    match action {
        ADD_FAVOURITE => {
            let order = append_order(favourites);
            Some(Request::Change(RoomTagChange::Favourite(order)))
        }
        REMOVE_FAVOURITE => Some(Request::Change(RoomTagChange::RemoveFavourite)),
        LOW_PRIORITY => Some(Request::Change(RoomTagChange::LowPriority)),
        NORMAL_PRIORITY => Some(Request::Change(RoomTagChange::RemoveLowPriority)),
        MOVE_UP | MOVE_DOWN => {
            let index = favourites.iter().position(|(id, _)| id == room_id)?;
            let orders = favourites
                .iter()
                .map(|(_, order)| *order)
                .collect::<Vec<_>>();
            let writes = plan_move(&orders, index, action == MOVE_UP)?;
            Some(Request::Orders(
                writes
                    .into_iter()
                    .map(|(position, order)| (favourites[position].0.clone(), order))
                    .collect(),
            ))
        }
        _ => None,
    }
}

/// An order placing a new favourite after the ordered ones. Rooms without
/// an order sort after ordered rooms, so a new favourite next to them gets
/// none and joins them by name.
fn append_order(favourites: &[(String, Option<TagOrder>)]) -> Option<TagOrder> {
    match favourites.last() {
        None => TagOrder::new(0.5),
        Some((_, Some(last))) if 1.0 - last.value() > MIN_ORDER_GAP => {
            TagOrder::new(last.value() + (1.0 - last.value()) / 2.0)
        }
        Some(_) => None,
    }
}

/// Orders to write so the favourite at `index` moves one place up or down.
/// One write between its new neighbours when their orders leave room;
/// otherwise the whole list is renumbered evenly in its new order, writing
/// only rooms whose order changes. `None` when it cannot move that way.
fn plan_move(
    orders: &[Option<TagOrder>],
    index: usize,
    up: bool,
) -> Option<Vec<(usize, TagOrder)>> {
    let len = orders.len();
    let target = if up { index.checked_sub(1)? } else { index + 1 };
    if index >= len || target >= len {
        return None;
    }
    // Moving one place swaps the room with its neighbour.
    let mut sequence = (0..len).collect::<Vec<_>>();
    sequence.swap(index, target);
    let lower = match target.checked_sub(1) {
        None => Some(0.0),
        Some(below) => orders[sequence[below]].map(TagOrder::value),
    };
    let upper = match sequence.get(target + 1) {
        None => Some(1.0),
        Some(&above) => orders[above].map(TagOrder::value),
    };
    if let (Some(lower), Some(upper)) = (lower, upper)
        && upper - lower > MIN_ORDER_GAP
        && let Some(middle) = TagOrder::new(lower + (upper - lower) / 2.0)
        && lower < middle.value()
        && middle.value() < upper
    {
        return Some(vec![(index, middle)]);
    }
    let step = 1.0 / (len as f64 + 1.0);
    Some(
        sequence
            .iter()
            .enumerate()
            .filter_map(|(position, &original)| {
                let order = TagOrder::new(step * (position as f64 + 1.0))?;
                (orders[original] != Some(order)).then_some((original, order))
            })
            .collect(),
    )
}

/// Fixed wording per failure; nothing from the server is shown.
fn failure_text(error: RoomTagError) -> Option<&'static str> {
    match error {
        RoomTagError::AccountChanged => None,
        RoomTagError::RoomUnavailable => Some("This room is no longer joined."),
        RoomTagError::UnsupportedTagData => {
            Some("Another app stored extra data with this room's tag; Morpheus left it unchanged.")
        }
        RoomTagError::NotFavourite => Some("This room is no longer a favorite."),
        RoomTagError::TooMany => Some("There are too many favorites to reorder at once."),
        RoomTagError::NotConfirmed => {
            Some("The homeserver did not confirm the change; the room keeps its place.")
        }
        RoomTagError::PartlySaved => Some(
            "Only part of the change was saved; the room list shows what the homeserver stored.",
        ),
    }
}

pub(crate) struct Organizer {
    live: Arc<LiveUi>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    /// The latest change; an older one's result only clears its own state.
    operation: AtomicU64,
    /// Account of the change in flight. Another account's change does not
    /// block this one, and cannot touch its rooms.
    pending_account: Mutex<String>,
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) -> Arc<Organizer> {
    let organizer = Arc::new(Organizer {
        live,
        ui: ui.as_weak(),
        runtime,
        operation: AtomicU64::new(0),
        pending_account: Mutex::new(String::new()),
    });
    let global = ui.global::<RoomOrganizationState>();
    let handler = organizer.clone();
    global.on_organize(move |room_id, action| handler.organize(&room_id, action));
    let weak = ui.as_weak();
    global.on_active_room_changed(move || {
        if let Some(ui) = weak.upgrade() {
            publish_active(&ui);
        }
    });
    organizer
}

impl Organizer {
    fn pending_account(&self) -> std::sync::MutexGuard<'_, String> {
        self.pending_account
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// One explicit menu choice for a room in the visible account.
    fn organize(self: &Arc<Self>, room_id: &str, action: i32) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<RoomOrganizationState>();
        let account_id = ui.get_active_account_local_id().to_string();
        // One change at a time per account; its menus are disabled meanwhile.
        if !ui.get_live_session()
            || (!global.get_pending_room().is_empty() && *self.pending_account() == account_id)
        {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account_id) else {
            return;
        };
        let favourites = ui
            .get_favourite_rooms()
            .iter()
            .map(|entry| {
                let id = entry.room.id.to_string();
                let order = order_of(&id);
                (id, order)
            })
            .collect::<Vec<_>>();
        let Some(request) = plan(action, room_id, &favourites) else {
            return;
        };
        if matches!(&request, Request::Orders(orders) if orders.is_empty()) {
            return;
        }
        let operation = self.operation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.pending_account() = account_id;
        global.set_pending_room(room_id.into());
        let organizer = self.clone();
        let room_id = room_id.to_owned();
        self.runtime.spawn(async move {
            let manager = organizer.live.manager.clone();
            let result = match request {
                Request::Change(change) => {
                    manager
                        .change_room_tags_selected(&selection, &room_id, change)
                        .await
                }
                Request::Orders(orders) => {
                    manager
                        .set_favourite_orders_selected(&selection, orders)
                        .await
                }
            };
            let weak = organizer.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if organizer.operation.load(Ordering::Acquire) != operation {
                    return;
                }
                ui.global::<RoomOrganizationState>()
                    .set_pending_room("".into());
                // The room list follows the stored tags once sync returns
                // them; only a failure for the still-visible account is told.
                if let Err(error) = result
                    && selection.is_current()
                    && ui.get_active_account_local_id().as_str() == selection.account_id()
                    && let Some(text) = failure_text(error)
                {
                    ui.set_session_error(text.into());
                }
            });
        });
    }
}

/// Publish the open room's organization for the room header menu. Called
/// after the sections are rebuilt and when the open room changes.
pub(crate) fn publish_active(ui: &AppWindow) {
    let active = ui.get_active_room_id();
    let favourites = ui.get_favourite_rooms();
    let index = favourites.iter().position(|entry| entry.room.id == active);
    let low_priority = ui
        .get_low_priority_rooms()
        .iter()
        .any(|entry| entry.room.id == active);
    let global = ui.global::<RoomOrganizationState>();
    global.set_active_favourite(index.is_some());
    global.set_active_low_priority(low_priority);
    global.set_active_can_move_up(index.is_some_and(|index| index > 0));
    global.set_active_can_move_down(
        index.is_some_and(|index| index + 1 < favourites.row_count()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(value: f64) -> Option<TagOrder> {
        TagOrder::new(value)
    }

    fn favourites(orders: &[Option<TagOrder>]) -> Vec<(String, Option<TagOrder>)> {
        orders
            .iter()
            .enumerate()
            .map(|(index, order)| (format!("!{index}:example.org"), *order))
            .collect()
    }

    /// The single write of a move, as (room position, order value).
    fn single(writes: Option<Vec<(usize, TagOrder)>>) -> (usize, f64) {
        let writes = writes.expect("the room can move");
        assert_eq!(writes.len(), 1, "{writes:?}");
        (writes[0].0, writes[0].1.value())
    }

    #[test]
    fn a_move_writes_one_order_between_its_new_neighbours() {
        let orders = [order(0.2), order(0.4), order(0.6)];
        let close = |actual: (usize, f64), expected: (usize, f64)| {
            assert_eq!(actual.0, expected.0);
            assert!((actual.1 - expected.1).abs() < 1e-12, "{actual:?}");
        };
        // Up to the top: between the start of the range and 0.2.
        close(single(plan_move(&orders, 1, true)), (1, 0.1));
        // Down to the end: between 0.6 and the end of the range.
        close(single(plan_move(&orders, 1, false)), (1, 0.8));
        // Down from the top: between 0.4 and 0.6.
        close(single(plan_move(&orders, 0, false)), (0, 0.5));
        // Edges cannot move further.
        assert_eq!(plan_move(&orders, 0, true), None);
        assert_eq!(plan_move(&orders, 2, false), None);
        assert_eq!(plan_move(&orders, 3, false), None);
    }

    #[test]
    fn missing_or_crowded_orders_renumber_only_what_changes() {
        // Unordered rooms: every room gets an even order in the new sequence.
        let unordered = [None, None, None];
        assert_eq!(
            plan_move(&unordered, 2, true),
            Some(vec![
                (0, order(0.25).unwrap()),
                (2, order(0.5).unwrap()),
                (1, order(0.75).unwrap()),
            ])
        );
        // Equal neighbours leave no gap; a room already at its even order is
        // not rewritten.
        let crowded = [order(0.25), order(0.25), order(0.75)];
        assert_eq!(
            plan_move(&crowded, 2, true),
            Some(vec![(2, order(0.5).unwrap()), (1, order(0.75).unwrap())])
        );
    }

    #[test]
    fn menu_choices_plan_one_explicit_change() {
        let list = favourites(&[order(0.25), order(0.75)]);
        assert_eq!(
            plan(ADD_FAVOURITE, "!new:example.org", &list),
            Some(Request::Change(RoomTagChange::Favourite(order(0.875))))
        );
        assert_eq!(
            plan(ADD_FAVOURITE, "!new:example.org", &[]),
            Some(Request::Change(RoomTagChange::Favourite(order(0.5))))
        );
        // Next to unordered favourites a new one has no order either.
        let unordered = favourites(&[order(0.25), None]);
        assert_eq!(
            plan(ADD_FAVOURITE, "!new:example.org", &unordered),
            Some(Request::Change(RoomTagChange::Favourite(None)))
        );
        assert_eq!(
            plan(LOW_PRIORITY, "!0:example.org", &list),
            Some(Request::Change(RoomTagChange::LowPriority))
        );
        assert_eq!(
            plan(MOVE_DOWN, "!0:example.org", &list),
            Some(Request::Orders(vec![(
                "!0:example.org".to_owned(),
                order(0.875).unwrap()
            )]))
        );
        // Only listed favourites move; unknown actions do nothing.
        assert_eq!(plan(MOVE_UP, "!other:example.org", &list), None);
        assert_eq!(plan(9, "!0:example.org", &list), None);
    }
}
