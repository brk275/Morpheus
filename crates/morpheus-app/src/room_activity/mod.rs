//! Room activity for the open room: who is typing, who has read an event
//! (Seen by), Mark as read, and this account's own typing notices and
//! automatic read receipts (see `morpheus_matrix::room_activity`).
//!
//! One context exists per account selection and open room. Every result
//! checks its context, the exact selection (A-B-A safe) and the visible
//! account and room; a context change clears the typing footer, the notice
//! and the Seen-by rows synchronously on the UI thread. Outgoing typing and
//! public receipts are off unless chosen in Settings. Typing goes through one
//! worker: composer events only update its state and wake it, so no
//! keystroke starts a task or a request, and turning typing off drops a start
//! not yet sent. Typed text never reaches this module, and nothing here logs
//! names, IDs or server responses.

mod model;
mod reading;

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_config::{Config, ConfigStore};
use morpheus_matrix::{AccountSelection, MAX_RECEIPT_EVENTS, MatrixCoreError, ReceiptScope};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

use crate::{AppWindow, RoomActivity, SeenByRow, live::LiveUi};
use model::{Composer, Preferences, Started, TypingStep};

const MAIN: usize = 0;
const THREAD: usize = 1;
/// How long a Mark as read result stays in the footer.
const NOTICE_TIME: Duration = Duration::from_secs(4);
/// The thread tab of the side panel.
const THREAD_TAB: i32 = 1;

thread_local! {
    // Clears the footer notice. UI thread only.
    static NOTICE_TIMER: Timer = Timer::default();
}

/// The open room of one account selection.
struct Context {
    id: u64,
    selection: AccountSelection,
    room: String,
}

impl Context {
    fn alive(&self) -> bool {
        self.selection.is_current()
    }
}

#[derive(Default)]
struct State {
    prefs: Preferences,
    context: Option<Arc<Context>>,
    /// The thread shown beside the conversation, or empty.
    thread: String,
    /// The conversation page is shown.
    chat_shown: bool,
    /// The room composer and the thread composer.
    composers: [Composer; 2],
    /// A typing start that may be active, the context it was sent for and
    /// when.
    started: Option<(Arc<Context>, Instant)>,
    stopping: bool,
    seen_op: u64,
    /// The Seen-by view shows a thread reply.
    seen_thread: bool,
    /// The running Mark as read.
    marking: Option<u64>,
    mark_op: u64,
    notice_op: u64,
    /// Automatic receipts of the conversation and of the thread.
    views: [reading::View; 2],
    read_op: u64,
}

pub(crate) struct RoomActivityUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    next: AtomicU64,
    state: Mutex<State>,
    /// The incoming typing watcher and the context it watches.
    watcher: Mutex<Option<(u64, JoinHandle<()>)>>,
    seen_task: Mutex<Option<JoinHandle<()>>>,
    /// Wakes the typing worker; any number of composer events coalesce.
    wake: Notify,
    /// Drops a typing start the worker has not sent yet.
    cancel_start: Notify,
}

fn preferences(ui: &AppWindow) -> Preferences {
    let activity = ui.global::<RoomActivity>();
    Preferences {
        send_typing: activity.get_send_typing(),
        public_receipts: activity.get_public_receipts(),
        read_tracking: activity.get_read_tracking(),
    }
}

/// Settings saves and observed config reloads. Turning a choice off takes
/// effect at once: an unsent typing start is dropped and automatic receipts
/// stop.
pub(crate) fn apply_config(ui: &AppWindow, config: &Config) {
    let next = Preferences::from_config(config);
    if preferences(ui) != next {
        let activity = ui.global::<RoomActivity>();
        activity.set_send_typing(next.send_typing);
        activity.set_public_receipts(next.public_receipts);
        activity.set_read_tracking(next.read_tracking);
        activity.invoke_preferences_changed();
    }
}

/// An account or room change: the typing footer, notice and Seen-by rows of
/// the previous context go now, without waiting for a task.
pub(crate) fn clear(ui: &AppWindow) {
    let activity = ui.global::<RoomActivity>();
    activity.set_typing_text("".into());
    activity.set_notice("".into());
    activity.set_seen_open(false);
    activity.set_seen_state(0);
    activity.set_seen_rows(ModelRc::new(VecModel::<SeenByRow>::default()));
    activity.set_seen_summary("".into());
    activity.invoke_reset();
}

/// The command route for "Mark as read" on the open room, the same request
/// as the room menu's. Returns false when no live room is open; true means it
/// was asked for, not that it reached the server.
#[allow(dead_code)] // Bound by the command registry's owner at integration.
pub(crate) fn mark_room_read(ui: &AppWindow) -> bool {
    let room = ui.get_active_room_id();
    if !ui.get_live_session() || room.is_empty() {
        return false;
    }
    ui.global::<RoomActivity>().invoke_mark_read(room);
    true
}

fn thread_shown(ui: &AppWindow) -> bool {
    ui.get_show_side_panel() && ui.get_side_panel_tab() == THREAD_TAB
}

impl RoomActivityUi {
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        store: ConfigStore,
        runtime: &Handle,
    ) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            next: AtomicU64::new(0),
            state: Mutex::new(State {
                prefs: preferences(ui),
                ..State::default()
            }),
            watcher: Mutex::new(None),
            seen_task: Mutex::new(None),
            wake: Notify::new(),
            cancel_start: Notify::new(),
        });
        let activity = ui.global::<RoomActivity>();
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_reset(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
                this.defer_context(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_preferences_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.preferences_changed(&ui);
            }
        });
        let weak = ui.as_weak();
        activity.on_set_preference(move |which, enabled| {
            if let Some(ui) = weak.upgrade() {
                let _ = crate::save_config_change(&ui, &store, |config| match which {
                    0 => config.activity.send_typing = enabled,
                    1 => config.activity.public_read_receipts = enabled,
                    2 => config.activity.read_tracking = enabled,
                    _ => {}
                });
            }
        });
        // Composer events carry only which composer and whether it holds
        // text.
        let this = owner.clone();
        activity.on_composer_changed(move |thread, has_text| {
            this.composer(thread, |composer| {
                composer.has_text = has_text;
                composer.last_edit = has_text.then(Instant::now);
            });
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_composer_focus(move |thread, focused| {
            this.composer(thread, |composer| composer.focused = focused);
            if focused && let Some(ui) = weak.upgrade() {
                // Focus may be what an automatic receipt waited for.
                this.read_recheck(&ui);
            }
        });
        let this = owner.clone();
        activity.on_composer_sent(move |thread| {
            this.composer(thread, |composer| {
                composer.has_text = false;
                composer.last_edit = None;
            });
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_read_view(move |thread, event| {
            if let Some(ui) = weak.upgrade() {
                this.read_view(&ui, thread, event.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_show_seen_by(move |event, thread| {
            if let Some(ui) = weak.upgrade() {
                this.show_seen_by(&ui, event.as_str(), thread);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_close_seen_by(move || {
            if let Some(ui) = weak.upgrade() {
                this.close_seen_by(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        activity.on_mark_read(move |room| {
            if let Some(ui) = weak.upgrade() {
                this.mark_read(&ui, room.as_str());
            }
        });
        owner.runtime.spawn(owner.clone().typing_worker());
        owner
    }

    /// After the window closes: no new start, and the last one is stopped
    /// while the runtime still runs. The server ends it after 4 seconds
    /// otherwise; nothing claims the stop arrived.
    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.lock().stopping = true;
        self.reset(ui);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publishable(&self, context: &Context, ui: &AppWindow) -> bool {
        self.lock()
            .context
            .as_ref()
            .is_some_and(|current| current.id == context.id)
            && context.alive()
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == context.selection.account_id()
            && ui.get_active_room_id().as_str() == context.room
    }

    fn current_context(&self, ui: &AppWindow) -> Option<Arc<Context>> {
        let context = self.lock().context.clone()?;
        self.publishable(&context, ui).then_some(context)
    }

    /// Synchronous: no typing name, notice or Seen-by row of the previous
    /// context survives this call, and no late result can bring one back.
    fn reset(&self, ui: &AppWindow) {
        {
            let mut state = self.lock();
            state.context = None;
            state.thread.clear();
            state.chat_shown = false;
            // A typing claim needs an edit made in the new context.
            for composer in &mut state.composers {
                composer.last_edit = None;
            }
            state.seen_op = state.seen_op.wrapping_add(1);
            state.notice_op = state.notice_op.wrapping_add(1);
            state.views = Default::default();
            state.read_op = state.read_op.wrapping_add(1);
        }
        self.release_watcher();
        self.abort_seen_task();
        reading::stop_timers();
        NOTICE_TIMER.with(Timer::stop);
        let activity = ui.global::<RoomActivity>();
        activity.set_typing_text("".into());
        activity.set_notice("".into());
        activity.set_seen_open(false);
        activity.set_seen_state(0);
        activity.set_seen_rows(ModelRc::new(VecModel::<SeenByRow>::default()));
        activity.set_seen_summary("".into());
        // An unsent start for the old context never goes out; one that may
        // have gone out is stopped by the worker.
        self.cancel_start.notify_waiters();
        self.wake.notify_one();
    }

    // A reset can come before the window shows the next room or account.
    // Recreate the context for whatever is visible once this event finishes.
    fn defer_context(self: &Arc<Self>, ui: &AppWindow) {
        let this = self.clone();
        let _ = ui
            .as_weak()
            .upgrade_in_event_loop(move |ui| this.context_changed(&ui));
    }

    fn context_changed(self: &Arc<Self>, ui: &AppWindow) {
        if self.lock().stopping {
            return;
        }
        let account = ui.get_active_account_local_id().to_string();
        let room = ui.get_active_room_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        let current = self.lock().context.clone();
        let same = current.is_some_and(|context| {
            context.alive() && context.selection.account_id() == account && context.room == room
        });
        if !(live && same) {
            self.reset(ui);
            if !live || account.is_empty() || room.is_empty() {
                return;
            }
            let Some(selection) = self.live.manager.account_selection(&account) else {
                return;
            };
            let id = self.next.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
            let context = Arc::new(Context {
                id,
                selection,
                room,
            });
            self.lock().context = Some(context);
        }
        let chat_shown = ui.get_page() == 0;
        let thread = if chat_shown && thread_shown(ui) {
            ui.get_thread_root_id().to_string()
        } else {
            String::new()
        };
        let (context, thread_changed, close_seen) = {
            let mut state = self.lock();
            let thread_changed = state.thread != thread;
            if thread_changed {
                // A composer removed while focused reports no blur, so a
                // closed thread pane is never taken for an active window.
                let focused = !thread.is_empty() && state.composers[THREAD].focused;
                state.composers[THREAD] = Composer {
                    focused,
                    ..Composer::default()
                };
                state.thread = thread;
                state.views[THREAD] = reading::View::default();
            }
            if !chat_shown {
                state.composers = Default::default();
                state.views = Default::default();
            }
            state.chat_shown = chat_shown;
            let close_seen = !chat_shown || (thread_changed && state.seen_thread);
            (state.context.clone(), thread_changed, close_seen)
        };
        if thread_changed {
            reading::stop_timer(THREAD);
        }
        if close_seen {
            self.close_seen_by(ui);
        }
        match context {
            // Typing names are watched only while the conversation is shown.
            Some(context) if chat_shown => self.ensure_watcher(&context),
            _ => {
                self.release_watcher();
                ui.global::<RoomActivity>().set_typing_text("".into());
            }
        }
        self.wake.notify_one();
        self.read_recheck(ui);
    }

    fn preferences_changed(&self, ui: &AppWindow) {
        let prefs = preferences(ui);
        let typing_off = {
            let mut state = self.lock();
            let typing_off = state.prefs.send_typing && !prefs.send_typing;
            state.prefs = prefs;
            typing_off
        };
        if typing_off {
            self.cancel_start.notify_waiters();
        }
        if !prefs.read_tracking {
            reading::stop_timers();
        }
        self.wake.notify_one();
    }

    fn composer(&self, thread: bool, change: impl FnOnce(&mut Composer)) {
        change(&mut self.lock().composers[usize::from(thread)]);
        self.wake.notify_one();
    }

    /// One bounded receiver for the context's room, until the context ends
    /// or the conversation is hidden. Updates replace the footer; one that
    /// is not renewed within TYPING_EXPIRY is cleared.
    fn ensure_watcher(self: &Arc<Self>, context: &Arc<Context>) {
        let mut watcher = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        if watcher
            .as_ref()
            .is_some_and(|(id, task)| *id == context.id && !task.is_finished())
        {
            return;
        }
        if let Some((_, task)) = watcher.take() {
            task.abort();
        }
        let this = self.clone();
        let watched = context.clone();
        let task = self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let opened = manager
                .typing_watch_selected(&watched.selection, &watched.room)
                .await;
            let Ok(mut watch) = opened else {
                return;
            };
            loop {
                let next = tokio::time::timeout(model::TYPING_EXPIRY, watch.next()).await;
                let (text, closed) = match next {
                    Ok(Some(typing)) => {
                        (model::typing_text(&typing.members, typing.omitted), false)
                    }
                    Ok(None) => (String::new(), true),
                    // Nothing new for a while: never keep an old claim.
                    Err(_) => (String::new(), false),
                };
                this.publish_typing(&watched, text);
                if closed {
                    break;
                }
            }
        });
        *watcher = Some((context.id, task));
    }

    fn release_watcher(&self) {
        let mut watcher = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, task)) = watcher.take() {
            task.abort();
        }
    }

    fn publish_typing(self: &Arc<Self>, context: &Arc<Context>, text: String) {
        let this = self.clone();
        let context = context.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if this.publishable(&context, &ui) && this.lock().chat_shown {
                ui.global::<RoomActivity>().set_typing_text(text.into());
            }
        });
    }

    /// Decides under the state lock; never holds it across a request.
    fn typing_plan(&self, now: Instant) -> (TypingStep, Option<Arc<Context>>, Duration, bool) {
        let unlocked =
            self.live.is_live.load(Ordering::Acquire) && !self.live.security_gate.blocked();
        let state = self.lock();
        let current = state.context.as_ref().filter(|context| context.alive());
        let enabled = unlocked
            && state.prefs.send_typing
            && state.chat_shown
            && !state.stopping
            && current.is_some();
        let started = state.started.as_ref().map(|(context, at)| Started {
            same_target: current.is_some_and(|current| current.id == context.id),
            at: *at,
        });
        let step = model::typing_step(enabled, &state.composers, started, now);
        let target = match step {
            TypingStep::Start => current.cloned(),
            TypingStep::Stop => state.started.as_ref().map(|(context, _)| context.clone()),
            TypingStep::Nothing => None,
        };
        let started_at = state.started.as_ref().map(|(_, at)| *at);
        let wait = model::typing_wait(&state.composers, started_at, now);
        (step, target, wait, state.stopping)
    }

    /// The one typing worker. A start goes out with the account selection of
    /// its context only; a stop goes to the account and room the start was
    /// for, and fails (never switching accounts) once that selection ended,
    /// leaving the server to end the start after its 4 seconds.
    async fn typing_worker(self: Arc<Self>) {
        loop {
            // Registered before deciding, so a disable or switch after the
            // decision still drops the start below.
            let cancelled = self.cancel_start.notified();
            tokio::pin!(cancelled);
            cancelled.as_mut().enable();
            let now = Instant::now();
            let (step, target, wait, stopping) = self.typing_plan(now);
            match (step, target) {
                (TypingStep::Start, Some(context)) => {
                    let manager = self.live.manager.clone();
                    // Recorded before the outcome is known: a start that may
                    // have reached the server is always followed by a stop.
                    self.lock().started = Some((context.clone(), now));
                    let send =
                        manager.send_typing_selected(&context.selection, &context.room, true);
                    tokio::select! {
                        biased;
                        () = &mut cancelled => {}
                        _ = send => {}
                    }
                    continue;
                }
                (TypingStep::Stop, Some(context)) => {
                    self.lock().started = None;
                    let manager = self.live.manager.clone();
                    let _ = manager
                        .send_typing_selected(&context.selection, &context.room, false)
                        .await;
                    continue;
                }
                _ => {}
            }
            if stopping {
                break;
            }
            tokio::select! {
                () = self.wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Public read receipts at or after one event of the conversation or of
    /// the open thread, from this device's store. Never a claim beyond them.
    fn show_seen_by(self: &Arc<Self>, ui: &AppWindow, event: &str, thread: bool) {
        let Some(context) = self.current_context(ui) else {
            return;
        };
        let (rows, scope) = if thread {
            let root = self.lock().thread.clone();
            if root.is_empty() || ui.get_thread_root_id().as_str() != root {
                return;
            }
            (ui.get_thread_messages(), ReceiptScope::Thread(root))
        } else {
            (ui.get_messages(), ReceiptScope::Room)
        };
        let Some(index) = crate::timeline_model::index_of_event(&rows, event) else {
            return;
        };
        let events: Vec<String> = (index..rows.row_count())
            .filter_map(|row| rows.row_data(row))
            .map(|item| item.event_id.to_string())
            .filter(|id| !id.is_empty())
            .take(MAX_RECEIPT_EVENTS)
            .collect();
        let op = {
            let mut state = self.lock();
            state.seen_op = state.seen_op.wrapping_add(1);
            state.seen_thread = thread;
            state.seen_op
        };
        let activity = ui.global::<RoomActivity>();
        activity.set_seen_rows(ModelRc::new(VecModel::<SeenByRow>::default()));
        activity.set_seen_summary("".into());
        let title = if thread {
            "Seen in this thread"
        } else {
            "Seen by"
        };
        activity.set_seen_title(title.into());
        activity.set_seen_state(1);
        activity.set_seen_open(true);
        let this = self.clone();
        let task = self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let seen = manager
                .seen_by_selected(&context.selection, &context.room, &scope, &events)
                .await;
            let shown = seen.ok().map(|seen| model::seen_rows(&seen));
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_seen(&ui, &context, op, shown);
            });
        });
        let mut slot = self.seen_task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = slot.replace(task) {
            previous.abort();
        }
    }

    fn abort_seen_task(&self) {
        let mut slot = self.seen_task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = slot.take() {
            task.abort();
        }
    }

    fn finish_seen(
        &self,
        ui: &AppWindow,
        context: &Context,
        op: u64,
        shown: Option<(Vec<(String, String, String)>, String)>,
    ) {
        if self.lock().seen_op != op || !self.publishable(context, ui) {
            return;
        }
        let activity = ui.global::<RoomActivity>();
        if !activity.get_seen_open() {
            return;
        }
        let Some((rows, summary)) = shown else {
            activity.set_seen_state(3);
            return;
        };
        let rows: Vec<SeenByRow> = rows
            .into_iter()
            .map(|(name, user_id, detail)| SeenByRow {
                name: name.into(),
                user_id: user_id.into(),
                detail: detail.into(),
            })
            .collect();
        activity.set_seen_rows(ModelRc::new(VecModel::from(rows)));
        activity.set_seen_summary(summary.into());
        activity.set_seen_state(2);
    }

    fn close_seen_by(&self, ui: &AppWindow) {
        {
            let mut state = self.lock();
            state.seen_op = state.seen_op.wrapping_add(1);
            state.seen_thread = false;
        }
        self.abort_seen_task();
        let activity = ui.global::<RoomActivity>();
        activity.set_seen_open(false);
        activity.set_seen_state(0);
        activity.set_seen_rows(ModelRc::new(VecModel::<SeenByRow>::default()));
        activity.set_seen_summary("".into());
    }

    /// Mark as read, chosen by the user for one room of the active account:
    /// the private fully-read marker plus a read receipt, public only when
    /// public receipts were chosen. One at a time. Unread counts change only
    /// when sync brings the server's result.
    fn mark_read(self: &Arc<Self>, ui: &AppWindow, room: &str) {
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        if !live || room.is_empty() || self.live.security_gate.blocked() {
            return;
        }
        let account = ui.get_active_account_local_id().to_string();
        let Some(selection) = self.live.manager.account_selection(&account) else {
            return;
        };
        let (op, public) = {
            let mut state = self.lock();
            if state.marking.is_some() || state.stopping {
                return;
            }
            state.mark_op = state.mark_op.wrapping_add(1);
            state.marking = Some(state.mark_op);
            (state.mark_op, state.prefs.public_receipts)
        };
        ui.global::<RoomActivity>().set_marking(true);
        let this = self.clone();
        let room = room.to_owned();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let marked = manager
                .explicit_mark_read_selected(&selection, &room, public)
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_mark(&ui, &selection, op, marked);
            });
        });
    }

    fn finish_mark(
        self: &Arc<Self>,
        ui: &AppWindow,
        selection: &AccountSelection,
        op: u64,
        marked: Result<bool, MatrixCoreError>,
    ) {
        {
            let mut state = self.lock();
            if state.marking != Some(op) {
                return;
            }
            state.marking = None;
        }
        ui.global::<RoomActivity>().set_marking(false);
        if !selection.is_current()
            || ui.get_active_account_local_id().as_str() != selection.account_id()
        {
            return;
        }
        let notice = match marked {
            Ok(true) => "Marked as read. Unread counts update when the server confirms.",
            Ok(false) => "Nothing new to mark as read.",
            Err(MatrixCoreError::SelectedAccountChanged) => return,
            Err(_) => "Could not mark as read. Try again.",
        };
        self.show_notice(ui, notice);
    }

    fn show_notice(self: &Arc<Self>, ui: &AppWindow, notice: &str) {
        let op = {
            let mut state = self.lock();
            state.notice_op = state.notice_op.wrapping_add(1);
            state.notice_op
        };
        ui.global::<RoomActivity>().set_notice(notice.into());
        let this = Arc::downgrade(self);
        let weak = ui.as_weak();
        NOTICE_TIMER.with(|timer| {
            timer.start(TimerMode::SingleShot, NOTICE_TIME, move || {
                let (Some(this), Some(ui)) = (this.upgrade(), weak.upgrade()) else {
                    return;
                };
                if this.lock().notice_op == op {
                    ui.global::<RoomActivity>().set_notice("".into());
                }
            });
        });
    }
}
