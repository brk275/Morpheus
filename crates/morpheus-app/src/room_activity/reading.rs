//! Automatic read receipts, the only automatic receipt sender, and only when
//! read tracking was chosen in Settings: for the event a live conversation
//! (or the open thread) shows as its last row while scrolled to the bottom,
//! once it stayed there for READ_SETTLE, and only while the window is shown,
//! not minimized and has keyboard focus in a composer. A room or cached
//! timeline that is merely open, a view scrolled into older history and a
//! view focused on an older event never move a receipt. Every condition is
//! checked again on the UI thread right before sending. Private unless public
//! receipts were chosen. One request per view at a time, at most every
//! READ_INTERVAL; the SDK only ever moves a receipt forwards.
//!
//! Each request carries an `ActivityCancellation`. A newer bottom event or
//! none, a failed gate check, lost composer focus, a hidden conversation,
//! another room, thread or account, and any change to read tracking or public
//! receipts revoke it at once: a request still waiting for its timeline, or
//! not yet handed to the SDK, is withdrawn and nothing is sent. One already
//! handed to the SDK cannot be recalled; it is only no longer waited for.
//!
//! Kept in this one file (plus the two `read-view` reports in the window).

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use morpheus_matrix::{ActivityCancellation, ReceiptOutcome};
use slint::{ComponentHandle, Model, Timer, TimerMode};

use super::{Context, MAIN, RoomActivityUi, State, THREAD, model};
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

/// Withdraws both views' requests: a gate they depended on was lost.
pub(super) fn revoke_all(state: &mut State) {
    state.views.iter_mut().for_each(View::revoke);
}

/// One view's automatic receipt state. Replacing or dropping it withdraws
/// its request; its last attempt is kept apart, in `State::attempts`.
#[derive(Default)]
pub(super) struct View {
    shown: Option<Shown>,
    /// The running request and what withdraws it.
    sending: Option<(u64, ActivityCancellation)>,
}

impl View {
    /// Nothing more of the running request is handed to the SDK.
    pub(super) fn revoke(&mut self) {
        if let Some((_, cancellation)) = self.sending.take() {
            cancellation.cancel();
        }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// A view's last automatic receipt attempt, kept in `State` apart from the
/// view so that neither a revoked request, a refocus nor a replaced view (a
/// conversation hidden and shown again) lets the same event go out again
/// sooner than READ_INTERVAL. Never shown as delivered.
pub(super) struct Attempt {
    op: u64,
    account: String,
    room: String,
    thread: String,
    event: String,
    at: Instant,
    /// It reached the SDK or failed there, so that event is never sent again.
    settled: bool,
}

/// The event a view reported at its bottom, for which context and thread,
/// and since when.
struct Shown {
    context: u64,
    thread: String,
    event: String,
    since: Instant,
}

/// What one receipt check reads from the window and the current context.
pub(super) struct ReadFacts {
    pub(super) now: Instant,
    pub(super) window_shown: bool,
    /// A live session whose account access is not locked.
    pub(super) live_session: bool,
    /// The event of the view's last row.
    pub(super) last_row: Option<String>,
    /// Not a view focused on an older event.
    pub(super) live_view: bool,
    pub(super) account: String,
    pub(super) room: String,
    pub(super) thread_root: String,
    /// The current context, when it is alive and shows `account` and `room`.
    pub(super) context: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// A gate failed: the running request is withdrawn.
    Revoke,
    /// Nothing to do until the next report, focus or setting change.
    Idle,
    Wait(Duration),
    Send {
        thread: String,
        event: String,
        public: bool,
    },
}

/// One receipt check, from the latest state and facts. The last attempt
/// counts whatever became of it, so revocation or refocus never lets a
/// duplicate skip READ_INTERVAL.
pub(super) fn read_plan(state: &State, slot: usize, facts: &ReadFacts) -> Plan {
    let view = &state.views[slot];
    let Some(shown) = view.shown.as_ref() else {
        return Plan::Idle;
    };
    let same_thread = slot == MAIN
        || (state.thread == shown.thread && facts.thread_root == shown.thread);
    let gate = model::ReadGate {
        tracking_enabled: state.prefs.read_tracking,
        live_session: facts.live_session,
        window_shown: facts.window_shown,
        focused: state.composers.iter().any(|composer| composer.focused),
        at_bottom: state.chat_shown
            && facts.live_view
            && facts.last_row.as_deref() == Some(shown.event.as_str()),
        same_scope: facts.context == Some(shown.context) && same_thread,
    };
    if !model::may_mark_read(&gate) {
        return Plan::Revoke;
    }
    if view.sending.is_some() {
        return Plan::Idle;
    }
    let last = state.attempts[slot]
        .as_ref()
        .filter(|attempt| {
            attempt.account == facts.account
                && attempt.room == facts.room
                && attempt.thread == shown.thread
        })
        .map(|attempt| model::LastAttempt {
            event: &attempt.event,
            at: attempt.at,
            settled: attempt.settled,
        });
    match model::read_wait(shown.since, last, &shown.event, facts.now) {
        None => Plan::Idle,
        Some(wait) if wait.is_zero() => Plan::Send {
            thread: shown.thread.clone(),
            event: shown.event.clone(),
            public: state.prefs.public_receipts,
        },
        Some(wait) => Plan::Wait(wait),
    }
}

/// Where a request goes, as recorded with its attempt.
pub(super) struct Target<'a> {
    pub(super) account: &'a str,
    pub(super) room: &'a str,
    pub(super) thread: &'a str,
    pub(super) event: &'a str,
}

/// Records a new attempt and the cancellation that withdraws its request.
pub(super) fn begin_attempt(
    state: &mut State,
    slot: usize,
    target: &Target<'_>,
    now: Instant,
) -> (u64, ActivityCancellation) {
    let cancellation = ActivityCancellation::new();
    state.read_op = state.read_op.wrapping_add(1);
    let op = state.read_op;
    state.views[slot].sending = Some((op, cancellation.clone()));
    state.attempts[slot] = Some(Attempt {
        op,
        account: target.account.to_owned(),
        room: target.room.to_owned(),
        thread: target.thread.to_owned(),
        event: target.event.to_owned(),
        at: now,
        settled: false,
    });
    (op, cancellation)
}

/// Records what became of an attempt: anything but a withdrawal settles its
/// event, even when its request had been revoked meanwhile. Returns whether
/// it was still the view's running request.
pub(super) fn finish_attempt(state: &mut State, slot: usize, op: u64, withdrawn: bool) -> bool {
    if !withdrawn
        && let Some(attempt) = state.attempts[slot].as_mut()
        && attempt.op == op
    {
        attempt.settled = true;
    }
    let view = &mut state.views[slot];
    if !view.sending.as_ref().is_some_and(|(id, _)| *id == op) {
        return false;
    }
    view.sending = None;
    true
}

/// What a check leads to once the state lock is released.
enum Next {
    Wait(Duration),
    Send(Request),
}

/// One request, decided and recorded under the state lock.
struct Request {
    context: Arc<Context>,
    op: u64,
    cancellation: ActivityCancellation,
    thread: String,
    event: String,
    public: bool,
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
            let reported = match context {
                Some(context) if !event.is_empty() && (!thread || !scope.is_empty()) => {
                    Some((context, scope))
                }
                _ => None,
            };
            let same = match (&reported, &view.shown) {
                (Some((context, scope)), Some(shown)) => {
                    shown.context == *context && shown.thread == *scope && shown.event == event
                }
                _ => false,
            };
            if !same {
                // A newer bottom event, or none: the old one's request goes
                // before anything else happens.
                view.revoke();
                view.shown = reported.map(|(context, thread)| Shown {
                    context,
                    thread,
                    event: event.to_owned(),
                    since: Instant::now(),
                });
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
        let window = ui.window();
        let rows = if slot == THREAD {
            ui.get_thread_messages()
        } else {
            ui.get_messages()
        };
        let last_row = rows
            .row_count()
            .checked_sub(1)
            .and_then(|row| rows.row_data(row))
            .map(|item| item.event_id.to_string());
        let mut facts = ReadFacts {
            now: Instant::now(),
            window_shown: window.is_visible() && !window.is_minimized(),
            live_session: ui.get_live_session()
                && self.live.is_live.load(Ordering::Acquire)
                && !self.live.security_gate.blocked(),
            last_row,
            // A view focused on an older event is never the live bottom.
            live_view: slot == THREAD || !ui.get_conversation_focused(),
            account: ui.get_active_account_local_id().to_string(),
            room: ui.get_active_room_id().to_string(),
            thread_root: ui.get_thread_root_id().to_string(),
            context: None,
        };
        let next = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let context = state.context.clone().filter(|context| {
                context.alive()
                    && context.selection.account_id() == facts.account
                    && context.room == facts.room
            });
            facts.context = context.as_ref().map(|context| context.id);
            match read_plan(state, slot, &facts) {
                // Every lost gate withdraws the running request.
                Plan::Revoke => {
                    state.views[slot].revoke();
                    return;
                }
                Plan::Idle => return,
                Plan::Wait(wait) => Next::Wait(wait),
                Plan::Send {
                    thread,
                    event,
                    public,
                } => {
                    let Some(context) = context else {
                        return;
                    };
                    let target = Target {
                        account: &facts.account,
                        room: &facts.room,
                        thread: &thread,
                        event: &event,
                    };
                    let (op, cancellation) = begin_attempt(state, slot, &target, facts.now);
                    Next::Send(Request {
                        context,
                        op,
                        cancellation,
                        thread,
                        event,
                        public,
                    })
                }
            }
        };
        match next {
            Next::Wait(wait) => {
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
            Next::Send(request) => self.send_read(slot, request),
        }
    }

    fn send_read(self: &Arc<Self>, slot: usize, request: Request) {
        let this = self.clone();
        self.runtime.spawn(async move {
            let Request {
                context,
                op,
                cancellation,
                thread,
                event,
                public,
            } = request;
            let manager = this.live.manager.clone();
            let root = (!thread.is_empty()).then_some(thread.as_str());
            let outcome = manager
                .send_viewed_receipt_selected(
                    &context.selection,
                    &context.room,
                    root,
                    &event,
                    public,
                    &cancellation,
                )
                .await;
            let withdrawn = matches!(outcome, Ok(ReceiptOutcome::Withdrawn));
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_read(&ui, slot, op, withdrawn);
            });
        });
    }

    /// The outcome is never shown. A withdrawn request sent nothing, so its
    /// event may go out once its gates hold again and READ_INTERVAL has
    /// passed; anything else reached the SDK or failed and is not repeated.
    fn finish_read(self: &Arc<Self>, ui: &AppWindow, slot: usize, op: u64, withdrawn: bool) {
        let running = finish_attempt(&mut self.lock(), slot, op, withdrawn);
        if running {
            self.read_check(ui, slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_or_replacing_a_view_withdraws_its_request() {
        // A newer bottom event, no bottom event or a failed gate check.
        let cancellation = ActivityCancellation::new();
        let mut view = View::default();
        view.sending = Some((1, cancellation.clone()));
        view.revoke();
        assert!(cancellation.is_cancelled() && view.sending.is_none());
        // Another room, thread or account, or a hidden conversation, replaces
        // the views; the replaced request is withdrawn with it.
        let replaced = ActivityCancellation::new();
        let mut views: [View; 2] = Default::default();
        views[THREAD].sending = Some((2, replaced.clone()));
        drop(std::mem::take(&mut views[THREAD]));
        assert!(replaced.is_cancelled());
        // Lost focus or a changed receipt setting withdraws both views.
        let mut state = State::default();
        let main = ActivityCancellation::new();
        let thread = ActivityCancellation::new();
        state.views[MAIN].sending = Some((3, main.clone()));
        state.views[THREAD].sending = Some((4, thread.clone()));
        revoke_all(&mut state);
        assert!(main.is_cancelled() && thread.is_cancelled());
    }

    #[test]
    fn a_duplicate_after_cancel_or_refocus_waits_for_the_read_interval() {
        let now = Instant::now();
        let room = "!room:example.org";
        let mut state = State::default();
        state.prefs.read_tracking = true;
        state.chat_shown = true;
        state.composers[MAIN].focused = true;
        let shown = |since| Shown {
            context: 7,
            thread: String::new(),
            event: "$a:example.org".into(),
            since,
        };
        state.views[MAIN].shown = Some(shown(now));
        let facts = |at| ReadFacts {
            now: at,
            window_shown: true,
            live_session: true,
            last_row: Some("$a:example.org".into()),
            live_view: true,
            account: "account".into(),
            room: room.into(),
            thread_root: String::new(),
            context: Some(7),
        };
        let sends = |plan: Plan| matches!(plan, Plan::Send { .. });
        let target = Target {
            account: "account",
            room,
            thread: "",
            event: "$a:example.org",
        };
        let in_view = now + model::READ_SETTLE;
        assert!(sends(read_plan(&state, MAIN, &facts(in_view))));
        let (op, cancellation) = begin_attempt(&mut state, MAIN, &target, in_view);
        // Focus is lost before the request reaches the SDK: it is withdrawn.
        state.composers[MAIN].focused = false;
        assert_eq!(read_plan(&state, MAIN, &facts(in_view)), Plan::Revoke);
        revoke_all(&mut state);
        assert!(cancellation.is_cancelled());
        assert!(!finish_attempt(&mut state, MAIN, op, true));
        // Focus comes back at once: the same event waits out READ_INTERVAL
        // from the withdrawn attempt instead of going out again.
        state.composers[MAIN].focused = true;
        let refocused = in_view + Duration::from_millis(100);
        assert_eq!(
            read_plan(&state, MAIN, &facts(refocused)),
            Plan::Wait(model::READ_INTERVAL - Duration::from_millis(100))
        );
        // A conversation hidden and shown again replaces the view, not the
        // attempt.
        state.views[MAIN] = View::default();
        state.views[MAIN].shown = Some(shown(refocused));
        let plan = read_plan(&state, MAIN, &facts(refocused));
        assert!(matches!(plan, Plan::Wait(_)));
        // Once READ_INTERVAL has passed the withdrawn event may go out.
        let later = in_view + model::READ_INTERVAL;
        assert!(sends(read_plan(&state, MAIN, &facts(later))));
        // An attempt that reached the SDK settles its event, even when its
        // request was revoked first: it is never sent again.
        let (op, _) = begin_attempt(&mut state, MAIN, &target, later);
        revoke_all(&mut state);
        assert!(!finish_attempt(&mut state, MAIN, op, false));
        let much_later = later + 10 * model::READ_INTERVAL;
        assert_eq!(read_plan(&state, MAIN, &facts(much_later)), Plan::Idle);
    }
}
