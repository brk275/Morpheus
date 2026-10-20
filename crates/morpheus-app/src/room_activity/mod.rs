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
//! keystroke starts a task or a request. A typing start not yet sent is
//! dropped when typing is turned off, the composer empties, is sent or loses
//! focus, the conversation is hidden, or the room or account changes. Read
//! receipts are withdrawn the same way (see reading.rs and Mark as read). A
//! request already handed to the SDK cannot be recalled. Typed text never
//! reaches this module, and nothing here logs names, IDs or server
//! responses.

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
use morpheus_matrix::{
    AccountSelection, ActivityCancellation, MAX_RECEIPT_EVENTS, MatrixCoreError, ReceiptOutcome,
    ReceiptScope,
};
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
    /// The typing start still on its way to the SDK and what withdraws it:
    /// every later input that ends typing, a thread or context replacement,
    /// and its idle deadline.
    pending_start: Option<(u64, ActivityCancellation)>,
    start_op: u64,
    stopping: bool,
    seen_op: u64,
    /// The Seen-by view shows a thread reply.
    seen_thread: bool,
    /// The running Mark as read, what withdraws it, and whether its receipt
    /// is public.
    marking: Option<(u64, ActivityCancellation, bool)>,
    mark_op: u64,
    notice_op: u64,
    /// Automatic receipts of the conversation and of the thread.
    views: [reading::View; 2],
    /// Their last attempts, which outlive a revoked request and the view.
    attempts: [Option<reading::Attempt>; 2],
    read_op: u64,
}

/// Withdraws the typing start still on its way to the SDK, if any.
fn revoke_start(state: &mut State) {
    if let Some((_, cancellation)) = state.pending_start.take() {
        cancellation.cancel();
    }
}

/// Queues a typing start that expires at `deadline`, replacing (and
/// withdrawing) any start still queued.
fn queue_start(state: &mut State, deadline: Instant) -> (u64, ActivityCancellation) {
    revoke_start(state);
    state.start_op = state.start_op.wrapping_add(1);
    let cancellation = ActivityCancellation::until(deadline);
    state.pending_start = Some((state.start_op, cancellation.clone()));
    (state.start_op, cancellation)
}

/// The thread shown beside the conversation became another one or none.
/// The old thread's composer, automatic receipt view and queued typing start
/// go with it. Returns whether it changed.
fn replace_thread(state: &mut State, thread: String) -> bool {
    if state.thread == thread {
        return false;
    }
    // A composer removed while focused reports no blur, so a closed thread
    // pane is never taken for an active window.
    let focused = !thread.is_empty() && state.composers[THREAD].focused;
    state.composers[THREAD] = Composer {
        focused,
        ..Composer::default()
    };
    state.thread = thread;
    state.views[THREAD] = reading::View::default();
    revoke_start(state);
    true
}

/// What the typing worker does next.
enum TypingAction {
    Start {
        context: Arc<Context>,
        op: u64,
        cancellation: ActivityCancellation,
        /// An earlier start for the same context may still be active.
        renewal: bool,
    },
    Stop(Arc<Context>),
    Nothing,
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
        {
            let mut state = self.lock();
            state.stopping = true;
            // Nothing more is handed to the SDK after the window closed.
            if let Some((_, cancellation, _)) = state.marking.as_ref() {
                cancellation.cancel();
            }
        }
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
            // An unsent start for the old context never goes out; one that
            // may have gone out is stopped by the worker.
            revoke_start(&mut state);
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
            // Another thread or none, including a closed thread pane while
            // the conversation stays shown.
            let thread_changed = replace_thread(&mut state, thread);
            if !chat_shown {
                state.composers = Default::default();
                state.views = Default::default();
                // Hidden conversation: an unsent typing start is dropped.
                revoke_start(&mut state);
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

    /// Turning typing off drops an unsent start. Any change to read tracking
    /// or public receipts withdraws both views' automatic requests, which
    /// are then checked again under the new choice; turning public receipts
    /// off also withdraws a public Mark as read not yet handed to the SDK.
    fn preferences_changed(self: &Arc<Self>, ui: &AppWindow) {
        let prefs = preferences(ui);
        {
            let mut state = self.lock();
            let before = state.prefs;
            state.prefs = prefs;
            let receipts_changed = before.read_tracking != prefs.read_tracking
                || before.public_receipts != prefs.public_receipts;
            if receipts_changed {
                reading::revoke_all(&mut state);
            }
            if before.public_receipts
                && !prefs.public_receipts
                && let Some((_, cancellation, true)) = state.marking.as_ref()
            {
                cancellation.cancel();
            }
            if before.send_typing && !prefs.send_typing {
                revoke_start(&mut state);
            }
        }
        if !prefs.read_tracking {
            reading::stop_timers();
        }
        self.wake.notify_one();
        self.read_recheck(ui);
    }

    /// Applies one composer event. Once no composer is both focused and
    /// non-empty, a typing start not yet sent is dropped; once none has
    /// focus, automatic receipts have lost their active-window gate and are
    /// withdrawn.
    fn composer(&self, thread: bool, change: impl FnOnce(&mut Composer)) {
        {
            let mut state = self.lock();
            change(&mut state.composers[usize::from(thread)]);
            if !state.composers.iter().any(|composer| composer.focused) {
                reading::revoke_all(&mut state);
            }
            if !model::start_wanted(&state.composers) {
                revoke_start(&mut state);
            }
        }
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

    /// Decides under the state lock and never holds it across a request. A
    /// start is queued here, under the same lock, so any later input that
    /// ends typing finds and withdraws it.
    fn typing_plan(&self, now: Instant) -> (TypingAction, Duration, bool) {
        let unlocked =
            self.live.is_live.load(Ordering::Acquire) && !self.live.security_gate.blocked();
        let mut guard = self.lock();
        let state = &mut *guard;
        let current = state.context.clone().filter(|context| context.alive());
        let enabled = unlocked
            && state.prefs.send_typing
            && state.chat_shown
            && !state.stopping
            && current.is_some();
        let started = state.started.as_ref().map(|(context, at)| Started {
            same_target: current
                .as_ref()
                .is_some_and(|current| current.id == context.id),
            at: *at,
        });
        let step = model::typing_step(enabled, &state.composers, started, now);
        let started_at = state.started.as_ref().map(|(_, at)| *at);
        let wait = model::typing_wait(&state.composers, started_at, now);
        let deadline = model::typing_deadline(&state.composers, now);
        let action = match (step, current, deadline) {
            (TypingStep::Start, Some(context), Some(deadline)) => {
                let renewal = started.is_some();
                let (op, cancellation) = queue_start(state, deadline);
                // Recorded before the outcome is known: a start that may
                // reach the server is always followed by a stop.
                state.started = Some((context.clone(), now));
                TypingAction::Start {
                    context,
                    op,
                    cancellation,
                    renewal,
                }
            }
            (TypingStep::Stop, ..) => {
                revoke_start(state);
                match state.started.take() {
                    Some((context, _)) => TypingAction::Stop(context),
                    None => TypingAction::Nothing,
                }
            }
            _ => TypingAction::Nothing,
        };
        (action, wait, state.stopping)
    }

    /// The one typing worker. A start goes out with the account selection of
    /// its context only. It is withdrawn unsent when, before it reaches the
    /// SDK, typing stops being wanted, the thread or context is replaced, or
    /// its idle deadline passes (the room lookup can wait on the account
    /// lock). A stop goes to the account and room the start was for; it is
    /// not sent while account access is locked, and fails (never switching
    /// accounts) once that selection ended. A lease already sent then ends
    /// on the server after its 4 seconds; nothing claims a stop arrived.
    async fn typing_worker(self: Arc<Self>) {
        loop {
            let now = Instant::now();
            let (action, wait, stopping) = self.typing_plan(now);
            match action {
                TypingAction::Start {
                    context,
                    op,
                    cancellation,
                    renewal,
                } => {
                    let manager = self.live.manager.clone();
                    let sent = manager
                        .send_typing_selected(
                            &context.selection,
                            &context.room,
                            true,
                            &cancellation,
                        )
                        .await;
                    let mut state = self.lock();
                    if state
                        .pending_start
                        .as_ref()
                        .is_some_and(|(id, _)| *id == op)
                    {
                        state.pending_start = None;
                    }
                    // Withdrawn: nothing reached the SDK, so there is no new
                    // lease to stop; an earlier one (a renewal) still is.
                    let ours = |(started, at): &(Arc<Context>, Instant)| {
                        started.id == context.id && *at == now
                    };
                    if matches!(sent, Ok(ReceiptOutcome::Withdrawn))
                        && !renewal
                        && state.started.as_ref().is_some_and(ours)
                    {
                        state.started = None;
                    }
                    continue;
                }
                TypingAction::Stop(context) => {
                    // Nothing is sent while account access is locked; the
                    // server ends the start within its 4 seconds.
                    if self.live.security_gate.blocked() {
                        continue;
                    }
                    let manager = self.live.manager.clone();
                    let stop = ActivityCancellation::new();
                    let _ = manager
                        .send_typing_selected(&context.selection, &context.room, false, &stop)
                        .await;
                    continue;
                }
                TypingAction::Nothing => {}
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
        if crate::room_addresses::is_open(ui) || crate::room_leave::is_open(ui) {
            return;
        }
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
        crate::matrix_links::clear(ui);
        crate::thread_browser::withdraw(ui);
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
    /// public receipts were chosen. One at a time. Turning public receipts off,
    /// or closing the window, withdraws it until it is handed to the SDK;
    /// leaving the room does not, since the user asked for that room. Unread
    /// counts change only when sync brings the server's result.
    fn mark_read(self: &Arc<Self>, ui: &AppWindow, room: &str) {
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        if !live || room.is_empty() || self.live.security_gate.blocked() {
            return;
        }
        let account = ui.get_active_account_local_id().to_string();
        let Some(selection) = self.live.manager.account_selection(&account) else {
            return;
        };
        let cancellation = ActivityCancellation::new();
        let (op, public) = {
            let mut state = self.lock();
            if state.marking.is_some() || state.stopping {
                return;
            }
            state.mark_op = state.mark_op.wrapping_add(1);
            let (op, public) = (state.mark_op, state.prefs.public_receipts);
            state.marking = Some((op, cancellation.clone(), public));
            (op, public)
        };
        ui.global::<RoomActivity>().set_marking(true);
        let this = self.clone();
        let room = room.to_owned();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let marked = manager
                .explicit_mark_read_selected(&selection, &room, public, &cancellation)
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
        marked: Result<ReceiptOutcome, MatrixCoreError>,
    ) {
        {
            let mut state = self.lock();
            if !state.marking.as_ref().is_some_and(|(id, ..)| *id == op) {
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
            Ok(ReceiptOutcome::Sent) => {
                "Marked as read. Unread counts update when the server confirms."
            }
            Ok(ReceiptOutcome::NotNeeded) => "Nothing new to mark as read.",
            Ok(ReceiptOutcome::Withdrawn) => "Mark as read was stopped before anything was sent.",
            Ok(ReceiptOutcome::Interrupted) => {
                "Mark as read was stopped; it may already have reached the server."
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_or_replacing_the_thread_withdraws_a_queued_typing_start() {
        let deadline = Instant::now() + model::TYPING_IDLE;
        let mut state = State::default();
        state.thread = "$first:example.org".into();
        state.composers[THREAD].focused = true;
        // Another thread replaces the open one while the conversation stays
        // shown: the start queued for typing in the old thread is withdrawn.
        let (_, first) = queue_start(&mut state, deadline);
        assert!(replace_thread(&mut state, "$second:example.org".into()));
        assert!(first.is_cancelled() && state.pending_start.is_none());
        assert!(state.composers[THREAD].focused);
        // Closing the thread pane withdraws it too, and the removed composer
        // no longer counts as focused.
        let (_, second) = queue_start(&mut state, deadline);
        assert!(replace_thread(&mut state, String::new()));
        assert!(second.is_cancelled() && !state.composers[THREAD].focused);
        // The same thread again changes nothing and keeps the queued start.
        let (_, kept) = queue_start(&mut state, deadline);
        assert!(!replace_thread(&mut state, String::new()));
        assert!(!kept.is_cancelled());
        // A newer start replaces, and withdraws, the one still queued.
        let (_, newer) = queue_start(&mut state, deadline);
        assert!(kept.is_cancelled() && !newer.is_cancelled());
        // A queued start also lapses on its own at its idle deadline.
        let (_, lapsed) = queue_start(&mut state, Instant::now());
        assert!(lapsed.is_cancelled());
    }
}
