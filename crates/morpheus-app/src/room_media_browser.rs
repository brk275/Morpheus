//! The native media browser of the visible room: the Media tab of the
//! details panel, opened from the room header.
//!
//! One scope exists per account selection and room while the tab is shown.
//! History is read only on an explicit request (opening the tab, then Load
//! older or Retry), a bounded load at a time; see
//! `morpheus_matrix::room_media_browser`. A result is shown only while its
//! scope is still the shown one: its account selection (A-B-A safe), the
//! visible account and room and the tab, and the navigation and security
//! epochs taken when its load started. Hiding the tab, a room or account
//! change, a lock or a detach drops every row synchronously and cancels the
//! load. Rows hold only declared file names, types and sizes, times and
//! senders, in memory. Search filters the loaded rows only and says so.
//!
//! Nothing is fetched for a row. Open goes through the window's shared
//! conversation seam; View and Save hand the row's event to the existing
//! image viewer and attachment save, each only on the user's explicit
//! choice and under their own checks and media settings.

use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use morpheus_matrix::{
    AccountManager, AccountSelection, MAX_MEDIA_NAME_CHARS, MAX_MEDIA_SENDER_CHARS,
    MEDIA_LOAD_REQUESTS, MEDIA_PAGE_EVENTS, RoomMediaCancellation, RoomMediaError, RoomMediaItem,
    RoomMediaKind, RoomMediaPage,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, RoomMedia, RoomMediaRow, live::LiveUi};

/// The details panel tab the browser is shown in.
pub(crate) const PANE_TAB: i32 = 5;
/// Items one browser keeps, at most; Load older ends there.
pub(crate) const MAX_LOADED_ITEMS: usize = 1_000;
/// Events one browser looks at in total, at most.
pub(crate) const MAX_EXAMINED: usize = 6_000;
const MAX_QUERY_CHARS: usize = 200;
const SECURITY_BUSY: &str =
    "Finish the account or security step in progress first, then try again.";
const CHANGED: &str = "The room or a security step changed while media loaded. Try again.";
const PREVIEWS_OFF: &str = "Image previews in encrypted rooms are off in Appearance. \
     You can still save the image.";
const ITEMS_CAPPED: &str = "The list reached its limit of 1,000 items. Close it and open it again to start from the newest.";
const EVENTS_CAPPED: &str = "The list reached its read budget of 6,000 messages. Close it and open it again to start from the newest.";
const UNDECRYPTABLE: &str =
    "could not be decrypted on this device, so any media in them is not listed.";

/// One load's answer, as the core gives it.
pub(crate) type Pending<'a> =
    Pin<Box<dyn Future<Output = Result<RoomMediaPage, RoomMediaError>> + Send + 'a>>;

/// What the browser asks of the account core: the selection of the account
/// shown, captured on the UI thread, and one bounded load under it.
pub(crate) trait MediaLoader: Send + Sync + 'static {
    type Selection: Send + Sync + 'static;
    fn selection(&self, account: &str) -> Option<Self::Selection>;
    fn current(&self, selection: &Self::Selection) -> bool;
    fn load<'a>(
        &'a self,
        selection: &'a Self::Selection,
        room: &'a str,
        from: Option<&'a str>,
        remaining_events: usize,
        cancellation: &'a RoomMediaCancellation,
    ) -> Pending<'a>;
}

impl MediaLoader for AccountManager {
    type Selection = AccountSelection;

    fn selection(&self, account: &str) -> Option<AccountSelection> {
        self.account_selection(account)
    }

    fn current(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
    }

    fn load<'a>(
        &'a self,
        selection: &'a AccountSelection,
        room: &'a str,
        from: Option<&'a str>,
        remaining_events: usize,
        cancellation: &'a RoomMediaCancellation,
    ) -> Pending<'a> {
        Box::pin(self.room_media_for_budget(selection, room, from, remaining_events, cancellation))
    }
}

/// Everything one browser loaded, within its bounds.
#[derive(Debug, Default)]
pub(crate) struct Loaded {
    pub(crate) items: Vec<RoomMediaItem>,
    ids: HashSet<String>,
    pub(crate) next: Option<String>,
    pub(crate) reached_start: bool,
    pub(crate) truncated: bool,
    /// Which browser bounds ended the list; neither erases unread history.
    pub(crate) items_capped: bool,
    pub(crate) events_capped: bool,
    pub(crate) examined: usize,
    /// Successful reads plus conservative allowances for failed loads:
    /// a failed load may already have read pages before returning its error.
    budget_spent: usize,
    pub(crate) undecryptable: usize,
    pub(crate) skipped: usize,
    pub(crate) room_encrypted: bool,
    pub(crate) loads: usize,
}

impl Loaded {
    /// Add one load: each event once, within the browser's bounds.
    pub(crate) fn merge(&mut self, page: RoomMediaPage) {
        self.loads += 1;
        self.examined = self.examined.saturating_add(page.examined);
        self.spend_budget(page.examined);
        self.undecryptable = self.undecryptable.saturating_add(page.undecryptable);
        self.skipped = self.skipped.saturating_add(page.skipped);
        self.room_encrypted |= page.room_encrypted;
        // A later page can never fill an earlier unread gap merely by
        // reporting its end. Incomplete coverage is retained for this list.
        self.truncated |= page.truncated;
        self.reached_start = !self.truncated && (self.reached_start || page.reached_start);
        self.next = if self.truncated || self.reached_start {
            None
        } else {
            page.next
        };
        for item in page.items {
            if self.ids.contains(&item.event_id) {
                continue;
            }
            if self.items.len() >= MAX_LOADED_ITEMS {
                self.items_capped = true;
                break;
            }
            if self.ids.insert(item.event_id.clone()) {
                self.items.push(item);
            }
        }
        self.items_capped |= self.items.len() >= MAX_LOADED_ITEMS;
    }

    fn remaining_events(&self) -> usize {
        MAX_EXAMINED.saturating_sub(self.budget_spent)
    }

    fn spend_budget(&mut self, events: usize) {
        self.budget_spent = self.budget_spent.saturating_add(events).min(MAX_EXAMINED);
        self.events_capped |= self.budget_spent >= MAX_EXAMINED;
    }

    fn capped(&self) -> bool {
        self.items_capped || self.events_capped
    }

    pub(crate) fn can_load_older(&self) -> bool {
        self.next.is_some() && !self.capped() && !self.reached_start && !self.truncated
    }

    fn item(&self, event_id: &str) -> Option<&RoomMediaItem> {
        self.items.iter().find(|item| item.event_id == event_id)
    }
}

#[derive(Default)]
struct Inner {
    loaded: Loaded,
    loading: bool,
    /// Where the failed or dropped load read from, for Retry.
    retry: Option<Option<String>>,
    query: String,
    kind: i32,
}

struct Scope<S> {
    id: u64,
    selection: S,
    account: String,
    room: String,
    cancellation: RoomMediaCancellation,
    inner: Mutex<Inner>,
}

impl<S> Scope<S> {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// The epochs a load started in; its result is shown only while both hold.
#[derive(Clone, Copy)]
struct Started {
    navigation: u64,
    security: u64,
    event_allowance: usize,
}

pub(crate) struct RoomMediaBrowser<L: MediaLoader> {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    loader: Arc<L>,
    next: AtomicU64,
    current: Mutex<Option<Arc<Scope<L::Selection>>>>,
}

impl<L: MediaLoader> RoomMediaBrowser<L> {
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: &Handle,
        loader: Arc<L>,
    ) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            loader,
            next: AtomicU64::new(0),
            current: Mutex::new(None),
        });
        let media = ui.global::<RoomMedia>();
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_reset(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
                this.defer_context(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_shown(move |shown| {
            if let Some(ui) = weak.upgrade() {
                this.shown(&ui, shown);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_load_older(move || {
            if let Some(ui) = weak.upgrade() {
                this.load_older(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_retry(move || {
            if let Some(ui) = weak.upgrade() {
                this.retry(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_filter(move |query, kind| {
            if let Some(ui) = weak.upgrade() {
                this.filter(&ui, query.as_str(), kind);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_open(move |id| {
            if let Some(ui) = weak.upgrade() {
                this.open(&ui, id.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_view(move |id| {
            if let Some(ui) = weak.upgrade() {
                this.view(&ui, id.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        media.on_save(move |id| {
            if let Some(ui) = weak.upgrade() {
                this.save(&ui, id.as_str());
            }
        });
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.reset(ui);
    }

    fn pane_visible(ui: &AppWindow) -> bool {
        ui.get_page() == 0 && ui.get_show_side_panel() && ui.get_side_panel_tab() == PANE_TAB
    }

    /// The scope is the shown one: still current here, its selection
    /// current, the session live, its account and room visible and the tab
    /// shown.
    fn publishable(&self, scope: &Scope<L::Selection>, ui: &AppWindow) -> bool {
        self.current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|current| current.id == scope.id)
            && !scope.cancellation.is_cancelled()
            && self.loader.current(&scope.selection)
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == scope.account
            && ui.get_active_room_id().as_str() == scope.room
            && Self::pane_visible(ui)
    }

    fn current_scope(&self, ui: &AppWindow) -> Option<Arc<Scope<L::Selection>>> {
        let scope = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        self.publishable(&scope, ui).then_some(scope)
    }

    /// Synchronous: no row, count, query or notice of the previous scope
    /// survives this call, and its load is cancelled.
    fn reset(&self, ui: &AppWindow) {
        let previous = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(scope) = previous {
            scope.cancellation.cancel();
            *scope.inner() = Inner::default();
        }
        clear_presentation(ui);
    }

    // A reset can come before the window shows the next room or account, or
    // for a switch that is then blocked. Recreate the scope for whatever is
    // visible once this event finishes.
    fn defer_context(self: &Arc<Self>, ui: &AppWindow) {
        let this = self.clone();
        let _ = ui
            .as_weak()
            .upgrade_in_event_loop(move |ui| this.context_changed(&ui));
    }

    fn context_changed(self: &Arc<Self>, ui: &AppWindow) {
        let account = ui.get_active_account_local_id().to_string();
        let room = ui.get_active_room_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        let wanted = live && !account.is_empty() && !room.is_empty() && Self::pane_visible(ui);
        let kept = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|scope| {
                !scope.cancellation.is_cancelled()
                    && self.loader.current(&scope.selection)
                    && scope.account == account
                    && scope.room == room
            });
        if wanted && kept {
            return;
        }
        self.reset(ui);
        if !wanted {
            return;
        }
        // The exact selection of the account shown, captured here.
        let Some(selection) = self.loader.selection(&account) else {
            return;
        };
        let Ok(previous) = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        else {
            return;
        };
        let scope = Arc::new(Scope {
            id: previous + 1,
            selection,
            account,
            room,
            cancellation: RoomMediaCancellation::new(),
            inner: Mutex::new(Inner::default()),
        });
        *self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(scope.clone());
        self.load(ui, &scope, None);
    }

    fn shown(self: &Arc<Self>, ui: &AppWindow, shown: bool) {
        let media = ui.global::<RoomMedia>();
        // Consumed on every visibility change, so a stale request never
        // moves focus on a later reveal.
        let wants_focus = shown && media.get_focus_pending();
        media.set_focus_pending(false);
        if !shown {
            // Dropping private rows needs no publication fence.
            self.reset(ui);
            return;
        }
        self.context_changed(ui);
        if wants_focus {
            media.set_focus_request(media.get_focus_request().wrapping_add(1));
        }
    }

    fn load_older(self: &Arc<Self>, ui: &AppWindow) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        let next = {
            let inner = scope.inner();
            if !inner.loaded.can_load_older() {
                return;
            }
            inner.loaded.next.clone()
        };
        self.load(ui, &scope, next);
    }

    fn retry(self: &Arc<Self>, ui: &AppWindow) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        let Some(from) = scope.inner().retry.clone() else {
            return;
        };
        self.load(ui, &scope, from);
    }

    /// One explicit load of `scope` from `from`, at most one at a time.
    fn load(
        self: &Arc<Self>,
        ui: &AppWindow,
        scope: &Arc<Scope<L::Selection>>,
        from: Option<String>,
    ) {
        if !self.publishable(scope, ui) {
            return;
        }
        let media = ui.global::<RoomMedia>();
        let remaining_events = {
            let mut inner = scope.inner();
            if inner.loading {
                return;
            }
            let remaining = inner.loaded.remaining_events();
            if remaining == 0 || inner.loaded.capped() || inner.loaded.truncated {
                inner.retry = None;
                media.set_can_retry(false);
                publish(ui, &inner, ui.get_image_previews_encrypted_enabled());
                return;
            }
            if self.live.security_gate.blocked() {
                inner.retry = Some(from);
                drop(inner);
                media.set_error(SECURITY_BUSY.into());
                media.set_can_retry(true);
                return;
            }
            inner.loading = true;
            inner.retry = None;
            remaining.min(MEDIA_LOAD_REQUESTS * MEDIA_PAGE_EVENTS as usize)
        };
        let started = Started {
            navigation: self.live.current_navigation_epoch(),
            security: self.live.security_gate.ringing_generation(),
            event_allowance: remaining_events,
        };
        media.set_loading(true);
        media.set_error("".into());
        media.set_can_retry(false);
        media.set_can_load_older(false);
        let this = self.clone();
        let scope = scope.clone();
        self.runtime.spawn(async move {
            let loader = this.loader.clone();
            let (selection, cancellation) = (&scope.selection, &scope.cancellation);
            let read = loader.load(
                selection,
                &scope.room,
                from.as_deref(),
                remaining_events,
                cancellation,
            );
            let result = read.await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish(&ui, &scope, from, started, result);
            });
        });
    }

    fn finish(
        &self,
        ui: &AppWindow,
        scope: &Arc<Scope<L::Selection>>,
        from: Option<String>,
        started: Started,
        result: Result<RoomMediaPage, RoomMediaError>,
    ) {
        if !self.publishable(scope, ui) {
            // Its scope ended: a reset already cleared what it showed.
            return;
        }
        let media = ui.global::<RoomMedia>();
        let mut inner = scope.inner();
        inner.loading = false;
        media.set_loading(false);
        // A room left and come back to, or a security step begun (even one
        // already ended) since the load started: nothing of it is shown.
        let held = self.live.navigation_is_current(started.navigation)
            && self.live.security_gate.ringing_allowed(started.security);
        let error = match result {
            Ok(page) if !held => {
                // Discarded rows still used their known read budget.
                inner.loaded.spend_budget(page.examined);
                Some((CHANGED, true))
            }
            Ok(page) => {
                inner.loaded.merge(page);
                None
            }
            Err(error) => {
                // An error can arrive after earlier pages were read. Keep
                // this bounded allowance spent rather than allowing retries
                // to read an unlimited number of unreported partial pages.
                inner.loaded.spend_budget(started.event_allowance);
                if error == RoomMediaError::Cancelled {
                    return;
                }
                Some((error.text(), error.retryable()))
            }
        };
        if let Some((text, retryable)) = error {
            let retryable = retryable && !inner.loaded.capped() && !inner.loaded.truncated;
            inner.retry = retryable.then_some(from);
            media.set_error(text.into());
            media.set_can_retry(retryable);
        }
        publish(ui, &inner, ui.get_image_previews_encrypted_enabled());
    }

    fn filter(&self, ui: &AppWindow, query: &str, kind: i32) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        let mut inner = scope.inner();
        inner.query = needle(query);
        inner.kind = kind.clamp(0, 4);
        publish(ui, &inner, ui.get_image_previews_encrypted_enabled());
        let media = ui.global::<RoomMedia>();
        media.set_generation(media.get_generation().wrapping_add(1));
    }

    /// The listed item `id` of the shown scope, with the scope.
    fn listed(
        &self,
        ui: &AppWindow,
        id: &str,
    ) -> Option<(Arc<Scope<L::Selection>>, RoomMediaItem)> {
        let scope = self.current_scope(ui)?;
        let item = scope.inner().loaded.item(id)?.clone();
        Some((scope, item))
    }

    fn open(&self, ui: &AppWindow, id: &str) {
        let Some((scope, item)) = self.listed(ui, id) else {
            return;
        };
        let target = crate::message_search::ConversationTarget {
            account_id: scope.account.clone(),
            room_id: scope.room.clone(),
            event_id: Some(item.event_id),
            thread_root: item.thread_root,
        };
        crate::message_search::open_conversation(ui, &self.live, &self.runtime, target);
    }

    fn view(&self, ui: &AppWindow, id: &str) {
        let Some((scope, item)) = self.listed(ui, id) else {
            return;
        };
        if item.kind != RoomMediaKind::Image {
            return;
        }
        let encrypted = scope.inner().loaded.room_encrypted || item.encrypted;
        if !can_view(&item, encrypted, ui.get_image_previews_encrypted_enabled()) {
            ui.global::<RoomMedia>().set_error(PREVIEWS_OFF.into());
            return;
        }
        // The viewer reads the event again under its own checks.
        ui.invoke_preview_image_attachment(item.event_id.into(), item.filename.into());
    }

    fn save(&self, ui: &AppWindow, id: &str) {
        let Some((_, item)) = self.listed(ui, id) else {
            return;
        };
        // The existing save asks where to save and checks the room again.
        ui.invoke_save_attachment(item.event_id.into(), item.filename.into());
    }
}

/// The browser's own reset, for a room or account change, a lock or a
/// detach: its scope ends and its rows go before this returns.
pub(crate) fn reset_window(ui: &AppWindow) {
    ui.global::<RoomMedia>().invoke_reset();
}

/// Every row, count and notice of the browser cleared. UI thread only.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    let media = ui.global::<RoomMedia>();
    media.set_rows(ModelRc::new(VecModel::<RoomMediaRow>::default()));
    media.set_summary("".into());
    media.set_error("".into());
    media.set_loading(false);
    media.set_can_load_older(false);
    media.set_can_retry(false);
    media.set_query("".into());
    media.set_kind(0);
    media.set_generation(media.get_generation().wrapping_add(1));
}

/// The rows the filter keeps and the summary, built on the UI thread.
fn publish(ui: &AppWindow, inner: &Inner, previews_encrypted: bool) {
    let media = ui.global::<RoomMedia>();
    let loaded = &inner.loaded;
    let rows: Vec<RoomMediaRow> = loaded
        .items
        .iter()
        .filter(|item| matches(item, &inner.query, inner.kind))
        .map(|item| {
            let encrypted = loaded.room_encrypted || item.encrypted;
            row(item, can_view(item, encrypted, previews_encrypted))
        })
        .collect();
    let filtered = !inner.query.is_empty() || inner.kind != 0;
    media.set_summary(summary(loaded, rows.len(), filtered).into());
    media.set_rows(ModelRc::new(VecModel::from(rows)));
    media.set_can_load_older(!inner.loading && loaded.can_load_older());
}

/// Whether View may open the item: an image, and, in an encrypted room or
/// for an encrypted message, only with encrypted-room previews turned on.
pub(crate) fn can_view(item: &RoomMediaItem, encrypted: bool, previews_encrypted: bool) -> bool {
    item.kind == RoomMediaKind::Image && (!encrypted || previews_encrypted)
}

pub(crate) fn row(item: &RoomMediaItem, can_view: bool) -> RoomMediaRow {
    RoomMediaRow {
        id: item.event_id.as_str().into(),
        kind: kind_index(item.kind),
        name: crate::directory_plain_text(&item.filename, MAX_MEDIA_NAME_CHARS).into(),
        detail: detail(item).into(),
        origin: origin(item).into(),
        thread: item.thread_root.is_some(),
        warning: item.authenticity_warning,
        can_view,
    }
}

fn kind_index(kind: RoomMediaKind) -> i32 {
    match kind {
        RoomMediaKind::Image => 0,
        RoomMediaKind::Video => 1,
        RoomMediaKind::Audio => 2,
        RoomMediaKind::File => 3,
    }
}

fn kind_label(kind: RoomMediaKind) -> &'static str {
    match kind {
        RoomMediaKind::Image => "Image",
        RoomMediaKind::Video => "Video",
        RoomMediaKind::Audio => "Audio",
        RoomMediaKind::File => "File",
    }
}

/// "Image · 2.0 KB · image/png": what the sender declared, when known.
fn detail(item: &RoomMediaItem) -> String {
    let mut parts = vec![kind_label(item.kind).to_owned()];
    if let Some(size) = item.size {
        parts.push(size_text(size));
    }
    if !item.mime_type.is_empty() {
        parts.push(crate::directory_plain_text(&item.mime_type, 60));
    }
    parts.join(" · ")
}

/// "Ana · 12 Mar 2026 10:42": the sender and, when known, the time.
fn origin(item: &RoomMediaItem) -> String {
    let name = crate::directory_plain_text(&item.sender_name, MAX_MEDIA_SENDER_CHARS);
    let sender = if name.is_empty() {
        crate::directory_plain_text(&item.sender_id, 120)
    } else {
        name
    };
    if item.timestamp_ms == 0 {
        sender
    } else {
        let time = crate::message_search::search_time(item.timestamp_ms);
        format!("{sender} · {time}")
    }
}

pub(crate) fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Lowercase, bounded search text. Empty matches everything.
pub(crate) fn needle(query: &str) -> String {
    query
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .trim()
        .to_lowercase()
}

/// 0 all, 1 images, 2 videos, 3 audio, 4 files; then the loaded name,
/// sender and type, never anything that was not loaded.
pub(crate) fn matches(item: &RoomMediaItem, needle: &str, kind: i32) -> bool {
    let kind_matches = match kind {
        1 => item.kind == RoomMediaKind::Image,
        2 => item.kind == RoomMediaKind::Video,
        3 => item.kind == RoomMediaKind::Audio,
        4 => item.kind == RoomMediaKind::File,
        _ => true,
    };
    kind_matches
        && (needle.is_empty()
            || [
                item.filename.as_str(),
                item.sender_name.as_str(),
                item.sender_id.as_str(),
                item.mime_type.as_str(),
            ]
            .iter()
            .any(|field| field.to_lowercase().contains(needle)))
}

/// What the list covers, in plain words. It never says the whole history
/// was searched unless the server reported its start, and a search always
/// says that it looked at loaded items only.
pub(crate) fn summary(loaded: &Loaded, shown: usize, filtered: bool) -> String {
    if loaded.loads == 0 && !loaded.capped() {
        return String::new();
    }
    let count = loaded.items.len();
    let read = plural(loaded.examined, "message", "messages");
    let found = if filtered {
        let loaded = plural(count, "loaded item", "loaded items");
        format!("{shown} of {loaded} match. Only loaded items are searched.")
    } else if count == 0 {
        format!("No readable media items were found in the {read} read so far.")
    } else {
        let items = plural(count, "item", "items");
        format!("{items} from the {read} read so far.")
    };
    let reach = if loaded.truncated {
        "This list is incomplete because part of the history could not be read. Close it and open it again."
    } else if loaded.events_capped {
        EVENTS_CAPPED
    } else if loaded.items_capped {
        ITEMS_CAPPED
    } else if loaded.reached_start {
        "This reaches the start of the history this account can see."
    } else {
        "Older media is not loaded yet."
    };
    let mut parts = vec![found, reach.to_owned()];
    if loaded.budget_spent > loaded.examined {
        parts.push("The message count excludes reads that failed or were discarded.".to_owned());
    }
    if loaded.undecryptable > 0 {
        let n = loaded.undecryptable;
        let unread = plural(n, "encrypted message", "encrypted messages");
        parts.push(format!("{unread} {UNDECRYPTABLE}"));
    }
    if loaded.skipped > 0 {
        let unread = plural(loaded.skipped, "message", "messages");
        parts.push(format!(
            "{unread} could not be read, so any media in them is not listed."
        ));
    }
    parts.join(" ")
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// A core stand-in for the production-callback regression in
/// `account_access_ui_tests`: each load waits until released, then answers
/// with the page it was given, as the bounded core load does.
#[cfg(test)]
pub(crate) struct HeldLoader {
    held: tokio::sync::Semaphore,
    page: Mutex<RoomMediaPage>,
    loads: std::sync::atomic::AtomicUsize,
    answered: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl HeldLoader {
    pub(crate) fn new(page: RoomMediaPage) -> Self {
        Self {
            held: tokio::sync::Semaphore::new(0),
            page: Mutex::new(page),
            loads: std::sync::atomic::AtomicUsize::new(0),
            answered: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn release(&self) {
        self.held.add_permits(1);
    }

    pub(crate) async fn wait_loads(&self, count: usize) {
        while self.loads.load(Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    pub(crate) async fn wait_answered(&self, count: usize) {
        while self.answered.load(Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

#[cfg(test)]
impl MediaLoader for HeldLoader {
    type Selection = ();

    fn selection(&self, _: &str) -> Option<()> {
        Some(())
    }

    fn current(&self, _: &()) -> bool {
        true
    }

    fn load<'a>(
        &'a self,
        _: &'a (),
        _: &'a str,
        _: Option<&'a str>,
        remaining_events: usize,
        cancellation: &'a RoomMediaCancellation,
    ) -> Pending<'a> {
        Box::pin(async move {
            assert!(
                remaining_events > 0
                    && remaining_events <= MEDIA_LOAD_REQUESTS * MEDIA_PAGE_EVENTS as usize
            );
            self.loads.fetch_add(1, Ordering::SeqCst);
            if let Ok(permit) = self.held.acquire().await {
                permit.forget();
            }
            let result = if cancellation.is_cancelled() {
                Err(RoomMediaError::Cancelled)
            } else {
                Ok(self.page.lock().unwrap().clone())
            };
            self.answered.fetch_add(1, Ordering::SeqCst);
            result
        })
    }
}

#[cfg(test)]
mod tests;
