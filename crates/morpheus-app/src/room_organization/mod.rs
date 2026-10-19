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
//!   them. Snapshots follow the homeserver in order, so each write also keeps
//!   the earlier states a snapshot may still show; a snapshot showing the
//!   write after those, an account change or the time limit ends it;
//! - after a request that failed but may have written, what was shown before
//!   it, with the tags it changed marked uncertain for the core, until a
//!   snapshot past the earlier states shows the outcome; a refusal made
//!   before anything was sent changes nothing;
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
    ConfirmedTags, FavouriteOrder, RoomInfo, RoomMembership, RoomTagChange, RoomTagError,
    TagKnowledge, TagOrder,
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
/// Earlier states kept per confirmed write; older ones are dropped.
const MAX_EARLIER: usize = 8;

/// One room's organization tags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoomTags {
    pub(crate) favourite: bool,
    pub(crate) low_priority: bool,
    /// Each tag's own order, for a tag the room has.
    pub(crate) favourite_order: Option<TagOrder>,
    pub(crate) low_priority_order: Option<TagOrder>,
}

impl RoomTags {
    /// The room's place in its section: Favorites for a room tagged both
    /// ways, else Low priority.
    pub(crate) fn order(&self) -> Option<TagOrder> {
        if self.favourite {
            self.favourite_order
        } else if self.low_priority {
            self.low_priority_order
        } else {
            None
        }
    }

    fn favourite_tag(&self) -> (bool, Option<TagOrder>) {
        (self.favourite, self.favourite_order)
    }

    fn low_priority_tag(&self) -> (bool, Option<TagOrder>) {
        (self.low_priority, self.low_priority_order)
    }
}

/// Which of a room's two tags this window knows the homeserver holds as
/// shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Known {
    favourite: bool,
    low_priority: bool,
}

impl Known {
    const ALL: Self = Self {
        favourite: true,
        low_priority: true,
    };

    /// These, and the tags a request decided.
    fn with(self, decided: Self) -> Self {
        Self {
            favourite: self.favourite || decided.favourite,
            low_priority: self.low_priority || decided.low_priority,
        }
    }

    /// These, less the tags a failed request may have written.
    fn without(self, touched: Self) -> Self {
        Self {
            favourite: self.favourite && !touched.favourite,
            low_priority: self.low_priority && !touched.low_priority,
        }
    }
}

/// What this window shows for a room ahead of sync, after its own request.
struct Confirmed {
    account: String,
    /// What the homeserver holds once the write was accepted; after a failed
    /// request, what this window showed before it.
    tags: RoomTags,
    /// The tags in `tags` the homeserver is known to hold; a tag a failed
    /// change may have written stays unknown until a request decides it.
    known: Known,
    /// Only a snapshot showing the known tags ends this entry. After a
    /// failed request the homeserver may hold any outcome, so the first
    /// snapshot past `earlier` ends it, whatever it shows.
    exact: bool,
    /// States a snapshot may still show while sync catches up, oldest first:
    /// what this window showed before this request and before each request
    /// since. Snapshots follow the homeserver in order, so one showing an
    /// earlier state is behind, even when it equals `tags` (a favourite
    /// added and removed again before sync returned either).
    earlier: Vec<RoomTags>,
    until: Instant,
}

impl Confirmed {
    /// Follow a snapshot's tags for the room; false once the entry ends. A
    /// snapshot showing an earlier state drops the states before it, which
    /// sync has passed.
    fn waits_for(&mut self, stored: RoomTags) -> bool {
        match self.earlier.iter().position(|earlier| *earlier == stored) {
            Some(reached) => {
                self.earlier.drain(..reached);
                true
            }
            None => self.exact && !self.shown_by(stored),
        }
    }

    /// Whether a snapshot shows every tag this entry knows.
    fn shown_by(&self, stored: RoomTags) -> bool {
        let favourite = stored.favourite_tag() == self.tags.favourite_tag();
        let low_priority = stored.low_priority_tag() == self.tags.low_priority_tag();
        (favourite || !self.known.favourite) && (low_priority || !self.known.low_priority)
    }

    /// A favourite the homeserver is known to hold.
    fn known_favourite(&self) -> bool {
        self.known.favourite && self.tags.favourite
    }
}

/// The tags a request decides: the tag a change adds and the one it
/// removes, the tag it only removes, or a reordered favourite.
fn decided(request: &Request) -> Known {
    let (favourite, low_priority) = match request {
        Request::Change(RoomTagChange::RemoveFavourite, _) | Request::Orders(_) => (true, false),
        Request::Change(RoomTagChange::RemoveLowPriority, _) => (false, true),
        Request::Change(..) => (true, true),
    };
    Known {
        favourite,
        low_priority,
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
        self.confirmed(room_id)
            .map(|confirmed| confirmed.tags)
            .unwrap_or_else(|| self.stored_tags(room_id))
    }

    /// What this window knows the homeserver holds for a room beyond the
    /// snapshot, for the core's add/remove decisions.
    fn confirmed_tags(&self, room_id: &str) -> ConfirmedTags {
        let Some(confirmed) = self.confirmed(room_id) else {
            return ConfirmedTags::default();
        };
        let knowledge = |known: bool, present: bool| match (known, present) {
            (false, _) => TagKnowledge::Uncertain,
            (true, true) => TagKnowledge::Present,
            (true, false) => TagKnowledge::Absent,
        };
        ConfirmedTags {
            favourite: knowledge(confirmed.known.favourite, confirmed.tags.favourite),
            low_priority: knowledge(confirmed.known.low_priority, confirmed.tags.low_priority),
        }
    }

    /// The tags of a room this window knows the homeserver holds.
    fn known(&self, room_id: &str) -> Known {
        self.confirmed(room_id)
            .map_or(Known::ALL, |confirmed| confirmed.known)
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
                    .then(|| (room_id.clone(), name.clone(), tags.favourite_order))
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

    /// Show an accepted request's writes until sync returns them. `before`
    /// is what this window showed for each written room when the request
    /// was planned.
    fn record(
        &mut self,
        account: &str,
        request: &Request,
        room_id: &str,
        before: &[(String, RoomTags)],
    ) {
        let written = written_tags(request, room_id, |id| self.tags(id));
        for (room, tags) in written {
            // The tags the request decided become known; another tag a
            // failed request left unknown stays so.
            let known = self.known(&room).with(decided(request));
            self.overlay(account, &room, tags, known, true, before);
        }
    }

    /// After a failed request that may have written: keep showing what this
    /// window showed before it, so the rooms keep their place, tell the core
    /// that a tag the request may have written is uncertain, and let the
    /// first snapshot past the earlier states show the outcome.
    fn uncertain(
        &mut self,
        account: &str,
        request: &Request,
        room_id: &str,
        before: &[(String, RoomTags)],
    ) {
        for room in request.rooms(room_id) {
            let Some((_, shown)) = before.iter().find(|(id, _)| id == room) else {
                continue;
            };
            let known = match request {
                // An order write never changes whether a room has a tag.
                Request::Orders(_) => self.known(room),
                Request::Change(..) => self.known(room).without(decided(request)),
            };
            self.overlay(account, room, *shown, known, false, before);
        }
    }

    fn overlay(
        &mut self,
        account: &str,
        room: &str,
        tags: RoomTags,
        known: Known,
        exact: bool,
        before: &[(String, RoomTags)],
    ) {
        let mut earlier = self
            .confirmed(room)
            .map(|previous| previous.earlier.clone())
            .unwrap_or_default();
        if let Some((_, shown)) = before.iter().find(|(id, _)| id == room) {
            earlier.push(*shown);
        }
        let excess = earlier.len().saturating_sub(MAX_EARLIER);
        earlier.drain(..excess);
        let mut confirmed = Confirmed {
            account: account.to_owned(),
            tags,
            known,
            exact,
            earlier,
            until: Instant::now() + CONFIRMED_LIFETIME,
        };
        // The latest snapshot may already end it.
        let ended = self
            .stored
            .get(room)
            .is_some_and(|(stored, _)| !confirmed.waits_for(*stored));
        if ended {
            self.confirmed.remove(room);
        } else {
            self.confirmed.insert(room.to_owned(), confirmed);
        }
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
                    favourite_order: room.favourite_order,
                    low_priority_order: room.low_priority_order,
                };
                (room.room_id.clone(), (tags, room.name.to_lowercase()))
            })
            .collect();
        let stored = &state.stored;
        state.confirmed.retain(|room_id, confirmed| {
            confirmed.account == account_id
                && now < confirmed.until
                && stored
                    .get(room_id)
                    .is_none_or(|(tags, _)| confirmed.waits_for(*tags))
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
/// the room has once it is accepted.
#[derive(Debug, PartialEq)]
enum Request {
    Change(RoomTagChange, RoomTags),
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
        // An existing tag is kept as stored, order included; the other tag of
        // a room tagged both ways keeps its own order until it is removed.
        ADD_FAVOURITE if !current.favourite || current.low_priority => {
            let order = if current.favourite {
                current.favourite_order
            } else {
                append_order(favourites)
            };
            let expected = RoomTags {
                favourite: true,
                favourite_order: order,
                ..RoomTags::default()
            };
            Some(Request::Change(RoomTagChange::Favourite(order), expected))
        }
        REMOVE_FAVOURITE if current.favourite => {
            let expected = RoomTags {
                favourite: false,
                favourite_order: None,
                ..current
            };
            Some(Request::Change(RoomTagChange::RemoveFavourite, expected))
        }
        LOW_PRIORITY if !current.low_priority || current.favourite => {
            let order = current.low_priority_order.filter(|_| current.low_priority);
            let expected = RoomTags {
                low_priority: true,
                low_priority_order: order,
                ..RoomTags::default()
            };
            let change = RoomTagChange::LowPriority(order);
            Some(Request::Change(change, expected))
        }
        NORMAL_PRIORITY if current.low_priority => {
            let expected = RoomTags {
                low_priority: false,
                low_priority_order: None,
                ..current
            };
            Some(Request::Change(RoomTagChange::RemoveLowPriority, expected))
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

/// The tags a confirmed request leaves on each written room.
fn written_tags(
    request: &Request,
    room_id: &str,
    before: impl Fn(&str) -> RoomTags,
) -> Vec<(String, RoomTags)> {
    match request {
        Request::Change(_, expected) => vec![(room_id.to_owned(), *expected)],
        Request::Orders(orders) => orders
            .iter()
            .map(|(room_id, order)| {
                let tags = RoomTags {
                    favourite: true,
                    favourite_order: Some(*order),
                    ..before(room_id)
                };
                (room_id.clone(), tags)
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
        Request::Change(change, _) => Call::Change(*change, state.confirmed_tags(room_id)),
        Request::Orders(orders) => Call::Orders(
            orders
                .iter()
                .map(|(room_id, order)| FavouriteOrder {
                    room_id: room_id.clone(),
                    order: *order,
                    confirmed_favourite: state
                        .confirmed(room_id)
                        .is_some_and(Confirmed::known_favourite),
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
        RoomTagError::PartlySaved => {
            Some("Only part of the change was saved; the room list updates from the homeserver.")
        }
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
            let before = request
                .rooms(room_id)
                .into_iter()
                .map(|id| (id.to_owned(), state.tags(id)))
                .collect::<Vec<_>>();
            state.next_operation = state.next_operation.wrapping_add(1);
            let operation = state.next_operation;
            state.pending = Some(Pending {
                account: account_id.clone(),
                room: room_id.to_owned(),
                operation,
            });
            Some((request, call, before, operation))
        });
        let Some((request, call, before, operation)) = started else {
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
                finish(
                    &ui, operation, &selection, &room_id, &request, &before, result,
                );
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
    before: &[(String, RoomTags)],
    result: Result<(), RoomTagError>,
) {
    let account = selection.account_id();
    let visible = with_state(|state| {
        let owned = state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.operation == operation);
        if owned {
            state.pending = None;
        }
        let current = selection.is_current();
        let visible = current && state.account == account;
        // The same account may still be shown under a new selection, which
        // cut this request off before or after the homeserver answered it.
        let cut_off = owned && !current && state.account == account;
        let shown = visible || cut_off;
        match &result {
            // Accepted by the homeserver: shown until sync returns it.
            Ok(()) if shown => state.record(account, request, room_id, before),
            Err(RoomTagError::NotConfirmed | RoomTagError::PartlySaved) if shown => {
                state.uncertain(account, request, room_id, before)
            }
            Err(RoomTagError::AccountChanged) if cut_off => {
                state.uncertain(account, request, room_id, before)
            }
            // Refused before anything was sent: what is shown still holds.
            _ => {}
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
    global.set_active_can_move_down(index.is_some_and(|index| index + 1 < favourites.row_count()));
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
            favourite_order: order,
            ..RoomTags::default()
        }
    }

    fn low(order: Option<TagOrder>) -> RoomTags {
        RoomTags {
            low_priority: true,
            low_priority_order: order,
            ..RoomTags::default()
        }
    }

    fn change(change: RoomTagChange, tags: RoomTags) -> Option<Request> {
        Some(Request::Change(change, tags))
    }

    /// A room tagged both ways by another client.
    fn both(favourite_order: Option<TagOrder>, low_priority_order: Option<TagOrder>) -> RoomTags {
        RoomTags {
            favourite: true,
            low_priority: true,
            favourite_order,
            low_priority_order,
        }
    }

    fn joined(id: &str, tags: RoomTags) -> RoomInfo {
        RoomInfo {
            room_id: id.into(),
            name: id.into(),
            topic: None,
            membership: RoomMembership::Joined,
            is_direct: false,
            is_space: false,
            avatar_mxc: None,
            favourite: tags.favourite,
            low_priority: tags.low_priority,
            favourite_order: tags.favourite_order,
            low_priority_order: tags.low_priority_order,
            unread_messages: 0,
            unread_notifications: 0,
            unread_mentions: 0,
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
        let planned = |action, room: &str, tags| plan(action, room, tags, &all, &visible);
        let normal = RoomTags::default();
        let new = favourite(order(0.875));
        assert_eq!(
            planned(ADD_FAVOURITE, "new", normal),
            change(RoomTagChange::Favourite(order(0.875)), new)
        );
        let first = favourite(order(0.5));
        assert_eq!(
            plan(ADD_FAVOURITE, "new", normal, &[], &[]),
            change(RoomTagChange::Favourite(order(0.5)), first)
        );
        // Next to unordered favourites a new one has no order either.
        let unordered = list(&[("a", order(0.25)), ("b", None)]);
        assert_eq!(
            plan(ADD_FAVOURITE, "new", normal, &unordered, &visible),
            change(RoomTagChange::Favourite(None), favourite(None))
        );
        // A new low-priority tag has no order.
        assert_eq!(
            planned(LOW_PRIORITY, "new", normal),
            change(RoomTagChange::LowPriority(None), low(None))
        );
        // A room tagged both ways keeps the tag it is given, with the order
        // it shows, and only loses the other one.
        let tagged = both(order(0.25), order(0.7));
        let kept = favourite(order(0.25));
        assert_eq!(
            planned(ADD_FAVOURITE, "a", tagged),
            change(RoomTagChange::Favourite(order(0.25)), kept)
        );
        assert_eq!(
            planned(NORMAL_PRIORITY, "a", tagged),
            change(RoomTagChange::RemoveLowPriority, kept)
        );
        let kept = low(order(0.7));
        assert_eq!(
            planned(LOW_PRIORITY, "a", tagged),
            change(RoomTagChange::LowPriority(order(0.7)), kept)
        );
        assert_eq!(
            planned(REMOVE_FAVOURITE, "a", tagged),
            change(RoomTagChange::RemoveFavourite, kept)
        );
        // A choice that changes nothing plans nothing.
        let a = favourite(order(0.25));
        assert_eq!(planned(LOW_PRIORITY, "l", low(order(0.7))), None);
        assert_eq!(planned(ADD_FAVOURITE, "a", a), None);
        assert_eq!(planned(REMOVE_FAVOURITE, "new", normal), None);
        assert_eq!(planned(NORMAL_PRIORITY, "a", a), None);
        let moved = vec![("a".to_owned(), order(0.875).unwrap())];
        assert_eq!(planned(MOVE_DOWN, "a", a), Some(Request::Orders(moved)));
        // A non-favourite, an edge or an unknown action moves nothing.
        assert_eq!(planned(MOVE_UP, "new", normal), None);
        assert_eq!(planned(MOVE_UP, "a", a), None);
        assert_eq!(planned(9, "a", a), None);
    }

    #[test]
    fn confirmed_writes_show_until_sync_returns_them_for_that_account_only() {
        with_state(|state| *state = State::default());
        let none = RoomTags::default();
        let added = favourite(order(0.5));
        let add = Request::Change(RoomTagChange::Favourite(order(0.5)), added);
        let remove = Request::Change(RoomTagChange::RemoveFavourite, none);
        remember("alice", &[joined("!a", none)]);
        with_state(|state| {
            state.record("alice", &add, "!a", &[("!a".to_owned(), none)]);
        });
        // A snapshot that has not caught up keeps showing the accepted write.
        remember("alice", &[joined("!a", none)]);
        assert_eq!(tags("!a"), added);
        // One that shows it ends the overlay.
        remember("alice", &[joined("!a", added)]);
        assert!(with_state(|state| state.confirmed.is_empty()));
        // Another account never sees it.
        with_state(|state| {
            state.record("alice", &remove, "!a", &[("!a".to_owned(), added)]);
        });
        remember("bob", &[joined("!a", favourite(order(0.25)))]);
        assert_eq!(tags("!a"), favourite(order(0.25)));
        assert!(with_state(|state| state.confirmed.is_empty()));
    }

    #[test]
    fn follow_up_choices_use_confirmed_writes() {
        with_state(|state| *state = State::default());
        let none = RoomTags::default();
        let added = favourite(order(0.5));
        let add = Request::Change(RoomTagChange::Favourite(order(0.5)), added);
        let moved = Request::Orders(vec![("!a".to_owned(), order(0.25).unwrap())]);
        let to_low = Request::Change(RoomTagChange::LowPriority(None), low(None));
        remember("alice", &[joined("!a", none)]);
        // A favourite added here but not synced yet: a following move tells
        // the core it is a favourite, and a following change tells it which
        // tags the homeserver holds.
        with_state(|state| {
            state.record("alice", &add, "!a", &[("!a".to_owned(), none)]);
            let Call::Orders(writes) = call_for(&moved, "!a", state) else {
                panic!("a move is an order write");
            };
            assert!(writes[0].confirmed_favourite);
            let Call::Change(_, known) = call_for(&to_low, "!a", state) else {
                panic!("a change is a tag change");
            };
            assert_eq!(known.favourite, TagKnowledge::Present);
            assert_eq!(known.low_priority, TagKnowledge::Absent);
            // An expired write is neither shown nor reported.
            state.confirmed.get_mut("!a").unwrap().until = Instant::now();
            assert_eq!(state.tags("!a"), none);
            assert_eq!(state.confirmed_tags("!a"), ConfirmedTags::default());
        });
    }

    #[test]
    fn a_failed_request_keeps_the_room_in_place_and_its_tags_uncertain() {
        with_state(|state| *state = State::default());
        let none = RoomTags::default();
        let added = favourite(order(0.5));
        let add = Request::Change(RoomTagChange::Favourite(order(0.5)), added);
        let to_low = Request::Change(RoomTagChange::LowPriority(None), low(None));
        let moved = Request::Orders(vec![("!b".to_owned(), order(0.25).unwrap())]);
        remember("alice", &[joined("!a", none), joined("!b", none)]);
        with_state(|state| {
            for room in ["!a", "!b"] {
                state.record("alice", &add, room, &[(room.to_owned(), none)]);
            }
            // Marking !a low priority may have written: it keeps its place,
            // and a retry is told both tags are uncertain, so it writes them
            // whatever the stored tags say.
            state.uncertain("alice", &to_low, "!a", &[("!a".to_owned(), added)]);
            assert_eq!(state.tags("!a"), added);
            let known = state.confirmed_tags("!a");
            assert_eq!(known.favourite, TagKnowledge::Uncertain);
            assert_eq!(known.low_priority, TagKnowledge::Uncertain);
            // A failed move of !b leaves it a known favourite.
            state.uncertain("alice", &moved, "!b", &[("!b".to_owned(), added)]);
            assert_eq!(state.tags("!b"), added);
            let known = state.confirmed_tags("!b");
            assert_eq!(known.favourite, TagKnowledge::Present);
        });
        // Snapshots that are behind keep both; the first one past them shows
        // the outcome, whatever it is.
        remember("alice", &[joined("!a", none), joined("!b", added)]);
        assert_eq!((tags("!a"), tags("!b")), (added, added));
        remember("alice", &[joined("!a", low(None)), joined("!b", added)]);
        assert_eq!(tags("!a"), low(None));
        assert!(with_state(|state| !state.confirmed.contains_key("!a")));
    }

    #[test]
    fn an_accepted_change_keeps_an_untouched_uncertain_tag_uncertain() {
        with_state(|state| *state = State::default());
        let none = RoomTags::default();
        let stored = favourite(order(0.5));
        let shown = [("!c".to_owned(), stored)];
        let to_low = Request::Change(RoomTagChange::LowPriority(None), low(None));
        let unfavourite = Request::Change(RoomTagChange::RemoveFavourite, none);
        remember("alice", &[joined("!c", stored)]);
        with_state(|state| {
            // Marking it low priority was saved only in part: the homeserver
            // may hold either tag.
            state.uncertain("alice", &to_low, "!c", &shown);
            // Removing the favourite then decides that tag only.
            state.record("alice", &unfavourite, "!c", &shown);
            let known = state.confirmed_tags("!c");
            assert_eq!(known.favourite, TagKnowledge::Absent);
            assert_eq!(known.low_priority, TagKnowledge::Uncertain);
        });
        // A snapshot showing the decided tag ends the entry, whatever the
        // other tag turned out to be.
        remember("alice", &[joined("!c", low(None))]);
        assert_eq!(tags("!c"), low(None));
        assert!(with_state(|state| state.confirmed.is_empty()));
    }

    #[test]
    fn a_stale_snapshot_never_ends_a_newer_write() {
        with_state(|state| *state = State::default());
        let none = RoomTags::default();
        let added = favourite(order(0.5));
        let add = Request::Change(RoomTagChange::Favourite(order(0.5)), added);
        let remove = Request::Change(RoomTagChange::RemoveFavourite, none);
        remember("alice", &[joined("!a", none)]);
        // Added, then removed again before sync returned either.
        with_state(|state| {
            state.record("alice", &add, "!a", &[("!a".to_owned(), none)]);
            state.record("alice", &remove, "!a", &[("!a".to_owned(), added)]);
        });
        // The snapshot from before the add equals the removal but is behind
        // it, and so is the delayed snapshot of the add.
        remember("alice", &[joined("!a", none)]);
        assert_eq!(tags("!a"), none);
        assert!(with_state(|state| state.confirmed.contains_key("!a")));
        remember("alice", &[joined("!a", added)]);
        assert_eq!(tags("!a"), none);
        // The snapshot after the add shows the removal and ends the overlay.
        remember("alice", &[joined("!a", none)]);
        assert!(with_state(|state| state.confirmed.is_empty()));
        assert_eq!(tags("!a"), none);
        // Sync may skip the add: the overlay then shows the removal, which
        // the homeserver holds, until its time limit.
        remember("alice", &[joined("!b", none)]);
        with_state(|state| {
            state.record("alice", &add, "!b", &[("!b".to_owned(), none)]);
            state.record("alice", &remove, "!b", &[("!b".to_owned(), added)]);
        });
        remember("alice", &[joined("!b", none)]);
        assert_eq!(tags("!b"), none);
        with_state(|state| {
            state.confirmed.get_mut("!b").unwrap().until = Instant::now();
        });
        remember("alice", &[joined("!b", none)]);
        assert!(with_state(|state| state.confirmed.is_empty()));
    }

    #[test]
    fn writes_on_rooms_tagged_both_ways_keep_the_low_priority_order() {
        with_state(|state| *state = State::default());
        let none = RoomTags::default();
        let tagged = both(order(0.25), order(0.7));
        let kept = low(order(0.7));
        remember("alice", &[joined("!b", tagged)]);
        let current = with_state(|state| state.tags("!b"));
        let favourites = with_state(|state| state.favourites());
        let planned = plan(LOW_PRIORITY, "!b", current, &favourites, &ids(&["!b"]));
        let expected = change(RoomTagChange::LowPriority(order(0.7)), kept);
        assert_eq!(planned, expected);
        let request = planned.expect("the favourite tag is removed");
        let shown = [("!b".to_owned(), tagged)];
        with_state(|state| state.record("alice", &request, "!b", &shown));
        // Until sync returns it, the room keeps its stored low-priority place.
        assert_eq!(tags("!b").order(), order(0.7));
        remember("alice", &[joined("!b", kept)]);
        assert!(with_state(|state| state.confirmed.is_empty()));
        // A snapshot may show a write before the homeserver's answer
        // arrives: nothing is left to overlay.
        let normal = Request::Change(RoomTagChange::RemoveLowPriority, none);
        let shown = [("!b".to_owned(), kept)];
        remember("alice", &[joined("!b", none)]);
        with_state(|state| {
            state.record("alice", &normal, "!b", &shown);
            assert!(state.confirmed.is_empty());
        });
    }
}
