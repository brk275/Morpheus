//! Native room organization: the Favorites and Low priority sections built
//! from the standard Matrix `m.favourite` and `m.lowpriority` tags, the room
//! context menu and the room header menu, and accessible favorite reordering.
//!
//! The tags are the account's server state (`morpheus_matrix::room_tags`).
//! This module keeps, on the UI thread only and never on disk:
//! - every joined room's tags from the visible account's latest snapshot, so
//!   decisions and favorite order use the whole account, not just the rooms
//!   the current Home or Space view shows;
//! - writes the homeserver confirmed but sync has not returned yet, applied
//!   over the snapshot for a short time so the list and a following move see
//!   them; a snapshot that already shows them, an account change or the time
//!   limit ends them;
//! - at most one change in flight per account.
//!
//! Sections only partition the rooms the current view already shows; see
//! `crate::navigation`. A change is one explicit choice for one room, fenced
//! by the account selection it started under; nothing is shown as saved
//! before the homeserver accepted it.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use morpheus_matrix::{
    ConfirmedTags, FavouriteOrder, RoomInfo, RoomMembership, RoomTagChange, RoomTagError, TagOrder,
};
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
/// A confirmed write is shown at most this long before sync returns it.
const CONFIRMED_LIFETIME: Duration = Duration::from_secs(30);

/// One room's organization tags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoomTags {
    pub(crate) favourite: bool,
    pub(crate) low_priority: bool,
    /// Order of the favourite tag, else of the low-priority tag.
    pub(crate) order: Option<TagOrder>,
}

struct Confirmed {
    account: String,
    tags: RoomTags,
    /// False when the write left an order this module cannot know (the
    /// low-priority order of a room that was tagged both ways).
    order_known: bool,
    until: Instant,
}

impl Confirmed {
    /// Whether a snapshot's tags show this write.
    fn shown_by(&self, stored: RoomTags) -> bool {
        if self.order_known {
            stored == self.tags
        } else {
            stored.favourite == self.tags.favourite
                && stored.low_priority == self.tags.low_priority
        }
    }
}

struct Pending {
    account: String,
    room: String,
    operation: u64,
}

#[derive(Default)]
struct State {
    /// Account of the latest snapshot.
    account: String,
    /// Tags and case-folded names of that account's joined rooms (no Spaces).
    stored: HashMap<String, (RoomTags, String)>,
    confirmed: HashMap<String, Confirmed>,
    pending: Option<Pending>,
    next_operation: u64,
}

impl State {
    fn stored_tags(&self, room_id: &str) -> RoomTags {
        self.stored
            .get(room_id)
            .map(|(tags, _)| *tags)
            .unwrap_or_default()
    }

    /// The confirmed write still shown for a room of the snapshot account.
    fn confirmed(&self, room_id: &str) -> Option<&Confirmed> {
        self.confirmed.get(room_id).filter(|confirmed| {
            confirmed.account == self.account && Instant::now() < confirmed.until
        })
    }

    fn tags(&self, room_id: &str) -> RoomTags {
        match self.confirmed(room_id) {
            Some(confirmed) if confirmed.order_known => confirmed.tags,
            Some(confirmed) => RoomTags {
                order: None,
                ..confirmed.tags
            },
            None => self.stored_tags(room_id),
        }
    }

    /// What the homeserver holds for a room beyond the snapshot, for the
    /// core's add/remove decisions.
    fn confirmed_tags(&self, room_id: &str) -> ConfirmedTags {
        self.confirmed(room_id)
            .map(|confirmed| ConfirmedTags {
                favourite: Some(confirmed.tags.favourite),
                low_priority: Some(confirmed.tags.low_priority),
            })
            .unwrap_or_default()
    }

    /// Every favourite of the account in tag order: ordered rooms first by
    /// order, the rest by name, room ID breaking ties.
    fn favourites(&self) -> Vec<(String, Option<TagOrder>)> {
        let mut favourites = self
            .stored
            .iter()
            .filter_map(|(room_id, (_, name))| {
                let tags = self.tags(room_id);
                tags.favourite
                    .then(|| (room_id.clone(), name.clone(), tags.order))
            })
            .collect::<Vec<_>>();
        favourites.sort_by(|a, b| {
            (a.2.is_none(), a.2, &a.1, &a.0).cmp(&(b.2.is_none(), b.2, &b.1, &b.0))
        });
        favourites
            .into_iter()
            .map(|(room_id, _, order)| (room_id, order))
            .collect()
    }

    fn pending_here(&self) -> Option<&Pending> {
        self.pending
            .as_ref()
            .filter(|pending| pending.account == self.account)
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with_state<T>(work: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|state| work(&mut state.borrow_mut()))
}

/// Remember the visible account's room snapshot. Called where the snapshot
/// is applied, before the room lists are ordered.
pub(crate) fn remember(account_id: &str, rooms: &[RoomInfo]) {
    let now = Instant::now();
    with_state(|state| {
        state.account = account_id.to_owned();
        state.stored = rooms
            .iter()
            .filter(|room| room.membership == RoomMembership::Joined && !room.is_space)
            .map(|room| {
                let tags = RoomTags {
                    favourite: room.favourite,
                    low_priority: room.low_priority,
                    order: room.tag_order,
                };
                (room.room_id.clone(), (tags, room.name.to_lowercase()))
            })
            .collect();
        let stored = &state.stored;
        state.confirmed.retain(|room_id, confirmed| {
            confirmed.account == account_id
                && now < confirmed.until
                && !stored
                    .get(room_id)
                    .is_some_and(|(tags, _)| confirmed.shown_by(*tags))
        });
    });
}

/// The tags of a visible room: a confirmed write, else the snapshot.
pub(crate) fn tags(room_id: &str) -> RoomTags {
    with_state(|state| state.tags(room_id))
}

/// Forget the departed account's sections and state.
pub(crate) fn clear(ui: &AppWindow) {
    with_state(|state| {
        let next_operation = state.next_operation;
        *state = State {
            next_operation,
            ..State::default()
        };
    });
    ui.set_favourite_rooms(ModelRc::new(VecModel::<NavigationRoomItem>::default()));
    ui.set_low_priority_rooms(ModelRc::new(VecModel::<NavigationRoomItem>::default()));
    publish_active(ui);
}

/// What one menu choice asks the homeserver for, and for a change the tags
/// the room has once it is accepted (and whether their order is known).
#[derive(Debug, PartialEq)]
enum Request {
    Change(RoomTagChange, RoomTags, bool),
    Orders(Vec<(String, TagOrder)>),
}

impl Request {
    /// Rooms the request writes.
    fn rooms<'a>(&'a self, room_id: &'a str) -> Vec<&'a str> {
        match self {
            Self::Change(..) => vec![room_id],
            Self::Orders(orders) => orders.iter().map(|(id, _)| id.as_str()).collect(),
        }
    }
}

/// Plan one menu choice for `room_id`, whose tags are `current`.
/// `favourites` is the account's whole Favorites order and `visible` the
/// displayed Favorites section. A choice that would change nothing, or a
/// move without a visible neighbour, plans nothing.
fn plan(
    action: i32,
    room_id: &str,
    current: RoomTags,
    favourites: &[(String, Option<TagOrder>)],
    visible: &[String],
) -> Option<Request> {
    match action {
        ADD_FAVOURITE if !current.favourite || current.low_priority => {
            // An existing favourite tag is kept as stored, order included.
            let order = if current.favourite {
                current.order
            } else {
                append_order(favourites)
            };
            let expected = RoomTags {
                favourite: true,
                low_priority: false,
                order,
            };
            let change = RoomTagChange::Favourite((!current.favourite).then_some(order).flatten());
            Some(Request::Change(change, expected, true))
        }
        REMOVE_FAVOURITE if current.favourite => {
            let expected = RoomTags {
                favourite: false,
                low_priority: current.low_priority,
                order: None,
            };
            // The low-priority order of a room tagged both ways is not known
            // here; sync supplies it.
            let order_known = !current.low_priority;
            Some(Request::Change(RoomTagChange::RemoveFavourite, expected, order_known))
        }
        LOW_PRIORITY if !current.low_priority || current.favourite => {
            let expected = RoomTags {
                favourite: false,
                low_priority: true,
                order: None,
            };
            Some(Request::Change(RoomTagChange::LowPriority, expected, true))
        }
        NORMAL_PRIORITY if current.low_priority => {
            let expected = RoomTags {
                favourite: current.favourite,
                low_priority: false,
                order: current.favourite.then_some(current.order).flatten(),
            };
            Some(Request::Change(RoomTagChange::RemoveLowPriority, expected, true))
        }
        MOVE_UP | MOVE_DOWN if current.favourite => {
            let up = action == MOVE_UP;
            let index = visible.iter().position(|id| id == room_id)?;
            let neighbour = if up {
                visible.get(index.checked_sub(1)?)?
            } else {
                visible.get(index + 1)?
            };
            let writes = plan_reorder(favourites, room_id, neighbour, up)?;
            (!writes.is_empty()).then_some(Request::Orders(writes))
        }
        _ => None,
    }
}

/// An order placing a new favourite after the account's ordered favourites.
/// Rooms without an order sort after ordered rooms, so next to them a new
/// favourite gets none and joins them by name.
fn append_order(favourites: &[(String, Option<TagOrder>)]) -> Option<TagOrder> {
    match favourites.last() {
        None => TagOrder::new(0.5),
        Some((_, Some(last))) if 1.0 - last.value() > MIN_ORDER_GAP => {
            TagOrder::new(last.value() + (1.0 - last.value()) / 2.0)
        }
        Some(_) => None,
    }
}

/// Orders to write so `moving` sits just before (`up`) or after `neighbour`
/// in the account's whole Favorites order, where favourites hidden by the
/// current view keep their place. One write between its new neighbours when
/// their orders leave room; otherwise the whole order is renumbered evenly,
/// writing only rooms whose order changes.
fn plan_reorder(
    favourites: &[(String, Option<TagOrder>)],
    moving: &str,
    neighbour: &str,
    up: bool,
) -> Option<Vec<(String, TagOrder)>> {
    if moving == neighbour {
        return None;
    }
    let from = favourites.iter().position(|(id, _)| id == moving)?;
    let mut sequence = (0..favourites.len())
        .filter(|&position| position != from)
        .collect::<Vec<_>>();
    let anchor = sequence
        .iter()
        .position(|&position| favourites[position].0 == neighbour)?;
    let at = if up { anchor } else { anchor + 1 };
    sequence.insert(at, from);
    let order_at = |position: usize| favourites[sequence[position]].1.map(TagOrder::value);
    let lower = match at.checked_sub(1) {
        None => Some(0.0),
        Some(below) => order_at(below),
    };
    let upper = if at + 1 < sequence.len() {
        order_at(at + 1)
    } else {
        Some(1.0)
    };
    if let (Some(lower), Some(upper)) = (lower, upper)
        && upper - lower > MIN_ORDER_GAP
        && let Some(middle) = TagOrder::new(lower + (upper - lower) / 2.0)
        && lower < middle.value()
        && middle.value() < upper
    {
        return Some(vec![(moving.to_owned(), middle)]);
    }
    let step = 1.0 / (sequence.len() as f64 + 1.0);
    Some(
        sequence
            .iter()
            .enumerate()
            .filter_map(|(rank, &position)| {
                let order = TagOrder::new(step * (rank as f64 + 1.0))?;
                let (room_id, current) = &favourites[position];
                (*current != Some(order)).then(|| (room_id.clone(), order))
            })
            .collect(),
    )
}

/// The tags a confirmed request leaves on each written room, and whether
/// their order is known.
fn written_tags(
    request: &Request,
    room_id: &str,
    before: impl Fn(&str) -> RoomTags,
) -> Vec<(String, RoomTags, bool)> {
    match request {
        Request::Change(_, expected, order_known) => {
            vec![(room_id.to_owned(), *expected, *order_known)]
        }
        Request::Orders(orders) => orders
            .iter()
            .map(|(room_id, order)| {
                let current = before(room_id);
                let tags = RoomTags {
                    favourite: true,
                    low_priority: current.low_priority,
                    order: Some(*order),
                };
                (room_id.clone(), tags, true)
            })
            .collect(),
    }
}

/// The core call for a planned request, with what this window knows from
/// confirmed writes sync has not stored yet.
enum Call {
    Change(RoomTagChange, ConfirmedTags),
    Orders(Vec<FavouriteOrder>),
}

fn call_for(request: &Request, room_id: &str, state: &State) -> Call {
    match request {
        Request::Change(change, _, _) => Call::Change(*change, state.confirmed_tags(room_id)),
        Request::Orders(orders) => Call::Orders(
            orders
                .iter()
                .map(|(room_id, order)| FavouriteOrder {
                    room_id: room_id.clone(),
                    order: *order,
                    confirmed_favourite: state
                        .confirmed(room_id)
                        .is_some_and(|confirmed| confirmed.tags.favourite),
                })
                .collect(),
        ),
    }
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
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) -> Arc<Organizer> {
    let organizer = Arc::new(Organizer {
        live,
        ui: ui.as_weak(),
        runtime,
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
    /// One explicit menu choice for a room of the visible account. A menu
    /// whose row was rebuilt meanwhile reports an empty room, which, like any
    /// room this account has not joined, does nothing.
    fn organize(self: &Arc<Self>, room_id: &str, action: i32) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let account_id = ui.get_active_account_local_id().to_string();
        if !ui.get_live_session() || account_id.is_empty() || room_id.is_empty() {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account_id) else {
            return;
        };
        let visible = ui
            .get_favourite_rooms()
            .iter()
            .map(|entry| entry.room.id.to_string())
            .collect::<Vec<_>>();
        let started = with_state(|state| {
            if state.account != account_id
                || !state.stored.contains_key(room_id)
                || state.pending_here().is_some()
            {
                return None;
            }
            let request = plan(
                action,
                room_id,
                state.tags(room_id),
                &state.favourites(),
                &visible,
            )?;
            let call = call_for(&request, room_id, state);
            state.next_operation = state.next_operation.wrapping_add(1);
            let operation = state.next_operation;
            state.pending = Some(Pending {
                account: account_id.clone(),
                room: room_id.to_owned(),
                operation,
            });
            Some((request, call, operation))
        });
        let Some((request, call, operation)) = started else {
            return;
        };
        publish_active(&ui);
        let organizer = self.clone();
        let room_id = room_id.to_owned();
        self.runtime.spawn(async move {
            let manager = organizer.live.manager.clone();
            let result = match call {
                Call::Change(change, confirmed) => {
                    manager
                        .change_room_tags_selected(&selection, &room_id, change, confirmed)
                        .await
                }
                Call::Orders(orders) => {
                    manager
                        .set_favourite_orders_selected(&selection, orders)
                        .await
                }
            };
            let weak = organizer.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                finish(&ui, operation, &selection, &room_id, &request, result);
            });
        });
    }
}

/// Apply one change's result on the UI thread.
fn finish(
    ui: &AppWindow,
    operation: u64,
    selection: &morpheus_matrix::AccountSelection,
    room_id: &str,
    request: &Request,
    result: Result<(), RoomTagError>,
) {
    let account = selection.account_id();
    let visible = with_state(|state| {
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.operation == operation)
        {
            state.pending = None;
        }
        let visible = selection.is_current() && state.account == account;
        if visible && result.is_ok() {
            // Accepted by the homeserver: shown until sync returns it.
            let until = Instant::now() + CONFIRMED_LIFETIME;
            let written = written_tags(request, room_id, |id| state.tags(id));
            for (room, tags, order_known) in written {
                state.confirmed.insert(
                    room,
                    Confirmed {
                        account: account.to_owned(),
                        tags,
                        order_known,
                        until,
                    },
                );
            }
        } else if visible {
            // What the homeserver now holds for these rooms is uncertain:
            // show the stored tags, not an earlier accepted write.
            for room in request.rooms(room_id) {
                state.confirmed.remove(room);
            }
        }
        visible
    });
    // Rebuild the sections with what the homeserver accepted, and clear the
    // pending state either way.
    crate::navigation::apply_room_order(ui);
    if let Err(error) = result
        && visible
        && ui.get_active_account_local_id().as_str() == account
        && let Some(text) = failure_text(error)
    {
        ui.set_session_error(text.into());
    }
}

/// Publish the open room's organization and the pending room for the menus.
/// Called after the sections are rebuilt and when the open room changes.
pub(crate) fn publish_active(ui: &AppWindow) {
    let active = ui.get_active_room_id();
    let (tags, pending) = with_state(|state| {
        let pending = state
            .pending_here()
            .map(|pending| pending.room.clone())
            .unwrap_or_default();
        (state.tags(active.as_str()), pending)
    });
    let favourites = ui.get_favourite_rooms();
    let index = favourites.iter().position(|entry| entry.room.id == active);
    let global = ui.global::<RoomOrganizationState>();
    global.set_pending_room(pending.into());
    global.set_active_favourite(tags.favourite);
    global.set_active_low_priority(tags.low_priority);
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

    fn list(orders: &[(&str, Option<TagOrder>)]) -> Vec<(String, Option<TagOrder>)> {
        orders
            .iter()
            .map(|(id, order)| ((*id).to_owned(), *order))
            .collect()
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn favourite(order: Option<TagOrder>) -> RoomTags {
        RoomTags {
            favourite: true,
            low_priority: false,
            order,
        }
    }

    /// The single write of a move, as (room, order value).
    fn single(writes: Option<Vec<(String, TagOrder)>>) -> (String, f64) {
        let writes = writes.expect("the room can move");
        assert_eq!(writes.len(), 1, "{writes:?}");
        (writes[0].0.clone(), writes[0].1.value())
    }

    fn close(actual: (String, f64), room: &str, value: f64) {
        assert_eq!(actual.0, room);
        assert!((actual.1 - value).abs() < 1e-12, "{actual:?}");
    }

    #[test]
    fn a_move_writes_one_order_between_its_new_neighbours() {
        let all = list(&[("a", order(0.2)), ("b", order(0.4)), ("c", order(0.6))]);
        close(single(plan_reorder(&all, "b", "a", true)), "b", 0.1);
        close(single(plan_reorder(&all, "b", "c", false)), "b", 0.8);
        close(single(plan_reorder(&all, "a", "b", false)), "a", 0.5);
        assert_eq!(plan_reorder(&all, "a", "a", true), None);
        assert_eq!(plan_reorder(&all, "x", "a", true), None);
    }

    #[test]
    fn hidden_favourites_keep_their_place_when_a_visible_one_moves() {
        // The current Space shows a and c; b is a favourite hidden here.
        let all = list(&[("a", order(0.2)), ("b", order(0.4)), ("c", order(0.6))]);
        let visible = ids(&["a", "c"]);
        let request = plan(MOVE_UP, "c", favourite(order(0.6)), &all, &visible).unwrap();
        // c lands just before a, below the start; b stays between a and c's
        // old place.
        let Request::Orders(writes) = request else {
            panic!("a move writes orders");
        };
        close(single(Some(writes)), "c", 0.1);
    }

    #[test]
    fn missing_or_crowded_orders_renumber_only_what_changes() {
        let unordered = list(&[("a", None), ("b", None), ("c", None)]);
        assert_eq!(
            plan_reorder(&unordered, "c", "b", true),
            Some(vec![
                ("a".to_owned(), order(0.25).unwrap()),
                ("c".to_owned(), order(0.5).unwrap()),
                ("b".to_owned(), order(0.75).unwrap()),
            ])
        );
        let crowded = list(&[("a", order(0.25)), ("b", order(0.25)), ("c", order(0.75))]);
        assert_eq!(
            plan_reorder(&crowded, "c", "b", true),
            Some(vec![
                ("c".to_owned(), order(0.5).unwrap()),
                ("b".to_owned(), order(0.75).unwrap()),
            ])
        );
    }

    #[test]
    fn menu_choices_plan_one_change_and_nothing_redundant() {
        let all = list(&[("a", order(0.25)), ("b", order(0.75))]);
        let visible = ids(&["a", "b"]);
        let normal = RoomTags::default();
        assert_eq!(
            plan(ADD_FAVOURITE, "new", normal, &all, &visible),
            Some(Request::Change(
                RoomTagChange::Favourite(order(0.875)),
                favourite(order(0.875)),
                true
            ))
        );
        assert_eq!(
            plan(ADD_FAVOURITE, "new", normal, &[], &[]),
            Some(Request::Change(
                RoomTagChange::Favourite(order(0.5)),
                favourite(order(0.5)),
                true
            ))
        );
        // Next to unordered favourites a new one has no order either.
        let unordered = list(&[("a", order(0.25)), ("b", None)]);
        assert_eq!(
            plan(ADD_FAVOURITE, "new", normal, &unordered, &visible),
            Some(Request::Change(
                RoomTagChange::Favourite(None),
                favourite(None),
                true
            ))
        );
        // An existing favourite keeps its tag: adding again only drops the
        // other tag of a room tagged both ways, and never rewrites the order.
        let both = RoomTags {
            favourite: true,
            low_priority: true,
            order: order(0.25),
        };
        assert_eq!(
            plan(ADD_FAVOURITE, "a", both, &all, &visible),
            Some(Request::Change(
                RoomTagChange::Favourite(None),
                favourite(order(0.25)),
                true
            ))
        );
        assert_eq!(plan(ADD_FAVOURITE, "a", favourite(order(0.25)), &all, &visible), None);
        assert_eq!(plan(REMOVE_FAVOURITE, "new", normal, &all, &visible), None);
        assert_eq!(plan(NORMAL_PRIORITY, "a", favourite(order(0.25)), &all, &visible), None);
        assert_eq!(
            plan(MOVE_DOWN, "a", favourite(order(0.25)), &all, &visible),
            Some(Request::Orders(vec![("a".to_owned(), order(0.875).unwrap())]))
        );
        // A non-favourite, an edge or an unknown action moves nothing.
        assert_eq!(plan(MOVE_UP, "new", normal, &all, &visible), None);
        assert_eq!(plan(MOVE_UP, "a", favourite(order(0.25)), &all, &visible), None);
        assert_eq!(plan(9, "a", favourite(order(0.25)), &all, &visible), None);
    }

    #[test]
    fn confirmed_writes_show_until_sync_returns_them_for_that_account_only() {
        let room = |id: &str, favourite: bool, order: Option<TagOrder>| RoomInfo {
            room_id: id.into(),
            name: id.into(),
            topic: None,
            membership: RoomMembership::Joined,
            is_direct: false,
            is_space: false,
            avatar_mxc: None,
            favourite,
            low_priority: false,
            tag_order: order,
            unread_messages: 0,
            unread_notifications: 0,
        };
        remember("alice", &[room("!a", false, None)]);
        with_state(|state| {
            state.confirmed.insert(
                "!a".into(),
                Confirmed {
                    account: "alice".into(),
                    tags: favourite(order(0.5)),
                    order_known: true,
                    until: Instant::now() + CONFIRMED_LIFETIME,
                },
            )
        });
        // A snapshot that has not caught up keeps showing the accepted write.
        remember("alice", &[room("!a", false, None)]);
        assert_eq!(tags("!a"), favourite(order(0.5)));
        // One that shows it ends the overlay.
        remember("alice", &[room("!a", true, order(0.5))]);
        assert!(with_state(|state| state.confirmed.is_empty()));
        // Another account never sees it.
        with_state(|state| {
            state.confirmed.insert(
                "!a".into(),
                Confirmed {
                    account: "alice".into(),
                    tags: favourite(order(0.25)),
                    order_known: true,
                    until: Instant::now() + CONFIRMED_LIFETIME,
                },
            )
        });
        remember("bob", &[room("!a", false, None)]);
        assert_eq!(tags("!a"), RoomTags::default());
        assert!(with_state(|state| state.confirmed.is_empty()));
    }

    #[test]
    fn follow_up_choices_use_confirmed_writes_and_unknown_orders_match_by_tags() {
        let room = |id: &str, favourite: bool, low_priority: bool, order| RoomInfo {
            room_id: id.into(),
            name: id.into(),
            topic: None,
            membership: RoomMembership::Joined,
            is_direct: false,
            is_space: false,
            avatar_mxc: None,
            favourite,
            low_priority,
            tag_order: order,
            unread_messages: 0,
            unread_notifications: 0,
        };
        remember("alice", &[room("!a", false, false, None)]);
        // A favourite added here but not synced yet: a following move tells
        // the core it is a favourite, and a following change tells it which
        // tags the homeserver holds.
        with_state(|state| {
            state.confirmed.insert(
                "!a".into(),
                Confirmed {
                    account: "alice".into(),
                    tags: favourite(order(0.5)),
                    order_known: true,
                    until: Instant::now() + CONFIRMED_LIFETIME,
                },
            );
            let Call::Orders(writes) = call_for(
                &Request::Orders(vec![("!a".to_owned(), order(0.25).unwrap())]),
                "!a",
                state,
            ) else {
                panic!("a move is an order write");
            };
            assert!(writes[0].confirmed_favourite);
            let Call::Change(_, confirmed) = call_for(
                &Request::Change(RoomTagChange::LowPriority, RoomTags::default(), true),
                "!a",
                state,
            ) else {
                panic!("a change is a tag change");
            };
            assert_eq!(confirmed.favourite, Some(true));
            assert_eq!(confirmed.low_priority, Some(false));
            // An expired write is neither shown nor reported.
            state.confirmed.get_mut("!a").unwrap().until = Instant::now();
            assert_eq!(state.tags("!a"), RoomTags::default());
            assert_eq!(state.confirmed_tags("!a"), ConfirmedTags::default());
        });
        // Removing the favourite of a room tagged both ways leaves a
        // low-priority order this window does not know: the write ends as
        // soon as sync shows the tags, whatever order it reports.
        remember("alice", &[room("!b", true, true, order(0.3))]);
        with_state(|state| {
            state.confirmed.insert(
                "!b".into(),
                Confirmed {
                    account: "alice".into(),
                    tags: RoomTags {
                        favourite: false,
                        low_priority: true,
                        order: None,
                    },
                    order_known: false,
                    until: Instant::now() + CONFIRMED_LIFETIME,
                },
            )
        });
        remember("alice", &[room("!b", false, true, order(0.7))]);
        assert!(with_state(|state| state.confirmed.is_empty()));
        assert_eq!(tags("!b").order, order(0.7));
    }
}
