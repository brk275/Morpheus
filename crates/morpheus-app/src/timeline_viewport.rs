//! Bringing one loaded event into the conversation's or open thread's
//! viewport, and keeping the reader's row in place when rows change above it.
//!
//! Slint 1.18's ListView has no indexed scroll for rows of different heights:
//! it creates delegates only for the drawn rows, and its content height is
//! their average height times the row count. A seek is therefore measured in
//! app-window.slint, in two steps. While the row is not drawn, the list moves
//! towards it: a far row straight to its estimated position, which the list
//! then lays out starting from exactly that row; a near one by most of a
//! screen, in the direction of the reported top row. Once drawn, the row
//! places itself from its own laid-out position and height, after every
//! layout of it (before that frame is drawn) and at least every 16 ms. The
//! thread's replies are all laid out, so a reply only ever places itself.
//!
//! A seek is bound to what its row was shown under: the account selection,
//! the visible account and room, the navigation it belongs to, and the
//! conversation's feed generation and focused or live view (the thread's root
//! and request generation for a reply), plus its owner's own conditions (a
//! search hit's or activation's Opening, an unread jump's click-time fence).
//! Every seek, anchors included, also obeys one policy: no protected surface
//! is up now (viewport-seek-allowed: the inventory links and the unread jump
//! use, and an open link card) and no security step began since the security
//! generation it captured at its click or publication. Its start, every move
//! and its end are admitted here first, so any change ends the seek before it
//! moves new content; a protected surface rising retires every seek for good,
//! and so do a newer seek of the same pane, a room or account change and
//! leaving a focused view. Each seek is bounded by SEEK_TIME of event-loop
//! time and SEEK_MOVES moves; then its owner is told the row could not be
//! shown. The person's own wheel, drag or scrollbar ends it at once. Nothing
//! is animated.
//!
//! The reader's anchor is the row drawn across the viewport's top, reported
//! by that row. When the conversation's or thread's rows are published again
//! under the same feed and view and that row moved (rows were inserted or
//! removed above it, such as by Load earlier), an anchor seek puts its top
//! back where it was drawn. In the conversation, height changes need no seek:
//! the ListView keeps its first drawn row in place. The thread's replies are a
//! plain layout, so there a keeper puts the anchor reply back at its measured
//! offset whenever the layout above it changes, under the same policy and the
//! thread's latest publication; only scrolling takes the anchor again.
//! Nothing here follows new messages to the bottom; the conversation never
//! did.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Weak, atomic::Ordering};
use std::time::Duration;

use morpheus_matrix::AccountSelection;
use slint::{ComponentHandle, ModelRc, Timer};

use crate::{AppWindow, MessageItem, live::LiveUi, security::SecurityGate, timeline_model};

/// The longest a seek may run, in event-loop time.
pub(crate) const SEEK_TIME: Duration = Duration::from_secs(2);
/// The most moves one seek may make; each is one admission.
pub(crate) const SEEK_MOVES: u32 = 240;

/// How app-window.slint reports a placed row (viewport-seek-ended); any other
/// code is the person's scrolling.
const PLACED: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pane {
    /// The conversation (timeline-list), live or focused.
    Room,
    /// The open thread's replies (thread-scroll).
    Thread,
}

/// How a seek ended, told to its owner while it still holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The row is in place in the viewport (or as close as its ends allow).
    Shown,
    /// The person scrolled; the seek stopped where they took over.
    Scrolled,
    /// The row could not be placed within the seek's bounds, or is gone.
    Unavailable,
}

/// Who asked for a reveal: its own conditions, asked before every move and
/// before its end is told, and what it does once the seek ends.
pub(crate) struct Owner {
    pub(crate) admits: Box<dyn Fn(&AppWindow, &LiveUi) -> bool>,
    pub(crate) ended: Box<dyn FnOnce(&AppWindow, Ended)>,
}

/// One reveal request: the pane, the owner's own account selection,
/// navigation epoch and security generation (taken at its click or
/// activation, never looked up again), and the loaded event.
pub(crate) struct Reveal {
    pub(crate) pane: Pane,
    pub(crate) selection: AccountSelection,
    pub(crate) navigation: u64,
    pub(crate) security: u64,
    pub(crate) event_id: String,
    pub(crate) owner: Owner,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Placement {
    /// A third of the way down the viewport.
    Reveal,
    /// The row's top this far below the viewport's top (zero or less).
    Anchor(f32),
}

/// The rows a seek moves: the conversation under a feed generation, focused
/// or live, or one thread under a thread request generation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum View {
    Room { feed: u64, focused: bool },
    Thread { root: String, generation: u64 },
}

/// What every move of a seek is bound to.
struct Scope {
    selection: AccountSelection,
    account: String,
    room: String,
    /// The navigation the rows belong to; a thread's anchor has none (its
    /// thread request generation ends with any thread change).
    navigation: Option<u64>,
    view: View,
}

impl Scope {
    fn holds(&self, ui: &AppWindow, live: &LiveUi) -> bool {
        let view = match &self.view {
            View::Room { feed, focused } => {
                live.feed_generation() == *feed && ui.get_conversation_focused() == *focused
            }
            View::Thread { root, generation } => {
                live.thread_request_is_current(*generation)
                    && ui.get_thread_root_id().as_str() == root
            }
        };
        view && self.selection.is_current()
            && live.is_live.load(Ordering::Acquire)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == self.account
            && ui.get_active_room_id().as_str() == self.room
            && self
                .navigation
                .is_none_or(|navigation| live.navigation_is_current(navigation))
    }

    fn check(self) -> Check {
        Box::new(move |ui: &AppWindow, live: &LiveUi| self.holds(ui, live))
    }
}

type Check = Box<dyn Fn(&AppWindow, &LiveUi) -> bool>;

/// The security generation a seek is bound to, read before the gate's state:
/// a security step that begins after the read shows as blocked here or as a
/// newer generation at every later check (`u64::MAX` is never allowed).
pub(crate) fn security_generation(gate: &SecurityGate) -> u64 {
    let generation = gate.ringing_generation();
    if gate.blocked() { u64::MAX } else { generation }
}

/// The policy every start, move and end obeys besides its scope and owner:
/// no protected surface is up now, and no security step began since
/// `security` was captured.
fn allowed(ui: &AppWindow, live: &LiveUi, security: u64) -> bool {
    ui.get_viewport_seek_allowed() && live.security_gate.ringing_allowed(security)
}

struct Seek {
    id: i32,
    pane: Pane,
    event_id: String,
    /// The conversation row's index when last resolved; unused for a reply.
    index: usize,
    /// Its Scope, checked before every move and before its end is told.
    scope: Check,
    /// The security generation of its click or publication (see `allowed`).
    security: u64,
    /// An anchor has no owner and tells nobody how it ended.
    owner: Option<Owner>,
    moves: u32,
}

impl Seek {
    /// Count one move; false once SEEK_MOVES were made.
    fn spend_move(&mut self) -> bool {
        if self.moves >= SEEK_MOVES {
            return false;
        }
        self.moves += 1;
        true
    }
}

/// The account, room and view a pane's rows were last published under.
type Shown = (String, String, View);

/// What the thread keeper's moves are bound to: the scope and security
/// generation of the thread's latest publication.
type Keeper = (Rc<dyn Fn(&AppWindow, &LiveUi) -> bool>, u64);

/// At most one seek per pane, on the UI thread only.
#[derive(Default)]
struct Seeks {
    last_id: i32,
    room: Option<Seek>,
    thread: Option<Seek>,
    room_shown: Option<Shown>,
    thread_shown: Option<Shown>,
    thread_keeper: Option<Keeper>,
}

impl Seeks {
    /// A positive id, never 0 (no seek in app-window.slint).
    fn next_id(&mut self) -> i32 {
        self.last_id = if self.last_id >= i32::MAX - 1 {
            1
        } else {
            self.last_id + 1
        };
        self.last_id
    }

    fn slot(&mut self, pane: Pane) -> &mut Option<Seek> {
        match pane {
            Pane::Room => &mut self.room,
            Pane::Thread => &mut self.thread,
        }
    }

    /// Make `seek` its pane's seek under a new id, returning the id and the
    /// seek it replaced.
    fn begin(&mut self, mut seek: Seek) -> (i32, Option<Seek>) {
        seek.id = self.next_id();
        let id = seek.id;
        (id, self.slot(seek.pane).replace(seek))
    }

    /// The seek `id` if it is still a pane's current one; any other id (an
    /// ended, retired or replaced seek, a late report) finds nothing.
    fn take(&mut self, id: i32) -> Option<Seek> {
        [Pane::Room, Pane::Thread]
            .into_iter()
            .find_map(|pane| self.slot(pane).take_if(|seek| seek.id == id))
    }

    /// Put a checked seek back, unless a newer one took its pane meanwhile.
    fn restore(&mut self, seek: Seek) {
        let slot = self.slot(seek.pane);
        if slot.is_none() {
            *slot = Some(seek);
        }
    }

    /// Record what `pane`'s rows are now published under; true when it is
    /// what they were published under before.
    fn republished(&mut self, pane: Pane, shown: Shown) -> bool {
        let last = match pane {
            Pane::Room => &mut self.room_shown,
            Pane::Thread => &mut self.thread_shown,
        };
        last.replace(shown.clone()) == Some(shown)
    }
}

thread_local! {
    static SEEKS: RefCell<Seeks> = RefCell::new(Seeks::default());
}

/// Never call window setters, callbacks, checks or owner closures inside `f`.
fn with<R>(f: impl FnOnce(&mut Seeks) -> R) -> R {
    SEEKS.with(|seeks| f(&mut seeks.borrow_mut()))
}

/// Answer app-window.slint's admissions, ends and withdrawals for this
/// window's live UI (called once, from message_search::register).
pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>) {
    let weak = ui.as_weak();
    let admit_live = live.clone();
    ui.on_viewport_seek_admit(move |id| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        admit(&ui, &admit_live, id)
    });
    let weak = ui.as_weak();
    let keeper_live = live.clone();
    ui.on_viewport_anchor_admit(move || {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        keeper_admitted(&ui, &keeper_live)
    });
    let weak = ui.as_weak();
    ui.on_viewport_seek_withdrawn(move || {
        if let Some(ui) = weak.upgrade() {
            withdraw(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_viewport_seek_ended(move |id, outcome| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let ended = if outcome == PLACED {
            Ended::Shown
        } else {
            Ended::Scrolled
        };
        if let Some(seek) = with(|seeks| seeks.take(id)) {
            finish(&ui, Some(live.as_ref()), seek, ended);
        }
    });
}

/// Start revealing the loaded `event_id` in `pane`, replacing that pane's
/// seek. A conversation reveal highlights the event and counts as one jump
/// (timeline-jump-request). False, with nothing changed, when the event is
/// not a loaded conversation row or no thread (other than the event's own
/// root) is open. True, with nothing moved, highlighted or said, when its
/// owner, scope or the policy refuses it at its start: it ends for good.
pub(crate) fn reveal(ui: &AppWindow, live: &Arc<LiveUi>, request: Reveal) -> bool {
    let Reveal {
        pane,
        selection,
        navigation,
        security,
        event_id,
        owner,
    } = request;
    let (view, index) = match pane {
        Pane::Room => {
            let rows = ui.get_messages();
            let Some(index) = timeline_model::index_of_event(&rows, &event_id) else {
                return false;
            };
            let view = View::Room {
                feed: live.feed_generation(),
                focused: ui.get_conversation_focused(),
            };
            (view, index)
        }
        Pane::Thread => {
            let root = ui.get_thread_root_id().to_string();
            if root.is_empty() || root == event_id {
                return false;
            }
            let generation = live.thread_request_generation();
            (View::Thread { root, generation }, 0)
        }
    };
    let scope = Scope {
        selection,
        account: ui.get_active_account_local_id().to_string(),
        room: ui.get_active_room_id().to_string(),
        navigation: Some(navigation),
        view,
    };
    if !allowed(ui, live, security) || !scope.holds(ui, live) || !(owner.admits)(ui, live) {
        return true;
    }
    if pane == Pane::Room {
        ui.set_timeline_jump_event_id(event_id.as_str().into());
        ui.set_timeline_jump_request(ui.get_timeline_jump_request().wrapping_add(1));
    }
    let seek = Seek {
        id: 0,
        pane,
        event_id,
        index,
        scope: scope.check(),
        security,
        owner: Some(owner),
        moves: 0,
    };
    start(ui, Some(live), seek, Placement::Reveal);
    true
}

/// Test builds only: seek the loaded `event_id` in `pane` (a reveal, or an
/// anchor at `offset`) for `owner`, without a session scope (a fixture window
/// has no account selection). The policy, the security generation captured
/// now, the owner's admission, the seek's bounds and its end are the
/// production ones; only the Scope check is left out, which `reveal` and the
/// publication hooks always add. Its return is `reveal`'s.
#[cfg(test)]
pub(crate) fn seek_for_test(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    pane: Pane,
    event_id: &str,
    offset: Option<f32>,
    owner: Option<Owner>,
) -> bool {
    let index = match pane {
        Pane::Room => {
            let rows = ui.get_messages();
            let Some(index) = timeline_model::index_of_event(&rows, event_id) else {
                return false;
            };
            index
        }
        Pane::Thread => 0,
    };
    let security = security_generation(&live.security_gate);
    let owner_admits = owner.as_ref().is_none_or(|owner| (owner.admits)(ui, live));
    if !allowed(ui, live, security) || !owner_admits {
        return true;
    }
    let seek = Seek {
        id: 0,
        pane,
        event_id: event_id.to_owned(),
        index,
        scope: Box::new(|_: &AppWindow, _: &LiveUi| true),
        security,
        owner,
        moves: 0,
    };
    let placement = offset.map_or(Placement::Reveal, Placement::Anchor);
    start(ui, Some(live), seek, placement);
    true
}

/// Test builds only: bind the thread keeper as a thread publication would,
/// with the security generation captured now and no session scope (see
/// `seek_for_test`).
#[cfg(test)]
pub(crate) fn keep_thread_for_test(live: &LiveUi) {
    let security = security_generation(&live.security_gate);
    let scope: Rc<dyn Fn(&AppWindow, &LiveUi) -> bool> = Rc::new(|_: &AppWindow, _: &LiveUi| true);
    with(|seeks| seeks.thread_keeper = Some((scope, security)));
}

fn start(ui: &AppWindow, live: Option<&Arc<LiveUi>>, seek: Seek, placement: Placement) {
    let (pane, event, index) = (seek.pane, seek.event_id.clone(), seek.index);
    let (id, replaced) = with(|seeks| seeks.begin(seek));
    // The replaced seek ends without a word: a newer target took its pane.
    drop(replaced);
    let (reveal, offset) = match placement {
        Placement::Reveal => (true, 0.0),
        Placement::Anchor(offset) => (false, offset.min(0.0)),
    };
    // The id last: it starts the seek's first approach in the window.
    match pane {
        Pane::Room => {
            ui.set_timeline_seek_event(event.as_str().into());
            ui.set_timeline_seek_index(i32::try_from(index).unwrap_or(i32::MAX));
            ui.set_timeline_seek_reveal(reveal);
            ui.set_timeline_seek_offset(offset);
            ui.set_timeline_seek_id(id);
        }
        Pane::Thread => {
            ui.set_thread_seek_event(event.as_str().into());
            ui.set_thread_seek_reveal(reveal);
            ui.set_thread_seek_offset(offset);
            ui.set_thread_seek_id(id);
        }
    }
    // The bound holds neither the window nor the live UI alive.
    let (weak, live) = (ui.as_weak(), live.map(Arc::downgrade));
    Timer::single_shot(SEEK_TIME, move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let live = live.as_ref().and_then(Weak::upgrade);
        if let Some(seek) = with(|seeks| seeks.take(id)) {
            finish(&ui, live.as_deref(), seek, Ended::Unavailable);
        }
    });
}

/// One move of seek `id`: false, and the seek ends, unless the policy, its
/// scope and owner still hold, its event is still a row and it has moves
/// left. A conversation row's index is resolved again only when the row it
/// had no longer shows the event.
fn admit(ui: &AppWindow, live: &LiveUi, id: i32) -> bool {
    let Some(mut seek) = with(|seeks| seeks.take(id)) else {
        return false;
    };
    let owner_holds = seek
        .owner
        .as_ref()
        .is_none_or(|owner| (owner.admits)(ui, live));
    if !allowed(ui, live, seek.security) || !owner_holds || !(seek.scope)(ui, live) {
        clear(ui, seek.pane, id);
        return false;
    }
    if seek.pane == Pane::Room {
        let rows = ui.get_messages();
        if !timeline_model::has_event_at(&rows, seek.index, &seek.event_id) {
            let Some(index) = timeline_model::index_of_event(&rows, &seek.event_id) else {
                finish(ui, Some(live), seek, Ended::Unavailable);
                return false;
            };
            seek.index = index;
            ui.set_timeline_seek_index(i32::try_from(index).unwrap_or(i32::MAX));
            rows_changed(ui);
        }
    }
    if !seek.spend_move() {
        finish(ui, Some(live), seek, Ended::Unavailable);
        return false;
    }
    with(|seeks| seeks.restore(seek));
    true
}

/// End a taken seek: the window stops it, and its owner is told only while
/// the policy, the seek's scope and the owner's own conditions still hold,
/// so nothing is said into a view the seek no longer belongs to.
fn finish(ui: &AppWindow, live: Option<&LiveUi>, seek: Seek, ended: Ended) {
    clear(ui, seek.pane, seek.id);
    let (Some(owner), Some(live)) = (seek.owner, live) else {
        return;
    };
    if allowed(ui, live, seek.security) && (seek.scope)(ui, live) && (owner.admits)(ui, live) {
        (owner.ended)(ui, ended);
    }
}

/// The thread keeper's one move: only under the thread's latest
/// publication, its security generation and no protected surface.
fn keeper_admitted(ui: &AppWindow, live: &LiveUi) -> bool {
    let Some((scope, security)) = with(|seeks| seeks.thread_keeper.clone()) else {
        return false;
    };
    allowed(ui, live, security) && (*scope)(ui, live)
}

/// The running conversation seek's row moved because rows changed under it:
/// the list's next top row is that seek's own, not the person's scrolling.
fn rows_changed(ui: &AppWindow) {
    ui.set_timeline_seek_rows(ui.get_timeline_seek_rows().wrapping_add(1));
}

/// Stop the window's seek of `pane` if it is still seek `id`.
fn clear(ui: &AppWindow, pane: Pane, id: i32) {
    match pane {
        Pane::Room if ui.get_timeline_seek_id() == id => ui.set_timeline_seek_id(0),
        Pane::Thread if ui.get_thread_seek_id() == id => ui.set_thread_seek_id(0),
        _ => {}
    }
}

/// A protected surface takes over: synchronously at its admitted entry
/// (message_search::surface_entered, from matrix_links::clear) and again
/// when viewport-seek-allowed falls. Both panes' seeks, anchors included,
/// end for good without a word, and the thread keeper stops until the
/// thread's next publication binds it again. Every move and end already
/// refuses while a surface is up; this keeps a seek from resuming once one
/// closes, even one opened and closed before any change handler ran.
pub(crate) fn withdraw(ui: &AppWindow) {
    let retired = with(|seeks| {
        seeks.thread_keeper = None;
        [seeks.room.take(), seeks.thread.take()]
    });
    for seek in retired.into_iter().flatten() {
        clear(ui, seek.pane, seek.id);
    }
}

/// End the conversation's seek without a word: the shown view changed
/// (message_search::reset_history: focused and live views, Return to live).
pub(crate) fn retire_room(ui: &AppWindow) {
    if let Some(seek) = with(|seeks| seeks.room.take()) {
        clear(ui, Pane::Room, seek.id);
    }
}

/// A room, account or session change (message_search's resets): end both
/// panes' seeks without a word and forget what their rows and anchors were
/// shown under.
pub(crate) fn reset(ui: &AppWindow) {
    let retired = with(|seeks| {
        seeks.room_shown = None;
        seeks.thread_shown = None;
        seeks.thread_keeper = None;
        [seeks.room.take(), seeks.thread.take()]
    });
    drop(retired);
    ui.set_timeline_seek_id(0);
    ui.set_thread_seek_id(0);
    forget_anchor(ui, Pane::Room);
    forget_anchor(ui, Pane::Thread);
}

fn forget_anchor(ui: &AppWindow, pane: Pane) {
    match pane {
        Pane::Room => {
            ui.set_timeline_anchor_index(-1);
            ui.set_timeline_anchor_event("".into());
        }
        Pane::Thread => {
            ui.set_thread_anchor_index(-1);
            ui.set_thread_anchor_event("".into());
        }
    }
}

/// Where the reader's top row (`event_id`, at `before` when last drawn) is
/// now, if rows were inserted or removed above it; `None` when it did not
/// move, is gone or was never reported.
fn anchor_index(model: &ModelRc<MessageItem>, event_id: &str, before: i32) -> Option<usize> {
    let before = usize::try_from(before).ok()?;
    if event_id.is_empty() || timeline_model::has_event_at(model, before, event_id) {
        return None;
    }
    timeline_model::index_of_event(model, event_id)
}

/// LIFECYCLE HOOK (live.rs), after the conversation's rows were published by
/// the live or focused feed under its own selection, feed, navigation and
/// visible-room checks; `gate` gives the publication's security generation.
/// The first rows of another account, room, feed or view forget the anchor
/// (its top row reports again); later rows that moved the reader's top row
/// start an anchor seek. A running seek keeps its own row, resolved again.
pub(crate) fn rows_published(
    ui: &AppWindow,
    selection: &AccountSelection,
    navigation: u64,
    feed: u64,
    gate: &SecurityGate,
) {
    let view = View::Room {
        feed,
        focused: ui.get_conversation_focused(),
    };
    let rows = ui.get_messages();
    let security = security_generation(gate);
    let publication = Publication {
        pane: Pane::Room,
        selection,
        navigation: Some(navigation),
        view,
        security,
    };
    published(ui, &rows, publication, gate);
}

/// LIFECYCLE HOOK (live.rs), after the open thread's replies were published
/// under the thread feed's selection, generation, account, room and root
/// checks; as `rows_published` for the thread, which also binds the thread's
/// keeper to this publication.
pub(crate) fn replies_published(
    ui: &AppWindow,
    selection: &AccountSelection,
    generation: u64,
    gate: &SecurityGate,
) {
    let view = View::Thread {
        root: ui.get_thread_root_id().to_string(),
        generation,
    };
    let replies = ui.get_thread_messages();
    let security = security_generation(gate);
    let publication = Publication {
        pane: Pane::Thread,
        selection,
        navigation: None,
        view,
        security,
    };
    published(ui, &replies, publication, gate);
}

/// What one publication of a pane's rows was made under.
struct Publication<'a> {
    pane: Pane,
    selection: &'a AccountSelection,
    navigation: Option<u64>,
    view: View,
    security: u64,
}

impl Publication<'_> {
    fn scope(&self, ui: &AppWindow) -> Scope {
        Scope {
            selection: self.selection.clone(),
            account: ui.get_active_account_local_id().to_string(),
            room: ui.get_active_room_id().to_string(),
            navigation: self.navigation,
            view: self.view.clone(),
        }
    }
}

fn published(
    ui: &AppWindow,
    rows: &ModelRc<MessageItem>,
    publication: Publication<'_>,
    gate: &SecurityGate,
) {
    let pane = publication.pane;
    if pane == Pane::Thread {
        let scope = publication.scope(ui);
        let check: Rc<dyn Fn(&AppWindow, &LiveUi) -> bool> =
            Rc::new(move |ui: &AppWindow, live: &LiveUi| scope.holds(ui, live));
        let keeper = (check, publication.security);
        with(|seeks| seeks.thread_keeper = Some(keeper));
    }
    let account = ui.get_active_account_local_id().to_string();
    let room = ui.get_active_room_id().to_string();
    let shown = (account, room, publication.view.clone());
    if !with(|seeks| seeks.republished(pane, shown)) {
        forget_anchor(ui, pane);
        return;
    }
    // A running seek keeps its row; one whose row is gone ends at its next
    // admission.
    let running = with(|seeks| {
        let seek = seeks.slot(pane).as_mut()?;
        let mut moved = false;
        if pane == Pane::Room
            && !timeline_model::has_event_at(rows, seek.index, &seek.event_id)
            && let Some(index) = timeline_model::index_of_event(rows, &seek.event_id)
        {
            seek.index = index;
            moved = true;
        }
        Some((seek.index, moved))
    });
    if let Some((index, moved)) = running {
        if pane == Pane::Room {
            ui.set_timeline_seek_index(i32::try_from(index).unwrap_or(i32::MAX));
            // Rows changed above the reader's top row too: the list then
            // draws another top row, which is this seek's, not the person's.
            let top = ui.get_timeline_anchor_event();
            let top_moved = usize::try_from(ui.get_timeline_anchor_index())
                .is_ok_and(|at| !timeline_model::has_event_at(rows, at, &top));
            if moved || top_moved {
                rows_changed(ui);
            }
        }
        return;
    }
    let (event, before, offset) = match pane {
        Pane::Room => (
            ui.get_timeline_anchor_event(),
            ui.get_timeline_anchor_index(),
            ui.get_timeline_anchor_offset(),
        ),
        Pane::Thread => (
            ui.get_thread_anchor_event(),
            ui.get_thread_anchor_index(),
            ui.get_thread_anchor_offset(),
        ),
    };
    // A thread reply that went is no anchor: the reply across the top
    // reports again.
    if pane == Pane::Thread
        && !event.is_empty()
        && timeline_model::index_of_event(rows, &event).is_none()
    {
        forget_anchor(ui, pane);
        return;
    }
    let Some(index) = anchor_index(rows, &event, before) else {
        return;
    };
    // Refused at its start like any seek: a protected surface is up, or a
    // security step is running.
    if !ui.get_viewport_seek_allowed() || !gate.ringing_allowed(publication.security) {
        return;
    }
    let seek = Seek {
        id: 0,
        pane,
        event_id: event.to_string(),
        index,
        scope: publication.scope(ui).check(),
        security: publication.security,
        owner: None,
        moves: 0,
    };
    start(ui, None, seek, Placement::Anchor(offset));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline_model::{tests::rich_fixture, update_shared};
    use morpheus_matrix::MessageInfo;

    fn rows(indices: impl IntoIterator<Item = usize>) -> Vec<Arc<MessageInfo>> {
        indices
            .into_iter()
            .map(|i| Arc::new(rich_fixture(i)))
            .collect()
    }

    /// The reader's top row is found again by its event after older rows are
    /// prepended or rows above it go, in a 10,000-row history of the mixed
    /// shapes the real ListView lays out at different heights; an append, an
    /// edit above it or an unmoved row starts nothing.
    #[test]
    fn the_anchor_follows_its_event_across_changes_above_it() {
        let model = update_shared(ModelRc::default(), rows(100..10_100));
        let anchor = "$fixture-4321:example.org";
        // Row 4,221 of the first snapshot: 4,321 - 100.
        assert!(timeline_model::has_event_at(&model, 4_221, anchor));
        assert_eq!(anchor_index(&model, anchor, 4_221), None, "not moved");

        // Load earlier: 100 older rows above everything.
        let model = update_shared(model, rows(0..10_100));
        assert_eq!(anchor_index(&model, anchor, 4_221), Some(4_321));

        // Newer rows below, and an edit of a row above, do not move it.
        let mut shown = rows(0..10_150);
        Arc::make_mut(&mut shown[17]).body = "Edited above the anchor".into();
        let model = update_shared(model, shown.clone());
        assert_eq!(anchor_index(&model, anchor, 4_321), None);

        // Two rows above it are removed.
        shown.remove(41);
        shown.remove(40);
        let model = update_shared(model, shown.clone());
        assert_eq!(anchor_index(&model, anchor, 4_321), Some(4_319));

        // Gone, never reported, or an empty event: nothing to keep.
        shown.retain(|row| row.event_id.as_deref() != Some(anchor));
        let model = update_shared(model, shown);
        assert_eq!(anchor_index(&model, anchor, 4_319), None);
        let other = "$fixture-9000:example.org";
        assert_eq!(anchor_index(&model, other, -1), None);
        assert_eq!(anchor_index(&model, "", 12), None);
        // 9,000 less the three removed rows before it.
        assert_eq!(anchor_index(&model, other, 20_000), Some(8_997));
    }

    fn seek(pane: Pane, event_id: &str) -> Seek {
        Seek {
            id: 0,
            pane,
            event_id: event_id.to_owned(),
            index: 0,
            scope: Box::new(|_: &AppWindow, _: &LiveUi| true),
            security: 0,
            owner: None,
            moves: 0,
        }
    }

    /// Ids are positive and never 0 (no seek), even after wrapping. A seek
    /// answers only to its own id while it is its pane's current one: a
    /// replaced, ended or late seek's report finds nothing, and a checked
    /// seek never overwrites a newer one of its pane.
    #[test]
    fn only_the_current_seek_of_a_pane_answers_to_its_id() {
        let mut seeks = Seeks::default();
        let (room, replaced) = seeks.begin(seek(Pane::Room, "$a:example.org"));
        assert_eq!((room, replaced.is_none()), (1, true));
        let (thread, _) = seeks.begin(seek(Pane::Thread, "$reply:example.org"));
        assert_eq!(thread, 2);
        let (newer, replaced) = seeks.begin(seek(Pane::Room, "$b:example.org"));
        assert_eq!(newer, 3);
        assert_eq!(
            replaced.map(|old| (old.id, old.event_id)),
            Some((1, String::from("$a:example.org")))
        );
        assert!(seeks.take(room).is_none(), "the replaced seek is gone");

        // Taken for its check, then a newer seek of the pane begins.
        let checked = seeks.take(newer).unwrap();
        assert_eq!(checked.event_id, "$b:example.org");
        let (newest, _) = seeks.begin(seek(Pane::Room, "$c:example.org"));
        seeks.restore(checked);
        assert!(seeks.take(newer).is_none(), "not over a newer seek");
        assert_eq!(seeks.take(newest).unwrap().event_id, "$c:example.org");
        assert_eq!(seeks.take(thread).unwrap().event_id, "$reply:example.org");
        assert!(seeks.take(thread).is_none(), "ended once");

        seeks.last_id = i32::MAX - 1;
        assert_eq!(seeks.next_id(), 1, "wraps past 0 and negative ids");
    }

    /// A seek makes at most 240 moves.
    #[test]
    fn a_seek_has_a_finite_number_of_moves() {
        let mut seek = seek(Pane::Room, "$a:example.org");
        assert_eq!((0..1_000).filter(|_| seek.spend_move()).count(), 240);
        assert!(!seek.spend_move());
    }

    /// Anchors carry over only between publications under the same account,
    /// room and view: another feed generation, a focused view or another
    /// thread starts afresh, and the two panes are kept apart.
    #[test]
    fn anchors_carry_over_only_within_one_feed_and_view() {
        let mut seeks = Seeks::default();
        let room = |feed, focused| {
            let view = View::Room { feed, focused };
            ("acct".to_owned(), "!room:example.org".to_owned(), view)
        };
        assert!(!seeks.republished(Pane::Room, room(7, false)), "first");
        assert!(seeks.republished(Pane::Room, room(7, false)));
        assert!(!seeks.republished(Pane::Room, room(8, false)), "new feed");
        assert!(!seeks.republished(Pane::Room, room(8, true)), "focused");
        assert!(seeks.republished(Pane::Room, room(8, true)));
        let thread = |root: &str| {
            let view = View::Thread {
                root: root.to_owned(),
                generation: 3,
            };
            ("acct".to_owned(), "!room:example.org".to_owned(), view)
        };
        assert!(!seeks.republished(Pane::Thread, thread("$a:example.org")));
        assert!(!seeks.republished(Pane::Thread, thread("$b:example.org")));
        assert!(seeks.republished(Pane::Thread, thread("$b:example.org")));
        assert!(seeks.republished(Pane::Room, room(8, true)), "per pane");
    }

    /// A seek's security generation is read before the gate's state: an idle
    /// gate gives its current generation, which a step begun since no longer
    /// allows, and a running step gives one that is never allowed.
    #[test]
    fn the_security_generation_ends_with_any_step_begun_since() {
        let gate = Arc::new(SecurityGate::default());
        let idle = security_generation(&gate);
        assert!(gate.ringing_allowed(idle));
        let step = gate
            .try_account_access()
            .expect("an idle gate begins a step");
        assert_eq!(security_generation(&gate), u64::MAX, "running: refused");
        assert!(!gate.ringing_allowed(idle), "a running step refuses");
        drop(step);
        assert!(!gate.ringing_allowed(idle), "begun since: refused");
        let after = security_generation(&gate);
        assert_ne!(after, idle);
        assert!(gate.ringing_allowed(after));
    }
}
