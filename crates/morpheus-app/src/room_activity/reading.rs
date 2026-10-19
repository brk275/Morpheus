//! Automatic read receipts, only when read tracking was chosen in Settings:
//! for the event a live conversation (or the open thread) shows as its last
//! row while scrolled to the bottom, once it stayed there for READ_SETTLE,
//! and only while the window is shown, not minimized and has keyboard focus
//! in a composer. A room or cached timeline that is merely open, a view
//! scrolled into older history and a view focused on an older event never
//! move a receipt. Every condition is checked again on the UI thread right
//! before sending. Private unless public receipts were chosen. One request
//! per view at a time, at most every READ_INTERVAL; the SDK only ever moves a
//! receipt forwards.
//!
//! Kept in this one file (plus the two `read-view` reports in the window) so
//! that a single automatic path remains when another receipt worker is
//! integrated; see the handoff.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use slint::{ComponentHandle, Model, Timer, TimerMode};

use super::{Context, MAIN, RoomActivityUi, THREAD, model};
use crate::AppWindow;

thread_local! {
    // One settle timer per view. UI thread only.
    static TIMERS: [Timer; 2] = [Timer::default(), Timer::default()];
}

pub(super) fn stop_timers() {
    TIMERS.with(|timers| timers.iter().for_each(Timer::stop));
}

pub(super) fn stop_timer(slot: usize) {
    TIMERS.with(|timers| timers[slot].stop());
}

/// One view's automatic receipt state.
#[derive(Default)]
pub(super) struct View {
    shown: Option<Shown>,
    /// The last receipt attempted for this view; never shown as delivered.
    sent: Option<(String, Instant)>,
    /// The running request.
    sending: Option<u64>,
}

/// The event a view reported at its bottom, for which context and thread,
/// and since when.
struct Shown {
    context: u64,
    thread: String,
    event: String,
    since: Instant,
}

enum Plan {
    Wait(std::time::Duration),
    Send(Arc<Context>, String, String, bool),
}

impl RoomActivityUi {
    /// The conversation (`thread` false) or the open thread shows `event` as
    /// its last row at the bottom, or stopped doing so (`event` empty).
    pub(super) fn read_view(self: &Arc<Self>, ui: &AppWindow, thread: bool, event: &str) {
        let slot = usize::from(thread);
        {
            let mut state = self.lock();
            let context = state.context.as_ref().map(|context| context.id);
            let scope = if thread {
                state.thread.clone()
            } else {
                String::new()
            };
            let view = &mut state.views[slot];
            match context {
                Some(context) if !event.is_empty() && (!thread || !scope.is_empty()) => {
                    let same = view.shown.as_ref().is_some_and(|shown| {
                        shown.context == context && shown.thread == scope && shown.event == event
                    });
                    if !same {
                        view.shown = Some(Shown {
                            context,
                            thread: scope,
                            event: event.to_owned(),
                            since: Instant::now(),
                        });
                    }
                }
                _ => view.shown = None,
            }
        }
        if event.is_empty() {
            stop_timer(slot);
        } else {
            self.read_check(ui, slot);
        }
    }

    pub(super) fn read_recheck(self: &Arc<Self>, ui: &AppWindow) {
        self.read_check(ui, MAIN);
        self.read_check(ui, THREAD);
    }

    fn read_check(self: &Arc<Self>, ui: &AppWindow, slot: usize) {
        let now = Instant::now();
        let window = ui.window();
        let window_shown = window.is_visible() && !window.is_minimized();
        let live_session = ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && !self.live.security_gate.blocked();
        let rows = if slot == THREAD {
            ui.get_thread_messages()
        } else {
            ui.get_messages()
        };
        let last = rows
            .row_count()
            .checked_sub(1)
            .and_then(|row| rows.row_data(row))
            .map(|item| item.event_id);
        // A view focused on an older event is never the live bottom.
        let live_view = slot == THREAD || !ui.get_conversation_focused();
        let account = ui.get_active_account_local_id();
        let room = ui.get_active_room_id();
        let thread_root = ui.get_thread_root_id();
        let plan = {
            let state = self.lock();
            let view = &state.views[slot];
            let Some(shown) = view.shown.as_ref() else {
                return;
            };
            let current = state.context.as_ref().filter(|context| {
                context.id == shown.context
                    && context.alive()
                    && context.selection.account_id() == account.as_str()
                    && context.room == room.as_str()
            });
            let same_thread = slot == MAIN
                || (state.thread == shown.thread && thread_root.as_str() == shown.thread);
            let gate = model::ReadGate {
                tracking_enabled: state.prefs.read_tracking,
                live_session,
                window_shown,
                focused: state.composers.iter().any(|composer| composer.focused),
                at_bottom: state.chat_shown
                    && live_view
                    && last.as_ref().is_some_and(|last| last.as_str() == shown.event),
                same_scope: current.is_some() && same_thread,
            };
            // A later report, focus or setting change checks again.
            if !model::may_mark_read(&gate) || view.sending.is_some() {
                return;
            }
            let sent = view.sent.as_ref().map(|(event, at)| (event.as_str(), *at));
            let Some(wait) = model::read_wait(shown.since, sent, &shown.event, now) else {
                return;
            };
            match current {
                Some(context) if wait.is_zero() => Plan::Send(
                    context.clone(),
                    shown.thread.clone(),
                    shown.event.clone(),
                    state.prefs.public_receipts,
                ),
                _ => Plan::Wait(wait),
            }
        };
        match plan {
            Plan::Wait(wait) => {
                let this = Arc::downgrade(self);
                let weak = ui.as_weak();
                TIMERS.with(|timers| {
                    timers[slot].start(TimerMode::SingleShot, wait, move || {
                        if let (Some(this), Some(ui)) = (this.upgrade(), weak.upgrade()) {
                            this.read_check(&ui, slot);
                        }
                    });
                });
            }
            Plan::Send(context, thread, event, public) => {
                self.send_read(slot, context, thread, event, public);
            }
        }
    }

    fn send_read(
        self: &Arc<Self>,
        slot: usize,
        context: Arc<Context>,
        thread: String,
        event: String,
        public: bool,
    ) {
        let op = {
            let mut state = self.lock();
            state.read_op = state.read_op.wrapping_add(1);
            let op = state.read_op;
            let view = &mut state.views[slot];
            view.sending = Some(op);
            view.sent = Some((event.clone(), Instant::now()));
            op
        };
        let this = self.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let root = (!thread.is_empty()).then_some(thread.as_str());
            // The outcome is not shown anywhere; a failure is retried only
            // for a newer event.
            let _ = manager
                .send_viewed_receipt_selected(
                    &context.selection,
                    &context.room,
                    root,
                    &event,
                    public,
                )
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                {
                    let mut state = this.lock();
                    let view = &mut state.views[slot];
                    if view.sending != Some(op) {
                        return;
                    }
                    view.sending = None;
                }
                this.read_check(&ui, slot);
            });
        });
    }
}
