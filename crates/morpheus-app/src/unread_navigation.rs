//! Unread navigation of the open room (`morpheus_matrix::unread_navigation`):
//! the "New messages" divider after this account's exact fully-read marker,
//! an explicit "Jump to first unread", and "Mark as unread".
//!
//! Reading the marker is local and changes nothing: opening, painting or
//! focusing a room never sends a receipt, moves a marker, sends typing or
//! clears the unread mark. The divider shows above the first shown message
//! after the marker that somebody else sent (`UnreadPosition`: the exact
//! order of the open room's own timeline and local event cache, wherever the
//! marker is, also on an event that is not a row), only in the live
//! conversation. There is none when every later shown message is this
//! account's own, or when the marker's place is not known (not loaded, or a
//! reply in a thread whose first message is not loaded). Its position is
//! never guessed from times or counts.
//!
//! A jump captures the account selection, room, navigation epoch and
//! security generation at the click and is cancelled by a room or account
//! change, a lock or a detach. It is also withdrawn for good, never resumed,
//! as soon as a modal, Settings, the call page or an account step appears
//! (`withdraw-jump`, from the window's `unread-jump-allowed`). Its result,
//! and each of its short scroll retries, moves the view only while the same
//! account, room, navigation and security generation still hold. It pages back through bounded history only, and
//! says so when the marker is further back or no longer in history. It goes
//! to the row the divider is above, and says so when nothing newer from
//! anyone else is loaded or the marker is in a thread that is not loaded.
//!
//! Mark as unread writes the room's `m.marked_unread` flag once, for the
//! account and room it was chosen for; it syncs to this account's other
//! devices. A receipt per account and room is reserved before the write
//! and kept by the write task itself: a write that got no answer pauses
//! further unread-mark changes of that room for the rest of the process
//! (or until the account is removed), because a later read cannot prove the
//! pending write ended. The explicit Mark as read clears the flag as part of
//! that same action (see `clear_mark_for_read`).
//!
//! Every value shown is built on the UI thread from plain data.

use std::sync::{Arc, Mutex, MutexGuard, atomic::Ordering};

use morpheus_matrix::{
    AccountManager, AccountSelection, ActivityCancellation, MarkedUnreadError, MarkedUnreadOutcome,
    UnreadJump, UnreadJumpError, UnreadPosition, UnreadState, UnreadWriteGuard,
};
use slint::{ComponentHandle, Timer};
use tokio::runtime::Handle;

use crate::{AppWindow, UnreadNavigation, live::LiveUi};

const FOCUSED: &str = "Return to the latest messages first, then jump to the first unread one.";
const NO_MARKER: &str = "This room has no read position for your account yet.";
const NOT_IN_HISTORY: &str = "Your read position is no longer in this room's available history.";
const NOT_REACHED: &str = "Your read position is further back than one jump loads. \
     Load earlier messages, then jump again.";
const NOTHING_NEWER: &str = "Nothing newer from anyone else is loaded after your read position.";
const NOT_SHOWN: &str = "The first unread message was loaded but is not shown in this list.";
const IN_THREAD: &str = "Your read position belongs to a thread and cannot be placed in this \
     conversation.";
const UNPLACEABLE: &str = "Your read position is loaded but cannot be placed in this conversation.";
const MARKED: &str = "Marked as unread. Your other devices show it once they sync.";
const ALREADY_MARKED: &str = "This room is already marked as unread.";
const PAUSED_PENDING: &str = "An unread-mark change for this room is still being sent.";
const PAUSED_UNKNOWN: &str = "An earlier unread-mark change for this room got no answer and may \
     still arrive. Nothing more is sent for this room's unread mark.";
const TOO_MANY: &str = "Too many unread-mark changes are unresolved. Nothing was sent.";
/// How often, and how long, a jump waits for the first unread row to appear.
const JUMP_RETRY_MS: u64 = 150;
const JUMP_ATTEMPTS: u8 = 6;
/// One local UI recheck at a time while a security step holds the gate.
const MARKER_READMIT_MS: u64 = 150;
/// Unresolved unread-mark writes remembered at most.
pub(crate) const MAX_FLAG_RECEIPTS: usize = 32;
/// Rooms remembered as marked unread by an accepted write this process made,
/// before sync shows it. Past this, every explicit clear checks the server.
pub(crate) const MAX_ACCEPTED_MARKS: usize = 256;

/// An open unread-mark receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlagReceipt {
    /// Reserved and maybe handed over; no answer yet.
    Pending,
    /// No answer: kept for the process, never evicted.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Busy(FlagReceipt),
    Full,
}

struct Entry {
    id: u64,
    account: String,
    room: String,
    state: FlagReceipt,
}

/// Unread-mark receipts per account and exact room, apart from any view.
/// Only open receipts are kept, so the bound is enforced in `reserve`. It
/// holds identifiers only, never a client or store.
#[derive(Default)]
pub(crate) struct FlagReceipts {
    entries: Vec<Entry>,
    next_id: u64,
    /// Rooms this process set unread (the homeserver accepted it, or already
    /// had it) and has not cleared since: the local `is_marked_unread` may
    /// still say false until sync, and an explicit clear must not be skipped
    /// on it. Never evicted; when full, `accepted_overflow` makes every
    /// explicit clear check the server instead.
    accepted: Vec<(String, String)>,
    accepted_overflow: bool,
}

impl FlagReceipts {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 0,
            accepted: Vec::new(),
            accepted_overflow: false,
        }
    }

    /// A write's known result for the room: set (`true`) remembers that the
    /// server holds the mark, clear (`false`) forgets it.
    pub(crate) fn note_accepted(&mut self, account: &str, room: &str, unread: bool) {
        let at = self
            .accepted
            .iter()
            .position(|(held, kept)| held == account && kept == room);
        match (unread, at) {
            (true, None) if self.accepted.len() >= MAX_ACCEPTED_MARKS => {
                self.accepted_overflow = true;
            }
            (true, None) => self.accepted.push((account.to_owned(), room.to_owned())),
            (false, Some(index)) => {
                self.accepted.remove(index);
            }
            _ => {}
        }
    }

    /// Whether an explicit clear must go to the server even though the
    /// local flag says the room is not marked.
    pub(crate) fn maybe_accepted(&self, account: &str, room: &str) -> bool {
        self.accepted_overflow
            || self
                .accepted
                .iter()
                .any(|(held, kept)| held == account && kept == room)
    }

    pub(crate) fn reserve(&mut self, account: &str, room: &str) -> Result<u64, Refusal> {
        if let Some(state) = self.blocking(account, room) {
            return Err(Refusal::Busy(state));
        }
        if self.entries.len() >= MAX_FLAG_RECEIPTS {
            return Err(Refusal::Full);
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push(Entry {
            id: self.next_id,
            account: account.to_owned(),
            room: room.to_owned(),
            state: FlagReceipt::Pending,
        });
        Ok(self.next_id)
    }

    /// Settle with what the answer proves: no answer stays (Unknown);
    /// anything else (accepted, already set, refused, never sent) releases.
    /// A receipt that is gone is never made again.
    pub(crate) fn complete(&mut self, id: u64, unanswered: bool) {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        if unanswered {
            self.entries[index].state = FlagReceipt::Unknown;
        } else {
            self.entries.remove(index);
        }
    }

    pub(crate) fn blocking(&self, account: &str, room: &str) -> Option<FlagReceipt> {
        self.entries
            .iter()
            .find(|entry| entry.account == account && entry.room == room)
            .map(|entry| entry.state)
    }

    pub(crate) fn forget_account(&mut self, account: &str) {
        self.entries.retain(|entry| entry.account != account);
        self.accepted.retain(|(held, _)| held != account);
    }
}

static RECEIPTS: Mutex<FlagReceipts> = Mutex::new(FlagReceipts::new());

fn receipts() -> MutexGuard<'static, FlagReceipts> {
    RECEIPTS.lock().unwrap_or_else(|error| error.into_inner())
}

/// Account removal or logout: forget its unread-mark receipts.
pub(crate) fn forget_account(account: &str) {
    receipts().forget_account(account);
}

fn refusal_text(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Busy(FlagReceipt::Pending) => PAUSED_PENDING,
        Refusal::Busy(FlagReceipt::Unknown) => PAUSED_UNKNOWN,
        Refusal::Full => TOO_MANY,
    }
}

/// Whether a write's result is a missing answer (the only kept receipt).
fn unanswered(result: &Result<MarkedUnreadOutcome, MarkedUnreadError>) -> bool {
    matches!(result, Err(MarkedUnreadError::Unknown))
}

/// Stops a write that has not been handed over yet.
struct Guard(ActivityCancellation);

impl UnreadWriteGuard for Guard {
    fn stopped(&self) -> bool {
        self.0.is_cancelled()
    }

    fn wait_stopped(&self) -> impl std::future::Future<Output = ()> + Send {
        self.0.cancelled()
    }
}

/// Whether an explicit clear must write: the local flag says marked, or
/// this process's own accepted mark may not have synced yet.
pub(crate) fn clear_needed(locally_marked: bool, maybe_accepted: bool) -> bool {
    locally_marked || maybe_accepted
}

/// The explicit Mark as read also clears the room's unread mark, as part
/// of that same user action and only when the room is (or may be) marked:
/// the one owned, lossless, bounded write of the flag (the SDK's receipt
/// path no longer touches it). `admitted` is the Mark as read click's own
/// fence (its selection, navigation and security generation); it is checked
/// before and after each wait and again right before the hand-off, never
/// re-captured. Returns a fixed note to add to the Mark as read result, or
/// `None` when the room was not marked.
pub(crate) async fn clear_mark_for_read(
    manager: &AccountManager,
    selection: &AccountSelection,
    room: &str,
    cancellation: &ActivityCancellation,
    admitted: &(dyn Fn() -> bool + Send + Sync),
) -> Option<&'static str> {
    if cancellation.is_cancelled() || !admitted() {
        return None;
    }
    let state = manager.unread_state_selected(selection, room).await.ok()?;
    let account = selection.account_id();
    let maybe_accepted = receipts().maybe_accepted(account, room);
    if !clear_needed(state.marked_unread, maybe_accepted) {
        return None;
    }
    if cancellation.is_cancelled() || !admitted() {
        return Some(NOT_CLEARED);
    }
    let receipt = match receipts().reserve(account, room) {
        Ok(receipt) => receipt,
        Err(refusal) => return Some(refusal_text(refusal)),
    };
    let guard = Guard(cancellation.clone());
    let result = manager
        .set_marked_unread_selected(selection, room, false, admitted, &guard)
        .await;
    {
        let mut book = receipts();
        book.complete(receipt, unanswered(&result));
        if result.is_ok() {
            book.note_accepted(account, room, false);
        }
    }
    Some(match result {
        Ok(_) => " The unread mark was cleared.",
        Err(MarkedUnreadError::Unknown) => {
            " Clearing the unread mark got no answer; it was not sent again."
        }
        Err(_) => NOT_CLEARED,
    })
}

const NOT_CLEARED: &str = " The unread mark could not be cleared.";

/// Withdraw a held jump now, synchronously, before a surface opens over the
/// conversation (each Rust opener of such a surface calls this first; the
/// window's own entries call `withdraw-jump` directly). Never resumed.
pub(crate) fn withdraw_jump(ui: &AppWindow) {
    ui.global::<UnreadNavigation>().invoke_withdraw_jump();
}

struct Jump {
    op: u64,
    cancellation: ActivityCancellation,
}

/// What a finished jump scrolls to, and what it must still find shown.
struct Target {
    op: u64,
    selection: AccountSelection,
    account: String,
    room: String,
    navigation: u64,
    security: u64,
    /// The first shown message after the marker that somebody else sent.
    first: String,
}

/// The local observer's scope, separate from a jump or write's authority.
#[derive(Clone)]
struct MarkerScope {
    reading: u64,
    selection: AccountSelection,
    account: String,
    room: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerPublication {
    Retired,
    Current,
    Readmit,
}

#[derive(Default)]
struct State {
    /// The jump generation: a reset or a withdrawal moves it on, so a held
    /// result or scroll retry of an earlier jump never acts.
    op: u64,
    jump: Option<Jump>,
    /// Mark as unread's own generation (separate, so a mark never ends a
    /// jump and a jump never ends a mark).
    marks: u64,
    marking: Option<(u64, ActivityCancellation)>,
    reading: u64,
    /// The open room's local read-marker watch (`context-changed` starts it,
    /// a reset or shutdown ends it).
    watch: Option<ActivityCancellation>,
    /// The window closed: nothing new starts.
    stopping: bool,
}

impl State {
    fn begin_marker_watch(&mut self, watch: ActivityCancellation) -> Option<u64> {
        if self.stopping {
            return None;
        }
        self.reading = self.reading.wrapping_add(1);
        if let Some(previous) = self.watch.replace(watch) {
            previous.cancel();
        }
        Some(self.reading)
    }

    /// Retire one observation once. A fresh local read may be admitted for
    /// the same context; this never changes jump or mark authority.
    fn marker_publication(
        &mut self,
        reading: u64,
        context_current: bool,
        admitted: bool,
    ) -> MarkerPublication {
        if self.stopping || self.reading != reading || self.watch.is_none() {
            return MarkerPublication::Retired;
        }
        if !context_current || !admitted {
            if let Some(watch) = self.watch.take() {
                watch.cancel();
            }
            return if context_current {
                MarkerPublication::Readmit
            } else {
                MarkerPublication::Retired
            };
        }
        MarkerPublication::Current
    }

    fn marker_readmission_current(&self, reading: u64, context_current: bool) -> bool {
        !self.stopping && self.reading == reading && self.watch.is_none() && context_current
    }
}

pub(crate) struct UnreadNavigationUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    state: Mutex<State>,
}

/// The view's own reset for an account change, a lock or a detach.
pub(crate) fn reset_window(ui: &AppWindow) {
    ui.global::<UnreadNavigation>().invoke_reset();
}

/// The row the divider goes above: the first shown later message that
/// somebody else sent, when the marker's place is exact. None otherwise:
/// unreadable, not loaded, a reply in a thread whose first message is not
/// loaded, or only this account's own messages after it.
fn divider_row(position: Option<UnreadPosition>) -> String {
    match position {
        Some(UnreadPosition::Placed { first: Some(first) }) => first,
        _ => String::new(),
    }
}

fn clear_presentation(ui: &AppWindow) {
    let global = ui.global::<UnreadNavigation>();
    global.set_first_unread_event_id("".into());
    global.set_can_jump(false);
    global.set_jumping(false);
    global.set_marking(false);
    global.set_notice("".into());
}

impl UnreadNavigationUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            state: Mutex::new(State::default()),
        });
        let global = ui.global::<UnreadNavigation>();
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.refresh(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_reset(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_jump(move || {
            if let Some(ui) = weak.upgrade() {
                this.jump(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_withdraw_jump(move || {
            if let Some(ui) = weak.upgrade() {
                this.withdraw_jump(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_mark_unread(move |room| {
            if let Some(ui) = weak.upgrade() {
                this.mark_unread(&ui, room.as_str());
            }
        });
        owner
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn live_here(&self, ui: &AppWindow) -> bool {
        ui.get_live_session() && self.live.is_live.load(Ordering::Acquire)
    }

    /// Synchronous: a running jump and the marker watch end, and nothing of
    /// the previous room or account stays shown. A running Mark as unread is
    /// left to finish: the user chose that room, and a write already handed
    /// over must keep its own answer rather than become Unknown (an account
    /// change still ends it in the core, as Unknown once handed over).
    fn reset(&self, ui: &AppWindow) {
        let mut state = self.lock();
        state.op = state.op.wrapping_add(1);
        state.reading = state.reading.wrapping_add(1);
        if let Some(jump) = state.jump.take() {
            jump.cancellation.cancel();
        }
        if let Some(watch) = state.watch.take() {
            watch.cancel();
        }
        let marking = state.marking.is_some();
        drop(state);
        clear_presentation(ui);
        ui.global::<UnreadNavigation>().set_marking(marking);
    }

    /// The window closed (before main waits on other cleanup): nothing new
    /// starts; the jump and watch end; a mark not yet handed over is
    /// withdrawn and sends nothing. One already handed over keeps its own
    /// bounded answer (Unknown if none); the cancel does not undo it.
    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        {
            let mut state = self.lock();
            state.stopping = true;
            if let Some((_, cancellation)) = state.marking.as_ref() {
                cancellation.cancel();
            }
        }
        self.reset(ui);
    }

    /// A modal, Settings, the call page or an account step appeared: the
    /// jump in progress, its result and its scroll retries end for good.
    /// Nothing is resumed when that surface closes; the user jumps again.
    /// The marker, the divider and a running mark are left as they are.
    fn withdraw_jump(&self, ui: &AppWindow) {
        let mut state = self.lock();
        state.op = state.op.wrapping_add(1);
        let withdrawn = state.jump.take();
        drop(state);
        if let Some(jump) = withdrawn {
            jump.cancellation.cancel();
        }
        ui.global::<UnreadNavigation>().set_jumping(false);
    }

    /// Show the open room's marker and keep it current: the local read
    /// marker as sync stores it, read now and again whenever a stored update
    /// changes it (another device's read, this client's explicit Mark as
    /// read once synced), so the divider and the jump follow real reads even
    /// when no message arrives. Local only; nothing is sent. Each read keeps
    /// its original navigation and security generation. If either expires
    /// while the exact account/room/selection is still shown, the observation
    /// is withdrawn and a fresh watch reads the current local marker after
    /// the gate is idle. No old jump or mark is re-admitted.
    fn refresh(self: &Arc<Self>, ui: &AppWindow) {
        let account = ui.get_active_account_local_id().to_string();
        let room = ui.get_active_room_id().to_string();
        let global = ui.global::<UnreadNavigation>();
        let selection = (self.live_here(ui) && !account.is_empty() && !room.is_empty())
            .then(|| self.live.manager.account_selection(&account))
            .flatten();
        let Some(selection) = selection else {
            if let Some(watch) = self.lock().watch.take() {
                watch.cancel();
            }
            global.set_first_unread_event_id("".into());
            global.set_can_jump(false);
            return;
        };
        let navigation = self.live.current_navigation_epoch();
        let security = self.live.security_gate.ringing_generation();
        let watch = ActivityCancellation::new();
        let Some(reading) = self.lock().begin_marker_watch(watch.clone()) else {
            return;
        };
        let scope = MarkerScope {
            reading,
            selection: selection.clone(),
            account,
            room: room.clone(),
        };
        let this = self.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let mut seen: Option<UnreadState> = None;
            loop {
                if watch.is_cancelled() {
                    break;
                }
                let next = manager
                    .next_unread_state_selected(&selection, &room, seen.as_ref(), watch.cancelled())
                    .await;
                if watch.is_cancelled() {
                    break;
                }
                let (shown_state, keep) = match next {
                    Ok(Some(state)) => (Some(state), true),
                    Ok(None) => break,
                    // Unreadable now: show no marker, and stop watching.
                    Err(_) => (None, false),
                };
                // Where the marker falls among the rows: local and exact
                // order only; read again with each list change (the
                // window's `unread-scope`). Unknown places no divider.
                let mut position = None;
                if let Some(state) = &shown_state {
                    let marker = state.fully_read.as_deref();
                    let placed = manager
                        .unread_position_selected(&selection, &room, marker)
                        .await;
                    position = placed.ok();
                }
                if watch.is_cancelled() {
                    break;
                }
                let divider = divider_row(position);
                let this_ui = this.clone();
                let scope_ui = scope.clone();
                let watch_ui = watch.clone();
                let published = shown_state.clone();
                let _ = this.ui.clone().upgrade_in_event_loop(move |ui| {
                    let context_current = this_ui.marker_context_current(&ui, &scope_ui);
                    let admitted = this_ui.live.navigation_is_current(navigation)
                        && this_ui.live.security_gate.ringing_allowed(security);
                    let publication = this_ui.lock().marker_publication(
                        scope_ui.reading,
                        context_current,
                        admitted,
                    );
                    match publication {
                        MarkerPublication::Retired => {
                            watch_ui.cancel();
                            return;
                        }
                        MarkerPublication::Readmit => {
                            let global = ui.global::<UnreadNavigation>();
                            global.set_first_unread_event_id("".into());
                            global.set_can_jump(false);
                            this_ui.readmit_marker_watch(&ui, scope_ui);
                            return;
                        }
                        MarkerPublication::Current => {}
                    }
                    let global = ui.global::<UnreadNavigation>();
                    let marker = published.and_then(|state| state.fully_read);
                    global.set_can_jump(marker.is_some());
                    global.set_first_unread_event_id(divider.into());
                });
                if !keep {
                    break;
                }
                seen = shown_state;
            }
        });
    }

    fn marker_context_current(&self, ui: &AppWindow, scope: &MarkerScope) -> bool {
        self.live_here(ui)
            && scope.selection.is_current()
            && ui.get_active_account_local_id().as_str() == scope.account
            && ui.get_active_room_id().as_str() == scope.room
    }

    /// A fresh local observation, never the old queued value. At most one
    /// timer belongs to this retired watch; reset, a newer watch or shutdown
    /// ends it. Waiting for the gate performs no account/store/network work.
    fn readmit_marker_watch(self: &Arc<Self>, ui: &AppWindow, scope: MarkerScope) {
        if !self
            .lock()
            .marker_readmission_current(scope.reading, self.marker_context_current(ui, &scope))
        {
            return;
        }
        if !self.live.security_gate.blocked() {
            self.refresh(ui);
            return;
        }
        let this = self.clone();
        let weak = ui.as_weak();
        Timer::single_shot(
            std::time::Duration::from_millis(MARKER_READMIT_MS),
            move || {
                if let Some(ui) = weak.upgrade() {
                    this.readmit_marker_watch(&ui, scope);
                }
            },
        );
    }

    fn jump(self: &Arc<Self>, ui: &AppWindow) {
        // The click's original captures, before any admission check. No
        // later result or retry adopts a newer generation or navigation.
        let security = self.live.security_gate.ringing_generation();
        let navigation = self.live.current_navigation_epoch();
        if self.lock().stopping {
            return;
        }
        let account = ui.get_active_account_local_id().to_string();
        let room = ui.get_active_room_id().to_string();
        if !self.live_here(ui)
            || account.is_empty()
            || room.is_empty()
            || ui.get_page() != 0
            || !ui.get_unread_jump_allowed()
            || self.live.security_gate.blocked()
        {
            return;
        }
        let global = ui.global::<UnreadNavigation>();
        if ui.get_conversation_focused() {
            global.set_notice(FOCUSED.into());
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account) else {
            return;
        };
        // A jump already running refuses this one before anything is retired.
        if self.lock().jump.is_some() {
            return;
        }
        // Admitted, the jump takes the conversation over: a call bar click
        // still waiting for the call controller, a shown link's read, join and
        // held follow-up, and identity details' focus-return intent end now,
        // synchronously, before the jump's own flags. Visible owners refused
        // above (unread-jump-allowed), so none of them is closed here.
        crate::withdraw_call_bar(ui);
        crate::matrix_links::clear(ui);
        crate::identity_warnings::withdraw(ui);
        let cancellation = ActivityCancellation::new();
        let op = {
            let mut state = self.lock();
            if state.jump.is_some() {
                return;
            }
            state.op = state.op.wrapping_add(1);
            state.jump = Some(Jump {
                op: state.op,
                cancellation: cancellation.clone(),
            });
            state.op
        };
        global.set_jumping(true);
        global.set_notice("".into());
        let this = self.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let jumped = manager
                .jump_to_fully_read_selected(&selection, &room, cancellation.cancelled())
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                {
                    let mut state = this.lock();
                    if !state.jump.as_ref().is_some_and(|jump| jump.op == op) {
                        return;
                    }
                    state.jump = None;
                }
                let global = ui.global::<UnreadNavigation>();
                global.set_jumping(false);
                if !this.still_shown(&ui, &selection, &account, &room, navigation, security) {
                    return;
                }
                let notice = match jumped {
                    Ok(UnreadJump::Found { first }) => {
                        // The divider follows what the jump loaded, even
                        // when no new row appeared in the list.
                        this.refresh(&ui);
                        let target = Target {
                            op: this.lock().op,
                            selection,
                            account,
                            room,
                            navigation,
                            security,
                            first,
                        };
                        this.scroll_after(&ui, target, JUMP_ATTEMPTS);
                        return;
                    }
                    Ok(UnreadJump::NoMarker) => NO_MARKER,
                    Ok(UnreadJump::NothingNewer) => NOTHING_NEWER,
                    Ok(UnreadJump::InThread) => IN_THREAD,
                    Ok(UnreadJump::Unplaceable) => UNPLACEABLE,
                    Ok(UnreadJump::NotInHistory) => NOT_IN_HISTORY,
                    Ok(UnreadJump::NotReached) => NOT_REACHED,
                    Err(UnreadJumpError::Cancelled | UnreadJumpError::AccountChanged) => return,
                    Err(error) => {
                        global.set_notice(error.to_string().into());
                        return;
                    }
                };
                global.set_notice(notice.into());
            });
        });
    }

    /// Whether a result may still move this view: the same account, room,
    /// selection and navigation, on the live conversation with no modal over
    /// it, and no security step begun since the click (`security`). Never
    /// re-captured: a later epoch or generation is not adopted.
    fn still_shown(
        &self,
        ui: &AppWindow,
        selection: &AccountSelection,
        account: &str,
        room: &str,
        navigation: u64,
        security: u64,
    ) -> bool {
        self.live_here(ui)
            && selection.is_current()
            && ui.get_active_account_local_id().as_str() == account
            && ui.get_active_room_id().as_str() == room
            && self.live.navigation_is_current(navigation)
            && ui.get_page() == 0
            && !ui.get_conversation_focused()
            && ui.get_unread_jump_allowed()
            && self.live.security_gate.ringing_allowed(security)
    }

    /// Scroll to the first shown later message that somebody else sent (the
    /// row the divider is above) once it is in the list, through the
    /// conversation's own jump (no row is rebuilt and no receipt is sent).
    /// Retries a few short times, then says so.
    fn scroll_after(self: &Arc<Self>, ui: &AppWindow, target: Target, attempts: u8) {
        if self.lock().op != target.op
            || !self.still_shown(
                ui,
                &target.selection,
                &target.account,
                &target.room,
                target.navigation,
                target.security,
            )
        {
            return;
        }
        let global = ui.global::<UnreadNavigation>();
        let messages = ui.get_messages();
        let Some(index) = crate::timeline_model::index_of_event(&messages, &target.first) else {
            if attempts > 1 {
                let this = self.clone();
                let weak = ui.as_weak();
                let wait = std::time::Duration::from_millis(JUMP_RETRY_MS);
                Timer::single_shot(wait, move || {
                    if let Some(ui) = weak.upgrade() {
                        this.scroll_after(&ui, target, attempts - 1);
                    }
                });
            } else {
                global.set_notice(NOT_SHOWN.into());
            }
            return;
        };
        ui.set_timeline_jump_event_id(target.first.as_str().into());
        ui.set_timeline_jump_index(i32::try_from(index).unwrap_or(i32::MAX));
        ui.set_timeline_jump_request(ui.get_timeline_jump_request().wrapping_add(1));
    }

    fn mark_unread(self: &Arc<Self>, ui: &AppWindow, room: &str) {
        // The click's original fence, captured FIRST: a security step that
        // begins after this shows as blocked just below or as a newer
        // generation at every later check, through the hand-off. It is never
        // captured again.
        let gate = self.live.security_gate.clone();
        let generation = gate.ringing_generation();
        let navigation = self.live.current_navigation_epoch();
        let account = ui.get_active_account_local_id().to_string();
        if !self.live_here(ui)
            || account.is_empty()
            || room.is_empty()
            || self.live.security_gate.blocked()
        {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account) else {
            return;
        };
        let global = ui.global::<UnreadNavigation>();
        {
            let state = self.lock();
            if state.marking.is_some() || state.stopping {
                return;
            }
        }
        let receipt = match receipts().reserve(&account, room) {
            Ok(receipt) => receipt,
            Err(refusal) => {
                global.set_notice(refusal_text(refusal).into());
                return;
            }
        };
        // The core checks this fence before and after each wait and right
        // before the hand-off.
        let live = self.live.clone();
        let admitted =
            move || gate.ringing_allowed(generation) && live.navigation_is_current(navigation);
        let cancellation = ActivityCancellation::new();
        let op = {
            let mut state = self.lock();
            state.marks = state.marks.wrapping_add(1);
            state.marking = Some((state.marks, cancellation.clone()));
            state.marks
        };
        global.set_marking(true);
        global.set_notice("".into());
        let this = self.clone();
        let room = room.to_owned();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let guard = Guard(cancellation);
            let result = manager
                .set_marked_unread_selected(&selection, &room, true, &admitted, &guard)
                .await;
            // Settled here, whatever the view does. An accepted (or already
            // stored) mark is remembered until a clear: the local flag may
            // still read false until sync, and Mark as read must not skip it.
            {
                let mut book = receipts();
                book.complete(receipt, unanswered(&result));
                if result.is_ok() {
                    book.note_accepted(selection.account_id(), &room, true);
                }
            }
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                {
                    let mut state = this.lock();
                    if !state.marking.as_ref().is_some_and(|(id, _)| *id == op) {
                        return;
                    }
                    state.marking = None;
                }
                let global = ui.global::<UnreadNavigation>();
                global.set_marking(false);
                if !selection.is_current() || ui.get_active_account_local_id().as_str() != account {
                    return;
                }
                let notice = match result {
                    Ok(MarkedUnreadOutcome::Accepted) => MARKED.to_owned(),
                    Ok(MarkedUnreadOutcome::AlreadySet) => ALREADY_MARKED.to_owned(),
                    Err(MarkedUnreadError::AccountChanged) => return,
                    Err(error) => error.to_string(),
                };
                global.set_notice(notice.into());
            });
        });
    }
}

#[cfg(test)]
mod tests;
