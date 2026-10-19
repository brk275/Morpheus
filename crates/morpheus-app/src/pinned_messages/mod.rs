//! Native pinned messages for the visible room: the timeline and thread
//! Pin/Unpin actions, the Pins side-panel tab and refresh on room state
//! changes.
//!
//! One scope exists per account selection and room. Every published result
//! checks the scope token, the account selection (A-B-A safe) and the visible
//! account and room. A reset clears every pinned row, flag and filter
//! synchronously on the UI thread. Decrypted pinned text lives only in memory,
//! and only while the pane is open for its scope. Opening a pinned message
//! goes through the window's shared `open-conversation` seam.

mod view;

use std::{
    cell::RefCell,
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, MAX_PINNED_DISPLAY, MessageInfo, PinnedChange, PinnedMessage,
    PinnedMessages, PinnedMessagesCancellation, PinnedMessagesError, PinnedRoomChange,
    PinnedState, room_updates_affect_pinned,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::{runtime::Handle, sync::broadcast::error::RecvError, time};

use crate::{AppWindow, PinnedMessageItem, live::LiveUi};

// A burst of sync batches causes one reread.
const SYNC_COALESCE: Duration = Duration::from_millis(750);
const PANE_TAB: i32 = 3;

thread_local! {
    // Pinned event IDs of the visible room's timeline rows. UI thread only.
    static ROW_PINS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Whether a timeline or thread row is pinned in the visible room. UI thread
/// only.
pub(crate) fn row_pinned(event_id: Option<&str>) -> bool {
    event_id.is_some_and(|id| ROW_PINS.with(|pins| pins.borrow().contains(id)))
}

fn set_row_pins(ui: &AppWindow, next: HashSet<String>) {
    let changed: HashSet<String> = ROW_PINS.with(|pins| {
        let mut pins = pins.borrow_mut();
        let changed = pins.symmetric_difference(&next).cloned().collect();
        *pins = next;
        changed
    });
    if changed.is_empty() {
        return;
    }
    let affected = |info: &MessageInfo| {
        info.event_id
            .as_deref()
            .is_some_and(|id| changed.contains(id))
    };
    crate::timeline_model::refresh_rows(&ui.get_messages(), affected);
    crate::timeline_model::refresh_rows(&ui.get_thread_messages(), affected);
}

/// Update the pane's rows in place, so every row whose event stays keeps its
/// delegate and with it keyboard focus. The first `items.len()` rows end up
/// equal to `items` whatever the previous order.
fn sync_rows(model: &VecModel<PinnedMessageItem>, items: Vec<PinnedMessageItem>) {
    let keep: HashSet<SharedString> = items.iter().map(|item| item.event_id.clone()).collect();
    for index in (0..model.row_count()).rev() {
        if model
            .row_data(index)
            .is_some_and(|row| !keep.contains(&row.event_id))
        {
            model.remove(index);
        }
    }
    let len = items.len();
    for (index, item) in items.into_iter().enumerate() {
        match model.row_data(index) {
            Some(row) if row.event_id == item.event_id => {
                if row != item {
                    model.set_row_data(index, item);
                }
            }
            _ => model.insert(index, item),
        }
    }
    while model.row_count() > len {
        model.remove(model.row_count() - 1);
    }
}

/// Whether the pane lacks rows, or a row for one of the newest shown pins.
fn rows_missing(state: &PinnedState, messages: Option<&[PinnedMessage]>) -> bool {
    let Some(messages) = messages else {
        return true;
    };
    let mut shown = state.event_ids.iter().rev().take(MAX_PINNED_DISPLAY);
    shown.any(|id| !messages.iter().any(|message| &message.event_id == id))
}

enum Loaded {
    State(PinnedState),
    Rows(PinnedMessages),
}

#[derive(Default)]
struct Inner {
    state: Option<PinnedState>,
    // Only while the pane is open for this scope.
    messages: Option<Vec<PinnedMessage>>,
    reading: bool,
    // The running read includes rows.
    reading_rows: bool,
    read: u64,
    // A read requested while another runs; true includes rows.
    queued: Option<bool>,
    // Bumped by every change that may have applied. A read begun before the
    // bump saw the old list, so its result is dropped and the read rerun.
    changes: u64,
    writing: bool,
    // The running change is the pane's Unpin, which removes the focused row.
    focus_after_change: bool,
    // A notice the next successful read resolves, and whether the pane (true)
    // or the timeline notice bar shows it.
    refresh_notice: Option<(&'static str, bool)>,
    filter: String,
}

struct Scope {
    id: u64,
    selection: AccountSelection,
    room: String,
    // Ends this scope's reads and sync watcher. Changes use the owner's
    // token, so navigating within the account does not abandon a change.
    cancellation: PinnedMessagesCancellation,
    inner: Mutex<Inner>,
}
impl Scope {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn alive(&self) -> bool {
        !self.cancellation.is_cancelled() && self.selection.is_current()
    }
}

pub(crate) struct PinnedMessagesUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    next: AtomicU64,
    current: Mutex<Option<Arc<Scope>>>,
    // Cancelled only at shutdown; an account change revokes the selection.
    writes: PinnedMessagesCancellation,
}

impl PinnedMessagesUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            next: AtomicU64::new(0),
            current: Mutex::new(None),
            writes: PinnedMessagesCancellation::new(),
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_pinned_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_reset_pinned_messages(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
                this.defer_context(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_pinned_pane_shown(move |shown| {
            if let Some(ui) = weak.upgrade() {
                this.pane_shown(&ui, shown);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_refresh_pinned_messages(move || {
            if let Some(ui) = weak.upgrade() {
                this.refresh(&ui);
            }
        });
        // Timeline and thread rows pass the account and room they showed.
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_toggle_message_pin(move |account, room, event, pin| {
            if let Some(ui) = weak.upgrade() {
                let target = (account.as_str(), room.as_str(), event.as_str());
                this.change(&ui, target, pin, false);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_unpin_pinned_message(move |event| {
            if let Some(ui) = weak.upgrade() {
                let account = ui.get_active_account_local_id();
                let room = ui.get_active_room_id();
                let target = (account.as_str(), room.as_str(), event.as_str());
                this.change(&ui, target, false, true);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_pinned_filter_edited(move |text| {
            if let Some(ui) = weak.upgrade() {
                this.filter_edited(&ui, text.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_open_pinned_message(move |event, thread| {
            if let Some(ui) = weak.upgrade() {
                this.open(&ui, event.as_str(), thread.as_str());
            }
        });
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.writes.cancel();
        self.reset(ui);
    }

    fn pane_visible(ui: &AppWindow) -> bool {
        ui.get_page() == 0 && ui.get_show_side_panel() && ui.get_side_panel_tab() == PANE_TAB
    }

    fn publishable(&self, scope: &Scope, ui: &AppWindow) -> bool {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|current| current.id == scope.id)
            && scope.alive()
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == scope.selection.account_id()
            && ui.get_active_room_id().as_str() == scope.room
    }

    fn current_scope(&self, ui: &AppWindow) -> Option<Arc<Scope>> {
        let scope = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        self.publishable(&scope, ui).then_some(scope)
    }

    /// Synchronous: no pinned row, flag, filter or notice of the previous
    /// scope survives this call.
    fn reset(&self, ui: &AppWindow) {
        let previous = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(scope) = previous {
            scope.cancellation.cancel();
            let mut inner = scope.inner();
            inner.messages = None;
            inner.state = None;
            inner.refresh_notice = None;
            inner.filter.clear();
        }
        ui.set_pinned_items(ModelRc::new(VecModel::<PinnedMessageItem>::default()));
        ui.set_pinned_status(0);
        ui.set_pinned_error("".into());
        ui.set_pinned_notice("".into());
        ui.set_pinned_summary("".into());
        ui.set_pinned_read_only_note("".into());
        ui.set_pinned_can_edit(false);
        ui.set_pinned_state_loaded(false);
        ui.set_pinned_busy(false);
        ui.set_pinned_filter("".into());
        set_row_pins(ui, HashSet::new());
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
        if live
            && self
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|scope| {
                    scope.alive() && scope.selection.account_id() == account && scope.room == room
                })
        {
            return;
        }
        self.reset(ui);
        if !live || account.is_empty() || room.is_empty() {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account) else {
            return;
        };
        let Ok(id) = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
        else {
            return;
        };
        let scope = Arc::new(Scope {
            id: id + 1,
            selection,
            room,
            cancellation: PinnedMessagesCancellation::new(),
            inner: Mutex::new(Inner::default()),
        });
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(scope.clone());
        let rows = Self::pane_visible(ui);
        self.read(ui, &scope, rows);
        self.runtime.spawn(self.clone().watch(scope));
    }

    fn pane_shown(self: &Arc<Self>, ui: &AppWindow, shown: bool) {
        // Consumed on every visibility change, so a stale request never moves
        // focus on a later reveal.
        let wants_focus = shown && ui.get_pinned_focus_pending();
        ui.set_pinned_focus_pending(false);
        if !shown {
            // Dropping private rows needs no publication fence.
            if let Some(scope) = self
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                let mut inner = scope.inner();
                inner.messages = None;
                inner.refresh_notice = None;
            }
            ui.set_pinned_items(ModelRc::new(VecModel::<PinnedMessageItem>::default()));
            ui.set_pinned_status(0);
            ui.set_pinned_error("".into());
            ui.set_pinned_notice("".into());
            return;
        }
        // Only an explicit open moves focus into the pane's search field.
        if wants_focus {
            ui.set_pinned_focus_request(ui.get_pinned_focus_request().wrapping_add(1));
        }
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        let (in_flight, missing) = {
            let inner = scope.inner();
            (inner.reading && inner.reading_rows, inner.messages.is_none())
        };
        if in_flight {
            // The running rows read publishes now that the pane is open.
            ui.set_pinned_status(1);
        } else if missing {
            self.read(ui, &scope, true);
        }
    }

    fn refresh(self: &Arc<Self>, ui: &AppWindow) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        ui.set_pinned_notice("".into());
        self.read(ui, &scope, Self::pane_visible(ui));
    }

    fn read(self: &Arc<Self>, ui: &AppWindow, scope: &Arc<Scope>, rows: bool) {
        if !self.publishable(scope, ui) {
            return;
        }
        let started = {
            let mut inner = scope.inner();
            if inner.reading {
                inner.queued = Some(inner.queued.unwrap_or(false) || rows);
                None
            } else {
                inner.reading = true;
                inner.reading_rows = rows;
                inner.read = inner.read.wrapping_add(1);
                Some((inner.read, inner.changes))
            }
        };
        if rows {
            ui.set_pinned_status(1);
        }
        let Some((generation, since)) = started else {
            return;
        };
        let this = self.clone();
        let scope = scope.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let result = if rows {
                Box::pin(manager.pinned_messages_for(
                    &scope.selection,
                    &scope.room,
                    &scope.cancellation,
                ))
                .await
                .map(Loaded::Rows)
            } else {
                Box::pin(manager.pinned_state_for(
                    &scope.selection,
                    &scope.room,
                    &scope.cancellation,
                ))
                .await
                .map(Loaded::State)
            };
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_read(&ui, &scope, (generation, since), rows, result);
            });
        });
    }

    fn finish_read(
        self: &Arc<Self>,
        ui: &AppWindow,
        scope: &Arc<Scope>,
        (generation, since): (u64, u64),
        rows: bool,
        result: Result<Loaded, PinnedMessagesError>,
    ) {
        let (mut queued, stale) = {
            let mut inner = scope.inner();
            if inner.read != generation {
                return;
            }
            inner.reading = false;
            inner.reading_rows = false;
            // A change that may have applied after this read began makes its
            // list stale; rerun it from the new baseline instead.
            let stale = inner.changes != since;
            if stale {
                inner.queued = Some(inner.queued.unwrap_or(false) || rows);
            }
            (inner.queued.take(), stale)
        };
        if !self.publishable(scope, ui) {
            return;
        }
        let visible = Self::pane_visible(ui);
        if !stale {
            match result {
                Ok(Loaded::State(state)) => {
                    // Removed pins disappear in project(). Only a newly shown
                    // pin, or a pane without rows, needs the rows fetched.
                    let need_rows = visible && {
                        let inner = scope.inner();
                        rows_missing(&state, inner.messages.as_deref())
                    };
                    self.apply_state(ui, scope, state);
                    Self::resolve_refresh_notice(ui, scope);
                    if need_rows {
                        queued = Some(true);
                    }
                }
                Ok(Loaded::Rows(PinnedMessages { state, messages })) => {
                    if visible {
                        scope.inner().messages = Some(messages);
                        ui.set_pinned_error("".into());
                        ui.set_pinned_status(2);
                    }
                    self.apply_state(ui, scope, state);
                    Self::resolve_refresh_notice(ui, scope);
                }
                Err(PinnedMessagesError::Cancelled | PinnedMessagesError::AccountChanged) => {
                    return;
                }
                Err(error) => {
                    if error == PinnedMessagesError::NotJoined {
                        scope.inner().state = None;
                        ui.set_pinned_can_edit(false);
                        ui.set_pinned_state_loaded(false);
                        ui.set_pinned_summary("".into());
                        ui.set_pinned_read_only_note("".into());
                        set_row_pins(ui, HashSet::new());
                    }
                    if visible || rows {
                        ui.set_pinned_error(view::load_error(error).into());
                        ui.set_pinned_status(3);
                    }
                }
            }
        }
        if let Some(rows) = queued {
            self.read(ui, scope, rows && Self::pane_visible(ui));
        } else if rows && !visible && ui.get_pinned_status() == 1 {
            ui.set_pinned_status(0);
        }
    }

    fn apply_state(&self, ui: &AppWindow, scope: &Scope, state: PinnedState) {
        set_row_pins(ui, state.event_ids.iter().cloned().collect());
        ui.set_pinned_can_edit(state.can_change());
        ui.set_pinned_state_loaded(true);
        ui.set_pinned_summary(view::summary(&state).into());
        ui.set_pinned_read_only_note(view::read_only_note(&state).into());
        scope.inner().state = Some(state);
        self.project(ui, scope);
    }

    // Rows no longer in the authoritative list disappear without a refetch.
    fn project(&self, ui: &AppWindow, scope: &Scope) {
        let items: Vec<PinnedMessageItem> = {
            let inner = scope.inner();
            let Some(messages) = inner.messages.as_ref() else {
                return;
            };
            let can_unpin = inner.state.as_ref().is_some_and(PinnedState::can_change);
            let needle = view::needle(&inner.filter);
            messages
                .iter()
                .filter(|message| {
                    inner
                        .state
                        .as_ref()
                        .is_none_or(|state| state.contains(&message.event_id))
                })
                .filter(|message| view::matches(message, &needle))
                .map(|message| view::item(message, can_unpin))
                .collect()
        };
        let current = ui.get_pinned_items();
        match current
            .as_any()
            .downcast_ref::<VecModel<PinnedMessageItem>>()
        {
            Some(model) => sync_rows(model, items),
            None => ui.set_pinned_items(ModelRc::new(VecModel::from(items))),
        }
    }

    fn filter_edited(&self, ui: &AppWindow, text: &str) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        scope.inner().filter = text.chars().take(view::MAX_FILTER_CHARS).collect();
        self.project(ui, &scope);
    }

    /// Show `text` on one surface: the pane when it is open, otherwise the
    /// timeline's notice bar. Returns true for the pane.
    fn notify(ui: &AppWindow, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        if Self::pane_visible(ui) {
            ui.set_pinned_notice(text.into());
            true
        } else {
            ui.set_transfer_notice(text.into());
            false
        }
    }

    // Clear a "refreshing" notice once a read has refreshed the list, unless
    // a newer notice replaced it.
    fn resolve_refresh_notice(ui: &AppWindow, scope: &Scope) {
        let Some((text, pane)) = scope.inner().refresh_notice.take() else {
            return;
        };
        if pane {
            if ui.get_pinned_notice().as_str() == text {
                ui.set_pinned_notice("".into());
            }
        } else if ui.get_transfer_notice().as_str() == text {
            ui.set_transfer_notice("".into());
        }
    }

    /// One explicit change for the visible room. The core rereads the list
    /// first and keeps every other entry and content field.
    fn change(
        self: &Arc<Self>,
        ui: &AppWindow,
        (account, room, event): (&str, &str, &str),
        pin: bool,
        from_pane: bool,
    ) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        if scope.selection.account_id() != account || scope.room != room || event.is_empty() {
            return;
        }
        let refused = {
            let mut inner = scope.inner();
            let reason = if inner.writing {
                Some("Wait for the current pin change to finish.")
            } else {
                match &inner.state {
                    None => Some("Pinned messages are still loading."),
                    Some(state) if !state.can_change() => Some(view::read_only_note(state)),
                    Some(_) => None,
                }
            };
            if reason.is_none() {
                inner.writing = true;
                inner.focus_after_change = from_pane;
            }
            reason
        };
        if let Some(reason) = refused {
            Self::notify(ui, reason);
            return;
        }
        ui.set_pinned_busy(true);
        ui.set_pinned_notice("".into());
        let this = self.clone();
        let event = event.to_owned();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let result = Box::pin(manager.set_message_pinned_for(
                &scope.selection,
                &scope.room,
                &event,
                pin,
                &this.writes,
            ))
            .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_change(&ui, &scope, pin, result);
            });
        });
    }

    fn finish_change(
        self: &Arc<Self>,
        ui: &AppWindow,
        scope: &Arc<Scope>,
        pin: bool,
        result: Result<PinnedChange, PinnedMessagesError>,
    ) {
        let focus = {
            let mut inner = scope.inner();
            inner.writing = false;
            // The single request may have applied even when it timed out.
            if matches!(
                result,
                Ok(PinnedChange { changed: true, .. })
                    | Err(PinnedMessagesError::TimedOut | PinnedMessagesError::Unavailable)
            ) {
                inner.changes = inner.changes.wrapping_add(1);
            }
            std::mem::take(&mut inner.focus_after_change)
        };
        if !self.publishable(scope, ui) {
            return;
        }
        ui.set_pinned_busy(false);
        let visible = Self::pane_visible(ui);
        match result {
            Ok(PinnedChange { changed, state }) => {
                let notice = view::write_notice(pin, changed, state.is_some());
                let in_pane = Self::notify(ui, notice);
                match state {
                    Some(state) => {
                        self.apply_state(ui, scope, state);
                        // A new pin needs its row.
                        if visible && pin && changed {
                            self.read(ui, scope, true);
                        }
                    }
                    // Accepted, but the reread failed: the list is unknown.
                    None => {
                        scope.inner().refresh_notice = Some((notice, in_pane));
                        self.read(ui, scope, visible);
                    }
                }
            }
            Err(PinnedMessagesError::Cancelled | PinnedMessagesError::AccountChanged) => {}
            Err(error) => {
                Self::notify(ui, view::write_error(error));
                self.read(ui, scope, visible);
            }
        }
        // The pane's Unpin removed the focused row; keep focus in the pane.
        if focus && visible {
            ui.set_pinned_focus_request(ui.get_pinned_focus_request().wrapping_add(1));
        }
    }

    /// Only an entry of the current scope's loaded list can be opened.
    fn open(&self, ui: &AppWindow, event: &str, thread: &str) {
        let Some(scope) = self.current_scope(ui) else {
            return;
        };
        let known = scope.inner().messages.as_ref().is_some_and(|messages| {
            messages.iter().any(|message| {
                message.event_id == event && message.thread_root.as_deref().unwrap_or("") == thread
            })
        });
        if !known {
            return;
        }
        // Integration seam owned by the messaging work; see the handoff.
        ui.invoke_open_conversation(
            scope.selection.account_id().into(),
            scope.room.as_str().into(),
            event.into(),
            thread.into(),
        );
    }

    async fn watch(self: Arc<Self>, scope: Arc<Scope>) {
        let Ok((owner, mut updates)) = self.live.manager.subscribe_room_updates().await else {
            return;
        };
        if owner != scope.selection.account_id() || !scope.alive() {
            return;
        }
        // This account's Matrix user, so only its own membership counts.
        let Some(own) = self
            .live
            .manager
            .active_account()
            .await
            .filter(|account| account.id == scope.selection.account_id())
            .map(|account| account.user_id)
        else {
            return;
        };
        if !scope.alive() {
            return;
        }
        let classify = |update: Result<matrix_sdk::sync::RoomUpdates, RecvError>| match update {
            Ok(update) => {
                let inner = scope.inner();
                let pinned = inner
                    .state
                    .as_ref()
                    .map_or(&[][..], |state| state.event_ids.as_slice());
                Some(room_updates_affect_pinned(
                    &update,
                    &scope.room,
                    &own,
                    pinned,
                ))
            }
            Err(RecvError::Lagged(_)) => Some(PinnedRoomChange {
                state: true,
                content: true,
            }),
            Err(RecvError::Closed) => None,
        };
        loop {
            let mut change = tokio::select! {
                biased;
                _ = scope.cancellation.cancelled() => return,
                _ = scope.selection.revoked() => return,
                update = updates.recv() => match classify(update) {
                    Some(change) => change,
                    None => return,
                },
            };
            if !change.state && !change.content {
                continue;
            }
            let settle = time::sleep(SYNC_COALESCE);
            tokio::pin!(settle);
            loop {
                tokio::select! {
                    biased;
                    _ = scope.cancellation.cancelled() => return,
                    _ = scope.selection.revoked() => return,
                    _ = &mut settle => break,
                    update = updates.recv() => match classify(update) {
                        Some(more) => {
                            change.state |= more.state;
                            change.content |= more.content;
                        }
                        None => return,
                    },
                }
            }
            let this = self.clone();
            let target = scope.clone();
            let weak = self.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if !this.publishable(&target, &ui) {
                    return;
                }
                let visible = Self::pane_visible(&ui);
                // A state change rereads only the list; finish_read fetches
                // rows just for newly shown pins. Rows reload only when a
                // shown pin may have been redacted.
                if change.state || (visible && change.content) {
                    this.read(&ui, &target, visible && change.content);
                }
            });
        }
    }
}

#[cfg(test)]
pub(crate) fn seed_row_pins_for_test(ids: &[&str]) {
    ROW_PINS.with(|pins| *pins.borrow_mut() = ids.iter().map(|id| (*id).to_owned()).collect());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_flags_follow_the_last_published_set() {
        assert!(!row_pinned(Some("$a:example.org")));
        ROW_PINS.with(|pins| {
            *pins.borrow_mut() = ["$a:example.org".to_owned()].into_iter().collect();
        });
        assert!(row_pinned(Some("$a:example.org")));
        assert!(!row_pinned(Some("$b:example.org")));
        assert!(!row_pinned(None));
        ROW_PINS.with(|pins| pins.borrow_mut().clear());
        assert!(!row_pinned(Some("$a:example.org")));
    }

    fn row(id: &str, text: &str) -> PinnedMessageItem {
        PinnedMessageItem {
            event_id: id.into(),
            text: text.into(),
            ..Default::default()
        }
    }
    fn ids(model: &VecModel<PinnedMessageItem>) -> Vec<String> {
        model.iter().map(|row| row.event_id.to_string()).collect()
    }

    #[test]
    fn only_newly_shown_pins_need_a_rows_fetch() {
        let message = |id: &str| PinnedMessage {
            event_id: id.into(),
            kind: morpheus_matrix::PinnedMessageKind::Message,
            sender_id: String::new(),
            sender_name: String::new(),
            timestamp_ms: 0,
            text: String::new(),
            attachment: None,
            thread_root: None,
            edited: false,
            authenticity_warning: false,
        };
        let state = |ids: &[&str]| PinnedState {
            event_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            unsupported_entries: 0,
            permitted: true,
            editable: true,
        };
        let loaded = [message("$a"), message("$b")];
        assert!(rows_missing(&state(&["$a"]), None));
        // A removed pin is hidden by project(); no fetch.
        assert!(!rows_missing(&state(&["$a"]), Some(&loaded[..])));
        assert!(!rows_missing(&state(&["$a", "$b"]), Some(&loaded[..])));
        assert!(rows_missing(&state(&["$a", "$b", "$c"]), Some(&loaded[..])));
        // Pins older than the shown window never trigger a fetch.
        let mut many: Vec<String> = vec!["$old".to_owned()];
        many.extend((0..MAX_PINNED_DISPLAY).map(|i| format!("$e{i}")));
        let shown: Vec<PinnedMessage> = many[1..].iter().map(|id| message(id.as_str())).collect();
        let long = PinnedState {
            event_ids: many,
            ..state(&[])
        };
        assert!(!rows_missing(&long, Some(&shown[..])));
    }

    #[test]
    fn rows_update_in_place_and_end_in_the_requested_order() {
        let model = VecModel::from(vec![row("$a", "a"), row("$b", "b"), row("$c", "c")]);
        sync_rows(&model, vec![row("$c", "c"), row("$b", "b2"), row("$a", "a")]);
        assert_eq!(ids(&model), ["$c", "$b", "$a"]);
        assert_eq!(model.row_data(1).unwrap().text.as_str(), "b2");
        sync_rows(&model, vec![row("$d", "d"), row("$b", "b2")]);
        assert_eq!(ids(&model), ["$d", "$b"]);
        sync_rows(&model, Vec::new());
        assert_eq!(model.row_count(), 0);
    }
}
