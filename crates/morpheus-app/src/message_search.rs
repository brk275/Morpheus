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

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use chrono::{Local, TimeZone};
use morpheus_matrix::{
    AccountManager, AccountSelection, HistoryReveal, MatrixCoreError, MessageInfo,
    MessageSearchPage, MessageSearchResult,
};
use slint::{ComponentHandle, Model, ModelRc, Timer, VecModel, Weak};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    AppWindow, MessageItem, MessageSearchItem,
    live::{LiveUi, NavigationAuthority},
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
const JUMP_RETRY: Duration = Duration::from_millis(120);
const JUMP_ATTEMPTS: u8 = 10;
const ARRIVAL_ATTEMPTS: u8 = 5;
/// Events per Load newer page of a focused conversation view.
const NEWER_PAGE: u16 = 50;

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
const THREAD_OLDER: &str =
    "This reply is older than the loaded thread. Use Load earlier replies to reach it.";
const FINDING: &str = "Loading earlier replies to reach this one…";
const REVEAL_FAILED: &str = "Earlier replies could not be loaded to reach this one.";
const FOCUSING: &str = "Opening the conversation around this message…";
const FOCUS_FAILED: &str =
    "The conversation around this message could not be opened, so it is shown here.";
const NEWER_FAILED: &str = "Could not load newer messages. Check sync and retry.";
const KEEP_COMPOSER: &str =
    "Send or cancel the reply or edit in this room before opening a result in another room.";
// Keep in step with MAX_RESULTS.
const RESULT_LIMIT: &str = "showing the first 200; refine the search to narrow them";

/// A result to present, or fixed failure text. `Err(None)` means the account
/// selection ended and nothing is shown.
type Shown<T> = Result<T, Option<&'static str>>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    Room(String),
    All,
}

/// The identity of one query: the account selection and window state it was
/// issued for. Publication requires every part of it to be unchanged.
#[derive(Clone)]
struct Ticket {
    operation: i32,
    selection: AccountSelection,
    account: String,
    scope: Scope,
    query: String,
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

    fn matches(&self, query: &str, scope: Option<&Scope>) -> bool {
        self.query == query && Some(&self.scope) == scope
    }
}

/// The query whose results are shown, its next index cursor and the
/// (room, event) identities already listed, bounded by MAX_RESULTS.
struct Request {
    ticket: Ticket,
    next_offset: Option<usize>,
    unavailable: usize,
    seen: HashSet<(String, String)>,
}

/// One hit chosen for opening, with the IDs that route it.
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

/// One open request: the query it came from, the hit and its context
/// operation, which every later step re-checks, and the authority of the
/// navigation it started, which a newer query, scope, open or reset
/// withdraws.
#[derive(Clone)]
struct Opening {
    ticket: Ticket,
    hit: Hit,
    operation: i32,
    authority: NavigationAuthority,
}

impl Opening {
    fn current(&self, window: &AppWindow) -> bool {
        self.ticket.current(window)
            && window.get_room_search_context_operation() == self.operation
            && self.hit.selected(window)
    }

    /// The hit's room is the open one.
    fn here(&self, window: &AppWindow) -> bool {
        window.get_active_room_id().as_str() == self.hit.room_id
    }
}

/// Search state lives on the UI thread; Tokio tasks only return owned
/// results through the event loop, where this state is read again.
#[derive(Default)]
struct Controller {
    request: Option<Request>,
    query_task: Option<JoinHandle<()>>,
    context_task: Option<JoinHandle<()>>,
    reveal_task: Option<JoinHandle<()>>,
    /// The navigation of the opened hit. It is withdrawn rather than
    /// aborted: an abort could stop it between its commit and rollback.
    navigation: Option<NavigationAuthority>,
}

impl Controller {
    /// End the work of the opened hit: its nearby context, history reveal
    /// and room or focus navigation.
    fn abort_opening(&mut self) {
        abort(self.context_task.take());
        abort(self.reveal_task.take());
        if let Some(navigation) = self.navigation.take() {
            navigation.withdraw();
        }
    }
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

/// Cancel every request and clear all search presentation. Used for account
/// changes and removal, sign-out, and room changes in the open-room scope.
pub(crate) fn reset(ui: &AppWindow) {
    with(|controller| {
        abort(controller.query_task.take());
        controller.abort_opening();
        controller.request = None;
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
/// account; open-room results belong to the room that was left.
pub(crate) fn room_changed(ui: &AppWindow, account_changed: bool) {
    if account_changed || ui.get_room_search_scope() != 1 {
        reset(ui);
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
    let weak = ui.as_weak();
    let edit_live = live.clone();
    let edit_runtime = runtime.clone();
    ui.on_room_search_edited(move |_| {
        if let Some(window) = weak.upgrade() {
            start(&window, &edit_live, &edit_runtime, Some(DEBOUNCE));
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

/// Return with nothing highlighted: highlight the first hit of a finished
/// search for the same query, so a second Return opens it; otherwise search
/// immediately without the typing pause.
fn submit(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    let query = window.get_room_search_query().trim().to_owned();
    let scope = current_scope(window);
    let operation = window.get_room_search_operation();
    let shown = shown_ticket()
        .filter(|ticket| ticket.operation == operation)
        .is_some_and(|ticket| ticket.matches(&query, scope.as_ref()));
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
        abort(controller.query_task.take());
        controller.abort_opening();
        controller.request = None;
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
    if query.is_empty() {
        return;
    }
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
    };
    window.set_room_search_loading(true);
    let weak = window.as_weak();
    let manager = live.manager.clone();
    let task = spawn_page(weak, manager, runtime, ticket.clone(), 0, delay);
    with(|controller| {
        controller.request = Some(Request {
            ticket,
            next_offset: None,
            unavailable: 0,
            seen: HashSet::new(),
        });
        controller.query_task = Some(task);
    });
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
                .search_messages_page_selected(selection, room, query, PAGE_SIZE, offset)
                .await
        }
        Scope::All => {
            manager
                .search_all_messages_page_selected(selection, query, PAGE_SIZE, offset)
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
    window.set_room_search_searched(true);
    let page = match outcome {
        Ok(page) => page,
        Err(message) => {
            // A failed first page shows the error; a failed later page keeps
            // the listed results and their cursor, so Load more can retry.
            window.set_room_search_error(message.unwrap_or("").into());
            return;
        }
    };
    let Some((items, more, unavailable)) = with(|controller| accept_page(controller, page)) else {
        return;
    };
    let total = append_results(window, items, append);
    window.set_room_search_has_more(more);
    window.set_room_search_notice(page_notice(unavailable, total).into());
}

/// Record a page in the shown request: drop repeats by (room, event)
/// identity, stop at MAX_RESULTS and keep the next cursor while room remains.
fn accept_page(
    controller: &mut Controller,
    page: MessageSearchPage,
) -> Option<(Vec<MessageSearchItem>, bool, usize)> {
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
    request.next_offset = page.next_offset;
    if request.seen.len() >= MAX_RESULTS {
        request.next_offset = None;
    }
    Some((items, request.next_offset.is_some(), request.unavailable))
}

fn page_notice(unavailable: usize, total: usize) -> String {
    let mut notice = Vec::new();
    if unavailable == 1 {
        notice.push("1 match could not be loaded".to_owned());
    } else if unavailable > 1 {
        notice.push(format!("{unavailable} matches could not be loaded"));
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
    // A reply or edit has no saved draft; leaving its room would drop the
    // typed text. Keep it and say why the hit did not open.
    let elsewhere = window.get_active_room_id().as_str() != room_id;
    if elsewhere && unsent_reply_or_edit(window) {
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
        controller.navigation = Some(authority.clone());
    });
    let opening = Opening {
        ticket,
        hit,
        operation,
        authority,
    };
    if !elsewhere {
        arrived(window, live, runtime, &opening, false);
        return;
    }
    // Leave the open room the way a room-list click does. Its drafts stay in
    // the draft store and return with the room.
    window.invoke_cancel_room_settings();
    window.invoke_close_image_preview();
    let epoch = live.invalidate_navigation();
    let weak = window.as_weak();
    let task_live = live.clone();
    let handle = runtime.clone();
    runtime.spawn(async move {
        let account = opening.ticket.account.as_str();
        let room = opening.hit.room_id.clone();
        let authority = opening.authority.clone();
        let outcome = task_live
            .reveal_room_for(account, room, epoch, authority, weak.clone())
            .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            navigated(&window, &task_live, &handle, &opening, epoch, outcome);
        });
    });
}

fn navigated(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    opening: &Opening,
    epoch: u64,
    outcome: Result<bool, MatrixCoreError>,
) {
    if !opening.current(window) {
        return;
    }
    let here = opening.here(window);
    match outcome {
        Ok(true) if here && live.navigation_is_current(epoch) => {
            arrived(window, live, runtime, opening, true);
        }
        // Withdrawn or replaced by another navigation; the hit stays listed.
        Ok(_) => window.set_room_search_context_loading(false),
        Err(_) => {
            window.set_room_search_context_loading(false);
            window.set_room_search_context_error(ROOM_NOT_OPENED.into());
        }
    }
}

/// The hit's room is open: show the event, then load its nearby messages.
/// A hit that is not among the loaded messages opens the conversation
/// around it (after a short wait when the room was just opened and its
/// latest messages may still be arriving); a thread hit opens its thread.
fn arrived(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    opening: &Opening,
    moved: bool,
) {
    let hit = &opening.hit;
    if !hit.thread_root.is_empty() {
        open_thread(window, &hit.room_id, &hit.thread_root);
        reveal(window, live, runtime, opening);
    } else if !jump_timeline(window, &hit.event_id) {
        let attempts = if moved { ARRIVAL_ATTEMPTS } else { 0 };
        jump_or_focus(window.as_weak(), live, runtime, opening, attempts);
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

/// Jump to the hit once its row is loaded, checking `attempts` more times;
/// then open the conversation around it instead.
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
    if !opening.current(&window) || !opening.here(&window) {
        return;
    }
    if jump_timeline(&window, &opening.hit.event_id) {
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
    window.set_room_search_context_notice(FOCUSING.into());
    let task_live = live.clone();
    let opening = opening.clone();
    let weak = window.as_weak();
    runtime.spawn(async move {
        let account = opening.ticket.account.as_str();
        let (room, event) = (opening.hit.room_id.clone(), opening.hit.event_id.as_str());
        let authority = opening.authority.clone();
        let focus = task_live.focus_event_for(account, room, event, authority, weak.clone());
        let outcome = tokio::time::timeout(FOCUS_TIMEOUT, focus).await;
        let _ = weak.upgrade_in_event_loop(move |window| focused(&window, &opening, outcome));
    });
}

fn focused(
    window: &AppWindow,
    opening: &Opening,
    outcome: Result<Result<bool, MatrixCoreError>, tokio::time::error::Elapsed>,
) {
    if !opening.current(window) || !opening.here(window) {
        return;
    }
    match outcome {
        Ok(Ok(true)) => {
            reset_history(window);
            window.set_room_search_context_notice("".into());
            jump_when_loaded(window.as_weak(), opening.clone(), JUMP_ATTEMPTS);
        }
        // Withdrawn, or the room or account changed meanwhile.
        Ok(Ok(false)) => window.set_room_search_context_notice("".into()),
        Ok(Err(_)) | Err(_) => window.set_room_search_context_notice(FOCUS_FAILED.into()),
    }
}

/// Page back through the hit's thread, a bounded number of pages, until
/// the reply is loaded. The thread panel shows what was loaded.
fn reveal(window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, opening: &Opening) {
    window.set_room_search_context_notice(FINDING.into());
    let manager = live.manager.clone();
    let opening = opening.clone();
    let weak = window.as_weak();
    let task = runtime.spawn(async move {
        let outcome = reveal_hit(&manager, &opening).await;
        let _ = weak.upgrade_in_event_loop(move |window| revealed(&window, &opening, outcome));
    });
    with(|controller| {
        abort(controller.reveal_task.take());
        controller.reveal_task = Some(task);
    });
}

/// How far the reveal got, or `None` when it failed, timed out or its
/// account selection ended.
async fn reveal_hit(manager: &AccountManager, opening: &Opening) -> Option<HistoryReveal> {
    let selection = &opening.ticket.selection;
    let hit = &opening.hit;
    let thread = Some(hit.thread_root.as_str());
    let (room, event) = (hit.room_id.as_str(), hit.event_id.as_str());
    let reveal = manager.reveal_event_selected(selection, room, thread, event, REVEAL_PAGES);
    let outcome = tokio::time::timeout(REVEAL_TIMEOUT, reveal).await;
    outcome.ok().and_then(Result::ok)
}

fn revealed(window: &AppWindow, opening: &Opening, outcome: Option<HistoryReveal>) {
    if !opening.current(window) || !opening.here(window) {
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
    window.set_room_search_context_notice(notice.into());
}

/// Jump once the hit's row reaches the conversation model.
fn jump_when_loaded(weak: Weak<AppWindow>, opening: Opening, attempts: u8) {
    Timer::single_shot(JUMP_RETRY, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !opening.current(&window) || !opening.here(&window) {
            return;
        }
        if jump_timeline(&window, &opening.hit.event_id) {
            return;
        }
        if attempts > 1 {
            jump_when_loaded(weak, opening, attempts - 1);
        } else {
            window.set_room_search_context_notice(NOT_SHOWN.into());
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
    with(|controller| {
        if let Some(navigation) = controller.navigation.take() {
            navigation.withdraw();
        }
    });
    reset_history(window);
    let live = live.clone();
    let weak = window.as_weak();
    runtime.spawn(async move { live.return_to_live_for(&account, room, weak).await });
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
    window.set_focus_newer_loading(true);
    window.set_history_error("".into());
    let manager = live.manager.clone();
    let weak = window.as_weak();
    runtime.spawn(async move {
        let newer = manager.paginate_focused_selected(&selection, &room, false, NEWER_PAGE);
        let result = newer.await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            if !selection.is_current()
                || window.get_active_account_local_id().as_str() != account
                || window.get_active_room_id().as_str() != room
            {
                return;
            }
            window.set_focus_newer_loading(false);
            match result {
                Ok(true) => window.invoke_return_to_live(),
                Ok(false) => {}
                Err(_) => window.set_history_error(NEWER_FAILED.into()),
            }
        });
    });
}

/// Load earlier messages pages whichever view is shown, live or focused; a
/// result for the other view no longer applies.
fn reset_history(window: &AppWindow) {
    window.set_history_operation(window.get_history_operation().wrapping_add(1));
    window.set_history_loading(false);
    window.set_history_at_start(false);
    window.set_history_error("".into());
    window.set_focus_newer_loading(false);
}

fn unsent_reply_or_edit(window: &AppWindow) -> bool {
    window.get_composer_mode() != 0 && !window.get_composer_text().trim().is_empty()
}

/// The nearby messages of a hit, or `None` once its account selection ended.
async fn load_context(
    manager: &AccountManager,
    opening: &Opening,
) -> Option<Shown<Vec<MessageInfo>>> {
    let selection = &opening.ticket.selection;
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

/// Scroll the live conversation to a loaded event. The row is highlighted by
/// the selected event ID; no row is rebuilt.
fn jump_timeline(window: &AppWindow, event_id: &str) -> bool {
    let messages = window.get_messages();
    let Some(index) = crate::timeline_model::index_of_event(&messages, event_id) else {
        return false;
    };
    window.set_timeline_jump_index(i32::try_from(index).unwrap_or(i32::MAX));
    window.set_timeline_jump_request(window.get_timeline_jump_request().wrapping_add(1));
    true
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
    window.set_thread_composer_text("".into());
    window.set_thread_send_pending(false);
    window.set_thread_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    window.invoke_show_thread_panel();
    window.invoke_open_thread(root_id.into(), root.sender, root.body);
}

/// Present a thread root that was opened from search before its message was
/// known, from the thread feed's own copy of the root event. A root shown
/// from the conversation already has its sender and is left unchanged.
pub(crate) fn show_thread_root(window: &AppWindow, messages: &[Arc<MessageInfo>], root_id: &str) {
    if !window.get_thread_root_sender().is_empty() {
        return;
    }
    let mut rows = messages.iter();
    if let Some(root) = rows.find(|row| row.event_id.as_deref() == Some(root_id)) {
        present_thread_root(window, root);
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

fn search_time(timestamp_ms: u64) -> String {
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
        });
        assert_eq!(item.room_id.as_str(), "!room:example.org");
        assert_eq!(item.event_id.as_str(), "$event:example.org");
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
}
