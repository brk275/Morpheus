//! Settings › Security & recovery › Devices: the signed-in account's
//! sessions. One panel per opened list holds the account selection it was
//! opened for. Every action captures that selection, the exact device ID and
//! a fresh operation number when it starts, and its result is shown only
//! while the same panel, operation and visible account are still current;
//! the core re-checks the selection before every request it sends. A
//! sign-out challenge lives only in the panel, is used once, and is dropped
//! with the panel on account change, logout, detach, leaving Settings or
//! shutdown. A typed password is never stored: it moves from the field
//! straight into zeroizing memory and into the one request that uses it.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use morpheus_matrix::{
    AccountSelection, CrossSigningState, DeviceSession, DeviceSessionCancellation,
    DeviceSessionError, DeviceSessionSnapshot, DeviceSessions, DeviceTrust, SignOutBlocker,
    SignOutChallenge, SignOutMethod, SignOutStep, VerificationInfo, bounded_device_name,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;
use url::Url;
use zeroize::Zeroizing;

use crate::{AppWindow, DeviceSessionRow, DeviceSessionsView, live::LiveUi};

/// Search input is cut to this many characters before matching.
const MAX_QUERY_CHARS: usize = 128;
const SAS_IDLE: i32 = 0;
const SAS_WAITING_FOR_REQUEST: i32 = 1;

const UNAVAILABLE: &str = "Sign in to see where this account is signed in.";
const LOADING: &str = "Loading this account's sessions…";
const BUSY_ELSEWHERE: &str = "Finish the security step in progress on this page, then try again.";
const SAS_IN_PROGRESS: &str =
    "A verification is already in progress above. Finish or cancel it first.";
const USE_LOG_OUT: &str =
    "This is the device you are using. Sign it out with Log out in the account menu.";
const STALE: &str = "That session is no longer in this list. Refresh and review it again.";

// The panel currently shown, so account changes, detach and shutdown can
// retire it (and any sign-out challenge it holds) without a controller.
static PANEL: Mutex<Option<Arc<Panel>>> = Mutex::new(None);
static NEXT_PANEL: AtomicU64 = AtomicU64::new(0);

struct Panel {
    id: u64,
    selection: AccountSelection,
    cancellation: DeviceSessionCancellation,
    state: Mutex<PanelState>,
}

impl Panel {
    fn state(&self) -> MutexGuard<'_, PanelState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Default)]
struct PanelState {
    session: Option<Arc<DeviceSessions>>,
    snapshot: Option<DeviceSessionSnapshot>,
    loading: bool,
    load: u64,
    // Every action takes a new number; its result needs the same number.
    operation: u64,
    operation_cancel: DeviceSessionCancellation,
    busy: bool,
    selected: String,
    sign_out: SignOut,
    pending: String,
    error: String,
    notice: String,
}

impl PanelState {
    fn begin(&mut self, pending: String) -> u64 {
        self.operation_cancel.cancel();
        self.operation_cancel = DeviceSessionCancellation::new();
        self.operation = self.operation.wrapping_add(1);
        self.busy = true;
        self.pending = pending;
        self.error.clear();
        self.notice.clear();
        self.operation
    }
    fn finish(&mut self) {
        self.operation_cancel.cancel();
        self.busy = false;
        self.pending.clear();
    }
}

/// The session a sign-out names, captured from the reviewed row.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    device_id: String,
    name: String,
}

#[derive(Default)]
enum SignOut {
    #[default]
    Closed,
    Review(Target),
    Password {
        target: Target,
        challenge: SignOutChallenge,
    },
    Browser {
        target: Target,
        challenge: SignOutChallenge,
        page: Url,
        opened: bool,
    },
    Provider {
        target: Target,
        page: Url,
        opened: bool,
    },
    Blocked {
        target: Target,
        blocker: SignOutBlocker,
    },
    Sending(Target),
}

/// What the sign-out pane shows; carries no challenge, page or secret.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stage {
    Closed,
    Review(Target),
    Password {
        target: Target,
        rejected: bool,
    },
    Browser {
        target: Target,
        opened: bool,
    },
    Provider {
        target: Target,
        opened: bool,
    },
    Blocked {
        target: Target,
        blocker: SignOutBlocker,
    },
    Sending(Target),
}

impl SignOut {
    fn stage(&self) -> Stage {
        match self {
            Self::Closed => Stage::Closed,
            Self::Review(target) => Stage::Review(target.clone()),
            Self::Password { target, challenge } => Stage::Password {
                target: target.clone(),
                rejected: challenge.rejected(),
            },
            Self::Browser { target, opened, .. } => Stage::Browser {
                target: target.clone(),
                opened: *opened,
            },
            Self::Provider { target, opened, .. } => Stage::Provider {
                target: target.clone(),
                opened: *opened,
            },
            Self::Blocked { target, blocker } => Stage::Blocked {
                target: target.clone(),
                blocker: *blocker,
            },
            Self::Sending(target) => Stage::Sending(target.clone()),
        }
    }
    fn target(&self) -> Option<&Target> {
        match self {
            Self::Closed => None,
            Self::Review(target) | Self::Sending(target) => Some(target),
            Self::Password { target, .. }
            | Self::Browser { target, .. }
            | Self::Provider { target, .. }
            | Self::Blocked { target, .. } => Some(target),
        }
    }
}

pub(crate) struct DeviceSessionsUi {
    live: Arc<LiveUi>,
    runtime: Handle,
}

impl DeviceSessionsUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_scope_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.open(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_refresh(move || {
            if let Some(ui) = weak.upgrade() {
                this.refresh(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.on_device_sessions_search(move |_| {
            if let Some(ui) = weak.upgrade()
                && let Some(panel) = shown_panel(&ui)
            {
                paint(&ui, &panel);
            }
        });
        let weak = ui.as_weak();
        ui.on_device_sessions_select(move |key| {
            if let Some(ui) = weak.upgrade() {
                select(&ui, key.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_rename(move |key, name| {
            if let Some(ui) = weak.upgrade() {
                this.rename(&ui, key.as_str(), name.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_verify(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.verify(&ui, key.as_str());
            }
        });
        let weak = ui.as_weak();
        ui.on_device_sessions_sign_out(move |key| {
            if let Some(ui) = weak.upgrade() {
                review_sign_out(&ui, key.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_confirm_sign_out(move || {
            if let Some(ui) = weak.upgrade() {
                this.confirm_sign_out(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_password(move |secret| {
            // Into zeroizing memory at once; the field already cleared itself.
            let secret = Zeroizing::new(secret.to_string());
            if let Some(ui) = weak.upgrade() {
                this.password(&ui, secret);
            }
        });
        let weak = ui.as_weak();
        ui.on_device_sessions_open_page(move || {
            if let Some(ui) = weak.upgrade() {
                open_page(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_device_sessions_continue(move || {
            if let Some(ui) = weak.upgrade() {
                this.continue_sign_out(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.on_device_sessions_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                cancel_sign_out(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.on_device_sessions_left(move || {
            if let Some(ui) = weak.upgrade() {
                clear_presentation(&ui);
            }
        });
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        clear_presentation(ui);
    }

    /// Open the list for the visible live account, replacing any other.
    fn open(self: &Arc<Self>, ui: &AppWindow) {
        retire();
        let account = ui.get_active_account_local_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        let selection = live
            .then(|| self.live.manager.account_selection(&account))
            .flatten();
        let Some(selection) = selection else {
            show_unavailable(ui);
            return;
        };
        let id = NEXT_PANEL.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let panel = Arc::new(Panel {
            id,
            selection,
            cancellation: DeviceSessionCancellation::new(),
            state: Mutex::new(PanelState::default()),
        });
        *lock_panel() = Some(panel.clone());
        self.load(ui, panel);
    }

    fn refresh(self: &Arc<Self>, ui: &AppWindow) {
        let Some(panel) = shown_panel(ui) else {
            self.open(ui);
            return;
        };
        if panel.state().busy {
            return;
        }
        self.load(ui, panel);
    }

    /// Read the list (opening the session first) under the panel's account.
    fn load(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        let (load, session) = {
            let mut state = panel.state();
            state.load = state.load.wrapping_add(1);
            state.loading = true;
            (state.load, state.session.clone())
        };
        paint(ui, &panel);
        let live = self.live.clone();
        let weak = ui.as_weak();
        let task = panel.clone();
        self.runtime.spawn(async move {
            let result = async {
                let session = match session {
                    Some(session) => session,
                    None => {
                        let _transition = live.account_transition.lock().await;
                        let (manager, selection) = (&live.manager, &task.selection);
                        let cancellation = task.cancellation.clone();
                        let opened = manager.open_device_sessions_for(selection, cancellation);
                        Arc::new(opened.await?)
                    }
                };
                let snapshot = session.refresh().await?;
                Ok::<_, DeviceSessionError>((session, snapshot))
            };
            let result = result.await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if !panel_shown(&ui, &task) {
                    return;
                }
                let mut state = task.state();
                if state.load != load {
                    return;
                }
                state.loading = false;
                match result {
                    Ok((session, snapshot)) => {
                        state.session = Some(session);
                        keep_reviewed(&mut state, &snapshot);
                        state.snapshot = Some(snapshot);
                    }
                    Err(error) => state.error = error_text(error, None),
                }
                drop(state);
                paint(&ui, &task);
            });
        });
    }

    fn rename(self: &Arc<Self>, ui: &AppWindow, key: &str, name: &str) {
        let Some((panel, session)) = idle_session(ui) else {
            return;
        };
        let mut state = panel.state();
        let snapshot = state.snapshot.as_ref();
        let target = snapshot.and_then(|snapshot| listed_target(snapshot, key));
        let Some(target) = target else {
            state.error = STALE.to_owned();
            drop(state);
            paint(ui, &panel);
            return;
        };
        let Some(name) = bounded_device_name(name) else {
            state.error = error_text(DeviceSessionError::InvalidName, None);
            drop(state);
            paint(ui, &panel);
            return;
        };
        let shown = &target.name;
        let operation = state.begin(format!("Renaming “{shown}”… Waiting for the homeserver."));
        let cancellation = state.operation_cancel.clone();
        drop(state);
        paint(ui, &panel);
        let live = self.live.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            let result = while_requested(&cancellation, async {
                let _transition = live.account_transition.lock().await;
                if live.security_gate.blocked() {
                    Err(None)
                } else {
                    session.rename(&target.device_id, &name).await.map_err(Some)
                }
            })
            .await
            .unwrap_or(Err(Some(DeviceSessionError::Cancelled)));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let Some(mut state) = result_state(&ui, &panel, operation) else {
                    return;
                };
                state.finish();
                match result {
                    Ok(outcome) => {
                        state.notice = if outcome.confirmed {
                            format!("Renamed to “{name}”.")
                        } else {
                            "The homeserver accepted the new name, but the list does not show \
                             it yet. Refresh to check."
                                .to_owned()
                        };
                        if let Some(snapshot) = outcome.snapshot {
                            keep_reviewed(&mut state, &snapshot);
                            state.snapshot = Some(snapshot);
                        }
                    }
                    Err(None) => state.error = BUSY_ELSEWHERE.to_owned(),
                    Err(Some(error)) => state.error = error_text(error, Some(&target)),
                }
                drop(state);
                paint(&ui, &panel);
            });
        });
    }

    /// Hand a session's verification to the existing SAS comparison.
    fn verify(self: &Arc<Self>, ui: &AppWindow, key: &str) {
        let Some((panel, session)) = idle_session(ui) else {
            return;
        };
        let mut state = panel.state();
        let snapshot = state.snapshot.as_ref();
        let found = snapshot.and_then(|snapshot| Some((snapshot, snapshot.session(key)?)));
        let Some((snapshot, row)) = found else {
            state.error = STALE.to_owned();
            drop(state);
            paint(ui, &panel);
            return;
        };
        let action = verify_action(row, snapshot);
        let target = Target {
            device_id: row.device_id.clone(),
            name: session_title(row),
        };
        if action == VerifyAction::None {
            return;
        }
        if !sas_idle(ui) || self.live.security_gate.blocked() {
            state.error = SAS_IN_PROGRESS.to_owned();
            drop(state);
            paint(ui, &panel);
            return;
        }
        if action == VerifyAction::ThisDevice {
            // The existing own-device request: Morpheus asks the account's
            // other devices and the person compares there. That handler only
            // starts from a status that already reads unverified.
            if ui.get_security_device_status().as_str() != "Unverified" {
                state.error = "Refresh the account security status above, then try again.".into();
                drop(state);
                paint(ui, &panel);
                return;
            }
            state.error.clear();
            state.notice = "Approve the request on another signed-in device, then compare the \
                            emoji under “Verify this device”."
                .to_owned();
            drop(state);
            paint(ui, &panel);
            ui.invoke_request_own_verification();
            return;
        }
        let name = &target.name;
        let operation = state.begin(format!("Asking “{name}” to verify… Waiting for it."));
        let cancellation = state.operation_cancel.clone();
        drop(state);
        paint(ui, &panel);
        let live = self.live.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            let result = while_requested(&cancellation, async {
                let _transition = live.account_transition.lock().await;
                if live.security_gate.blocked() {
                    Err(None)
                } else {
                    let sent = session.request_verification(&target.device_id).await;
                    sent.map_err(Some)
                }
            })
            .await
            .unwrap_or(Err(Some(DeviceSessionError::Cancelled)));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let Some(mut state) = result_state(&ui, &panel, operation) else {
                    return;
                };
                state.finish();
                let name = &target.name;
                match result {
                    Ok(flow) if sas_idle(&ui) => {
                        hand_off(&ui, flow, name);
                        state.notice = format!(
                            "Sent a verification request to “{name}”. Continue under “Verify \
                             this device” above; nothing is trusted until you confirm a match."
                        );
                    }
                    Ok(_) => state.error = SAS_IN_PROGRESS.to_owned(),
                    Err(None) => state.error = BUSY_ELSEWHERE.to_owned(),
                    Err(Some(error)) => state.error = error_text(error, Some(&target)),
                }
                drop(state);
                paint(&ui, &panel);
            });
        });
    }

    /// The person confirmed the review: start the sign-out once.
    fn confirm_sign_out(self: &Arc<Self>, ui: &AppWindow) {
        let Some((panel, session)) = idle_session(ui) else {
            return;
        };
        let mut state = panel.state();
        let SignOut::Review(target) = &state.sign_out else {
            return;
        };
        let target = target.clone();
        let snapshot = state.snapshot.as_ref();
        let fresh = snapshot.and_then(|snapshot| listed_target(snapshot, &target.device_id));
        if fresh.as_ref() != Some(&target) {
            state.sign_out = SignOut::Closed;
            state.error = STALE.to_owned();
            drop(state);
            paint(ui, &panel);
            return;
        }
        state.sign_out = SignOut::Sending(target.clone());
        let name = &target.name;
        let operation = state.begin(format!("Signing out “{name}”… Waiting for the homeserver."));
        drop(state);
        paint(ui, &panel);
        let device_id = target.device_id.clone();
        self.send_step(ui, panel, operation, target, async move {
            session.begin_sign_out(&device_id).await
        });
    }

    fn password(self: &Arc<Self>, ui: &AppWindow, secret: Zeroizing<String>) {
        let Some((panel, session)) = idle_session(ui) else {
            return;
        };
        if secret.is_empty() {
            return;
        }
        let mut state = panel.state();
        let SignOut::Password { target, .. } = &state.sign_out else {
            return;
        };
        let target = target.clone();
        let SignOut::Password { challenge, .. } =
            std::mem::replace(&mut state.sign_out, SignOut::Sending(target.clone()))
        else {
            return;
        };
        let name = &target.name;
        let operation = state.begin(format!("Signing out “{name}”… Waiting for the homeserver."));
        drop(state);
        paint(ui, &panel);
        self.send_step(ui, panel, operation, target, async move {
            session.sign_out_with_password(challenge, secret).await
        });
    }

    fn continue_sign_out(self: &Arc<Self>, ui: &AppWindow) {
        let Some((panel, session)) = idle_session(ui) else {
            return;
        };
        let mut state = panel.state();
        if matches!(state.sign_out, SignOut::Provider { .. }) {
            // The provider's page acts on its own; only a fresh list says
            // what happened.
            state.sign_out = SignOut::Closed;
            drop(state);
            self.load(ui, panel);
            return;
        }
        let target = match &state.sign_out {
            SignOut::Browser {
                target,
                opened: true,
                ..
            } => target.clone(),
            _ => return,
        };
        let SignOut::Browser { challenge, .. } =
            std::mem::replace(&mut state.sign_out, SignOut::Sending(target.clone()))
        else {
            return;
        };
        let name = &target.name;
        let operation = state.begin(format!("Signing out “{name}”… Waiting for the homeserver."));
        drop(state);
        paint(ui, &panel);
        self.send_step(ui, panel, operation, target, async move {
            session.sign_out_after_browser(challenge).await
        });
    }

    /// Run one sign-out request and show its step if this panel, operation
    /// and account are still current; otherwise the step (and any challenge
    /// in it) is dropped.
    fn send_step(
        &self,
        ui: &AppWindow,
        panel: Arc<Panel>,
        operation: u64,
        target: Target,
        request: impl Future<Output = Result<SignOutStep, DeviceSessionError>> + Send + 'static,
    ) {
        let cancellation = {
            let state = panel.state();
            if !state.busy || state.operation != operation {
                return;
            }
            state.operation_cancel.clone()
        };
        let live = self.live.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            let result = while_requested(&cancellation, async {
                let _transition = live.account_transition.lock().await;
                if live.security_gate.blocked() {
                    Err(None)
                } else {
                    request.await.map_err(Some)
                }
            })
            .await
            .unwrap_or(Err(Some(DeviceSessionError::Cancelled)));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let Some(mut state) = result_state(&ui, &panel, operation) else {
                    return;
                };
                state.finish();
                let reload = apply_step(&mut state, target, result);
                drop(state);
                paint(&ui, &panel);
                if reload {
                    reload_panel(&ui);
                }
            });
        });
    }
}

/// Cancellation wraps the transition lock as well as the request. A confirmed
/// sign-out cancelled while queued must never start sending after that lock
/// becomes available. Dropping an already-sent request cannot undo its server
/// effect; the UI continues to say to refresh in that case.
async fn while_requested<T>(
    cancellation: &DeviceSessionCancellation,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = work => Some(result),
    }
}

/// The visible panel, while it still belongs to the visible live account.
fn shown_panel(ui: &AppWindow) -> Option<Arc<Panel>> {
    let panel = lock_panel().clone()?;
    panel_shown(ui, &panel).then_some(panel)
}

fn lock_panel() -> MutexGuard<'static, Option<Arc<Panel>>> {
    PANEL.lock().unwrap_or_else(|error| error.into_inner())
}

fn panel_shown(ui: &AppWindow, panel: &Panel) -> bool {
    let current = lock_panel().as_ref().map(|current| current.id);
    let view = ui.get_device_sessions_view();
    panel_applies(&PanelCheck {
        current,
        panel: panel.id,
        shown: view.panel_id.as_str(),
        selection_current: panel.selection.is_current(),
        cancelled: panel.cancellation.is_cancelled(),
        live: ui.get_live_session(),
        visible_account: ui.get_active_account_local_id().as_str(),
        panel_account: panel.selection.account_id(),
    })
}

struct PanelCheck<'a> {
    current: Option<u64>,
    panel: u64,
    shown: &'a str,
    selection_current: bool,
    cancelled: bool,
    live: bool,
    visible_account: &'a str,
    panel_account: &'a str,
}

/// A panel's result may be shown only while it is the current panel, the
/// window shows it, its selection was not revoked or its view cancelled, and
/// the visible live account is the one it was opened for.
fn panel_applies(check: &PanelCheck<'_>) -> bool {
    check.current == Some(check.panel)
        && check.shown == check.panel.to_string()
        && check.selection_current
        && !check.cancelled
        && check.live
        && check.visible_account == check.panel_account
}

/// The panel's state for an action's result, when it is still current and
/// no newer action replaced it.
fn result_state<'a>(
    ui: &AppWindow,
    panel: &'a Panel,
    operation: u64,
) -> Option<MutexGuard<'a, PanelState>> {
    if !panel_shown(ui, panel) {
        return None;
    }
    let state = panel.state();
    (state.operation == operation && state.busy).then_some(state)
}

/// The shown panel and its open session, when nothing else is running.
fn idle_session(ui: &AppWindow) -> Option<(Arc<Panel>, Arc<DeviceSessions>)> {
    let panel = shown_panel(ui)?;
    let state = panel.state();
    if state.busy || state.loading {
        return None;
    }
    let session = state.session.clone()?;
    drop(state);
    Some((panel, session))
}

fn sas_idle(ui: &AppWindow) -> bool {
    ui.get_sas_stage() == SAS_IDLE
        && !ui.get_sas_busy()
        && ui.get_security_recovery_key().is_empty()
        && !ui.get_security_busy()
}

/// Show the new request in the existing SAS panel exactly as its own
/// request path does; that panel drives every later step.
fn hand_off(ui: &AppWindow, flow: VerificationInfo, name: &str) {
    // Invalidate any late result of an earlier verification action.
    ui.set_sas_operation(ui.get_sas_operation().wrapping_add(1));
    ui.set_sas_incoming_loading(false);
    ui.set_sas_error("".into());
    ui.set_sas_compared(false);
    ui.set_sas_flow_id(flow.flow_id.into());
    ui.set_sas_other_user_id(flow.other_user_id.into());
    ui.set_sas_request_initiator(flow.we_started);
    crate::withdraw_call_bar(ui);
    crate::unread_navigation::withdraw_jump(ui);
    ui.set_sas_stage(SAS_WAITING_FOR_REQUEST);
    ui.set_sas_notice(format!("Accept the request on “{name}”, then compare the values.").into());
}

fn select(ui: &AppWindow, key: &str) {
    let Some(panel) = shown_panel(ui) else {
        return;
    };
    let mut state = panel.state();
    if matches!(state.sign_out, SignOut::Sending(_)) {
        return;
    }
    let snapshot = state.snapshot.as_ref();
    let exists = snapshot.is_some_and(|shown| shown.session(key).is_some());
    let key = if exists { key } else { "" };
    let other = state.sign_out.target().is_some_and(|t| t.device_id != key);
    if other {
        // Leaving a session ends its sign-out step and drops its challenge.
        state.sign_out = SignOut::Closed;
    }
    state.selected = key.to_owned();
    state.error.clear();
    drop(state);
    paint(ui, &panel);
}

/// Open the review for one session. Nothing is sent until it is confirmed.
fn review_sign_out(ui: &AppWindow, key: &str) {
    let Some((panel, _)) = idle_session(ui) else {
        return;
    };
    let mut state = panel.state();
    let snapshot = state.snapshot.as_ref();
    match snapshot.and_then(|snapshot| snapshot.session(key)) {
        None => state.error = STALE.to_owned(),
        Some(row) if row.current => state.error = USE_LOG_OUT.to_owned(),
        Some(row) => {
            let target = Target {
                device_id: row.device_id.clone(),
                name: session_title(row),
            };
            state.selected = target.device_id.clone();
            state.sign_out = SignOut::Review(target);
            state.error.clear();
            state.notice.clear();
        }
    }
    drop(state);
    paint(ui, &panel);
}

fn open_page(ui: &AppWindow) {
    let Some(panel) = shown_panel(ui) else {
        return;
    };
    let mut state = panel.state();
    let page = match &mut state.sign_out {
        SignOut::Browser { page, opened, .. } | SignOut::Provider { page, opened, .. } => {
            *opened = true;
            page.clone()
        }
        _ => return,
    };
    // The system browser, as for sign-in; the page was checked to be HTTPS
    // (or loopback development) on the account's own servers.
    if webbrowser::open(page.as_str()).is_err() {
        state.error = "Could not open the system browser.".to_owned();
    }
    drop(state);
    paint(ui, &panel);
}

fn cancel_sign_out(ui: &AppWindow) {
    let Some(panel) = shown_panel(ui) else {
        return;
    };
    let mut state = panel.state();
    if matches!(state.sign_out, SignOut::Sending(_)) {
        // The request may already be with the homeserver; its result is
        // dropped and the list says what happened.
        state.operation = state.operation.wrapping_add(1);
        state.finish();
        state.notice = "Stopped waiting. The sign-out may still complete; refresh to check.".into();
    }
    state.sign_out = SignOut::Closed;
    drop(state);
    paint(ui, &panel);
}

/// Apply one sign-out step; returns whether the list should be read again.
fn apply_step(
    state: &mut PanelState,
    target: Target,
    result: Result<SignOutStep, Option<DeviceSessionError>>,
) -> bool {
    let name = target.name.clone();
    state.sign_out = SignOut::Closed;
    match result {
        Ok(SignOutStep::Done { gone }) => {
            state.notice = if gone {
                format!(
                    "The homeserver signed out “{name}”. Messages and keys already downloaded may remain on that device."
                )
            } else {
                format!("The homeserver accepted signing out “{name}”. Refresh to confirm.")
            };
            state.selected.clear();
            true
        }
        Ok(SignOutStep::Password(challenge)) => {
            state.sign_out = SignOut::Password { target, challenge };
            false
        }
        Ok(SignOutStep::Browser { challenge, page }) => {
            state.sign_out = SignOut::Browser {
                target,
                challenge,
                page,
                opened: false,
            };
            false
        }
        Ok(SignOutStep::ProviderPage { page }) => {
            state.sign_out = SignOut::Provider {
                target,
                page,
                opened: false,
            };
            false
        }
        Ok(SignOutStep::Unsupported(blocker)) => {
            state.sign_out = SignOut::Blocked { target, blocker };
            false
        }
        Err(None) => {
            state.error = BUSY_ELSEWHERE.to_owned();
            false
        }
        Err(Some(error)) => {
            state.error = error_text(error, Some(&target));
            reread_after(error)
        }
    }
}

fn reload_panel(ui: &AppWindow) {
    // Read through the registered refresh path, which re-checks the panel.
    ui.invoke_device_sessions_refresh();
}

/// Keep the selection and an open sign-out only while the fresh list still
/// shows the same session.
fn keep_reviewed(state: &mut PanelState, snapshot: &DeviceSessionSnapshot) {
    if snapshot.session(&state.selected).is_none() {
        state.selected.clear();
    }
    let reviewed = state.sign_out.target().map(|t| t.device_id.clone());
    let still = reviewed.is_some_and(|device_id| listed_target(snapshot, &device_id).is_some());
    if !still && !matches!(state.sign_out, SignOut::Sending(_)) {
        state.sign_out = SignOut::Closed;
    }
}

/// The session a request may name: listed now, with its current title.
fn listed_target(snapshot: &DeviceSessionSnapshot, key: &str) -> Option<Target> {
    let row = snapshot.session(key)?;
    Some(Target {
        device_id: row.device_id.clone(),
        name: session_title(row),
    })
}

/// Retire the shown panel: its session, tasks and any sign-out challenge end.
fn retire() {
    let previous = lock_panel().take();
    if let Some(panel) = previous {
        panel.cancellation.cancel();
        let mut state = panel.state();
        state.operation_cancel.cancel();
        state.sign_out = SignOut::Closed;
        state.session = None;
        state.snapshot = None;
    }
}

/// Account change, logout, detach, leaving Settings and shutdown: retire the
/// panel and clear everything this list showed.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    retire();
    ui.set_device_session_rows(ModelRc::new(VecModel::<DeviceSessionRow>::default()));
    ui.set_device_sessions_view(DeviceSessionsView::default());
    ui.set_device_sessions_query("".into());
}

fn show_unavailable(ui: &AppWindow) {
    ui.set_device_session_rows(ModelRc::new(VecModel::<DeviceSessionRow>::default()));
    ui.set_device_sessions_view(DeviceSessionsView {
        summary: UNAVAILABLE.into(),
        ..DeviceSessionsView::default()
    });
}

fn paint(ui: &AppWindow, panel: &Panel) {
    let state = panel.state();
    let query = ui.get_device_sessions_query();
    let now = now_ms();
    let account = state.snapshot.as_ref().map(|shown| shown.user_id.clone());
    let identity = state.snapshot.as_ref().map(identity_text);
    let (rows, total) = match &state.snapshot {
        Some(snapshot) => visible(snapshot, query.as_str()),
        None => (Vec::new(), 0),
    };
    let shown = shown_text(rows.len(), total, state.snapshot.as_ref());
    let rows: Vec<DeviceSessionRow> = rows.into_iter().map(|row| row_item(row, now)).collect();
    let selected = state.snapshot.as_ref().and_then(|snapshot| {
        let row = snapshot.session(&state.selected)?;
        Some((snapshot, row))
    });
    let stage = state.sign_out.stage();
    let (stage_title, stage_body) = stage_text(&stage, account.as_deref().unwrap_or_default());
    let mut view = DeviceSessionsView {
        panel_id: panel.id.to_string().into(),
        available: true,
        loading: state.loading,
        loaded: state.snapshot.is_some(),
        busy: state.busy,
        summary: summary_text(state.snapshot.as_ref(), state.loading).into(),
        identity: identity.unwrap_or_default().into(),
        pending: state.pending.clone().into(),
        error: state.error.clone().into(),
        notice: state.notice.clone().into(),
        shown: shown.into(),
        stage: stage_code(&stage),
        stage_title: stage_title.into(),
        stage_body: stage_body.into(),
        rejected: matches!(stage, Stage::Password { rejected: true, .. }),
        page_opened: matches!(
            stage,
            Stage::Browser { opened: true, .. } | Stage::Provider { opened: true, .. }
        ),
        ..DeviceSessionsView::default()
    };
    if let Some((snapshot, row)) = selected {
        let verify = verify_action(row, snapshot);
        let (sign_out_label, sign_out_hint) = sign_out_text(row, snapshot.sign_out);
        view.selected_key = row.device_id.clone().into();
        view.selected = row_item(row, now);
        view.selected_detail = detail_text(row, now).into();
        view.can_rename = true;
        view.verify_label = verify_label(verify).into();
        view.verify_hint = verify_hint(row, snapshot).into();
        view.sign_out_label = sign_out_label.into();
        view.sign_out_hint = sign_out_hint.into();
    }
    drop(state);
    ui.set_device_session_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_device_sessions_view(view);
}

fn now_ms() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let since = since.unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// Rows whose name or session ID contains the bounded query, ignoring case,
/// and how many sessions there are in all.
fn visible<'a>(
    snapshot: &'a DeviceSessionSnapshot,
    query: &str,
) -> (Vec<&'a DeviceSession>, usize) {
    let query: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    let query = query.to_lowercase();
    let matches = |row: &&DeviceSession| {
        let name = row.name.as_deref().unwrap_or_default().to_lowercase();
        query.is_empty() || name.contains(&query) || row.device_id.to_lowercase().contains(&query)
    };
    let total = snapshot.sessions.len() + snapshot.omitted;
    (snapshot.sessions.iter().filter(matches).collect(), total)
}

fn shown_text(shown: usize, total: usize, snapshot: Option<&DeviceSessionSnapshot>) -> String {
    let omitted = snapshot.map_or(0, |snapshot| snapshot.omitted);
    if snapshot.is_none() || (shown == total && omitted == 0) {
        return String::new();
    }
    if omitted > 0 {
        return format!("Showing {shown} of {total} sessions; the newest are listed first.");
    }
    format!("Showing {shown} of {total} sessions.")
}

fn row_item(row: &DeviceSession, now: u64) -> DeviceSessionRow {
    DeviceSessionRow {
        key: row.device_id.clone().into(),
        name: session_title(row).into(),
        raw_name: row.name.clone().unwrap_or_default().into(),
        device_id: row.device_id.clone().into(),
        status: trust_text(row.trust).into(),
        last_seen: last_seen_text(row.last_seen_ms, now, row.current).into(),
        current: row.current,
        trust: trust_code(row.trust),
    }
}

fn session_title(row: &DeviceSession) -> String {
    row.name.as_deref().unwrap_or("Unnamed session").to_owned()
}

fn trust_code(trust: DeviceTrust) -> i32 {
    match trust {
        DeviceTrust::Verified => 0,
        DeviceTrust::LocallyTrusted => 1,
        DeviceTrust::SignedUnconfirmed => 2,
        DeviceTrust::Unverified => 3,
        DeviceTrust::Blocked => 4,
        DeviceTrust::NoKeys => 5,
        DeviceTrust::Unknown => 6,
    }
}

fn trust_text(trust: DeviceTrust) -> &'static str {
    match trust {
        DeviceTrust::Verified => "Verified",
        DeviceTrust::LocallyTrusted => "Trusted on this device only",
        DeviceTrust::SignedUnconfirmed => "Signed by your account, not confirmed here",
        DeviceTrust::Unverified => "Not verified",
        DeviceTrust::Blocked => "Blocked on this device",
        DeviceTrust::NoKeys => "No encryption keys",
        DeviceTrust::Unknown => "Encryption state unavailable",
    }
}

/// Activity as a relative time; the server's address is never shown.
fn last_seen_text(seen: Option<u64>, now: u64, current: bool) -> String {
    if current {
        return "Active now".to_owned();
    }
    let Some(seen) = seen else {
        return "No activity reported".to_owned();
    };
    let Some(elapsed) = now.checked_sub(seen) else {
        return "Active recently".to_owned();
    };
    let minutes = elapsed / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    let plural = |count: u64, unit: &str| {
        let s = if count == 1 { "" } else { "s" };
        format!("Active {count} {unit}{s} ago")
    };
    if minutes < 2 {
        "Active in the last few minutes".to_owned()
    } else if hours < 1 {
        plural(minutes, "minute")
    } else if days < 1 {
        plural(hours, "hour")
    } else if days < 60 {
        plural(days, "day")
    } else if days < 730 {
        plural(days / 30, "month")
    } else {
        "Last active over two years ago".to_owned()
    }
}

fn detail_text(row: &DeviceSession, now: u64) -> String {
    let seen = last_seen_text(row.last_seen_ms, now, row.current);
    let trust = trust_text(row.trust);
    let explanation = match row.trust {
        DeviceTrust::Verified => "Its encryption identity is verified.",
        DeviceTrust::LocallyTrusted => {
            "You trusted it here earlier; your account has not signed it."
        }
        DeviceTrust::SignedUnconfirmed => {
            "Your account signed it, but this device is not verified, so it cannot confirm that."
        }
        DeviceTrust::Unverified => "If you don't recognize it, sign it out.",
        DeviceTrust::Blocked => "This device does not send it encrypted messages.",
        DeviceTrust::NoKeys => "No encryption keys are known here, so it cannot be verified yet.",
        DeviceTrust::Unknown => "This device could not read its encryption store.",
    };
    format!("{trust}. {explanation} {seen}.")
}

/// A session this device could still verify.
fn unverified(trust: DeviceTrust) -> bool {
    matches!(
        trust,
        DeviceTrust::Unverified | DeviceTrust::SignedUnconfirmed
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerifyAction {
    None,
    /// This device: the existing request to the account's other devices.
    ThisDevice,
    /// Another session, from this verified device.
    Session,
}

/// Verification never trusts anything by itself: each choice only starts
/// the existing comparison, which the person must confirm on both devices.
fn verify_action(row: &DeviceSession, snapshot: &DeviceSessionSnapshot) -> VerifyAction {
    let unverified = unverified(row.trust);
    match snapshot.this_device_verified {
        Some(false) if row.current || unverified => VerifyAction::ThisDevice,
        Some(true) if !row.current && unverified => VerifyAction::Session,
        _ => VerifyAction::None,
    }
}

fn verify_label(action: VerifyAction) -> &'static str {
    match action {
        VerifyAction::None => "",
        VerifyAction::ThisDevice => "Verify this device…",
        VerifyAction::Session => "Verify session…",
    }
}

fn verify_hint(row: &DeviceSession, snapshot: &DeviceSessionSnapshot) -> &'static str {
    match (verify_action(row, snapshot), row.current) {
        (VerifyAction::ThisDevice, false) => {
            "Verify this device first; then it can verify your other sessions."
        }
        (VerifyAction::ThisDevice, true) => {
            "Compare emoji with another signed-in device of this account."
        }
        (VerifyAction::Session, _) => "Both devices must be open to compare emoji.",
        (VerifyAction::None, _) if snapshot.this_device_verified.is_none() => {
            "This device's verification state is not known yet. Refresh in a moment."
        }
        (VerifyAction::None, _) => "",
    }
}

fn sign_out_text(row: &DeviceSession, method: SignOutMethod) -> (&'static str, &'static str) {
    match (row.current, method) {
        (true, _) => ("", USE_LOG_OUT),
        (false, SignOutMethod::Interactive) => ("Sign out…", ""),
        (false, SignOutMethod::ProviderPage) => (
            "Sign out…",
            "Your sign-in provider signs sessions out on its own page.",
        ),
    }
}

fn summary_text(snapshot: Option<&DeviceSessionSnapshot>, loading: bool) -> String {
    let Some(snapshot) = snapshot else {
        return if loading {
            LOADING
        } else {
            "Sessions could not be loaded."
        }
        .to_owned();
    };
    let total = snapshot.sessions.len() + snapshot.omitted;
    let other_unverified = |row: &&DeviceSession| !row.current && unverified(row.trust);
    let others = snapshot.sessions.iter().filter(other_unverified).count();
    let account = &snapshot.user_id;
    let sessions = if total == 1 { "session" } else { "sessions" };
    let signed_in = format!("{account}: {total} {sessions} signed in");
    match others {
        0 => format!("{signed_in}."),
        1 => format!("{signed_in}; 1 other is not verified."),
        _ => format!("{signed_in}; {others} others are not verified."),
    }
}

fn identity_text(snapshot: &DeviceSessionSnapshot) -> String {
    let this = match snapshot.this_device_verified {
        Some(true) => "This device is verified.",
        Some(false) => "This device is not verified.",
        None => "This device's verification state is not known yet.",
    };
    let cross = match snapshot.cross_signing {
        CrossSigningState::Ready => "Cross-signing is ready on this device.",
        CrossSigningState::NotReadyHere => {
            "Cross-signing is set up for this account but not ready on this device."
        }
        CrossSigningState::NotSetUp => "No cross-signing identity is known for this account.",
        CrossSigningState::Unknown => "Cross-signing state is unavailable on this device.",
    };
    format!("{this} {cross}")
}

fn stage_code(stage: &Stage) -> i32 {
    match stage {
        Stage::Closed => 0,
        Stage::Review(_) => 1,
        Stage::Password { .. } => 2,
        Stage::Browser { .. } => 3,
        Stage::Provider { .. } => 4,
        Stage::Blocked { .. } => 5,
        Stage::Sending(_) => 6,
    }
}

/// The pane's title and body. They name the exact session and account and
/// never contain a page address, server session or secret.
fn stage_text(stage: &Stage, account: &str) -> (String, String) {
    let session = |target: &Target| format!("“{}” ({})", target.name, target.device_id);
    match stage {
        Stage::Closed => (String::new(), String::new()),
        Stage::Review(target) => (
            format!("Sign out {}?", session(target)),
            format!(
                "The homeserver will sign it out of {account}. Messages and keys already \
                 downloaded may remain on that device. This device stays signed in."
            ),
        ),
        Stage::Password { target, .. } => (
            format!("Confirm with your password to sign out {}", session(target)),
            format!(
                "Your homeserver asks for the password of {account}. It is sent once for this \
                 sign-out and is not saved."
            ),
        ),
        Stage::Browser { target, .. } => (
            format!("Confirm in your browser to sign out {}", session(target)),
            "Your homeserver asks you to confirm on its own page. Open it, finish there, then \
             choose Continue."
                .to_owned(),
        ),
        Stage::Provider { target, .. } => (
            format!(
                "Sign out {} on your sign-in provider's page",
                session(target)
            ),
            "This account's sessions are managed by its sign-in provider. Open its page, sign \
             the session out there, then refresh the list. Morpheus cannot see the result \
             until then."
                .to_owned(),
        ),
        Stage::Blocked { target, blocker } => (
            format!("Morpheus cannot sign out {} here", session(target)),
            blocker_text(*blocker).to_owned(),
        ),
        Stage::Sending(target) => (
            format!("Signing out {}…", session(target)),
            "Waiting for the homeserver.".to_owned(),
        ),
    }
}

fn blocker_text(blocker: SignOutBlocker) -> &'static str {
    match blocker {
        SignOutBlocker::UnsupportedAuthentication => {
            "Your homeserver asks for a kind of confirmation Morpheus does not support. Sign \
             the session out on that device itself, or with another app for this account."
        }
        SignOutBlocker::InsecurePage => {
            "Your homeserver's confirmation page is not a secure HTTPS page, so Morpheus will \
             not open it. Sign the session out on that device itself."
        }
        SignOutBlocker::NoProviderPage => {
            "Your sign-in provider names no page for managing sessions. Sign the session out \
             on that device itself, or on the provider's own website."
        }
    }
}

/// Fixed, actionable text. No server message, address or secret is shown.
fn error_text(error: DeviceSessionError, target: Option<&Target>) -> String {
    let name = target.map_or_else(|| "That session".to_owned(), |t| format!("“{}”", t.name));
    match error {
        DeviceSessionError::Cancelled | DeviceSessionError::AccountChanged => {
            "The account changed before this finished. Nothing more was sent.".to_owned()
        }
        DeviceSessionError::Unavailable => {
            "The homeserver could not be reached or did not answer. Check the connection, then \
             refresh."
                .to_owned()
        }
        DeviceSessionError::RateLimited => {
            "The homeserver is limiting requests. Wait a moment, then refresh.".to_owned()
        }
        DeviceSessionError::TimedOut => {
            "The homeserver did not answer in time. It may still have applied the change; \
             refresh before trying again."
                .to_owned()
        }
        DeviceSessionError::NotFound => format!("{name} is no longer signed in to this account."),
        DeviceSessionError::CurrentDevice => USE_LOG_OUT.to_owned(),
        DeviceSessionError::InvalidName => {
            "Enter a name of at most 64 characters, without control characters.".to_owned()
        }
        DeviceSessionError::Forbidden => "The homeserver refused this change.".to_owned(),
        DeviceSessionError::Unsupported => {
            "Your homeserver does not support this change from Morpheus.".to_owned()
        }
        DeviceSessionError::ThisDeviceUnverified => {
            "Verify this device first; then it can verify your other sessions.".to_owned()
        }
        DeviceSessionError::NoKeys => format!("{name} has no encryption keys to verify."),
        DeviceSessionError::AlreadyVerified => format!("{name} is already verified."),
        DeviceSessionError::ChallengeMismatch => {
            "This sign-out step expired. Start again from the session.".to_owned()
        }
    }
}

/// Failures that leave the list possibly out of date are followed by a read,
/// never by a resend.
fn reread_after(error: DeviceSessionError) -> bool {
    !matches!(
        error,
        DeviceSessionError::Cancelled
            | DeviceSessionError::AccountChanged
            | DeviceSessionError::InvalidName
            | DeviceSessionError::CurrentDevice
            | DeviceSessionError::ChallengeMismatch
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_queued_session_write_never_sends_when_transition_unlocks() {
        let transition = tokio::sync::Mutex::new(());
        let held = transition.lock().await;
        let cancellation = DeviceSessionCancellation::new();
        let sent = std::sync::atomic::AtomicBool::new(false);
        let writing = while_requested(&cancellation, async {
            let _transition = transition.lock().await;
            sent.store(true, Ordering::SeqCst);
        });
        let cancel = async {
            tokio::task::yield_now().await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(writing, cancel);
        assert_eq!(result, None);
        drop(held);
        tokio::task::yield_now().await;
        assert!(!sent.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn finishing_or_superseding_an_operation_cannot_revive_its_pending_write() {
        let mut state = PanelState::default();
        state.begin("First operation".into());
        let first = state.operation_cancel.clone();
        state.begin("New operation".into());
        assert_eq!(
            while_requested(&first, async { panic!("superseded operation sent") }).await,
            None::<()>
        );
        let current = state.operation_cancel.clone();
        assert_eq!(while_requested(&current, async { 7 }).await, Some(7));
        state.finish();
        assert_eq!(
            while_requested(&current, async { panic!("cancelled operation sent") }).await,
            None::<()>
        );
    }

    fn row(id: &str, name: Option<&str>, trust: DeviceTrust, current: bool) -> DeviceSession {
        DeviceSession {
            device_id: id.to_owned(),
            name: name.map(str::to_owned),
            current,
            trust,
            last_seen_ms: Some(1_000),
        }
    }

    fn snapshot(this_verified: Option<bool>) -> DeviceSessionSnapshot {
        DeviceSessionSnapshot {
            user_id: "@me:example.org".to_owned(),
            current_device_id: "THIS".to_owned(),
            sessions: vec![
                row("THIS", Some("Desk"), DeviceTrust::Verified, true),
                row("PHONE", Some("Phone"), DeviceTrust::Unverified, false),
                row("OLD", None, DeviceTrust::NoKeys, false),
                row(
                    "SIGNED",
                    Some("Tablet"),
                    DeviceTrust::SignedUnconfirmed,
                    false,
                ),
            ],
            omitted: 0,
            cross_signing: CrossSigningState::Ready,
            this_device_verified: this_verified,
            sign_out: SignOutMethod::Interactive,
        }
    }

    fn check<'a>(account: &'a str, shown: &'a str) -> PanelCheck<'a> {
        PanelCheck {
            current: Some(7),
            panel: 7,
            shown,
            selection_current: true,
            cancelled: false,
            live: true,
            visible_account: account,
            panel_account: "alice",
        }
    }

    #[test]
    fn results_apply_only_to_the_same_panel_account_and_selection() {
        assert!(panel_applies(&check("alice", "7")));
        // Another account is shown (switch, or A -> B -> A with a new panel).
        assert!(!panel_applies(&check("bob", "7")));
        // The window shows a newer panel, or none after detach.
        assert!(!panel_applies(&check("alice", "8")));
        assert!(!panel_applies(&check("alice", "")));
        let retired = PanelCheck {
            current: None,
            ..check("alice", "7")
        };
        assert!(!panel_applies(&retired));
        let revoked = PanelCheck {
            selection_current: false,
            ..check("alice", "7")
        };
        assert!(!panel_applies(&revoked));
        let closed = PanelCheck {
            cancelled: true,
            ..check("alice", "7")
        };
        assert!(!panel_applies(&closed));
        let offline = PanelCheck {
            live: false,
            ..check("alice", "7")
        };
        assert!(!panel_applies(&offline));
    }

    #[test]
    fn this_device_and_stale_rows_are_never_reviewed_for_sign_out() {
        let shown = snapshot(Some(true));
        assert_eq!(listed_target(&shown, "GONE"), None);
        let phone = listed_target(&shown, "PHONE").unwrap();
        assert_eq!(phone.name, "Phone");
        let (label, hint) = sign_out_text(shown.session("THIS").unwrap(), shown.sign_out);
        assert_eq!((label, hint), ("", USE_LOG_OUT));
        let (label, _) = sign_out_text(shown.session("PHONE").unwrap(), shown.sign_out);
        assert_eq!(label, "Sign out…");

        // A refreshed list without the reviewed session closes the review.
        let mut state = PanelState {
            selected: "PHONE".to_owned(),
            sign_out: SignOut::Review(phone.clone()),
            ..PanelState::default()
        };
        let mut gone = shown.clone();
        gone.sessions.retain(|row| row.device_id != "PHONE");
        keep_reviewed(&mut state, &gone);
        assert!(state.selected.is_empty());
        assert_eq!(state.sign_out.stage(), Stage::Closed);

        // A reviewed session that is still listed keeps its review.
        state.selected = "PHONE".to_owned();
        state.sign_out = SignOut::Review(phone);
        keep_reviewed(&mut state, &shown);
        assert!(matches!(state.sign_out.stage(), Stage::Review(_)));
    }

    #[test]
    fn verification_only_starts_the_existing_comparison() {
        let verified = snapshot(Some(true));
        let at = |snapshot: &DeviceSessionSnapshot, id: &str| {
            verify_action(snapshot.session(id).unwrap(), snapshot)
        };
        assert_eq!(at(&verified, "THIS"), VerifyAction::None);
        assert_eq!(at(&verified, "PHONE"), VerifyAction::Session);
        assert_eq!(at(&verified, "SIGNED"), VerifyAction::Session);
        assert_eq!(at(&verified, "OLD"), VerifyAction::None);
        // From an unverified device, every path verifies this device first.
        let unverified = snapshot(Some(false));
        assert_eq!(at(&unverified, "THIS"), VerifyAction::ThisDevice);
        assert_eq!(at(&unverified, "PHONE"), VerifyAction::ThisDevice);
        let unknown = snapshot(None);
        assert_eq!(at(&unknown, "PHONE"), VerifyAction::None);
        let hint = verify_hint(unknown.session("PHONE").unwrap(), &unknown);
        assert!(hint.contains("not known yet"));
    }

    #[test]
    fn sign_out_steps_name_the_session_and_account_and_carry_no_secret() {
        let target = Target {
            device_id: "PHONE".to_owned(),
            name: "Phone".to_owned(),
        };
        let stages = [
            Stage::Review(target.clone()),
            Stage::Password {
                target: target.clone(),
                rejected: true,
            },
            Stage::Browser {
                target: target.clone(),
                opened: false,
            },
            Stage::Provider {
                target: target.clone(),
                opened: true,
            },
            Stage::Blocked {
                target: target.clone(),
                blocker: SignOutBlocker::InsecurePage,
            },
            Stage::Sending(target.clone()),
        ];
        for stage in &stages {
            let (title, body) = stage_text(stage, "@me:example.org");
            assert!(title.contains("“Phone” (PHONE)"), "{title}");
            for text in [&title, &body] {
                assert!(
                    !text.contains("http") && !text.contains("session="),
                    "{text}"
                );
            }
            assert_ne!(stage_code(stage), 0);
        }
        let (_, review) = stage_text(&stages[0], "@me:example.org");
        assert!(review.contains("@me:example.org") && review.contains("This device stays"));
        assert!(review.contains("Messages and keys already downloaded may remain"));
        let (_, password) = stage_text(&stages[1], "@me:example.org");
        assert!(password.contains("not saved"));
        for blocker in [
            SignOutBlocker::UnsupportedAuthentication,
            SignOutBlocker::InsecurePage,
            SignOutBlocker::NoProviderPage,
        ] {
            assert!(blocker_text(blocker).contains("Sign the session out"));
        }

        // A refused or failed step closes the pane; the list is read again
        // only when it may be out of date, never resent.
        let mut state = PanelState::default();
        let refused = Err(Some(DeviceSessionError::Forbidden));
        assert!(apply_step(&mut state, target.clone(), refused));
        assert_eq!(state.sign_out.stage(), Stage::Closed);
        assert_eq!(state.error, "The homeserver refused this change.");
        let switched = Err(Some(DeviceSessionError::AccountChanged));
        assert!(!apply_step(&mut state, target.clone(), switched));
        let done = Ok(SignOutStep::Done { gone: true });
        assert!(apply_step(&mut state, target, done));
        assert!(state.notice.contains("homeserver signed out “Phone”"));
        assert!(
            state
                .notice
                .contains("Messages and keys already downloaded may remain")
        );
    }

    #[test]
    fn rows_are_searchable_bounded_and_never_show_an_address() {
        let shown = snapshot(Some(true));
        let ids = |query: &str| {
            let (rows, total) = visible(&shown, query);
            let ids: Vec<String> = rows.iter().map(|row| row.device_id.clone()).collect();
            (ids, total)
        };
        assert_eq!(ids("").0.len(), 4);
        assert_eq!(ids("  PHO ").0, ["PHONE"]);
        assert_eq!(ids("tablet").0, ["SIGNED"]);
        assert_eq!(ids("zzz"), (Vec::<String>::new(), 4));
        let long = "x".repeat(MAX_QUERY_CHARS * 10);
        assert!(ids(&long).0.is_empty());

        let item = row_item(shown.session("OLD").unwrap(), 2_000);
        assert_eq!(item.name, "Unnamed session");
        assert!(item.raw_name.is_empty());
        assert_eq!(item.status, "No encryption keys");
        assert_eq!(item.trust, 5);
        assert_eq!(last_seen_text(None, 5, false), "No activity reported");
        assert_eq!(last_seen_text(Some(10), 5, false), "Active recently");
        let day = 24 * 60 * 60 * 1000;
        assert_eq!(last_seen_text(Some(0), 3 * day, false), "Active 3 days ago");
        assert_eq!(
            last_seen_text(Some(0), day / 24, false),
            "Active 1 hour ago"
        );
        assert_eq!(last_seen_text(Some(0), 3 * day, true), "Active now");
        let summary = summary_text(Some(&shown), false);
        let expected = "@me:example.org: 4 sessions signed in; 2 others are not verified.";
        assert_eq!(summary, expected);
    }

    #[test]
    fn errors_are_fixed_actionable_text() {
        let target = Target {
            device_id: "PHONE".to_owned(),
            name: "Phone".to_owned(),
        };
        let gone = error_text(DeviceSessionError::NotFound, Some(&target));
        assert_eq!(gone, "“Phone” is no longer signed in to this account.");
        assert_eq!(
            error_text(DeviceSessionError::CurrentDevice, None),
            USE_LOG_OUT
        );
        assert!(!reread_after(DeviceSessionError::AccountChanged));
        assert!(reread_after(DeviceSessionError::TimedOut));
    }
}
