//! Native message search over the account's encrypted on-device index: the
//! open-room and all-room scopes, debounced and cancellable queries, bounded
//! incremental pages, and opening a hit in its exact room, event and thread.
//!
//! Every request captures the account selection, scope and query when it is
//! issued. Every publication re-checks that selection, the visible account,
//! the search operation and, for the room scope, the open room, so a switched,
//! removed or re-selected account (A -> B -> A) never receives a departed
//! request's results or context. Opening a hit reuses the window's own room
//! and thread navigation, so composer, room and thread drafts are kept by the
//! same draft store as any other navigation. Nothing here logs a query, a
//! message or an account identifier.
//!
//! The same navigation is the app's seam for any activation that names a
//! conversation, such as a notification: see `open_conversation`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use morpheus_matrix::{
    AccountManager, AccountSelection, HistoryReveal, MatrixCoreError, MessageInfo,
    MessageSearchPage, MessageSearchResult,
};
use morpheus_matrix::{AttachmentFilter, SearchFilters};
use slint::{ComponentHandle, Model, ModelRc, Timer, VecModel, Weak};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    AppWindow, MessageItem, MessageSearchItem,
    live::{LiveUi, NavigationAuthority},
    timeline_viewport::{self, Ended, Owner as SeekOwner, Pane, Reveal},
};

/// Hits requested per page.
const PAGE_SIZE: usize = 25;
/// Rows kept for one query; refining the query beats paging further.
const MAX_RESULTS: usize = 200;
/// Typing pause before a query runs. Each keystroke cancels the last request.
const DEBOUNCE: Duration = Duration::from_millis(280);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(20);
/// Events loaded around a hit, and how many of them are shown before and
/// after it, so the hit itself stays near the top of the context list.
const CONTEXT_EVENTS: u16 = 6;
const CONTEXT_BEFORE: usize = 2;
const CONTEXT_AFTER: usize = 5;
const MAX_QUERY_BYTES: usize = 256;
/// Thread history pages (of 50 events) loaded at most to reach an older
/// thread reply.
const REVEAL_PAGES: u8 = 4;
const REVEAL_TIMEOUT: Duration = Duration::from_secs(30);
const FOCUS_TIMEOUT: Duration = Duration::from_secs(30);
/// Feeds apply loaded rows asynchronously; a row is looked for this often,
/// this many times, before the conversation around it is opened instead.
/// The opt-in live controller suite steps Slint's mock clock by these.
pub(crate) const JUMP_RETRY: Duration = Duration::from_millis(120);
const JUMP_ATTEMPTS: u8 = 10;
pub(crate) const ARRIVAL_ATTEMPTS: u8 = 5;
/// Events per Load newer page of a focused conversation view.
const NEWER_PAGE: u16 = 50;
/// How long a target in another account waits for that account to become
/// visible (an app password or a call confirmation may come first).
const PENDING_TARGET_LIFETIME: Duration = Duration::from_secs(120);

// Fixed wording only: SDK error text is never shown, since it could echo
// the query or an identifier.
const NOT_SIGNED_IN: &str = "Sign in to search messages";
const NO_ROOM: &str = "Open a room, or search all rooms";
const INVALID_QUERY: &str = "Search for up to 256 bytes of plain text";
const ROOM_GONE: &str = "This room can no longer be searched";
const SEARCH_FAILED: &str = "Message search failed on this device. Try again.";
const SEARCH_SLOW: &str = "Message search timed out. Try again.";
const ROOM_NOT_OPENED: &str = "This room could not be opened. It may no longer be joined.";
const CONTEXT_FAILED: &str = "Nearby messages could not be loaded.";
const CONTEXT_SLOW: &str = "Nearby messages took too long to load.";
const CONTEXT_MISSING: &str = "The matched message is no longer available in this conversation.";
const NOT_SHOWN: &str = "This message could not be shown in the conversation; it is shown here.";
const THREAD_NOT_SHOWN: &str = "This reply could not be shown in its thread; it is shown here.";
// An activation's loaded target that its seek could not bring into view.
const NOT_IN_VIEW: &str = "The message was opened but could not be brought into view.";
const THREAD_OLDER: &str =
    "This reply is older than the loaded thread. Use Load earlier replies to reach it.";
const FINDING: &str = "Loading earlier replies to reach this one…";
const REVEAL_FAILED: &str = "Earlier replies could not be loaded to reach this one.";
const FOCUSING: &str = "Opening the conversation around this message…";
const FOCUS_FAILED: &str =
    "The conversation around this message could not be opened, so it is shown here.";
const NEWER_FAILED: &str = "Could not load newer messages. Check sync and retry.";
const KEEP_COMPOSER: &str =
    "Save or cancel the edit in this room before opening another conversation.";
// Keep in step with MAX_RESULTS.
const RESULT_LIMIT: &str = "showing the first 200; refine the search to narrow them";
/// A date filter is exactly "YYYY-MM-DD".
const DATE_CHARS: usize = 10;
const FILTER_SENDER: &str = "Enter the sender's full user ID, like @name:example.org";
const FILTER_DATE: &str = "Enter dates as YYYY-MM-DD, from 1970 on";
const FILTER_RANGE: &str = "The date range ends before it starts";
const FILTER_NEEDS_WORDS: &str =
    "An attachment filter needs words, a sender or dates to search for";

/// A result to present, or fixed failure text. `Err(None)` means the account
/// selection ended and nothing is shown.
type Shown<T> = Result<T, Option<&'static str>>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    Room(String),
    All,
}

/// The identity of one query: the account selection, window state and filters
/// it was issued for. Publication requires every part of it to be unchanged.
#[derive(Clone)]
struct Ticket {
    operation: i32,
    selection: AccountSelection,
    account: String,
    scope: Scope,
    query: String,
    filters: SearchFilters,
}

impl Ticket {
    fn current(&self, window: &AppWindow) -> bool {
        self.selection.is_current()
            && window.get_live_session()
            && window.get_active_account_local_id().as_str() == self.account
            && window.get_room_search_operation() == self.operation
            && match &self.scope {
                Scope::Room(room) => window.get_active_room_id().as_str() == room,
                Scope::All => true,
            }
    }

    fn matches(&self, query: &str, scope: Option<&Scope>, filters: &SearchFilters) -> bool {
        self.query == query && Some(&self.scope) == scope && self.filters == *filters
    }
}

/// The query whose results are shown, its next index cursor and the
/// (room, event) identities already listed, bounded by MAX_RESULTS.
struct Request {
    ticket: Ticket,
    next_offset: Option<usize>,
    unavailable: usize,
    filtered: usize,
    seen: HashSet<(String, String)>,
}

/// Where an activation asks to open: one account's room and, optionally,
/// one event in it and the thread that event belongs to. IDs route only;
/// none is displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConversationTarget {
    /// Opaque local account ID, as in `AccountOption.local-id`.
    pub(crate) account_id: String,
    pub(crate) room_id: String,
    /// The event to show; `None` opens the room (or thread) only.
    pub(crate) event_id: Option<String>,
    /// The root of the event's thread; the thread panel opens on it.
    pub(crate) thread_root: Option<String>,
}

/// One target being opened, with the IDs that route it. An empty event ID
/// or thread root means none.
#[derive(Clone)]
struct Hit {
    room_id: String,
    event_id: String,
    thread_root: String,
}

impl Hit {
    fn selected(&self, window: &AppWindow) -> bool {
        window.get_room_search_selected_room_id().as_str() == self.room_id
            && window.get_room_search_selected_event_id().as_str() == self.event_id
    }
}

/// Who opened a target, which decides what keeps the open current.
#[derive(Clone)]
enum Owner {
    /// A listed search hit: its query, and the context operation of the
    /// hit; the hit must also stay selected.
    Search { ticket: Ticket, operation: i32 },
    /// An activation such as a notification: the account selection it was
    /// started under and its activation generation, which any newer
    /// activation replaces.
    Activation {
        selection: AccountSelection,
        account: String,
        generation: u64,
    },
}

/// One open request: its owner, which every later step re-checks, the
/// target, the authority of the navigation it started, which a newer open
/// withdraws (and, for search, a newer query, scope or reset), and the
/// navigation epoch it opened under. Backend steps take the owner's own
/// selection and this epoch, never ones looked up again later. Its viewport
/// seeks also keep the security generation taken when it opened.
#[derive(Clone)]
struct Opening {
    owner: Owner,
    hit: Hit,
    authority: NavigationAuthority,
    navigation: u64,
    security: u64,
}

impl Opening {
    fn current(&self, window: &AppWindow) -> bool {
        match &self.owner {
            Owner::Search { ticket, operation } => {
                ticket.current(window)
                    && window.get_room_search_context_operation() == *operation
                    && self.hit.selected(window)
            }
            Owner::Activation {
                selection,
                account,
                generation,
            } => {
                selection.is_current()
                    && window.get_live_session()
                    && window.get_active_account_local_id().as_str() == account
                    && activation_is_current(*generation)
            }
        }
    }

    /// The target's room is the open one.
    fn here(&self, window: &AppWindow) -> bool {
        window.get_active_room_id().as_str() == self.hit.room_id
    }

    /// Whether a delayed step (a scroll retry, the focused view's result)
    /// may still show anything: the owner is current, its room is open, its
    /// authority admits it (its own conditions, and no surface it names up,
    /// which would withdraw it for good) and this open's own navigation is
    /// still the current one. A newer navigation, even of the same room or a
    /// Space whose snapshot is still held, withdraws this open alone; no
    /// epoch is taken or advanced.
    fn admitted(&self, window: &AppWindow, live: &LiveUi) -> bool {
        if !self.current(window) || !self.here(window) || !self.authority.admits(window) {
            return false;
        }
        if live.navigation_is_current(self.navigation) {
            return true;
        }
        self.authority.withdraw();
        false
    }

    fn searching(&self) -> bool {
        matches!(self.owner, Owner::Search { .. })
    }

    fn selection(&self) -> &AccountSelection {
        match &self.owner {
            Owner::Search { ticket, .. } => &ticket.selection,
            Owner::Activation { selection, .. } => selection,
        }
    }

    /// Progress text in the search panel; an activation shows none.
    fn notice(&self, window: &AppWindow, text: &str) {
        if self.searching() {
            window.set_room_search_context_notice(text.into());
        }
    }

    /// A loaded target could not be brought into view (its seek ran out or
    /// its row went): say so in the search panel, or as an activation's
    /// session error.
    fn not_in_view(&self, window: &AppWindow, notice: &'static str) {
        if self.searching() {
            window.set_room_search_context_notice(notice.into());
        } else {
            window.set_session_error(NOT_IN_VIEW.into());
        }
    }

    /// This opening as the owner of a seek: its own admission before every
    /// move, and `notice` (or the activation's error) if the target could
    /// not be brought into view. A scrolled or placed seek says nothing.
    fn seek_owner(&self, notice: &'static str) -> SeekOwner {
        let (admitting, telling) = (self.clone(), self.clone());
        SeekOwner {
            admits: Box::new(move |window: &AppWindow, live: &LiveUi| {
                admitting.admitted(window, live)
            }),
            ended: Box::new(move |window: &AppWindow, ended: Ended| {
                if ended == Ended::Unavailable {
                    telling.not_in_view(window, notice);
                }
            }),
        }
    }

    /// The room did not open: end the search's loading state, or show an
    /// activation's failure as a session error.
    fn not_opened(&self, window: &AppWindow, error: Option<&'static str>) {
        if self.searching() {
            window.set_room_search_context_loading(false);
            if let Some(error) = error {
                window.set_room_search_context_error(error.into());
            }
        } else if let Some(error) = error {
            window.set_session_error(error.into());
        }
    }
}

/// A target in another account, waiting for the window's protected switch
/// (app password, active call) to make that account visible.
struct PendingTarget {
    target: ConversationTarget,
    live: Arc<LiveUi>,
    runtime: Handle,
    generation: u64,
    expires: Instant,
}

/// Search state lives on the UI thread; Tokio tasks only return owned
/// results through the event loop, where this state is read again.
#[derive(Default)]
struct Controller {
    request: Option<Request>,
    query_task: Option<JoinHandle<()>>,
    /// Clears the search when the shown request's account selection ends.
    revoke_task: Option<JoinHandle<()>>,
    context_task: Option<JoinHandle<()>>,
    reveal_task: Option<JoinHandle<()>>,
    /// The navigation of the latest open, and whether search owns it. It is
    /// withdrawn rather than aborted: an abort could stop it between its
    /// commit and rollback.
    navigation: Option<(NavigationAuthority, bool)>,
    /// Generation of the latest activation (`open_conversation`).
    activation: u64,
    /// The latest Load newer request of a focused view.
    newer_request: u64,
    pending: Option<PendingTarget>,
}

impl Controller {
    /// End the work of the opened hit: its nearby context and, when search
    /// owns the open, its thread reveal and room or focus navigation. An
    /// activation's open is left to finish.
    fn abort_opening(&mut self) {
        abort(self.context_task.take());
        if let Some((navigation, true)) = &self.navigation {
            navigation.withdraw();
            abort(self.reveal_task.take());
            self.navigation = None;
        }
    }

    /// End the shown request's work: its page and revocation watcher.
    fn abort_request(&mut self) {
        abort(self.query_task.take());
        abort(self.revoke_task.take());
        self.request = None;
    }

    /// A newer open supersedes the previous navigation, whoever owns it.
    fn claim_navigation(&mut self, authority: NavigationAuthority, search: bool) {
        if let Some((previous, _)) = self.navigation.replace((authority, search)) {
            previous.withdraw();
        }
    }
}

fn activation_is_current(generation: u64) -> bool {
    with(|controller| controller.activation == generation)
}

thread_local! {
    static CONTROLLER: RefCell<Controller> = RefCell::new(Controller::default());
}

/// Never call window setters or callbacks inside `f`.
fn with<R>(f: impl FnOnce(&mut Controller) -> R) -> R {
    CONTROLLER.with(|controller| f(&mut controller.borrow_mut()))
}

fn abort(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
    }
}

fn shown_ticket() -> Option<Ticket> {
    with(|controller| Some(controller.request.as_ref()?.ticket.clone()))
}

fn next_page() -> Option<(Ticket, usize)> {
    with(|controller| {
        let request = controller.request.as_ref()?;
        Some((request.ticket.clone(), request.next_offset?))
    })
}

/// The shown request is still the one this ticket was issued for.
fn owns(ticket: &Ticket) -> bool {
    shown_ticket().is_some_and(|shown| shown.operation == ticket.operation)
}

/// Cancel every request and clear all search presentation, filters
/// included: they can name a person or a time and belong to the account.
/// Used for account changes and removal, sign-out and revocation.
pub(crate) fn reset(ui: &AppWindow) {
    timeline_viewport::reset(ui);
    clear_results(ui);
    ui.set_room_search_filter_sender("".into());
    ui.set_room_search_filter_since("".into());
    ui.set_room_search_filter_until("".into());
    ui.set_room_search_filter_attachment(0);
    ui.set_room_search_filter_error("".into());
    ui.set_room_search_filters_open(false);
}

/// Cancel every request and clear the query, results and context; the
/// account's filters stay.
fn clear_results(ui: &AppWindow) {
    with(|controller| {
        controller.abort_request();
        controller.abort_opening();
    });
    ui.set_room_search_operation(ui.get_room_search_operation().wrapping_add(1));
    ui.set_room_search_context_operation(ui.get_room_search_context_operation().wrapping_add(1));
    ui.set_room_search_loading(false);
    ui.set_room_search_loading_more(false);
    ui.set_room_search_has_more(false);
    ui.set_room_search_context_loading(false);
    ui.set_room_search_query("".into());
    ui.set_room_search_error("".into());
    ui.set_room_search_notice("".into());
    ui.set_room_search_context_error("".into());
    ui.set_room_search_context_notice("".into());
    ui.set_room_search_searched(false);
    ui.set_room_search_index(-1);
    ui.set_room_search_results(ModelRc::new(VecModel::<MessageSearchItem>::default()));
    ui.set_room_search_context(ModelRc::new(VecModel::<MessageSearchItem>::default()));
    clear_selection(ui);
}

/// A different room became visible. All-room results stay with their
/// account; open-room results belong to the room that was left. Filters
/// stay until the account changes. Seeks and anchors belong to the room
/// that was left.
pub(crate) fn room_changed(ui: &AppWindow, account_changed: bool) {
    timeline_viewport::reset(ui);
    if account_changed {
        reset(ui);
    } else if ui.get_room_search_scope() != 1 {
        clear_results(ui);
    }
}

fn clear_selection(ui: &AppWindow) {
    ui.set_room_search_selected_room_id("".into());
    ui.set_room_search_selected_event_id("".into());
    ui.set_room_search_selected_thread_root("".into());
    ui.set_room_search_selected_sender("".into());
    ui.set_room_search_selected_time("".into());
    ui.set_room_search_selected_body("".into());
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) {
    // The window's seeks (opened hits, activations, unread jumps, anchors).
    timeline_viewport::register(ui, live.clone());
    let weak = ui.as_weak();
    let edit_live = live.clone();
    let edit_runtime = runtime.clone();
    ui.on_room_search_edited(move |_| {
        if let Some(window) = weak.upgrade() {
            start(&window, &edit_live, &edit_runtime, Some(DEBOUNCE));
        }
    });

    // A changed filter supersedes the shown query like an edited one.
    let weak = ui.as_weak();
    let filter_live = live.clone();
    let filter_runtime = runtime.clone();
    ui.on_room_search_filters_edited(move || {
        if let Some(window) = weak.upgrade() {
            start(&window, &filter_live, &filter_runtime, Some(DEBOUNCE));
        }
    });

    let weak = ui.as_weak();
    let submit_live = live.clone();
    let submit_runtime = runtime.clone();
    ui.on_query_room_messages(move || {
        if let Some(window) = weak.upgrade() {
            submit(&window, &submit_live, &submit_runtime);
        }
    });

    let weak = ui.as_weak();
    let scope_live = live.clone();
    let scope_runtime = runtime.clone();
    ui.on_choose_room_search_scope(move |scope| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let scope = if scope == 1 { 1 } else { 0 };
        if scope == 0 && window.get_active_room_id().is_empty() {
            return;
        }
        if window.get_room_search_scope() != scope {
            window.set_room_search_scope(scope);
            start(&window, &scope_live, &scope_runtime, None);
        }
    });

    let weak = ui.as_weak();
    let more_live = live.clone();
    let more_runtime = runtime.clone();
    ui.on_load_more_room_search(move || {
        if let Some(window) = weak.upgrade() {
            load_more(&window, &more_live, &more_runtime);
        }
    });

    let weak = ui.as_weak();
    let return_live = live.clone();
    let return_runtime = runtime.clone();
    ui.on_return_to_live(move || {
        if let Some(window) = weak.upgrade() {
            return_to_live(&window, &return_live, &return_runtime);
        }
    });

    let weak = ui.as_weak();
    let newer_live = live.clone();
    let newer_runtime = runtime.clone();
    ui.on_load_newer_focused(move || {
        if let Some(window) = weak.upgrade() {
            load_newer(&window, &newer_live, &newer_runtime);
        }
    });

    // The navigation seam as a window callback: account, room, event and
    // thread root IDs, with empty strings for no event or thread.
    let weak = ui.as_weak();
    let target_live = live.clone();
    let target_runtime = runtime.clone();
    ui.on_open_conversation(move |account, room, event, thread| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let target = ConversationTarget {
            account_id: account.to_string(),
            room_id: room.to_string(),
            event_id: (!event.is_empty()).then(|| event.to_string()),
            thread_root: (!thread.is_empty()).then(|| thread.to_string()),
        };
        open_conversation(&window, &target_live, &target_runtime, target);
    });

    let weak = ui.as_weak();
    ui.on_open_search_result(move |room_id, event_id| {
        if let Some(window) = weak.upgrade() {
            open(&window, &live, &runtime, &room_id, &event_id);
        }
    });
}

fn current_scope(window: &AppWindow) -> Option<Scope> {
    if window.get_room_search_scope() == 1 {
        return Some(Scope::All);
    }
    let room = window.get_active_room_id().to_string();
    (!room.is_empty()).then_some(Scope::Room(room))
}

/// The window's filter fields as bounded search filters, or fixed feedback
/// for the first malformed one. The sender is checked by the core's own
/// user ID rule; dates are local calendar days, inclusive at both ends.
fn read_filters(window: &AppWindow) -> Result<SearchFilters, &'static str> {
    let sender = window.get_room_search_filter_sender().trim().to_owned();
    let since = window.get_room_search_filter_since().trim().to_owned();
    let until = window.get_room_search_filter_until().trim().to_owned();
    let mut filters = SearchFilters::default();
    if !sender.is_empty() {
        filters.sender = Some(sender);
        filters.validate().map_err(|_| FILTER_SENDER)?;
    }
    if !since.is_empty() {
        filters.since_ms = Some(local_day(&since).ok_or(FILTER_DATE)?.0);
    }
    if !until.is_empty() {
        filters.until_ms = Some(local_day(&until).ok_or(FILTER_DATE)?.1);
    }
    filters.validate().map_err(|_| FILTER_RANGE)?;
    filters.attachment = match window.get_room_search_filter_attachment() {
        1 => AttachmentFilter::With,
        2 => AttachmentFilter::Without,
        _ => AttachmentFilter::Any,
    };
    Ok(filters)
}

/// The first and last millisecond of the local calendar day `text` names,
/// written exactly as YYYY-MM-DD from 1970 on.
fn local_day(text: &str) -> Option<(u64, u64)> {
    local_day_in_zone(text, &Local)
}

fn local_day_in_zone(text: &str, zone: &impl TimeZone) -> Option<(u64, u64)> {
    let bytes = text.as_bytes();
    if bytes.len() != DATE_CHARS
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, value)| i != 4 && i != 7 && !value.is_ascii_digit())
    {
        return None;
    }
    let mut parts = text.splitn(3, '-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let resolve = |wall_time: &NaiveDateTime| {
        zone.from_local_datetime(wall_time)
            .earliest()
            .map(|time| time.timestamp_millis())
    };
    let (first_wall_time, start) = local_day_boundary(date, &resolve)?;
    // A timezone can skip an entire calendar day when crossing the date line.
    if first_wall_time.date() != date {
        return None;
    }
    let (_, next_start) = local_day_boundary(date.succ_opt()?, &resolve)?;
    let end = u64::try_from(next_start.checked_sub(1)?).ok()?;
    // Matrix timestamps cannot name an instant before the Unix epoch. A local
    // 1970-01-01 in a positive UTC offset still has a searchable partial day.
    let start = u64::try_from(start.max(0)).ok()?;
    (start <= end).then_some((start, end))
}

/// First valid wall time on or after midnight, and its earliest UTC instant.
/// Midnight may be missing at a clock change; the following date may itself
/// be skipped. Bound the search to two days and refine the first valid minute
/// to a millisecond, preserving timezone transitions with second offsets.
fn local_day_boundary(
    date: NaiveDate,
    resolve: &impl Fn(&NaiveDateTime) -> Option<i64>,
) -> Option<(NaiveDateTime, i64)> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    if let Some(timestamp) = resolve(&midnight) {
        return Some((midnight, timestamp));
    }
    for minute in 1..=2 * 24 * 60 {
        let upper_ms = minute * 60_000;
        let wall_time = midnight.checked_add_signed(chrono::Duration::milliseconds(upper_ms))?;
        if resolve(&wall_time).is_none() {
            continue;
        }
        let mut lower = upper_ms - 60_000;
        let mut upper = upper_ms;
        while upper - lower > 1 {
            let middle = lower + (upper - lower) / 2;
            let probe = midnight.checked_add_signed(chrono::Duration::milliseconds(middle))?;
            if resolve(&probe).is_some() {
                upper = middle;
            } else {
                lower = middle;
            }
        }
        let first = midnight.checked_add_signed(chrono::Duration::milliseconds(upper))?;
        return Some((first, resolve(&first)?));
    }
    None
}

/// Return with nothing highlighted: highlight the first hit of a finished
/// search for the same query, so a second Return opens it; otherwise search
/// immediately without the typing pause.
fn submit(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    let query = window.get_room_search_query().trim().to_owned();
    let Ok(filters) = read_filters(window) else {
        start(window, live, runtime, None);
        return;
    };
    let scope = current_scope(window);
    let operation = window.get_room_search_operation();
    let shown = shown_ticket()
        .filter(|ticket| ticket.operation == operation)
        .is_some_and(|ticket| ticket.matches(&query, scope.as_ref(), &filters));
    let listed = window.get_room_search_results().row_count();
    if shown && listed > 0 && !window.get_room_search_loading() {
        window.set_room_search_index(0);
        return;
    }
    start(window, live, runtime, None);
}

/// Begin a new query, cancelling the previous query and context requests.
fn start(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, delay: Option<Duration>) {
    with(|controller| {
        controller.abort_request();
        controller.abort_opening();
    });
    let operation = window.get_room_search_operation().wrapping_add(1);
    window.set_room_search_operation(operation);
    window.set_room_search_context_operation(
        window.get_room_search_context_operation().wrapping_add(1),
    );
    window.set_room_search_loading(false);
    window.set_room_search_loading_more(false);
    window.set_room_search_has_more(false);
    window.set_room_search_searched(false);
    window.set_room_search_error("".into());
    window.set_room_search_notice("".into());
    window.set_room_search_context_loading(false);
    window.set_room_search_context_error("".into());
    window.set_room_search_context_notice("".into());
    window.set_room_search_index(-1);
    window.set_room_search_results(ModelRc::new(VecModel::<MessageSearchItem>::default()));
    window.set_room_search_context(ModelRc::new(VecModel::<MessageSearchItem>::default()));
    clear_selection(window);

    let query = window.get_room_search_query().trim().to_owned();
    // A malformed filter is reported even before words are typed, and no
    // search runs.
    let filters = match read_filters(window) {
        Ok(filters) => filters,
        Err(message) => {
            window.set_room_search_filter_error(message.into());
            return;
        }
    };
    // Without words, a sender or date filter searches the index (the open
    // room, or every joined room) newest first; attachment presence alone
    // cannot.
    if query.is_empty() {
        let refusal = if filters.is_empty() {
            Some("")
        } else if !filters.indexed() {
            Some(FILTER_NEEDS_WORDS)
        } else {
            None
        };
        if let Some(refusal) = refusal {
            window.set_room_search_filter_error(refusal.into());
            return;
        }
    }
    window.set_room_search_filter_error("".into());
    if !live.is_live.load(Ordering::Acquire) || !window.get_live_session() {
        window.set_room_search_error(NOT_SIGNED_IN.into());
        return;
    }
    let account = window.get_active_account_local_id().to_string();
    let selection = if account.is_empty() {
        None
    } else {
        live.manager.account_selection(&account)
    };
    let Some(selection) = selection else {
        window.set_room_search_error(NOT_SIGNED_IN.into());
        return;
    };
    let Some(scope) = current_scope(window) else {
        window.set_room_search_error(NO_ROOM.into());
        return;
    };
    if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        window.set_room_search_error(INVALID_QUERY.into());
        return;
    }
    let ticket = Ticket {
        operation,
        selection,
        account,
        scope,
        query,
        filters,
    };
    window.set_room_search_loading(true);
    let weak = window.as_weak();
    let manager = live.manager.clone();
    let task = spawn_page(weak, manager, runtime, ticket.clone(), 0, delay);
    let watcher = watch_revocation(window.as_weak(), runtime, ticket.clone());
    with(|controller| {
        controller.request = Some(Request {
            ticket,
            next_offset: None,
            unavailable: 0,
            filtered: 0,
            seen: HashSet::new(),
        });
        controller.query_task = Some(task);
        controller.revoke_task = Some(watcher);
    });
}

/// Clear the search once its request's account selection ends: a switch,
/// a removal or A -> B -> A, even when the window shows the same account
/// again and no account change resets the search.
fn watch_revocation(weak: Weak<AppWindow>, runtime: &Handle, ticket: Ticket) -> JoinHandle<()> {
    runtime.spawn(async move {
        ticket.selection.revoked().await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            if owns(&ticket) {
                reset(&window);
            }
        });
    })
}

fn load_more(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    if window.get_room_search_loading() || window.get_room_search_loading_more() {
        return;
    }
    let Some((ticket, offset)) = next_page() else {
        return;
    };
    if !ticket.current(window) {
        return;
    }
    window.set_room_search_loading_more(true);
    window.set_room_search_error("".into());
    let weak = window.as_weak();
    let manager = live.manager.clone();
    let task = spawn_page(weak, manager, runtime, ticket, offset, None);
    with(|controller| {
        abort(controller.query_task.take());
        controller.query_task = Some(task);
    });
}

async fn search_page(
    manager: &AccountManager,
    ticket: &Ticket,
    offset: usize,
) -> Result<MessageSearchPage, MatrixCoreError> {
    let (selection, query) = (&ticket.selection, ticket.query.as_str());
    match &ticket.scope {
        Scope::Room(room) => {
            manager
                .search_messages_filtered_selected(
                    selection,
                    room,
                    query,
                    &ticket.filters,
                    PAGE_SIZE,
                    offset,
                )
                .await
        }
        Scope::All => {
            manager
                .search_all_messages_filtered_selected(
                    selection,
                    query,
                    &ticket.filters,
                    PAGE_SIZE,
                    offset,
                )
                .await
        }
    }
}

fn spawn_page(
    weak: Weak<AppWindow>,
    manager: Arc<AccountManager>,
    runtime: &Handle,
    ticket: Ticket,
    offset: usize,
    delay: Option<Duration>,
) -> JoinHandle<()> {
    runtime.spawn(async move {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let search = search_page(&manager, &ticket, offset);
        let wait = tokio::time::timeout(SEARCH_TIMEOUT, search);
        // A revoked selection ends the wait at once and publishes nothing.
        let Some(outcome) = ticket.selection.guard(wait).await else {
            return;
        };
        let outcome = match outcome {
            Ok(result) => result.map_err(search_failure),
            Err(_) => Err(Some(SEARCH_SLOW)),
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            publish_page(&window, &ticket, offset > 0, outcome);
        });
    })
}

/// Fixed wording for a failed request. `None` means the account changed.
fn search_failure(error: MatrixCoreError) -> Option<&'static str> {
    match error {
        MatrixCoreError::SelectedAccountChanged
        | MatrixCoreError::NoActiveAccount
        | MatrixCoreError::AccountNotFound(_) => None,
        MatrixCoreError::InvalidSearchQuery(_) => Some(INVALID_QUERY),
        MatrixCoreError::RoomNotFound(_) | MatrixCoreError::InvalidRoomId(_) => Some(ROOM_GONE),
        _ => Some(SEARCH_FAILED),
    }
}

fn publish_page(
    window: &AppWindow,
    ticket: &Ticket,
    append: bool,
    outcome: Shown<MessageSearchPage>,
) {
    if !ticket.current(window) || !owns(ticket) {
        return;
    }
    window.set_room_search_loading(false);
    window.set_room_search_loading_more(false);
    let page = match outcome {
        Ok(page) => page,
        // The account changed under this request: claim no result. Its
        // revocation watcher clears the search.
        Err(None) => return,
        Err(Some(message)) => {
            // A failed first page shows the error; a failed later page keeps
            // the listed results and their cursor, so Load more can retry.
            window.set_room_search_searched(true);
            window.set_room_search_error(message.into());
            return;
        }
    };
    window.set_room_search_searched(true);
    let Some((items, more, counts)) = with(|controller| accept_page(controller, page)) else {
        return;
    };
    let total = append_results(window, items, append);
    window.set_room_search_has_more(more);
    window.set_room_search_notice(page_notice(counts, total).into());
}

/// Record a page in the shown request: drop repeats by (room, event)
/// identity, stop at MAX_RESULTS and keep the next cursor while room remains.
fn accept_page(
    controller: &mut Controller,
    page: MessageSearchPage,
) -> Option<(Vec<MessageSearchItem>, bool, (usize, usize))> {
    let request = controller.request.as_mut()?;
    let mut items = Vec::new();
    for result in page.results {
        if request.seen.len() >= MAX_RESULTS {
            break;
        }
        let key = (result.room_id.clone(), result.event_id.clone());
        if request.seen.insert(key) {
            items.push(result_item(result));
        }
    }
    request.unavailable += page.unavailable;
    request.filtered += page.filtered;
    request.next_offset = page.next_offset;
    if request.seen.len() >= MAX_RESULTS {
        request.next_offset = None;
    }
    let counts = (request.unavailable, request.filtered);
    Some((items, request.next_offset.is_some(), counts))
}

/// Hits that could not be loaded, and loaded word matches the filters hid,
/// are counted, never silently claimed as no match.
fn page_notice((unavailable, filtered): (usize, usize), total: usize) -> String {
    let mut notice = Vec::new();
    if unavailable == 1 {
        notice.push("1 match could not be loaded".to_owned());
    } else if unavailable > 1 {
        notice.push(format!("{unavailable} matches could not be loaded"));
    }
    if filtered == 1 {
        notice.push("1 word match hidden by the filters".to_owned());
    } else if filtered > 1 {
        notice.push(format!("{filtered} word matches hidden by the filters"));
    }
    if total >= MAX_RESULTS {
        notice.push(RESULT_LIMIT.to_owned());
    }
    notice.join(" · ")
}

fn append_results(window: &AppWindow, items: Vec<MessageSearchItem>, append: bool) -> usize {
    let current = window.get_room_search_results();
    if append {
        let shown = current
            .as_any()
            .downcast_ref::<VecModel<MessageSearchItem>>();
        if let Some(model) = shown {
            model.extend(items);
            return model.row_count();
        }
        let mut rows = current.iter().collect::<Vec<_>>();
        rows.extend(items);
        let total = rows.len();
        window.set_room_search_results(ModelRc::new(VecModel::from(rows)));
        return total;
    }
    let total = items.len();
    window.set_room_search_results(ModelRc::new(VecModel::from(items)));
    total
}

fn listed(window: &AppWindow, room_id: &str, event_id: &str) -> Option<MessageSearchItem> {
    let results = window.get_room_search_results();
    let mut rows = results.iter();
    rows.find(|item| item.room_id.as_str() == room_id && item.event_id.as_str() == event_id)
}

/// Open one listed hit: its room (revealing its Space when needed), then the
/// exact event in the conversation or its thread, and the nearby messages.
fn open(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, room_id: &str, event_id: &str) {
    // Taken first: a security step begun while this click is admitted never
    // gives its seeks a newer generation.
    let security = timeline_viewport::security_generation(&live.security_gate);
    if !live.is_live.load(Ordering::Acquire) || room_id.is_empty() || event_id.is_empty() {
        return;
    }
    let Some(ticket) = shown_ticket().filter(|ticket| ticket.current(window)) else {
        return;
    };
    // Only an exact listed hit can be opened; nothing else routes here.
    let Some(item) = listed(window, room_id, event_id) else {
        return;
    };
    // An edit has no saved draft and cannot be restored; leaving its room
    // would drop the typed text. Keep it and say why the hit did not open.
    let elsewhere = window.get_active_room_id().as_str() != room_id;
    if elsewhere && unsent_edit(window) {
        window.set_room_search_notice(KEEP_COMPOSER.into());
        return;
    }
    let hit = Hit {
        room_id: room_id.to_owned(),
        event_id: event_id.to_owned(),
        thread_root: item.thread_root.to_string(),
    };
    window.set_room_search_selected_room_id(item.room_id);
    window.set_room_search_selected_event_id(item.event_id);
    window.set_room_search_selected_thread_root(item.thread_root);
    window.set_room_search_selected_sender(item.sender);
    window.set_room_search_selected_time(item.time);
    window.set_room_search_selected_body(item.body);
    let operation = window.get_room_search_context_operation().wrapping_add(1);
    window.set_room_search_context_operation(operation);
    window.set_room_search_context_loading(true);
    window.set_room_search_context_error("".into());
    window.set_room_search_context_notice("".into());
    window.set_room_search_context(ModelRc::new(VecModel::<MessageSearchItem>::default()));
    let authority = NavigationAuthority::default();
    with(|controller| {
        controller.abort_opening();
        controller.claim_navigation(authority.clone(), true);
    });
    let opening = Opening {
        owner: Owner::Search { ticket, operation },
        hit,
        authority,
        navigation: live.current_navigation_epoch(),
        security,
    };
    navigate(window, live, runtime, opening);
}

/// NAVIGATION SEAM. Open an exact account, room and, optionally, event and
/// thread on behalf of an activation such as a notification; search hits
/// take the same path below `open`.
///
/// - Another account is made visible only through the window's own
///   protected switch (`switch-account`: Security dialog gate, active-call
///   confirmation, local app password). The target waits for that account
///   for at most `PENDING_TARGET_LIFETIME` and is dropped if any other
///   account becomes visible or a newer activation starts.
/// - Once its account is visible, the open is fenced by that account's
///   selection (A -> B -> A safe), the visible account and this activation's
///   generation; a newer activation or search open withdraws it.
/// - The room is opened like a room-list click (revealing its Space), a
///   loaded event is scrolled to, an older one opens the conversation around
///   it with Return to live, and a thread target opens the thread panel.
/// - An edit being typed in another room is never dropped: the open is
///   refused with a session error. A reply is kept for its room and drafts
///   are kept as for any navigation.
pub(crate) fn open_conversation(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    target: ConversationTarget,
) {
    if !live.is_live.load(Ordering::Acquire)
        || !window.get_live_session()
        || target.account_id.is_empty()
        || target.room_id.is_empty()
    {
        return;
    }
    let generation = with(|controller| {
        controller.activation = controller.activation.wrapping_add(1);
        controller.pending = None;
        controller.activation
    });
    if window.get_active_account_local_id().as_str() == target.account_id {
        if let Some(selection) = live.manager.account_selection(&target.account_id) {
            activate(window, live, runtime, target, generation, selection);
        }
        return;
    }
    let options = window.get_account_options();
    let mut accounts = options.iter();
    let Some(account) = accounts.find(|option| option.local_id.as_str() == target.account_id)
    else {
        return;
    };
    let pending = PendingTarget {
        target,
        live: live.clone(),
        runtime: runtime.clone(),
        generation,
        expires: Instant::now() + PENDING_TARGET_LIFETIME,
    };
    with(|controller| controller.pending = Some(pending));
    window.invoke_switch_account(account.index);
}

/// NAVIGATION SEAM, owned continuation: show `target`'s event in the room
/// an owner's own navigation already shows, under that navigation's epoch
/// (`navigation`), never a new one. Nothing happens unless the owner's exact
/// `selection` is current and on screen, `navigation` is still the current
/// navigation, `authority` (the owner's own, with its click-time
/// conditions and the surfaces that end it) admits it and the room open now
/// is the target's. That authority is registered like any open's, so a
/// newer activation or search open withdraws it too, and every later step
/// (each scroll retry, the focused view's commit, its first rows and its
/// scroll) asks it and `navigation` again (Opening::admitted). Callers:
/// matrix_links' finish_join, under the join's own receipt (LiveUi's
/// JoinAsked::owned_open and follow_up_authority).
pub(crate) fn continue_conversation(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    selection: AccountSelection,
    target: ConversationTarget,
    navigation: u64,
    authority: NavigationAuthority,
) {
    // The join's click-time security stays in `authority`, which every seek
    // move asks too; the seeks also keep this continuation's, taken first.
    let security = timeline_viewport::security_generation(&live.security_gate);
    let owned = live.navigation_is_current(navigation)
        && authority.admits(window)
        && selection.is_current()
        && window.get_live_session()
        && selection.account_id() == target.account_id
        && window.get_active_account_local_id().as_str() == target.account_id
        && window.get_active_room_id().as_str() == target.room_id;
    if !owned {
        return;
    }
    let generation = with(|controller| {
        controller.activation = controller.activation.wrapping_add(1);
        controller.pending = None;
        controller.activation
    });
    with(|controller| controller.claim_navigation(authority.clone(), false));
    let hit = Hit {
        room_id: target.room_id,
        event_id: target.event_id.unwrap_or_default(),
        thread_root: target.thread_root.unwrap_or_default(),
    };
    let owner = Owner::Activation {
        selection,
        account: target.account_id,
        generation,
    };
    let opening = Opening {
        owner,
        hit,
        authority,
        navigation,
        security,
    };
    // The room was just opened: its latest messages may still be arriving.
    arrived(window, live, runtime, &opening, true);
}

/// The visible account changed (called once its snapshot is applied). A
/// pending target for that account opens now; any other one is dropped.
pub(crate) fn account_shown(window: &AppWindow) {
    let Some(pending) = with(|controller| controller.pending.take()) else {
        return;
    };
    let account = window.get_active_account_local_id();
    if account.as_str() != pending.target.account_id
        || Instant::now() > pending.expires
        || !activation_is_current(pending.generation)
    {
        return;
    }
    // The exact selection this snapshot shows. A later A -> B -> A makes it
    // stale, so the deferred open below can never revive this target.
    let manager = &pending.live.manager;
    let Some(selection) = manager.account_selection(&pending.target.account_id) else {
        return;
    };
    // Open after the snapshot that made the account visible is applied.
    let weak = window.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let PendingTarget {
            target,
            live,
            runtime,
            generation,
            ..
        } = pending;
        let visible = window.get_active_account_local_id().as_str() == target.account_id;
        if visible && selection.is_current() && activation_is_current(generation) {
            activate(&window, &live, &runtime, target, generation, selection);
        }
    });
}

/// Open a target in the visible account under its exact `selection`.
fn activate(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    target: ConversationTarget,
    generation: u64,
    selection: AccountSelection,
) {
    // Taken first, when the target opens in its now visible account (after
    // a protected switch, once that switch's own step is over).
    let security = timeline_viewport::security_generation(&live.security_gate);
    if !selection.is_current() || selection.account_id() != target.account_id {
        return;
    }
    let elsewhere = window.get_active_room_id().as_str() != target.room_id;
    if elsewhere && unsent_edit(window) {
        window.set_session_error(KEEP_COMPOSER.into());
        return;
    }
    let authority = NavigationAuthority::default();
    with(|controller| controller.claim_navigation(authority.clone(), false));
    let hit = Hit {
        room_id: target.room_id,
        event_id: target.event_id.unwrap_or_default(),
        thread_root: target.thread_root.unwrap_or_default(),
    };
    let owner = Owner::Activation {
        selection,
        account: target.account_id,
        generation,
    };
    let opening = Opening {
        owner,
        hit,
        authority,
        navigation: live.current_navigation_epoch(),
        security,
    };
    navigate(window, live, runtime, opening);
}

/// Open the target's room unless it is already open, then show the target.
fn navigate(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, mut opening: Opening) {
    if opening.here(window) {
        arrived(window, live, runtime, &opening, false);
        return;
    }
    // Leave the open room the way a room-list click does. Its drafts stay in
    // the draft store and return with the room.
    window.invoke_cancel_room_settings();
    window.invoke_close_image_preview();
    // A pending Load newer of the left room's focused view no longer applies.
    with(|controller| controller.newer_request = controller.newer_request.wrapping_add(1));
    window.set_focus_newer_loading(false);
    opening.navigation = live.invalidate_navigation();
    let weak = window.as_weak();
    let task_live = live.clone();
    let handle = runtime.clone();
    runtime.spawn(async move {
        let selection = opening.selection().clone();
        let room = opening.hit.room_id.clone();
        let (epoch, authority) = (opening.navigation, opening.authority.clone());
        let outcome = task_live
            .reveal_room_for(selection, room, epoch, authority, weak.clone())
            .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            navigated(&window, &task_live, &handle, &opening, outcome);
        });
    });
}

fn navigated(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    opening: &Opening,
    outcome: Result<bool, MatrixCoreError>,
) {
    if !opening.current(window) {
        return;
    }
    let here = opening.here(window);
    // A newer open of either kind withdrew this one: it shows nothing more.
    let withdrawn = opening.authority.withdrawn();
    match outcome {
        Ok(true) if here && !withdrawn && live.navigation_is_current(opening.navigation) => {
            arrived(window, live, runtime, opening, true);
        }
        // Withdrawn or replaced by another navigation.
        Ok(_) => opening.not_opened(window, None),
        Err(_) => opening.not_opened(window, Some(ROOM_NOT_OPENED)),
    }
}

/// The target's room is open: show its event, then (for a search hit) load
/// the nearby messages. An event that is not among the loaded messages opens
/// the conversation around it (after a short wait when the room was just
/// opened and its latest messages may still be arriving); a thread target
/// opens its thread.
fn arrived(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    opening: &Opening,
    moved: bool,
) {
    let hit = &opening.hit;
    if !hit.thread_root.is_empty() {
        // The reply is highlighted, and brought into the thread's view once
        // its reveal finds it loaded (revealed).
        window.set_timeline_jump_event_id(hit.event_id.as_str().into());
        open_thread(window, &hit.room_id, &hit.thread_root);
        if !hit.event_id.is_empty() {
            reveal(window, live, runtime, opening);
        }
    } else if !hit.event_id.is_empty() && !jump_timeline(window, live, opening) {
        let attempts = if moved { ARRIVAL_ATTEMPTS } else { 0 };
        jump_or_focus(window.as_weak(), live, runtime, opening, attempts);
    }
    if !opening.searching() {
        return;
    }
    let manager = live.manager.clone();
    let opening = opening.clone();
    let weak = window.as_weak();
    let task = runtime.spawn(async move {
        let Some(outcome) = load_context(&manager, &opening).await else {
            return;
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            publish_context(&window, &opening, outcome);
        });
    });
    with(|controller| {
        abort(controller.context_task.take());
        controller.context_task = Some(task);
    });
}

/// Jump to the target once its row is loaded, checking `attempts` more
/// times; then open the conversation around it instead. Every check is the
/// opening's own admission, its navigation included.
fn jump_or_focus(
    weak: Weak<AppWindow>,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    opening: &Opening,
    attempts: u8,
) {
    let Some(window) = weak.upgrade() else {
        return;
    };
    if !opening.admitted(&window, live) {
        return;
    }
    if jump_timeline(&window, live, opening) {
        return;
    }
    if attempts == 0 {
        focus(&window, live, runtime, opening);
        return;
    }
    let (live, runtime, opening) = (live.clone(), runtime.clone(), opening.clone());
    Timer::single_shot(JUMP_RETRY, move || {
        jump_or_focus(weak, &live, &runtime, &opening, attempts - 1);
    });
}

/// Show the conversation around the hit in place of the latest messages,
/// with Return to live (LiveUi::focus_event_for). The room and its drafts
/// are unchanged; the opening's authority stops it before it replaces the
/// live conversation.
fn focus(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, opening: &Opening) {
    opening.notice(window, FOCUSING);
    let task_live = live.clone();
    let opening = opening.clone();
    let weak = window.as_weak();
    runtime.spawn(async move {
        let selection = opening.selection().clone();
        let (room, event) = (opening.hit.room_id.clone(), opening.hit.event_id.as_str());
        let (epoch, authority) = (opening.navigation, opening.authority.clone());
        let focus =
            task_live.focus_event_for(selection, room, event, epoch, authority, weak.clone());
        let outcome = tokio::time::timeout(FOCUS_TIMEOUT, focus).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            focused(&window, &task_live, &opening, outcome);
        });
    });
}

fn focused(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    opening: &Opening,
    outcome: Result<Result<bool, MatrixCoreError>, tokio::time::error::Elapsed>,
) {
    if !opening.current(window) || !opening.here(window) {
        return;
    }
    match outcome {
        // Withdrawn (by its requester or its own conditions) or navigated
        // since: nothing of it is shown or scrolled to.
        _ if !opening.admitted(window, live) => opening.notice(window, ""),
        Ok(Ok(true)) => {
            reset_history(window);
            opening.notice(window, "");
            jump_when_loaded(
                window.as_weak(),
                live.clone(),
                opening.clone(),
                JUMP_ATTEMPTS,
            );
        }
        // The room or account changed meanwhile.
        Ok(Ok(false)) => opening.notice(window, ""),
        Ok(Err(_)) | Err(_) => opening.notice(window, FOCUS_FAILED),
    }
}

/// Page back through the hit's thread, a bounded number of pages, until
/// the reply is loaded. The thread panel shows what was loaded.
fn reveal(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, opening: &Opening) {
    opening.notice(window, FINDING);
    let task_live = live.clone();
    let opening = opening.clone();
    let weak = window.as_weak();
    let task = runtime.spawn(async move {
        let outcome = reveal_hit(&task_live.manager, &opening).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            revealed(&window, &task_live, &opening, outcome);
        });
    });
    with(|controller| {
        abort(controller.reveal_task.take());
        controller.reveal_task = Some(task);
    });
}

/// How far the reveal got, or `None` when it failed, timed out or its
/// account selection ended.
async fn reveal_hit(manager: &AccountManager, opening: &Opening) -> Option<HistoryReveal> {
    let selection = opening.selection();
    let hit = &opening.hit;
    let thread = Some(hit.thread_root.as_str());
    let (room, event) = (hit.room_id.as_str(), hit.event_id.as_str());
    let reveal = manager.reveal_event_selected(selection, room, thread, event, REVEAL_PAGES);
    let outcome = tokio::time::timeout(REVEAL_TIMEOUT, reveal).await;
    outcome.ok().and_then(Result::ok)
}

fn revealed(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    opening: &Opening,
    outcome: Option<HistoryReveal>,
) {
    if !opening.current(window) || !opening.here(window) {
        return;
    }
    // Navigated since or withdrawn: the thread panel is no longer this
    // open's to mark.
    if !opening.admitted(window, live) {
        opening.notice(window, "");
        return;
    }
    let thread_open = window.get_thread_root_id().as_str() == opening.hit.thread_root;
    if thread_open && outcome.is_some_and(|reveal| reveal.start_reached) {
        // The reveal read the same history Load earlier replies pages.
        window.set_thread_history_at_start(true);
    }
    let notice = match outcome {
        Some(reveal) if reveal.found => "",
        Some(_) => THREAD_OLDER,
        None => REVEAL_FAILED,
    };
    opening.notice(window, notice);
    // The loaded reply (never the root, which heads the panel) is brought
    // into the thread's view once its row is laid out, under this opening.
    if thread_open && outcome.is_some_and(|reveal| reveal.found) {
        let reveal = Reveal {
            pane: Pane::Thread,
            selection: opening.selection().clone(),
            navigation: opening.navigation,
            security: opening.security,
            event_id: opening.hit.event_id.clone(),
            owner: opening.seek_owner(THREAD_NOT_SHOWN),
        };
        timeline_viewport::reveal(window, live, reveal);
    }
}

/// Jump once the hit's row reaches the conversation model, under the
/// opening's own admission at every check.
fn jump_when_loaded(weak: Weak<AppWindow>, live: Arc<LiveUi>, opening: Opening, attempts: u8) {
    Timer::single_shot(JUMP_RETRY, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !opening.admitted(&window, &live) {
            return;
        }
        if jump_timeline(&window, &live, &opening) {
            return;
        }
        if attempts > 1 {
            jump_when_loaded(weak, live, opening, attempts - 1);
        } else {
            opening.notice(&window, NOT_SHOWN);
        }
    });
}

/// Leave a focused conversation view for the room's latest messages. A
/// pending open of a hit cannot focus the conversation again afterwards.
fn return_to_live(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    let account = window.get_active_account_local_id().to_string();
    let room = window.get_active_room_id().to_string();
    if !window.get_conversation_focused() || account.is_empty() || room.is_empty() {
        return;
    }
    // The selection and the view shown at this click: a stale request
    // (A -> B -> A, a view shown since) ends nothing.
    let Some(selection) = live.manager.account_selection(&account) else {
        return;
    };
    let shown = live.focused_view(&room);
    with(|controller| {
        if let Some((navigation, _)) = controller.navigation.take() {
            navigation.withdraw();
        }
    });
    reset_history(window);
    let live = live.clone();
    let weak = window.as_weak();
    runtime.spawn(async move { live.return_to_live_for(selection, room, shown, weak).await });
}

/// Page a focused conversation view towards the latest messages. Once it
/// reaches them, the live conversation takes over, so new messages arrive.
fn load_newer(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    if !window.get_conversation_focused() || window.get_focus_newer_loading() {
        return;
    }
    let account = window.get_active_account_local_id().to_string();
    let room = window.get_active_room_id().to_string();
    let Some(selection) = live.manager.account_selection(&account) else {
        return;
    };
    // Page exactly the view shown at this click.
    let Some(view) = live.focused_view(&room) else {
        return;
    };
    // Each request supersedes the last; focusing again or returning to live
    // (reset_history) supersedes them all.
    let request = with(|controller| {
        controller.newer_request = controller.newer_request.wrapping_add(1);
        controller.newer_request
    });
    window.set_focus_newer_loading(true);
    window.set_history_error("".into());
    let task_live = live.clone();
    let weak = window.as_weak();
    runtime.spawn(async move {
        let manager = &task_live.manager;
        let newer = manager.paginate_focused_selected(&selection, &view, false, NEWER_PAGE);
        let result = tokio::time::timeout(FOCUS_TIMEOUT, newer).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            // Only the latest request, for the same selection, account, room
            // and shown view, touches the loading state or its result.
            let latest = with(|controller| controller.newer_request == request);
            let shown = task_live.focused_view(&room);
            if !latest
                || !selection.is_current()
                || window.get_active_account_local_id().as_str() != account
                || window.get_active_room_id().as_str() != room
                || !shown.is_some_and(|shown| shown.same_view(&view))
            {
                return;
            }
            window.set_focus_newer_loading(false);
            match result {
                Ok(Ok(true)) => window.invoke_return_to_live(),
                Ok(Ok(false)) => {}
                Ok(Err(_)) | Err(_) => window.set_history_error(NEWER_FAILED.into()),
            }
        });
    });
}

/// SURFACE ENTRY: called by matrix_links::clear, which every admitted route
/// that takes over the window calls first, before its surface's own flag
/// changes (a refused entry returns before it), and by an admitted link
/// click (matrix_links::open). A link's own ends and an older link's stale
/// cleanup never call it (matrix_links::end_link). The held open's original
/// authority is withdrawn for good. No later step of it acts: a scroll
/// retry, the focused view's commit or first rows, or a reveal. The
/// timeline's seeks and thread keeper end too (timeline_viewport::withdraw).
/// Nothing is recaptured; a later open takes its own new authority.
pub(crate) fn surface_entered(window: &AppWindow) {
    crate::message_copy::withdraw(window);
    with(|controller| {
        if let Some((navigation, _)) = controller.navigation.take() {
            navigation.withdraw();
        }
    });
    timeline_viewport::withdraw(window);
}

/// Test builds only: hold `authority` as the latest open's, as
/// `claim_navigation` does for a search hit or activation.
#[cfg(test)]
pub(crate) fn hold_navigation_for_test(authority: NavigationAuthority) {
    with(|controller| controller.claim_navigation(authority, false));
}

/// Load earlier messages pages whichever view is shown, live or focused; a
/// result for the other view no longer applies, nor does a seek in it.
pub(crate) fn reset_history(window: &AppWindow) {
    timeline_viewport::retire_room(window);
    with(|controller| controller.newer_request = controller.newer_request.wrapping_add(1));
    window.set_history_operation(window.get_history_operation().wrapping_add(1));
    window.set_history_loading(false);
    window.set_history_at_start(false);
    window.set_history_error("".into());
    window.set_focus_newer_loading(false);
}

/// An edit being typed: it has no saved draft and cannot be restored over
/// its reloaded baseline. A reply is kept per room (see kept_replies).
fn unsent_edit(window: &AppWindow) -> bool {
    window.get_composer_mode() == 2 && !window.get_composer_text().trim().is_empty()
}

/// The nearby messages of a hit, or `None` once its account selection ended.
async fn load_context(
    manager: &AccountManager,
    opening: &Opening,
) -> Option<Shown<Vec<MessageInfo>>> {
    let selection = opening.selection();
    let (room, event) = (opening.hit.room_id.as_str(), opening.hit.event_id.as_str());
    let context = manager.search_result_context_selected(selection, room, event, CONTEXT_EVENTS);
    let wait = tokio::time::timeout(CONTEXT_TIMEOUT, context);
    Some(match selection.guard(wait).await? {
        Ok(result) => result.map_err(|error| search_failure(error).map(|_| CONTEXT_FAILED)),
        Err(_) => Err(Some(CONTEXT_SLOW)),
    })
}

fn publish_context(window: &AppWindow, opening: &Opening, outcome: Shown<Vec<MessageInfo>>) {
    if !opening.current(window) {
        return;
    }
    window.set_room_search_context_loading(false);
    let messages = match outcome {
        Ok(messages) => messages,
        Err(message) => {
            window.set_room_search_context_error(message.unwrap_or("").into());
            return;
        }
    };
    let hit = &opening.hit;
    let found = position_of(&messages, &hit.event_id).is_some();
    let rows = trim_context(messages, &hit.event_id)
        .into_iter()
        .map(|message| context_item(message, &hit.room_id))
        .collect::<Vec<_>>();
    window.set_room_search_context(ModelRc::new(VecModel::from(rows)));
    if !found {
        // A message gone from its room cannot be revealed in it either.
        with(|controller| abort(controller.reveal_task.take()));
        window.set_room_search_context_notice(CONTEXT_MISSING.into());
    }
}

fn position_of(messages: &[MessageInfo], event_id: &str) -> Option<usize> {
    messages
        .iter()
        .position(|message| message.event_id.as_deref() == Some(event_id))
}

/// Keep at most CONTEXT_BEFORE events before the hit and CONTEXT_AFTER after.
fn trim_context(mut messages: Vec<MessageInfo>, event_id: &str) -> Vec<MessageInfo> {
    let at = position_of(&messages, event_id);
    let end = match at {
        Some(at) => {
            let start = at.saturating_sub(CONTEXT_BEFORE);
            messages = messages.split_off(start);
            at - start + 1 + CONTEXT_AFTER
        }
        None => CONTEXT_BEFORE + 1 + CONTEXT_AFTER,
    };
    messages.truncate(end);
    messages
}

/// Bring the opening's loaded event into the shown conversation's view,
/// highlighted, measured and bounded (timeline_viewport); no row is rebuilt.
/// Every move asks the opening's own admission, and a row that cannot be
/// placed says so (NOT_SHOWN) rather than counting as shown. False when the
/// event is not a loaded row.
fn jump_timeline(window: &AppWindow, live: &Arc<LiveUi>, opening: &Opening) -> bool {
    let reveal = Reveal {
        pane: Pane::Room,
        selection: opening.selection().clone(),
        navigation: opening.navigation,
        security: opening.security,
        event_id: opening.hit.event_id.clone(),
        owner: opening.seek_owner(NOT_SHOWN),
    };
    timeline_viewport::reveal(window, live, reveal)
}

/// Open a thread the way the timeline's thread action does. A root that is
/// not in the loaded conversation is presented once its thread feed loads.
fn open_thread(window: &AppWindow, room_id: &str, root_id: &str) {
    if window.get_active_room_id().as_str() != room_id {
        return;
    }
    if window.get_thread_root_id().as_str() == root_id {
        window.invoke_show_thread_panel();
        return;
    }
    let messages = window.get_messages();
    let root = crate::timeline_model::item_of_event(&messages, root_id).unwrap_or_default();
    window.set_thread_root_id(root_id.into());
    window.set_thread_root_sender(root.sender.clone());
    window.set_thread_root_is_own(root.is_own);
    window.set_thread_root_palette_index(root.sender_palette_index);
    window.set_thread_root_accent(root.accent);
    window.set_thread_root_body(root.body.clone());
    window.set_thread_root_rich_body(root.rich_body);
    window.set_thread_root_rich_accessible_body(root.rich_accessible_body);
    window.set_thread_root_copy_token(root.copy_token);
    window.set_thread_composer_text("".into());
    window.set_thread_send_pending(false);
    window.set_thread_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    window.invoke_show_thread_panel();
    window.invoke_open_thread(root_id.into(), root.sender, root.body);
}

/// Present a thread root that was opened from search before its message was
/// known, from the thread feed's own copy of the root event. A root shown
/// from the conversation already has its sender and is left unchanged.
pub(crate) fn show_thread_root(
    window: &AppWindow,
    messages: &[Arc<MessageInfo>],
    root_id: &str,
    copy_scope: Option<std::rc::Rc<crate::message_copy::CopyScope>>,
) {
    let root = messages
        .iter()
        .find(|row| row.event_id.as_deref() == Some(root_id));
    if window.get_thread_root_sender().is_empty() {
        if let Some(root) = root {
            present_thread_root(window, root);
            crate::message_copy::publish_thread_root(window, Some(root), copy_scope);
        }
    }
}

fn present_thread_root(window: &AppWindow, root: &MessageInfo) {
    let item = crate::live::message_item(root);
    window.set_thread_root_sender(item.sender);
    window.set_thread_root_is_own(item.is_own);
    window.set_thread_root_palette_index(item.sender_palette_index);
    window.set_thread_root_accent(item.accent);
    window.set_thread_root_body(item.body);
    window.set_thread_root_rich_body(item.rich_body);
    window.set_thread_root_rich_accessible_body(item.rich_accessible_body);
}

pub(crate) fn search_time(timestamp_ms: u64) -> String {
    i64::try_from(timestamp_ms)
        .ok()
        .and_then(|timestamp| Local.timestamp_millis_opt(timestamp).single())
        .map(|time| time.format("%b %-d, %-I:%M %p").to_string())
        .unwrap_or_else(|| "Unknown time".into())
}

/// Bounded plain text for untrusted names, or a fixed fallback.
fn plain_or(value: &str, max_chars: usize, fallback: &str) -> String {
    let text = crate::directory_plain_text(value, max_chars);
    if text.is_empty() {
        fallback.to_owned()
    } else {
        text
    }
}

pub(crate) fn result_item(result: MessageSearchResult) -> MessageSearchItem {
    let body = crate::directory_plain_text(&result.body, 500);
    MessageSearchItem {
        room_id: result.room_id.into(),
        event_id: result.event_id.into(),
        sender: plain_or(&result.sender, 100, "Unknown sender").into(),
        time: search_time(result.timestamp_ms).into(),
        preview: body.chars().take(150).collect::<String>().into(),
        body: body.into(),
        room_name: plain_or(&result.room_name, 80, "Unnamed room").into(),
        thread_root: result.thread_root.unwrap_or_default().into(),
        has_attachment: result.has_attachment,
    }
}

fn context_item(info: MessageInfo, room_id: &str) -> MessageSearchItem {
    let body = crate::directory_plain_text(&info.body, 500);
    MessageSearchItem {
        room_id: room_id.into(),
        event_id: info.event_id.unwrap_or_default().into(),
        sender: plain_or(&info.sender, 100, "Unknown sender").into(),
        time: search_time(info.timestamp_ms).into(),
        preview: body.chars().take(150).collect::<String>().into(),
        body: body.into(),
        room_name: "".into(),
        thread_root: "".into(),
        has_attachment: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::MessageSendState;

    fn info(event_id: &str) -> MessageInfo {
        MessageInfo {
            key: event_id.into(),
            event_id: Some(event_id.into()),
            sender: "@casey:example.org".into(),
            body: "context".into(),
            rich_markdown: None,
            rich_accessible: None,
            attachment_name: None,
            attachment_is_image: false,
            attachment_is_audio: false,
            poll: None,
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own: false,
            is_local_echo: false,
            send_state: MessageSendState::Sent,
        }
    }

    fn timeline(count: usize) -> Vec<MessageInfo> {
        (0..count)
            .map(|index| info(&format!("$e{index}")))
            .collect()
    }

    fn ids(messages: &[MessageInfo]) -> Vec<String> {
        messages
            .iter()
            .map(|message| message.event_id.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn search_result_display_sanitizes_untrusted_text_without_changing_ids() {
        let item = result_item(MessageSearchResult {
            room_id: "!room:example.org".into(),
            room_name: "Plans\u{202e}\nroom".into(),
            event_id: "$event:example.org".into(),
            thread_root: Some("$root:example.org".into()),
            sender: "@alice\u{202e}:example.org".into(),
            body: format!("Hello\n\u{200b}world {}", "x".repeat(600)),
            timestamp_ms: 0,
            relevance_score: 1.0,
            has_attachment: true,
        });
        assert_eq!(item.room_id.as_str(), "!room:example.org");
        assert_eq!(item.event_id.as_str(), "$event:example.org");
        assert!(item.has_attachment);
        assert_eq!(item.thread_root.as_str(), "$root:example.org");
        assert_eq!(item.sender.as_str(), "@alice :example.org");
        assert_eq!(item.room_name.as_str(), "Plans  room");
        assert!(item.body.as_str().starts_with("Hello  world"));
        assert!(item.body.as_str().chars().count() <= 500);
        assert!(item.preview.as_str().chars().count() <= 150);
    }

    #[test]
    fn context_keeps_the_hit_near_the_top_and_stays_bounded() {
        let kept = trim_context(timeline(13), "$e6");
        let expected = ["$e4", "$e5", "$e6", "$e7", "$e8", "$e9", "$e10", "$e11"];
        assert_eq!(ids(&kept), expected);
        let first = trim_context(timeline(4), "$e0");
        assert_eq!(ids(&first), ["$e0", "$e1", "$e2", "$e3"]);
        let missing = trim_context(timeline(13), "$x");
        assert_eq!(missing.len(), CONTEXT_BEFORE + 1 + CONTEXT_AFTER);
    }

    #[test]
    fn date_filters_name_whole_local_days_and_refuse_malformed_text() {
        let (start, end) = local_day("2026-03-14").unwrap();
        // A whole local day: 24 hours, or 23 to 25 across a clock change.
        let length = end - start + 1;
        let hour = 3_600_000;
        assert!((23 * hour..=25 * hour).contains(&length));
        let (next, _) = local_day("2026-03-15").unwrap();
        assert_eq!(next, end + 1);
        for text in [
            "",
            "2026-3-14",
            "2026-3-014",
            "2026-+3-14",
            "2026-03-+4",
            "14-03-2026",
            "2026-02-30",
            "2026-13-01",
            "1969-12-31",
            "2026/03/14",
            "2026-03-14T",
            "two months",
        ] {
            assert_eq!(local_day(text), None, "{text:?} must be refused");
        }
    }

    #[test]
    fn date_filters_clip_only_the_pre_epoch_part_of_a_local_day() {
        let east = chrono::FixedOffset::east_opt(3_600).unwrap();
        let west = chrono::FixedOffset::west_opt(3_600).unwrap();
        assert_eq!(
            local_day_in_zone("1970-01-01", &east),
            Some((0, 23 * 3_600_000 - 1))
        );
        assert_eq!(
            local_day_in_zone("1970-01-01", &west),
            Some((3_600_000, 25 * 3_600_000 - 1))
        );
        let (_, end) = local_day_in_zone("1970-01-01", &east).unwrap();
        let (next, _) = local_day_in_zone("1970-01-02", &east).unwrap();
        assert_eq!(next, end + 1);
    }

    #[test]
    fn date_boundary_finds_a_missing_midnight_without_rounding_the_gap() {
        let date = NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
        let first = date.and_hms_opt(0, 30, 45).unwrap();
        let calls = std::cell::Cell::new(0);
        let resolve = |wall_time: &NaiveDateTime| {
            calls.set(calls.get() + 1);
            (*wall_time >= first).then(|| wall_time.and_utc().timestamp_millis())
        };
        assert_eq!(
            local_day_boundary(date, &resolve),
            Some((first, first.and_utc().timestamp_millis()))
        );
        assert!(calls.get() < 100);
    }

    #[test]
    fn date_boundary_crosses_a_skipped_following_day_and_bounds_missing_time() {
        let date = NaiveDate::from_ymd_opt(2011, 12, 30).unwrap();
        let first = date.succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap();
        let resolve = |wall_time: &NaiveDateTime| {
            (*wall_time >= first).then(|| wall_time.and_utc().timestamp_millis())
        };
        // An end boundary can cross this skipped day. A start boundary's
        // caller refuses it because the returned calendar date is different.
        let (wall_time, timestamp) = local_day_boundary(date, &resolve).unwrap();
        assert_eq!(wall_time, first);
        assert_ne!(wall_time.date(), date);
        assert_eq!(timestamp, first.and_utc().timestamp_millis());
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            local_day_boundary(date, &|_| {
                calls.set(calls.get() + 1);
                None
            }),
            None
        );
        assert_eq!(calls.get(), 2 * 24 * 60 + 1);
    }
}
