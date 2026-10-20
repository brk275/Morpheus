//! The native thread browser: the thread roots of the room on screen, a
//! bounded page at a time, searchable among the loaded roots, each opening
//! its thread through the window's guarded `open-conversation` seam.
//!
//! Pages come from `AccountManager::thread_roots_for` (the account's own
//! homeserver, one bounded request per page, decrypted and projected like
//! pinned messages). The first page is read when the browser opens; any
//! other page is an explicit Load more or Retry, never automatic. A
//! homeserver that does not list threads gets nothing more: the browser then
//! reads, once and with nothing sent, the thread roots this device already
//! holds (`cached_thread_roots_for`) and says that is what they are. Every page
//! is bound to the exact `AccountSelection` and room captured at the click
//! and to an operation number that each open, page and clear advances, so a
//! result for a departed account (A-B-A included), room or browser is
//! dropped. A switch, detach, lock, sign-in or room change clears every
//! decrypted row synchronously and aborts the read in flight.
//!
//! Nothing here is a second thread conversation: choosing a root closes the
//! browser and asks the existing navigation to open that thread, which keeps
//! every draft (each composer edit is already saved for its own thread) and
//! refuses while an edit is being typed elsewhere. No unread count is shown:
//! only what the server's thread summary states.

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, ThreadListError, ThreadRootKind, ThreadRootPage, ThreadRootSummary,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{AppWindow, ThreadBrowser, ThreadBrowserRow, live::LiveUi};

// The panel assertions run from the real-window suite.
#[cfg(test)]
pub(crate) mod tests;

/// Pages one open follows before it stops offering more.
pub(crate) const MAX_PAGES: usize = 20;
/// Roots one open keeps.
pub(crate) const MAX_ROOTS: usize = 500;
pub(crate) const MAX_QUERY_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 120;
const ROW_TEXT_CHARS: usize = 300;
const SENDER_CHARS: usize = 100;
const SENDER_ID_CHARS: usize = 120;
// Above the core's own 30 s bound for a whole page.
const PAGE_TIMEOUT: Duration = Duration::from_secs(40);
/// Said beside roots read from this device because the homeserver does not
/// list threads.
pub(crate) const CACHED_NOTE: &str = "Your homeserver does not list threads. These were found in \
    messages already loaded on this device; the room may have others.";

/// A read for the browser's room: a page from the homeserver, or (`cached`)
/// the roots this device already holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) operation: u64,
    pub(crate) room_id: String,
    pub(crate) from: Option<String>,
    pub(crate) cached: bool,
}

/// The browser's state for one account selection and room. Pure: no Slint,
/// client or task; the UI glue below owns those.
#[derive(Debug)]
pub(crate) struct Browser {
    room_id: String,
    pub(crate) roots: Vec<ThreadRootSummary>,
    seen: HashSet<String>,
    next: Option<String>,
    pub(crate) pages: usize,
    /// The server sent a page without a continuation token: nothing older.
    pub(crate) complete: bool,
    /// The page or root bound was reached with more on the server.
    pub(crate) capped: bool,
    /// The server cut a page short, sent an unusable token or repeated one.
    pub(crate) truncated: bool,
    /// Events the server listed that could not be shown as roots.
    pub(crate) skipped: usize,
    /// The homeserver does not list threads; the roots shown are the ones
    /// this device already held. Nothing more is read.
    pub(crate) cached: bool,
    /// This device's roots were asked for once already.
    cache_tried: bool,
    pending: Option<Request>,
    /// The page that failed, kept for an explicit Retry, and why.
    failed: Option<(Option<String>, ThreadListError)>,
    query: String,
}

impl Browser {
    pub(crate) fn new(room_id: &str) -> Self {
        Self {
            room_id: room_id.to_owned(),
            roots: Vec::new(),
            seen: HashSet::new(),
            next: None,
            pages: 0,
            complete: false,
            capped: false,
            truncated: false,
            skipped: 0,
            cached: false,
            cache_tried: false,
            pending: None,
            failed: None,
            query: String::new(),
        }
    }
    pub(crate) fn room_id(&self) -> &str {
        &self.room_id
    }
    pub(crate) fn loading(&self) -> bool {
        self.pending.is_some()
    }
    pub(crate) fn waits_for(&self, request: &Request) -> bool {
        self.pending.as_ref() == Some(request)
    }
    pub(crate) fn error(&self) -> Option<ThreadListError> {
        self.failed.as_ref().map(|(_, error)| *error)
    }
    // Roots read from this device end the reading: the homeserver lists
    // none. The whole predicate is gated, so a continuation an earlier page
    // offered is never asked for in that mode (it is also dropped on entry).
    fn wants_page(&self) -> bool {
        !self.cached
            && !self.cache_tried
            && (self.pages == 0
                || (!self.complete
                    && !self.capped
                    && self.next.is_some()
                    && self.pages < MAX_PAGES
                    && self.roots.len() < MAX_ROOTS))
    }
    pub(crate) fn can_load_more(&self) -> bool {
        self.pending.is_none() && self.failed.is_none() && self.pages > 0 && self.wants_page()
    }
    pub(crate) fn can_retry(&self) -> bool {
        self.pending.is_none()
            && self
                .failed
                .as_ref()
                .is_some_and(|(_, error)| error.retryable())
    }
    fn begin(&mut self, operation: u64, from: Option<String>, cached: bool) -> Request {
        let request = Request {
            operation,
            room_id: self.room_id.clone(),
            from,
            cached,
        };
        self.pending = Some(request.clone());
        request
    }
    /// The homeserver said it does not list threads before any root was
    /// loaded, and this device's roots were not asked for yet.
    pub(crate) fn wants_cache(&self) -> bool {
        self.pending.is_none()
            && !self.cache_tried
            && self.roots.is_empty()
            && matches!(self.failed, Some((_, ThreadListError::Unsupported)))
    }
    /// The one read of the roots this device already holds, for a
    /// homeserver that does not list threads. Nothing is sent for it.
    pub(crate) fn cache_request(&mut self, operation: u64) -> Option<Request> {
        if !self.wants_cache() {
            return None;
        }
        self.cache_tried = true;
        Some(self.begin(operation, None, true))
    }
    /// The first page, or the next one after a page that offered more. A
    /// failed page is only asked for again by `retry`.
    pub(crate) fn next_request(&mut self, operation: u64) -> Option<Request> {
        if self.pending.is_some() || self.failed.is_some() || !self.wants_page() {
            return None;
        }
        let from = if self.pages == 0 {
            None
        } else {
            self.next.clone()
        };
        Some(self.begin(operation, from, false))
    }
    /// The page that failed, once, for an explicit Retry of a transient
    /// failure. The roots already loaded stay.
    pub(crate) fn retry(&mut self, operation: u64) -> Option<Request> {
        if !self.can_retry() {
            return None;
        }
        let (from, _) = self.failed.take()?;
        Some(self.begin(operation, from, false))
    }
    /// Take the result of the request this browser waits for. Anything
    /// else is ignored and `false` is returned.
    pub(crate) fn apply(
        &mut self,
        request: &Request,
        result: Result<ThreadRootPage, ThreadListError>,
    ) -> bool {
        if !self.waits_for(request) {
            return false;
        }
        self.pending = None;
        if request.cached {
            match result {
                Ok(page) if page.room_id == self.room_id => {
                    self.failed = None;
                    self.cached = true;
                    // The server's continuation ends with its listing.
                    self.next = None;
                    self.skipped = self.skipped.saturating_add(page.skipped);
                    self.truncated |= page.truncated;
                    self.add(page.roots);
                }
                Err(error) if withdraws_access(error) => {
                    self.forget();
                    self.failed = Some((None, error));
                }
                // Any other failed local read leaves the homeserver's answer
                // as it was.
                _ => {}
            }
            return true;
        }
        let page = match result {
            Ok(page) if page.room_id == self.room_id => page,
            Ok(_) => {
                self.failed = Some((request.from.clone(), ThreadListError::Unavailable));
                return true;
            }
            Err(error) => {
                // No longer joined or allowed: what was read before is not
                // shown any more.
                if withdraws_access(error) {
                    self.forget();
                }
                self.failed = Some((request.from.clone(), error));
                return true;
            }
        };
        self.failed = None;
        self.pages += 1;
        self.skipped = self.skipped.saturating_add(page.skipped);
        self.truncated |= page.truncated;
        self.add(page.roots);
        // A server that answers with the token it was given would page
        // forever: stop, and say the list was cut short.
        let repeated = page.next_batch.is_some() && page.next_batch == request.from;
        if repeated {
            self.truncated = true;
            self.next = None;
        } else {
            self.next = page.next_batch;
        }
        self.complete = self.next.is_none() && !page.truncated && !repeated;
        if self.next.is_some() && (self.pages >= MAX_PAGES || self.roots.len() >= MAX_ROOTS) {
            self.capped = true;
        }
        true
    }
    /// Drop every root read so far, and stop offering more.
    fn forget(&mut self) {
        self.roots.clear();
        self.seen.clear();
        self.next = None;
        self.complete = false;
        self.cached = false;
    }
    /// Keep each root once, up to the bound.
    fn add(&mut self, roots: Vec<ThreadRootSummary>) {
        for root in roots {
            if self.roots.len() >= MAX_ROOTS {
                self.capped = true;
                break;
            }
            if self.seen.insert(root.root_id.clone()) {
                self.roots.push(root);
            }
        }
    }
    pub(crate) fn set_query(&mut self, query: &str) {
        self.query = needle(query);
    }
    pub(crate) fn visible(&self) -> impl Iterator<Item = &ThreadRootSummary> {
        self.roots.iter().filter(|root| matches(root, &self.query))
    }
    /// The room to open `root_id` in, only for a root this browser listed.
    pub(crate) fn open_target(&self, root_id: &str) -> Option<&str> {
        (root_id.starts_with('$') && self.roots.iter().any(|root| root.root_id == root_id))
            .then_some(self.room_id.as_str())
    }
    /// The status line. "All" is said only after the server ended the list
    /// with nothing left out and no error.
    pub(crate) fn summary(&self) -> String {
        let loaded = self.roots.len();
        if self.failed.is_some() {
            return match loaded {
                0 => String::new(),
                1 => "1 thread loaded before the error · the list is incomplete".into(),
                n => format!("{n} threads loaded before the error · the list is incomplete"),
            };
        }
        let shown = self.visible().count();
        if self.cached {
            // The device read is a bounded scan, newest first. One that ended
            // early (at the examined, root or byte bound) looked only at the
            // newest messages: it says so, and nothing about what older ones
            // hold.
            let place = if self.truncated {
                "the newest messages loaded on this device"
            } else {
                "messages loaded on this device"
            };
            let mut text = if !self.query.is_empty() {
                format!("{shown} of {loaded} threads found on this device match")
            } else {
                match loaded {
                    0 => format!("No threads found in {place}"),
                    1 => format!("1 thread found in {place}"),
                    n => format!("{n} threads found in {place}"),
                }
            };
            if self.truncated {
                text.push_str(" · older loaded messages were not checked");
            }
            if self.capped {
                text.push_str(" · more than can be listed here");
            }
            match self.skipped {
                0 => {}
                1 => text.push_str(" · 1 could not be shown"),
                n => text.push_str(&format!(" · {n} could not be shown")),
            }
            return text;
        }
        let whole = self.complete && !self.truncated && !self.capped && self.skipped == 0;
        let mut text = if !self.query.is_empty() {
            format!("{shown} of {loaded} loaded threads match")
        } else if whole && loaded == 0 {
            "This room has no threads".into()
        } else if whole {
            match loaded {
                1 => "1 thread, all listed".into(),
                n => format!("{n} threads, all listed"),
            }
        } else {
            match loaded {
                1 => "1 thread loaded".into(),
                n => format!("{n} threads loaded"),
            }
        };
        if self.capped {
            text.push_str(" · more than can be listed here");
        } else if self.next.is_some() {
            text.push_str(" · more on the server");
        }
        match self.skipped {
            0 => {}
            1 => text.push_str(" · 1 could not be shown"),
            n => text.push_str(&format!(" · {n} could not be shown")),
        }
        if self.truncated {
            text.push_str(" · the server's list was cut short");
        }
        text
    }
    /// The error line: a fixed sentence, plus what is still shown.
    pub(crate) fn error_text(&self) -> String {
        if self.cached {
            return CACHED_NOTE.into();
        }
        let Some(error) = self.error() else {
            return String::new();
        };
        if self.roots.is_empty() {
            return error.text().into();
        }
        format!(
            "{} The threads shown were loaded earlier and are not refreshed.",
            error.text()
        )
    }
}

/// The account may no longer read the room: it left, was removed or is not
/// allowed. Roots it read before are dropped, not kept on screen.
fn withdraws_access(error: ThreadListError) -> bool {
    matches!(
        error,
        ThreadListError::NotJoined | ThreadListError::Forbidden
    )
}

/// Lowercase, bounded search text. Empty matches everything.
pub(crate) fn needle(query: &str) -> String {
    query
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .trim()
        .to_lowercase()
}

pub(crate) fn matches(root: &ThreadRootSummary, needle: &str) -> bool {
    needle.is_empty()
        || [
            root.sender_name.as_str(),
            root.sender_id.as_str(),
            root.excerpt.as_str(),
            root.attachment.as_deref().unwrap_or(""),
            status_text(root.kind),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(needle))
}

pub(crate) fn status_text(kind: ThreadRootKind) -> &'static str {
    match kind {
        ThreadRootKind::Message => "",
        ThreadRootKind::Redacted => "This message was deleted.",
        ThreadRootKind::Undecryptable => "This device can't decrypt this message.",
        ThreadRootKind::Unsupported => "Morpheus can't show this kind of message.",
    }
}

/// What the server's summary states about replies; empty when it stated
/// nothing. Never an unread count.
pub(crate) fn latest_text(root: &ThreadRootSummary) -> String {
    let mut parts = Vec::new();
    if let Some(latest) = &root.latest {
        let sender = crate::directory_plain_text(&latest.sender_name, SENDER_CHARS);
        let sender = if sender.is_empty() {
            crate::directory_plain_text(&latest.sender_id, SENDER_ID_CHARS)
        } else {
            sender
        };
        parts.push(if latest.timestamp_ms == 0 {
            format!("Latest reply by {sender}")
        } else {
            format!(
                "Latest reply {} by {sender}",
                crate::message_search::search_time(latest.timestamp_ms)
            )
        });
    }
    match root.replies {
        Some(1) => parts.push("1 reply".into()),
        Some(count) => parts.push(format!("{count} replies")),
        None => {}
    }
    if root.participated {
        // Sent the root or a reply: the summary does not say which.
        parts.push("You took part".into());
    }
    parts.join(" · ")
}

pub(crate) fn row_item(root: &ThreadRootSummary) -> ThreadBrowserRow {
    let sender_id = crate::directory_plain_text(&root.sender_id, SENDER_ID_CHARS);
    let sender = crate::directory_plain_text(&root.sender_name, SENDER_CHARS);
    ThreadBrowserRow {
        id: root.root_id.as_str().into(),
        sender: if sender.is_empty() {
            sender_id.as_str().into()
        } else {
            sender.into()
        },
        sender_id: sender_id.into(),
        time: if root.timestamp_ms == 0 {
            "".into()
        } else {
            crate::message_search::search_time(root.timestamp_ms).into()
        },
        excerpt: crate::directory_plain_text(&root.excerpt, ROW_TEXT_CHARS).into(),
        status: status_text(root.kind).into(),
        attachment: root
            .attachment
            .as_deref()
            .map(|text| crate::directory_plain_text(text, 140))
            .unwrap_or_default()
            .into(),
        latest: latest_text(root).into(),
        warning: root.authenticity_warning,
        edited: root.edited,
    }
}

struct Session {
    selection: AccountSelection,
    browser: Browser,
    // The page read in flight; aborted by every clear.
    task: Option<JoinHandle<()>>,
}

thread_local! {
    // Slint callbacks and every publication run on the UI thread only.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    // Each open, page and clear takes a new number, so nothing started for
    // an earlier one is ever taken.
    static OPERATION: Cell<u64> = const { Cell::new(0) };
    static ROWS: Rc<VecModel<ThreadBrowserRow>> = Rc::new(VecModel::default());
}

fn next_operation() -> u64 {
    OPERATION.with(|operation| {
        let next = operation.get().wrapping_add(1);
        operation.set(next);
        next
    })
}

/// End the browser and forget every root it showed, now. The page read in
/// flight is aborted.
pub(crate) fn clear(ui: &AppWindow) {
    next_operation();
    let session = SESSION.with(|session| session.borrow_mut().take());
    if let Some(task) = session.and_then(|session| session.task) {
        task.abort();
    }
    ROWS.with(|rows| rows.set_vec(Vec::new()));
    let state = ui.global::<ThreadBrowser>();
    state.set_open(false);
    state.set_room_name("".into());
    state.set_summary("".into());
    state.set_error("".into());
    state.set_query("".into());
    state.set_loading(false);
    state.set_can_load_more(false);
    state.set_can_retry(false);
}

/// Whether the browser is open.
pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<ThreadBrowser>().get_open()
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let state = ui.global::<ThreadBrowser>();
    state.set_rows(ROWS.with(|rows| ModelRc::from(rows.clone())));

    let weak = ui.as_weak();
    let (show_live, show_handle) = (live.clone(), handle.clone());
    state.on_show(move || {
        if let Some(ui) = weak.upgrade() {
            show(&ui, &show_live, &show_handle);
        }
    });

    let weak = ui.as_weak();
    let (more_live, more_handle) = (live.clone(), handle.clone());
    state.on_load_more(move || {
        if let Some(ui) = weak.upgrade() {
            step(&ui, &more_live, &more_handle, Browser::next_request);
        }
    });

    let weak = ui.as_weak();
    state.on_retry(move || {
        if let Some(ui) = weak.upgrade() {
            step(&ui, &live, &handle, Browser::retry);
        }
    });

    let weak = ui.as_weak();
    state.on_filter(move |query| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let shown = SESSION.with(|session| {
            let mut session = session.borrow_mut();
            session.as_mut()?.browser.set_query(&query);
            Some(())
        });
        if shown.is_some() {
            publish(&ui);
        }
    });

    let weak = ui.as_weak();
    state.on_choose(move |root_id| {
        if let Some(ui) = weak.upgrade() {
            choose(&ui, &root_id);
        }
    });

    let weak = ui.as_weak();
    state.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            close(&ui);
        }
    });

    let weak = ui.as_weak();
    state.on_context_changed(move || {
        if let Some(ui) = weak.upgrade() {
            context_changed(&ui);
        }
    });

    // A modal or protected surface's own entry, before it opens: end the
    // browser now, whatever it holds, so closing that surface again never
    // brings a held page or retry back.
    let weak = ui.as_weak();
    state.on_withdraw(move || {
        if let Some(ui) = weak.upgrade() {
            clear(&ui);
        }
    });
}

/// The browser may not show anything or act. This reads app-window's
/// `thread-browser-allowed`, so Rust and Slint use the one predicate: the
/// session is live, the conversation page is shown, and nothing else owns
/// the window. That excludes:
/// - sign-in;
/// - the account and security surfaces: access, Manage accounts, the account
///   menu, a pending switch, security work, a recovery key, a verification;
/// - the panels and dialogs: profile, People, member details, the Space
///   explorer and Space rooms, room addresses, room notifications, Seen by,
///   the quick switcher, room actions, room administration (details,
///   permissions, redaction), a member action's confirmation, the capture
///   picker, leaving a call, the image preview, a poll, a shortcut
///   recording, a Matrix link card.
/// The browser's own panel is not among them. The panel is not drawn and
/// its rows are cleared as soon as any of them begins, and each of their
/// entries also withdraws the browser first (`withdraw`, `clear`).
pub(crate) fn held(ui: &AppWindow) -> bool {
    !ui.get_thread_browser_allowed()
}

/// The selection of the account on screen, if it may read now.
fn current_selection(ui: &AppWindow, live: &LiveUi) -> Option<AccountSelection> {
    if !live.is_live.load(Ordering::Acquire) || !ui.get_live_session() || held(ui) {
        return None;
    }
    let account_id = ui.get_active_account_local_id();
    if account_id.is_empty() {
        return None;
    }
    live.manager
        .account_selection(&account_id)
        .filter(AccountSelection::is_current)
}

/// The open session's selection while its account and room are still the
/// ones on screen and nothing holds the window (see [`held`]: another page,
/// a lock, a pending switch, a security key, any dialog or overlay opened
/// since). Anything else clears the browser, now. Every publication, page
/// request and choice goes through this.
fn live_session(ui: &AppWindow) -> Option<AccountSelection> {
    let (selection, room_id) = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        Some((
            session.selection.clone(),
            session.browser.room_id().to_owned(),
        ))
    })?;
    let on_screen = ui.get_live_session()
        && ui.get_active_account_local_id().as_str() == selection.account_id()
        && ui.get_active_room_id().as_str() == room_id;
    if selection.is_current() && on_screen && !held(ui) {
        Some(selection)
    } else {
        clear(ui);
        None
    }
}

fn show(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    // The browser's own entry: a Matrix link card, and a joined link's
    // message still to come (its follow-up), end first, synchronously.
    crate::matrix_links::clear(ui);
    clear(ui);
    let Some(selection) = current_selection(ui, live) else {
        return;
    };
    let room_id = ui.get_active_room_id().to_string();
    if !room_id.starts_with('!') {
        return;
    }
    let name = crate::directory_plain_text(&ui.get_active_room_name(), MAX_TITLE_CHARS);
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            selection,
            browser: Browser::new(&room_id),
            task: None,
        });
    });
    let state = ui.global::<ThreadBrowser>();
    state.set_room_name(if name.is_empty() {
        "This room".into()
    } else {
        name.into()
    });
    // A new list starts at its first row.
    state.set_generation(state.get_generation().wrapping_add(1));
    state.set_open(true);
    step(ui, live, handle, Browser::next_request);
}

/// Close on the browser's own Close or Escape: focus goes back to the
/// header action that opened it.
fn close(ui: &AppWindow) {
    let was_open = is_open(ui);
    clear(ui);
    if was_open {
        let state = ui.global::<ThreadBrowser>();
        state.set_return_focus(state.get_return_focus().wrapping_add(1));
    }
}

/// The account or room on screen changed, or the window was locked: a
/// browser for anything but what is on screen ends now.
fn context_changed(ui: &AppWindow) {
    if SESSION.with(|session| session.borrow().is_some()) {
        let _ = live_session(ui);
    } else if is_open(ui) {
        clear(ui);
    }
}

/// Ask for one page (or, for a homeserver without `/threads`, this device's
/// roots) if `next` produces a request, then show the state. The selection,
/// room and operation are captured here, before any await.
fn step(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    next: impl FnOnce(&mut Browser, u64) -> Option<Request>,
) {
    let Some(selection) = live_session(ui) else {
        return;
    };
    let request = SESSION.with(|session| {
        next(
            &mut session.borrow_mut().as_mut()?.browser,
            next_operation(),
        )
    });
    publish(ui);
    let Some(request) = request else {
        return;
    };
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    let sent = request.clone();
    let (live, task_handle) = (live.clone(), handle.clone());
    let task = handle.spawn(async move {
        let read = async {
            if sent.cached {
                manager
                    .cached_thread_roots_for(&selection, &sent.room_id)
                    .await
            } else {
                manager
                    .thread_roots_for(&selection, &sent.room_id, sent.from.as_deref())
                    .await
            }
        };
        let result = tokio::time::timeout(PAGE_TIMEOUT, read)
            .await
            .unwrap_or(Err(ThreadListError::TimedOut));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish(&ui, &live, &task_handle, &selection, &sent, result);
        });
    });
    SESSION.with(|session| match session.borrow_mut().as_mut() {
        Some(session) if session.browser.waits_for(&request) => session.task = Some(task),
        _ => task.abort(),
    });
}

fn finish(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    selection: &AccountSelection,
    request: &Request,
    result: Result<ThreadRootPage, ThreadListError>,
) {
    // Operation numbers are never reused, so a page for an earlier browser,
    // room or selection is simply not waited for.
    let waiting = SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .is_some_and(|session| session.browser.waits_for(request))
    });
    if !waiting {
        return;
    }
    let departed = matches!(result, Err(ThreadListError::AccountChanged));
    if departed || !selection.is_current() || live_session(ui).is_none() {
        clear(ui);
        return;
    }
    let applied = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let session = session.as_mut()?;
        session.task = None;
        Some(session.browser.apply(request, result))
    });
    if applied != Some(true) {
        return;
    }
    publish(ui);
    // A homeserver without `/threads`: show the roots this device already
    // holds, once, with nothing sent (labelled as such).
    let wants_cache = SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .is_some_and(|session| session.browser.wants_cache())
    });
    if wants_cache {
        step(ui, live, handle, Browser::cache_request);
    }
}

/// Open a listed root's thread through the window's guarded navigation.
/// The browser closes first, so no row outlives the choice.
fn choose(ui: &AppWindow, root_id: &str) {
    // Held (see `held`) clears the browser here instead of opening anything.
    let Some(selection) = live_session(ui) else {
        return;
    };
    let target = SESSION.with(|session| {
        let session = session.borrow();
        Some(session.as_ref()?.browser.open_target(root_id)?.to_owned())
    });
    let Some(room_id) = target else {
        return;
    };
    let account_id = selection.account_id().to_owned();
    // Another thread's composer takes focus as it opens; the thread already
    // open only has its panel shown, so its composer is asked for focus.
    let already_open = ui.get_thread_root_id().as_str() == root_id;
    clear(ui);
    // message_search::open_conversation: the same account opens under its
    // current selection and generation, an edit typed elsewhere refuses the
    // open, the open thread's draft is already saved and the chosen
    // thread's own draft is loaded by the existing open-thread path.
    ui.invoke_open_conversation(account_id.into(), room_id.into(), "".into(), root_id.into());
    if already_open {
        ui.invoke_focus_composer(true);
    }
}

/// Show the browser's state. Rows update in place, so a Load more keeps the
/// list's position and the selected row.
fn publish(ui: &AppWindow) {
    let state = ui.global::<ThreadBrowser>();
    SESSION.with(|session| {
        let session = session.borrow();
        let Some(session) = session.as_ref() else {
            return;
        };
        let browser = &session.browser;
        let rows = browser.visible().map(row_item).collect::<Vec<_>>();
        ROWS.with(|model| sync(model, rows));
        state.set_summary(browser.summary().into());
        state.set_error(browser.error_text().into());
        state.set_loading(browser.loading());
        state.set_can_load_more(browser.can_load_more());
        state.set_can_retry(browser.can_retry());
    });
}

/// Make `model` hold `rows`, changing only the rows that differ.
fn sync(model: &VecModel<ThreadBrowserRow>, rows: Vec<ThreadBrowserRow>) {
    let shared = rows.len().min(model.row_count());
    for (index, row) in rows.iter().enumerate().take(shared) {
        if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for row in rows.into_iter().skip(shared) {
        model.push(row);
    }
}
