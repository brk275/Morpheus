//! The persistent call bar (call-bar.slint): this device's native call on
//! every page but the call page.
//!
//! - It is shown while a native call blocks account changes (connecting,
//!   connected, reconnecting, leaving, or an incomplete cleanup) and the call
//!   page is not current. On a protected surface (no live session, sign-in,
//!   the account and security surfaces, a shown link card, and every modal:
//!   app-window's call-bar-protected, one list with matrix-link-blocked) it
//!   is never shown; the window shows only a generic call and capture badge
//!   there, with no room, account or participant name and no control.
//!   Every entry that opens one calls CallBar.withdraw (main.rs's
//!   withdraw_call_bar) synchronously before setting its flag, so a click
//!   still waiting for the controller is refused for good at once.
//! - Its state comes from the call's actual status, presented through the
//!   status watcher's own fence (main.rs). Capture and microphone
//!   indicators are the window's published capture properties, never a
//!   wanted setting, and they are never hidden by the layout.
//! - The elapsed time is local to this process: from the moment this device
//!   first saw the call joined, kept while the same call reconnects or
//!   rejoins (same account, room and call generation) and dropped as soon as
//!   the call leaves Joined/Reconnecting or another call replaces it. Before
//!   that it is not shown. It is never stored, logged or sent, and its
//!   one-second timer runs only while it is known; it stops with the call.
//! - Every control is bound to the exact call the bar was presented for
//!   (`ObservedCall`, read together with that status under the controller's
//!   slot lock): its account, room and lifecycle call generation, and for
//!   media its shown join (presentation). A click hands that identity,
//!   with the click's own security and account context (`Admit`), to the
//!   controller, which admits it only under its slot's lock, in the step
//!   that acts; a replacement call, even in the same room and account, or a
//!   security step started since the click (even one that ended) is refused
//!   and nothing changes. The UI's own check at the click only refuses
//!   early. While connecting, leaving or after an incomplete cleanup no
//!   exact call is known, so Leave and the media controls are disabled
//!   (the call page handles those). Mute and Deafen also require the
//!   window's applied audio generation (main.rs's applied_audio_token), and
//!   Stop its applied capture generation (applied_capture_token), to be the
//!   shown join, so they never act with an earlier join's state. A Stop is
//!   queued for its call's own capture worker without waiting; a full queue
//!   sends nothing and says so. Return only shows the call page and hands
//!   keyboard focus to its stage: it never joins, rejoins or starts a
//!   capture. Stop never starts a capture or opens a device picker.

use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use slint::{ComponentHandle, SharedString, Timer, TimerMode};
use tokio::runtime::Handle;

use crate::{
    AppWindow, CallBar,
    calls::{CallController, CallControllerError, CallStatus, ObservedCall},
    live::LiveUi,
    local_video::CaptureKind,
};

/// Shown when the controller refused a click for a call or context that
/// changed: nothing was done.
const CHANGED: &str = "The call or account changed before this was sent; nothing was done.";

/// Shown when the call's capture worker still had an earlier request queued:
/// this Stop was not sent.
const CAPTURE_BUSY: &str = "Capture is still handling an earlier request; nothing was sent.";

/// Shown when a click finds the account or a security step closed to it
/// (the security gate is held, or the account is no longer the selected one).
const CONTEXT_CLOSED: &str = "Finish the account or security step first; nothing was done.";

/// The call a bar shows: its account and room and, while it runs, exactly
/// which call and join (`ObservedCall`). An opening slot, a leave and an
/// incomplete cleanup have no exact call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShownCall {
    pub(crate) account_id: String,
    pub(crate) room_id: String,
    pub(crate) observed: Option<ObservedCall>,
}

impl ShownCall {
    /// The same call, through any of its joins: never another call of the
    /// same account and room.
    fn same_call(&self, other: &Self) -> bool {
        let call = |shown: &Self| shown.observed.as_ref().map(|observed| observed.call);
        self.account_id == other.account_id
            && self.room_id == other.room_id
            && call(self) == call(other)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Connecting,
    Connected,
    Reconnecting,
    Leaving,
    CleanupIncomplete,
}

/// What the bar shows for one call status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallView {
    pub(crate) call: Option<ShownCall>,
    pub(crate) phase: Option<Phase>,
    pub(crate) state: String,
}

impl CallView {
    pub(crate) fn none() -> Self {
        Self {
            call: None,
            phase: None,
            state: String::new(),
        }
    }

    /// Media controls apply: the exact join is known and joined or recovering.
    fn controls_media(&self) -> bool {
        let joined = matches!(self.phase, Some(Phase::Connected | Phase::Reconnecting));
        joined
            && self
                .call
                .as_ref()
                .and_then(|call| call.observed.as_ref())
                .is_some_and(|observed| observed.presentation != 0)
    }
}

/// The bar's view of `status`, with `observed` the controller's observation
/// of that same status (`CallController::observe`). Only a status that
/// blocks account changes has a call; only a running one an exact call.
pub(crate) fn view_of(status: &CallStatus, observed: Option<ObservedCall>) -> CallView {
    let exact = matches!(
        status,
        CallStatus::Starting { .. } | CallStatus::Joined { .. } | CallStatus::Reconnecting { .. }
    );
    let (account_id, room_id, phase, state) = match status {
        CallStatus::OpeningSlot {
            account_id,
            room_id,
        }
        | CallStatus::Starting {
            account_id,
            room_id,
        } => (account_id, room_id, Phase::Connecting, "Connecting…".into()),
        CallStatus::Joined { account_id, call } => (
            account_id,
            &call.room_id,
            Phase::Connected,
            "Connected".into(),
        ),
        CallStatus::Reconnecting {
            account_id,
            call,
            interruption,
        } => (
            account_id,
            &call.room_id,
            Phase::Reconnecting,
            interruption.label(),
        ),
        CallStatus::Ending {
            account_id,
            room_id,
        } => (account_id, room_id, Phase::Leaving, "Leaving…".into()),
        CallStatus::CleanupFailed {
            account_id,
            room_id,
            ..
        } => (
            account_id,
            room_id,
            Phase::CleanupIncomplete,
            "Call cleanup incomplete".into(),
        ),
        CallStatus::Idle | CallStatus::Failed { .. } | CallStatus::Ended { .. } => {
            return CallView::none();
        }
    };
    let observed = observed.filter(|observed| {
        exact && observed.account_id == *account_id && observed.room_id == *room_id
    });
    CallView {
        call: Some(ShownCall {
            account_id: account_id.clone(),
            room_id: room_id.clone(),
            observed,
        }),
        phase: Some(phase),
        state,
    }
}

/// When this device first saw the shown call joined: kept while the same
/// call stays connected, reconnects or rejoins, started at `now` for a call
/// seen connected for the first time, and unknown otherwise.
fn joined_since(
    previous: Option<(&ShownCall, Instant)>,
    view: &CallView,
    now: Instant,
) -> Option<Instant> {
    let kept = |call: &ShownCall| {
        previous
            .filter(|(shown, _)| call.observed.is_some() && shown.same_call(call))
            .map(|(_, at)| at)
    };
    match (&view.call, view.phase) {
        (Some(call), Some(Phase::Connected)) => Some(kept(call).unwrap_or(now)),
        (Some(call), Some(Phase::Reconnecting)) => kept(call),
        _ => None,
    }
}

/// The local time in a call, short (m:ss or h:mm:ss) and in words.
fn elapsed_text(elapsed: Duration) -> (String, String) {
    let total = elapsed.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    let short = if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    };
    let unit = |count: u64, one: &str, many: &str| {
        if count == 1 {
            format!("1 {one}")
        } else {
            format!("{count} {many}")
        }
    };
    let words = if hours > 0 {
        format!(
            "{} {} in this call",
            unit(hours, "hour", "hours"),
            unit(minutes, "minute", "minutes")
        )
    } else if minutes > 0 {
        format!("{} in this call", unit(minutes, "minute", "minutes"))
    } else {
        format!("{} in this call", unit(seconds, "second", "seconds"))
    };
    (short, words)
}

/// One click's action, for the exact call it was shown for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BarAction {
    Mute(bool),
    Deafen(bool),
    Stop(CaptureKind),
    Leave,
}

/// A click's own security and account context: true only while it is
/// unchanged since the click. Evaluated by the controller at admission.
pub(crate) type Admit = Arc<dyn Fn() -> bool + Send + Sync>;

pub(crate) type Acting = Pin<Box<dyn Future<Output = Result<(), CallControllerError>> + Send>>;

/// The call controller as the bar uses it.
pub(crate) trait BarControls: 'static {
    /// The current call, observed now without waiting; `None` when that
    /// cannot be told right now. Only the UI's early check: it is not proof,
    /// the controller decides at admission.
    fn current(&self) -> Option<CallView>;

    /// Hand `action` for exactly `observed` to the controller, admitted there
    /// only while `admit` holds.
    fn act(&self, action: BarAction, observed: ObservedCall, admit: Admit) -> Acting;
}

impl BarControls for Arc<CallController> {
    fn current(&self) -> Option<CallView> {
        let status = self.status();
        let observed = self.try_observe(&status).ok()?;
        Some(view_of(&status, observed))
    }

    fn act(&self, action: BarAction, observed: ObservedCall, admit: Admit) -> Acting {
        let calls = self.clone();
        Box::pin(async move {
            let admit = &*admit;
            match action {
                BarAction::Mute(muted) => {
                    calls
                        .set_microphone_muted_for(&observed, muted, admit)
                        .await
                }
                BarAction::Deafen(deafened) => {
                    calls.set_deafened_for(&observed, deafened, admit).await
                }
                BarAction::Stop(kind) => calls.stop_video_capture_for(&observed, kind, admit).await,
                BarAction::Leave => calls.end_call_for(&observed, admit).await,
            }
        })
    }
}

/// The live session's admission for a click on `account_id`'s call: `None`
/// unless the session is live, that account is the current selection and no
/// security step (account access, recovery, restart) is under way. The check
/// it returns stays true only while that selection is current and no
/// security step has started since the click, even one that has ended.
pub(crate) fn live_admission(live: &LiveUi, account_id: &str) -> Option<Admit> {
    let selection = live.manager.account_selection(account_id)?;
    let gate = live.security_gate.clone();
    let is_live = live.is_live.clone();
    // The gate's epoch advances whenever a security step starts.
    let generation = gate.ringing_generation();
    let admitted = move || {
        is_live.load(Ordering::Acquire)
            && selection.is_current()
            && gate.ringing_allowed(generation)
    };
    admitted().then(|| Arc::new(admitted) as Admit)
}

/// Advanced synchronously by every entry that opens a protected surface
/// (CallBar.withdraw, before its flag is set; see main.rs's
/// withdraw_call_bar), and again when call-bar-protected becomes true (the
/// deferred fallback). A click captures it, so a click still waiting for the
/// controller is refused once one was opened, even after it closed again.
static PROTECTED_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Who owns each kind's partial busy flag (camera, screen), on the UI thread.
///
/// - `baseline` is the last authoritative busy value of that kind: what a
///   full update or clear of the capture watcher wrote (main.rs's
///   apply_capture_status and clear_native_capture, `capture_state_applied`),
///   or, when a click begins with no other click of that kind pending, the
///   busy flag it found shown.
/// - `pending` lists the clicks of that kind whose optimistic busy flag is
///   still outstanding (the bar's Stop, the call page's start and stop),
///   each with the authoritative generation it began under.
///
/// A click that fails puts back the baseline, never its own or another
/// click's optimistic value, and only when no other click of that kind is
/// still pending and no full update arrived since it began (the worker
/// publishes Failed or Off before it answers). The two kinds are fully
/// independent, and nothing is disabled or serialized.
#[derive(Default)]
struct KindOwners {
    baseline: bool,
    generation: u64,
    pending: Vec<(u64, u64)>,
}

#[derive(Default)]
struct CaptureOwners {
    camera: KindOwners,
    screen: KindOwners,
    next_owner: u64,
}

impl CaptureOwners {
    fn kind(&mut self, kind: CaptureKind) -> &mut KindOwners {
        match kind {
            CaptureKind::Camera => &mut self.camera,
            CaptureKind::Screen => &mut self.screen,
        }
    }
}

thread_local! {
    static CAPTURE_OWNERS: RefCell<CaptureOwners> = RefCell::new(CaptureOwners::default());
}

/// What a finished click may do with its kind's busy flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureRollback {
    /// A full update arrived since the click began (or its record is gone):
    /// the window shows the newer truth; change nothing.
    Superseded,
    /// It succeeded, or another click of the same kind is still pending and
    /// owns the optimistic flag: leave the flag as it is.
    Kept,
    /// The last pending click of its kind failed: put back this value.
    Restore(bool),
}

/// A full authoritative write of both kinds' capture state (the capture
/// watcher's apply or clear), with the busy values it wrote. Every pending
/// click of either kind is superseded.
pub(crate) fn capture_state_applied(camera_busy: bool, screen_busy: bool) {
    CAPTURE_OWNERS.with(|owners| {
        let mut owners = owners.borrow_mut();
        for (kind, busy) in [
            (CaptureKind::Camera, camera_busy),
            (CaptureKind::Screen, screen_busy),
        ] {
            let owned = owners.kind(kind);
            owned.baseline = busy;
            owned.generation = owned.generation.wrapping_add(1);
            owned.pending.clear();
        }
    });
}

/// A click of `kind` is about to set its optimistic busy flag; `shown_busy`
/// is the flag as the window shows it now. Returns the click's owner ID.
pub(crate) fn capture_owner_begin(kind: CaptureKind, shown_busy: bool) -> u64 {
    CAPTURE_OWNERS.with(|owners| {
        let mut owners = owners.borrow_mut();
        owners.next_owner = owners.next_owner.wrapping_add(1);
        let id = owners.next_owner;
        let owned = owners.kind(kind);
        if owned.pending.is_empty() {
            // No other click owns this kind: what is shown is authoritative.
            owned.baseline = shown_busy;
        }
        let generation = owned.generation;
        owned.pending.push((id, generation));
        id
    })
}

/// A click of `kind` finished (`failed` when its hand-off or worker
/// failed). It always gives up its ownership; see `CaptureRollback`.
pub(crate) fn capture_owner_end(kind: CaptureKind, id: u64, failed: bool) -> CaptureRollback {
    CAPTURE_OWNERS.with(|owners| {
        let mut owners = owners.borrow_mut();
        let owned = owners.kind(kind);
        let Some(at) = owned.pending.iter().position(|(owner, _)| *owner == id) else {
            return CaptureRollback::Superseded;
        };
        let (_, began) = owned.pending.remove(at);
        if began != owned.generation {
            CaptureRollback::Superseded
        } else if !failed || !owned.pending.is_empty() {
            CaptureRollback::Kept
        } else {
            CaptureRollback::Restore(owned.baseline)
        }
    })
}

/// Revision (UI thread) of the window's one shared capture error text
/// (native-capture-error): every writer advances it, and a failed click
/// writes its text only while no newer write owns that text.
static ERROR_REVISION: AtomicU64 = AtomicU64::new(0);

/// Record one write of the shared capture error text; returns its revision.
pub(crate) fn capture_error_written() -> u64 {
    ERROR_REVISION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Nothing has written the shared capture error text since `revision`.
pub(crate) fn capture_error_unchanged_since(revision: u64) -> bool {
    ERROR_REVISION.load(Ordering::SeqCst) == revision
}

struct Wiring {
    controls: Box<dyn BarControls>,
    handle: Handle,
    admission: Box<dyn Fn(&str) -> Option<Admit>>,
    // The audio generation whose state the window shows now (main.rs's
    // applied_audio_token, written on the UI thread by its audio watcher).
    applied_audio_token: Arc<AtomicU64>,
    // The same for its camera and screen state (main.rs's
    // applied_capture_token, written by its capture watcher).
    applied_capture_token: Arc<AtomicU64>,
}

#[derive(Default)]
struct Bar {
    presented: Option<ShownCall>,
    joined_at: Option<Instant>,
    timer: Option<Timer>,
    wiring: Option<Wiring>,
}

thread_local! {
    // Presentation, clicks and the timer run on the UI thread only.
    static BAR: RefCell<Bar> = RefCell::new(Bar::default());
}

/// Present `view` (from the status watcher's fenced UI update).
pub(crate) fn present(ui: &AppWindow, view: CallView) {
    let now = Instant::now();
    let joined_at = BAR.with(|bar| {
        let mut bar = bar.borrow_mut();
        let previous = bar.presented.as_ref().zip(bar.joined_at);
        let joined_at = joined_since(previous, &view, now);
        bar.presented = view.call.clone();
        bar.joined_at = joined_at;
        joined_at
    });
    let exact = matches!(
        &view.call,
        Some(ShownCall {
            observed: Some(_),
            ..
        })
    );
    let state = ui.global::<CallBar>();
    state.set_active(view.call.is_some());
    state.set_status_text(view.state.as_str().into());
    state.set_can_leave(exact);
    state.set_can_control(view.controls_media());
    refresh_elapsed(ui);
    keep_ticking(ui, joined_at.is_some());
}

fn refresh_elapsed(ui: &AppWindow) {
    let joined_at = BAR.with(|bar| bar.borrow().joined_at);
    let (short, words) = joined_at
        .map(|at| elapsed_text(at.elapsed()))
        .unwrap_or_default();
    let state = ui.global::<CallBar>();
    state.set_elapsed(short.into());
    state.set_elapsed_label(words.into());
}

/// Run the one-second timer only while the elapsed time is known.
fn keep_ticking(ui: &AppWindow, ticking: bool) {
    BAR.with(|bar| {
        let mut bar = bar.borrow_mut();
        if !ticking {
            // Dropping the timer stops it.
            bar.timer = None;
            return;
        }
        if bar.timer.as_ref().is_some_and(Timer::running) {
            return;
        }
        let timer = Timer::default();
        let weak = ui.as_weak();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            if let Some(ui) = weak.upgrade() {
                refresh_elapsed(&ui);
            }
        });
        bar.timer = Some(timer);
    });
}

/// Wire the bar's controls to `controls` (production: the call controller),
/// spawning their hand-offs on `handle`. `admission` captures a click's own
/// security and account context for the shown call's account (production:
/// `live_admission`). `applied_audio_token` is the audio generation whose
/// ready, muted and deafened state the window shows, `applied_capture_token`
/// the one whose camera and screen state it shows (production: main.rs's
/// watchers' own applied tokens, never the controller's live tokens).
pub(crate) fn register(
    ui: &AppWindow,
    controls: impl BarControls,
    handle: Handle,
    applied_audio_token: Arc<AtomicU64>,
    applied_capture_token: Arc<AtomicU64>,
    admission: impl Fn(&str) -> Option<Admit> + 'static,
) {
    BAR.with(|bar| {
        bar.borrow_mut().wiring = Some(Wiring {
            controls: Box::new(controls),
            handle,
            admission: Box::new(admission),
            applied_audio_token,
            applied_capture_token,
        });
    });
    let state = ui.global::<CallBar>();
    state.on_protected_shown(|| {
        PROTECTED_EPOCH.fetch_add(1, Ordering::SeqCst);
    });
    state.on_withdraw(|| {
        PROTECTED_EPOCH.fetch_add(1, Ordering::SeqCst);
    });
    let weak = ui.as_weak();
    state.on_mute(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_native_audio_ready()
        {
            send(&ui, BarAction::Mute(!ui.get_native_microphone_muted()));
        }
    });
    let weak = ui.as_weak();
    state.on_deafen(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_native_audio_ready()
        {
            send(&ui, BarAction::Deafen(!ui.get_native_deafened()));
        }
    });
    let weak = ui.as_weak();
    state.on_stop_camera(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_native_camera_active()
            && !ui.get_native_camera_busy()
        {
            send(&ui, BarAction::Stop(CaptureKind::Camera));
        }
    });
    let weak = ui.as_weak();
    state.on_stop_share(move || {
        if let Some(ui) = weak.upgrade()
            && (ui.get_native_screen_active() || ui.get_native_screen_selecting())
        {
            send(&ui, BarAction::Stop(CaptureKind::Screen));
        }
    });
    let weak = ui.as_weak();
    state.on_return_to_call(move || {
        // Navigation only: it never joins, rejoins or starts a capture.
        // Keyboard focus follows to the stage (CallStage.focus-stage).
        if let Some(ui) = weak.upgrade()
            && !ui.get_call_bar_protected()
            && showing_current_call()
        {
            // Admitted, Return takes the window to the call page: a held
            // unread jump, a shown link's read, join and follow-up, and
            // identity details' focus-return intent end first, synchronously,
            // as at the rail's call-page entry.
            crate::unread_navigation::withdraw_jump(&ui);
            crate::matrix_links::clear(&ui);
            crate::identity_warnings::withdraw(&ui);
            ui.set_call_stage_focus_pending(true);
            ui.invoke_navigate_page(2);
        }
    });
    let weak = ui.as_weak();
    state.on_leave(move || {
        if let Some(ui) = weak.upgrade() {
            send(&ui, BarAction::Leave);
        }
    });
}

/// The presented call is still the current one (any of its joins). When
/// that cannot be told right now, Return does nothing.
fn showing_current_call() -> bool {
    BAR.with(|bar| {
        let bar = bar.borrow();
        let (Some(wiring), Some(presented)) = (&bar.wiring, &bar.presented) else {
            return false;
        };
        let current = wiring.controls.current().and_then(|view| view.call);
        current.is_some_and(|current| presented.same_call(&current))
    })
}

/// The UI's early check at a click: the current call is still the presented
/// one, and for media still its shown join. Not proof; the controller
/// checks again under its lock.
fn still_shown(action: BarAction, presented: &ShownCall, current: Option<&ShownCall>) -> bool {
    let (Some(current), Some(observed)) = (current, presented.observed.as_ref()) else {
        return false;
    };
    if observed.call == 0 {
        return false;
    }
    match action {
        BarAction::Leave => presented.same_call(current),
        _ => observed.presentation != 0 && presented == current,
    }
}

/// Hand `action` for the presented call to the controller with the click's
/// own context. Nothing runs on a protected surface, without an exact
/// presented call, or when the early check or the admission refuses.
fn send(ui: &AppWindow, action: BarAction) {
    if ui.get_call_bar_protected() {
        return;
    }
    let mut context_closed = false;
    let started = BAR.with(|bar| {
        let bar = bar.borrow();
        let wiring = bar.wiring.as_ref()?;
        let presented = bar.presented.as_ref()?;
        let observed = presented.observed.clone()?;
        // Mute and Deafen read the window's ready, muted and deafened state,
        // Stop its camera and screen state; each belongs to the generation
        // the window applied. They act only when that is exactly the shown
        // join, so a join presented before its state never receives an
        // earlier join's values.
        let applied = match action {
            BarAction::Mute(_) | BarAction::Deafen(_) => Some(&wiring.applied_audio_token),
            BarAction::Stop(_) => Some(&wiring.applied_capture_token),
            BarAction::Leave => None,
        };
        if applied.is_some_and(|token| token.load(Ordering::SeqCst) != observed.presentation) {
            return None;
        }
        // When the current call cannot be told right now, the controller's
        // own check decides alone.
        if let Some(current) = wiring.controls.current()
            && !still_shown(action, presented, current.call.as_ref())
        {
            return None;
        }
        let Some(context) = (wiring.admission)(&observed.account_id) else {
            context_closed = true;
            return None;
        };
        let epoch = PROTECTED_EPOCH.load(Ordering::SeqCst);
        let admit: Admit =
            Arc::new(move || PROTECTED_EPOCH.load(Ordering::SeqCst) == epoch && context());
        let acting = wiring.controls.act(action, observed.clone(), admit);
        Some((wiring.handle.clone(), acting, observed))
    });
    // This click's own optimistic writes (a Stop only): its ownership of its
    // kind's busy flag and its revision of the shared error text.
    let mut written = Owned::default();
    let Some((handle, acting, observed)) = started else {
        // Said where it would otherwise look like the click was lost.
        if context_closed {
            match action {
                BarAction::Leave => ui.set_session_error(CONTEXT_CLOSED.into()),
                BarAction::Stop(_) => {
                    ui.set_native_capture_error(CONTEXT_CLOSED.into());
                    capture_error_written();
                }
                BarAction::Mute(_) | BarAction::Deafen(_) => {}
            }
        }
        return;
    };
    match action {
        BarAction::Stop(kind) => {
            let shown = match kind {
                CaptureKind::Camera => ui.get_native_camera_busy(),
                CaptureKind::Screen => ui.get_native_screen_busy(),
            };
            ui.set_native_capture_error(SharedString::default());
            written = Owned {
                owner: capture_owner_begin(kind, shown),
                error: capture_error_written(),
            };
            set_capture_busy(ui, kind, true);
        }
        // The bar goes with the call: keep focus in the conversation.
        BarAction::Leave if ui.get_page() == 0 && !ui.get_active_room_id().is_empty() => {
            ui.invoke_focus_composer(false);
        }
        _ => {}
    }
    let weak = ui.as_weak();
    #[cfg(test)]
    let handoff = handoff_started_for_test();
    handle.spawn(async move {
        let result = acting.await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish(&ui, action, &observed, written, result);
            // Test-only: this hand-off's own finish has published everything
            // it writes (busy flags, error texts, ownership).
            #[cfg(test)]
            handoff_finished(handoff);
        });
    });
}

/// One click's own optimistic capture writes: its owner ID for its kind's
/// busy flag (`capture_owner_begin`) and its revision of the error text.
#[derive(Clone, Copy, Debug, Default)]
struct Owned {
    owner: u64,
    error: u64,
}

fn set_capture_busy(ui: &AppWindow, kind: CaptureKind, busy: bool) {
    match kind {
        CaptureKind::Camera => ui.set_native_camera_busy(busy),
        CaptureKind::Screen => ui.set_native_screen_busy(busy),
    }
}

/// A finished hand-off, on the UI thread. A refusal changed nothing; it is
/// reported only where it would otherwise look like the click worked.
/// `written` is a Stop's own optimistic writes (its busy-flag ownership and
/// error-text revision).
fn finish(
    ui: &AppWindow,
    action: BarAction,
    observed: &ObservedCall,
    written: Owned,
    result: Result<(), CallControllerError>,
) {
    // A Stop gives up its ownership whatever the outcome.
    let rollback = match action {
        BarAction::Stop(kind) => capture_owner_end(kind, written.owner, result.is_err()),
        _ => CaptureRollback::Kept,
    };
    let (join_shown, capture_shown) = BAR.with(|bar| {
        let bar = bar.borrow();
        let join_shown = matches!(
            &bar.presented,
            Some(ShownCall { observed: Some(shown), .. }) if shown == observed
        );
        // The window still shows this join's capture state: its capture
        // watcher has neither cleared it (the call ended) nor replaced it.
        let capture_shown = bar.wiring.as_ref().is_some_and(|wiring| {
            wiring.applied_capture_token.load(Ordering::SeqCst) == observed.presentation
        });
        (join_shown, capture_shown)
    });
    let protected = ui.get_call_bar_protected();
    match (action, result) {
        (_, Ok(())) => {}
        (BarAction::Stop(kind), Err(error)) => {
            // Only while the window still shows that join's capture state
            // (the capture watcher resets a replaced or ended join's) and no
            // full update arrived since this click (the watcher's Failed,
            // Off or Active is newer and stays). The busy flag goes back to
            // its kind's authoritative baseline (a screen selection is busy),
            // never to this or another click's optimistic value, and only
            // once no other click of that kind is pending; the other kind is
            // independent. The shared error text is written only while no
            // newer writer owns it.
            let superseded = rollback == CaptureRollback::Superseded;
            if join_shown && capture_shown && !superseded {
                if let CaptureRollback::Restore(busy) = rollback {
                    set_capture_busy(ui, kind, busy);
                }
                if capture_error_unchanged_since(written.error) {
                    let text = match error {
                        CallControllerError::StaleCall => CHANGED.to_owned(),
                        CallControllerError::AlreadyBusy => CAPTURE_BUSY.to_owned(),
                        error => crate::safe_capture_label(&error.to_string()),
                    };
                    ui.set_native_capture_error(text.into());
                    capture_error_written();
                }
            }
        }
        (BarAction::Leave, Err(CallControllerError::StaleCall)) => {
            if !protected {
                ui.set_session_error(CHANGED.into());
            }
        }
        // The published audio state still shows the truth, as on the call page.
        (_, Err(CallControllerError::StaleCall)) => {}
        // As the call page's own handlers report them.
        (_, Err(error)) => ui.set_session_error(error.to_string().into()),
    }
}

// Test-only hand-off identities: each spawned hand-off gets its own ID, and
// only that hand-off's finish closure acknowledges it, so a test waits for
// one specific click's publication and no other finish can satisfy it.
#[cfg(test)]
static NEXT_HANDOFF: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
static FINISHED_HANDOFFS: std::sync::Mutex<std::collections::BTreeSet<u64>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

#[cfg(test)]
thread_local! {
    /// The hand-offs this thread's clicks started, in order.
    static STARTED_HANDOFFS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn handoff_started_for_test() -> u64 {
    let id = NEXT_HANDOFF.fetch_add(1, Ordering::SeqCst);
    STARTED_HANDOFFS.with(|started| started.borrow_mut().push(id));
    id
}

#[cfg(test)]
fn handoff_finished(id: u64) {
    FINISHED_HANDOFFS.lock().unwrap().insert(id);
}

/// Test-only: the hand-offs `click` started on this thread (send() runs
/// synchronously up to its spawn), in order; empty when it sent nothing.
#[cfg(test)]
pub(crate) fn handoffs_for_test(click: impl FnOnce()) -> Vec<u64> {
    let before = STARTED_HANDOFFS.with(|started| started.borrow().len());
    click();
    STARTED_HANDOFFS.with(|started| started.borrow()[before..].to_vec())
}

/// Test-only: whether hand-off `id`'s own finish closure has run.
#[cfg(test)]
pub(crate) fn handoff_finished_for_test(id: u64) -> bool {
    FINISHED_HANDOFFS.lock().unwrap().contains(&id)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::calls::{CallInfo, CallInterruption};

    fn call(room: &str) -> CallInfo {
        CallInfo {
            room_id: room.into(),
            slot_id: "slot".into(),
            membership_id: "membership".into(),
            local_identity: "device".into(),
        }
    }

    pub(crate) fn observed_for_test(
        account: &str,
        room: &str,
        call: u64,
        presentation: u64,
    ) -> ObservedCall {
        ObservedCall {
            account_id: account.into(),
            room_id: room.into(),
            call,
            presentation,
        }
    }

    fn shown(observed: &ObservedCall) -> ShownCall {
        ShownCall {
            account_id: observed.account_id.clone(),
            room_id: observed.room_id.clone(),
            observed: Some(observed.clone()),
        }
    }

    /// A joined call's view, as the status watcher presents it.
    pub(crate) fn joined_view_for_test(observed: &ObservedCall) -> CallView {
        CallView {
            call: Some(shown(observed)),
            phase: Some(Phase::Connected),
            state: "Connected".into(),
        }
    }

    #[test]
    fn only_a_call_this_device_is_in_has_a_bar_and_its_state_is_the_status() {
        let exact = observed_for_test("work", "!r:x", 7, 9);
        for status in [
            CallStatus::Idle,
            CallStatus::Failed {
                room_id: "!r:x".into(),
                reason: "secret".into(),
            },
            CallStatus::Ended {
                room_id: "!r:x".into(),
                reason: "secret".into(),
            },
        ] {
            let view = view_of(&status, Some(exact.clone()));
            assert_eq!(view, CallView::none(), "{status:?}");
        }
        let joined = CallStatus::Joined {
            account_id: "work".into(),
            call: call("!r:x"),
        };
        let view = view_of(&joined, Some(exact.clone()));
        assert_eq!(view, joined_view_for_test(&exact));
        assert!(view.controls_media());
        let opening = CallStatus::OpeningSlot {
            account_id: "work".into(),
            room_id: "!r:x".into(),
        };
        // An opening slot has no exact call, whatever is offered for it.
        let view = view_of(&opening, Some(exact.clone()));
        let unbound = matches!(&view.call, Some(ShownCall { observed: None, .. }));
        assert!(unbound && view.phase == Some(Phase::Connecting));
        // An observation of another account or room is never this call's.
        let elsewhere = observed_for_test("home", "!r:x", 7, 9);
        let view = view_of(&joined, Some(elsewhere));
        assert!(view.call.is_some_and(|call| call.observed.is_none()));
        let cleanup = CallStatus::CleanupFailed {
            account_id: "work".into(),
            room_id: "!r:x".into(),
            reason: "server text".into(),
        };
        let view = view_of(&cleanup, Some(exact));
        assert_eq!(view.phase, Some(Phase::CleanupIncomplete));
        assert!(matches!(&view.call, Some(ShownCall { observed: None, .. })));
        assert!(!view.controls_media());
        assert!(!view.state.contains("server text"), "no reason text");
    }

    #[test]
    fn the_elapsed_time_is_known_only_from_this_device_s_own_join() {
        let start = Instant::now();
        let later = start + Duration::from_secs(90);
        let first = observed_for_test("work", "!r:x", 7, 7);
        let reconnecting = |observed: &ObservedCall| CallView {
            phase: Some(Phase::Reconnecting),
            state: "Reconnecting call media…".into(),
            ..joined_view_for_test(observed)
        };
        let connected = joined_view_for_test(&first);
        // First seen joined: counted from now.
        assert_eq!(joined_since(None, &connected, start), Some(start));
        // The same call reconnecting, then joined again: the same start.
        let first_shown = shown(&first);
        let kept = Some((&first_shown, start));
        let again = joined_since(kept, &reconnecting(&first), later);
        assert_eq!(again, Some(start));
        assert_eq!(joined_since(kept, &connected, later), Some(start));
        // A rejoin is a new join of the same call: still the same start.
        let rejoined = observed_for_test("work", "!r:x", 7, 8);
        let view = joined_view_for_test(&rejoined);
        assert_eq!(joined_since(kept, &view, later), Some(start));
        // Never seen joined before reconnecting: not reliably known.
        assert_eq!(joined_since(None, &reconnecting(&first), later), None);
        // Another call in the same room starts again; leaving forgets it.
        let second = observed_for_test("work", "!r:x", 8, 8);
        let view = joined_view_for_test(&second);
        assert_eq!(joined_since(kept, &view, later), Some(later));
        let leaving = CallView {
            call: Some(ShownCall {
                observed: None,
                ..first_shown.clone()
            }),
            phase: Some(Phase::Leaving),
            state: "Leaving…".into(),
        };
        assert_eq!(joined_since(kept, &leaving, later), None);
        assert_eq!(joined_since(kept, &CallView::none(), later), None);
    }

    #[test]
    fn elapsed_time_reads_short_and_in_words() {
        let at = |seconds| elapsed_text(Duration::from_secs(seconds));
        assert_eq!(at(0), ("0:00".into(), "0 seconds in this call".into()));
        assert_eq!(at(61), ("1:01".into(), "1 minute in this call".into()));
        assert_eq!(
            at(3_725),
            ("1:02:05".into(), "1 hour 2 minutes in this call".into())
        );
    }

    #[test]
    fn a_control_acts_only_on_the_call_it_was_shown_for() {
        let shown_join = observed_for_test("work", "!r:x", 7, 7);
        let presented = shown(&shown_join);
        for action in [BarAction::Leave, BarAction::Mute(true)] {
            assert!(still_shown(action, &presented, Some(&presented)));
            // No call now, or a replacement call, even of the same account
            // and room, or a zero generation: no action.
            assert!(!still_shown(action, &presented, None));
            for other in [
                observed_for_test("work", "!r:x", 8, 8),
                observed_for_test("work", "!other:x", 7, 7),
                observed_for_test("home", "!r:x", 7, 7),
            ] {
                let other = shown(&other);
                assert!(!still_shown(action, &presented, Some(&other)), "{other:?}");
            }
            let zero = shown(&observed_for_test("work", "!r:x", 0, 0));
            assert!(!still_shown(action, &zero, Some(&zero)));
            // Without an exact call (opening, leaving, cleanup): no action.
            let unbound = ShownCall {
                observed: None,
                ..presented.clone()
            };
            assert!(!still_shown(action, &unbound, Some(&unbound)));
        }
        // A rejoin keeps the call (Leave) but not the shown join (media).
        let rejoined = shown(&observed_for_test("work", "!r:x", 7, 8));
        assert!(still_shown(BarAction::Leave, &presented, Some(&rejoined)));
        let stop = BarAction::Stop(CaptureKind::Camera);
        assert!(!still_shown(stop, &presented, Some(&rejoined)));
        let status = CallStatus::Reconnecting {
            account_id: "work".into(),
            call: call("!r:x"),
            interruption: CallInterruption::Media,
        };
        let view = view_of(&status, Some(observed_for_test("work", "!r:x", 7, 8)));
        assert!(view.call.is_some_and(|call| presented.same_call(&call)));
    }

    /// A failed or refused Stop puts back its click's busy flag only while
    /// nothing has written the capture state since its own optimistic write:
    /// every watcher apply and clear, and every other optimistic writer,
    /// advances the revision.
    #[test]
    fn a_failed_stop_puts_back_its_busy_flag_only_when_nothing_newer_wrote_it() {
        use CaptureKind::{Camera, Screen};
        use CaptureRollback::{Kept, Restore, Superseded};
        // Each test thread has its own owners; start from a full clear.
        capture_state_applied(false, false);
        // One click alone: refused, it puts back the shown (authoritative)
        // flag; a positive control with a picker still selecting (true).
        let id = capture_owner_begin(Screen, true);
        assert_eq!(capture_owner_end(Screen, id, true), Restore(true));
        let id = capture_owner_begin(Screen, false);
        assert_eq!(capture_owner_end(Screen, id, true), Restore(false));
        // A success gives up ownership and changes nothing.
        let id = capture_owner_begin(Camera, false);
        assert_eq!(capture_owner_end(Camera, id, false), Kept);
        // Duplicate same-kind clicks, both answer orders: the second click
        // saw the first one's optimistic true, but only the authoritative
        // baseline (false) is ever put back, once the last one is answered.
        for first_answers_first in [true, false] {
            let first = capture_owner_begin(Screen, false);
            let second = capture_owner_begin(Screen, true);
            let (one, two) = if first_answers_first {
                (first, second)
            } else {
                (second, first)
            };
            assert_eq!(capture_owner_end(Screen, one, true), Kept);
            assert_eq!(capture_owner_end(Screen, two, true), Restore(false));
        }
        // Cross-kind, both orders: the other kind never takes a rollback.
        for camera_first in [true, false] {
            let camera = capture_owner_begin(Camera, false);
            let screen = capture_owner_begin(Screen, true);
            if camera_first {
                assert_eq!(capture_owner_end(Camera, camera, true), Restore(false));
                assert_eq!(capture_owner_end(Screen, screen, true), Restore(true));
            } else {
                assert_eq!(capture_owner_end(Screen, screen, true), Restore(true));
                assert_eq!(capture_owner_end(Camera, camera, true), Restore(false));
            }
        }
        // A same-kind later operation still pending keeps the flag.
        let older = capture_owner_begin(Camera, false);
        let newer = capture_owner_begin(Camera, true);
        assert_eq!(capture_owner_end(Camera, older, true), Kept);
        assert_eq!(capture_owner_end(Camera, newer, true), Restore(false));
        // A full update supersedes every pending click of both kinds, and
        // sets the baseline a later lone click starts from.
        let camera = capture_owner_begin(Camera, false);
        let screen = capture_owner_begin(Screen, false);
        capture_state_applied(false, true);
        assert_eq!(capture_owner_end(Camera, camera, true), Superseded);
        assert_eq!(capture_owner_end(Screen, screen, true), Superseded);
        let after = capture_owner_begin(Screen, true);
        assert_eq!(capture_owner_end(Screen, after, true), Restore(true));
        // An older click answered after a full update and a newer click:
        // superseded, and the newer click still restores its baseline.
        let old = capture_owner_begin(Camera, false);
        capture_state_applied(false, false);
        let new = capture_owner_begin(Camera, false);
        assert_eq!(capture_owner_end(Camera, old, true), Superseded);
        assert_eq!(capture_owner_end(Camera, new, true), Restore(false));
        // The shared error text has its own owner.
        let error = capture_error_written();
        assert!(capture_error_unchanged_since(error));
        capture_error_written();
        assert!(!capture_error_unchanged_since(error));
        // Source guards compare the same function boundaries in LF and CRLF checkouts.
        let source = include_str!("call_bar.rs").replace("\r\n", "\n");
        // Everything up to the first top-level test item (the hand-off
        // identities after finish()); send()'s indented test hooks stay in.
        let production = source.split("\n#[cfg(test)]\n").next().unwrap();
        assert!(production.contains("fn finish("));
        for fenced in [
            "owner: capture_owner_begin(kind, shown),",
            "BarAction::Stop(kind) => capture_owner_end(kind, written.owner, result.is_err()),",
            "if let CaptureRollback::Restore(busy) = rollback {",
            "if capture_error_unchanged_since(written.error) {",
        ] {
            assert!(production.contains(fenced), "{fenced}");
        }
        assert!(
            !production.contains("busy_before"),
            "never the click's own value"
        );
        let main = include_str!("main.rs").replace("\r\n", "\n");
        for writer in [
            "fn clear_native_capture(ui: &AppWindow) {",
            "fn apply_capture_status(ui: &AppWindow, status: LocalVideoStatus) {",
        ] {
            let (_, body) = main.split_once(writer).unwrap();
            let head = body.lines().take(5).collect::<Vec<_>>().join("\n");
            assert!(
                head.contains("call_bar::capture_state_applied("),
                "{writer}"
            );
        }
        // The call page's own optimistic writers take part too, each owning
        // only its own kind and the error text it cleared, and giving up
        // ownership on success as on failure.
        let begin = "call_bar::capture_owner_begin(owned_kind, shown)";
        assert_eq!(main.matches(begin).count(), 2);
        let end = "call_bar::capture_owner_end(owned_kind, owner, result.is_err())";
        assert_eq!(main.matches(end).count(), 2);
        let error_fence = "call_bar::capture_error_unchanged_since(error_written)";
        assert_eq!(main.matches(error_fence).count(), 2);
        assert!(!main.contains("capture_state_unchanged_since"));
    }

    /// The two Settings states that protect the bar: their real entries
    /// withdraw a waiting bar click synchronously before their state
    /// changes, and the bar stays available in ordinary Settings.
    #[test]
    fn settings_entries_withdraw_the_bar_before_their_state_changes() {
        let privacy = include_str!("member_privacy.rs").replace("\r\n", "\n");
        for entry in ["fn review_unignore(", "fn confirm_unignore("] {
            let (_, body) = privacy.split_once(entry).unwrap();
            let body = body.split("\n    fn ").next().unwrap();
            let withdraw = body.find("crate::withdraw_call_bar(ui);").expect(entry);
            let paint = body.find("self.paint_panel(ui);").unwrap();
            assert!(withdraw < paint, "{entry}");
        }
        let shortcuts = include_str!("shortcuts.rs").replace("\r\n", "\n");
        let (_, record) = shortcuts.split_once("ui.on_shortcut_record(").unwrap();
        let withdraw = record.find("crate::withdraw_call_bar(&ui);").unwrap();
        let set = record
            .find("ui.set_shortcut_recording(action.name().into());")
            .unwrap();
        assert!(withdraw < set);
        let window = include_str!("../ui/app-window.slint").replace("\r\n", "\n");
        let (_, protected) = window
            .split_once("out property <bool> call-bar-protected:")
            .unwrap();
        let protected = protected.split(';').next().unwrap();
        for term in [
            "MemberPrivacy.ignored.stage != 0",
            "MemberPrivacy.ignored.busy",
            "root.shortcut-recording != \"\"",
        ] {
            assert!(protected.contains(term), "{term}");
        }
        assert!(
            !protected.contains("root.page"),
            "available in ordinary Settings"
        );
        // The thread browser: in the list, and its real entry withdraws the
        // bar synchronously before its state changes.
        assert!(protected.contains("ThreadBrowser.open"));
        let browser = include_str!("thread_browser.rs").replace("\r\n", "\n");
        let (_, show) = browser
            .split_once("fn show(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {")
            .unwrap();
        let withdraw = show.find("crate::withdraw_call_bar(ui);").unwrap();
        let cleared = show.find("\n    clear(ui);").unwrap();
        let opened = show.find("state.set_open(true)").unwrap_or(usize::MAX);
        assert!(withdraw < cleared && withdraw < opened);
        // The Leave dialog: in the list, and its actual start withdraws the
        // bar after its own admission (a refused start changes nothing) and
        // before its view opens.
        assert!(protected.contains("RoomLeave.view.open"));
        let leave = include_str!("room_leave.rs").replace("\r\n", "\n");
        let (_, start) = leave
            .split_once("fn start(self: &Arc<Self>, ui: &AppWindow) {")
            .unwrap();
        let start = start.split("\n    fn ").next().unwrap();
        let refused = start
            .find("if is_open(ui) || ui.get_room_leave_blocked()")
            .unwrap();
        let withdraw = start.find("crate::withdraw_call_bar(ui);").unwrap();
        let opened = start.find("open: true,").unwrap();
        assert!(refused < withdraw && withdraw < opened);
        // Identity details and their ViewMember route are protected entries
        // too. Their real current-scope/window refusal precedes withdrawal,
        // and withdrawal precedes either details or inspector flag changes.
        assert!(protected.contains("IdentityWarnings.open"));
        let identity = include_str!("identity_warnings.rs").replace("\r\n", "\n");
        let (_, show) = identity.split_once("    fn show_details(").unwrap();
        let show = show.split("\n    fn view_member(").next().unwrap();
        let refused = show.find("ui.get_identity_warnings_allowed()").unwrap();
        let withdraw = show.find("crate::withdraw_call_bar(ui);").unwrap();
        let opened = show.find("view.set_open(true)").unwrap();
        assert!(refused < withdraw && withdraw < opened);
        let (_, route) = identity.split_once("pub(crate) fn route_member(").unwrap();
        let route = route.split("\nfn clear(").next().unwrap();
        let refused = route.find("if !view.get_open()").unwrap();
        let withdraw = route.find("crate::withdraw_call_bar(ui);").unwrap();
        let closed = route.find("view.set_open(false)").unwrap();
        let inspected = route
            .find("crate::member_privacy::inspect_admitted")
            .unwrap();
        assert!(refused < withdraw && withdraw < closed && closed < inspected);
    }

    /// Present `view`, as the status watcher does.
    pub(crate) fn present_for_test(ui: &AppWindow, view: CallView) {
        present(ui, view);
    }

    /// End the presentation, as the status watcher does when the call ends.
    pub(crate) fn clear_for_test(ui: &AppWindow) {
        present(ui, CallView::none());
    }
}
