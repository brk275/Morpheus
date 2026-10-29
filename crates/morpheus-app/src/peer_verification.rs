//! Peer SAS controller. Raw IDs and SDK sessions stay in this owner; Slint gets
//! escaped labels and opaque row keys only. Every async result belongs to one
//! exact selection/view/operation. All spawned application futures stay owned
//! until their real join receipts, including after protection or navigation.

use crate::{AppWindow, PeerVerification, PeerVerificationRow, PeerVerificationView, live::LiveUi};
use morpheus_matrix::{
    AccountSelection, OwnVerificationError as Error, OwnVerificationPhase as Phase,
    PeerVerificationInfo, PeerVerificationOutcome as Outcome, PeerVerificationRequest,
    PeerVerificationSession, PeerVerificationTarget, PendingPeerVerifications, SasPresentation,
    VerificationCancellation,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::sync::{Arc, Mutex, atomic::Ordering};
use tokio::runtime::Handle;

const UNAVAILABLE: &str = "Open a joined direct conversation with this exact Matrix ID, then try again. No room or invitation was created.";

#[derive(Clone)]
struct Scope {
    identity: Arc<()>,
    selection: AccountSelection,
    cancellation: VerificationCancellation,
    lease: crate::verification_work::VerificationWorkLease,
}
struct View {
    scope: Scope,
    operation: Arc<()>,
    active: Option<PeerVerificationSession>,
    proposed: Option<PeerVerificationTarget>,
    inbox: Vec<(i32, PeerVerificationInfo)>,
    busy: bool,
    has_values: bool,
    confirmed: bool,
}
impl Drop for View {
    fn drop(&mut self) {
        self.scope.cancellation.cancel();
    }
}
#[derive(Default)]
struct State {
    view: Option<View>,
    closed: bool,
    next_row: i32,
}

pub(crate) struct PeerVerificationUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    state: Mutex<State>,
}
#[derive(Clone, Copy)]
enum Action {
    Request(bool),
    Review(i32),
    Refresh,
    Accept,
    Advance,
    Confirm,
    Cancel,
}
struct Reply {
    session: Option<PeerVerificationSession>,
    info: Option<PeerVerificationInfo>,
    values: Option<SasPresentation>,
    outcome: Option<Outcome>,
    listing: Option<PendingPeerVerifications>,
    request_sent: Option<bool>,
}
impl Reply {
    fn session(session: PeerVerificationSession, info: PeerVerificationInfo) -> Self {
        Self {
            session: Some(session),
            info: Some(info),
            values: None,
            outcome: None,
            listing: None,
            request_sent: None,
        }
    }
    fn listing(listing: PendingPeerVerifications) -> Self {
        Self {
            session: None,
            info: None,
            values: None,
            outcome: None,
            listing: Some(listing),
            request_sent: None,
        }
    }
}

/// The event queue owns only this revocable slot, never an SDK client/SAS
/// directly. The producer withdraws an unclaimed reply before its join receipt,
/// including cancellation, queue failure and unwinding. A claimed session is
/// then owned by View and synchronously dropped by protect/shutdown.
struct Publication<T>(Mutex<Option<T>>);
impl<T> Publication<T> {
    fn take(&self) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}
struct PendingPublication<T>(Arc<Publication<T>>);
impl<T> Drop for PendingPublication<T> {
    fn drop(&mut self) {
        self.0.take();
    }
}

/// Serializes callback admission with withdrawal of a timed-out/cancelled
/// operation. A callback that lost this capability cannot affect a later view.
struct PendingWithdrawal(Arc<Mutex<bool>>);
impl Drop for PendingWithdrawal {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = false;
    }
}

/// The caller must already hold the shared account work pause, so no new peer
/// view can acquire a lease. This is actual UI-owned session withdrawal before
/// task drain/store revocation, not a substitute for the real task join.
pub(crate) async fn withdraw_paused_account(
    ui: slint::Weak<AppWindow>,
    account: String,
) -> Result<(), crate::verification_work::VerificationDrainError> {
    use crate::verification_work::VerificationDrainError;
    let pending = PendingWithdrawal(Arc::new(Mutex::new(true)));
    let admission = pending.0.clone();
    let (ack, acknowledged) = tokio::sync::oneshot::channel();
    ui.upgrade_in_event_loop(move |window| {
        let mut admitted = admission.lock().unwrap_or_else(|error| error.into_inner());
        if !*admitted || ack.is_closed() {
            return;
        }
        // The registered callback drops only the captured account's view;
        // quick removal of A cannot withdraw the visible unrelated account B.
        let absent = window
            .global::<PeerVerification>()
            .invoke_withdraw_account(account.into());
        *admitted = false;
        let _ = ack.send(absent);
    })
    .map_err(|_| VerificationDrainError::Failed)?;
    let result = match tokio::time::timeout(std::time::Duration::from_secs(10), acknowledged).await
    {
        Ok(Ok(true)) => Ok(()),
        Ok(_) => Err(VerificationDrainError::Failed),
        Err(_) => Err(VerificationDrainError::Pending),
    };
    // Waiters withdrawing this admission linearize with an already-entered UI
    // callback under the same short mutex. No late callback can retire a view
    // after this producer relinquishes the operation.
    drop(pending);
    result
}

/// Exact IDs remain visible, with invisible controls/directional overrides
/// escaped rather than deleted, substituted or used as routing strings.
fn exact_label(id: &str) -> String {
    let mut label = String::new();
    for ch in id.chars() {
        if ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            use std::fmt::Write;
            let _ = write!(&mut label, "\\u{{{:x}}}", u32::from(ch));
        } else {
            label.push(ch);
        }
    }
    label
}
fn values(presentation: SasPresentation) -> Option<(String, String)> {
    let emoji = presentation
        .emoji
        .unwrap_or_default()
        .into_iter()
        .take(7)
        .map(|item| {
            format!(
                "{} {}",
                exact_label(&item.symbol),
                exact_label(&item.description)
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let decimals = presentation
        .decimals
        .map(|v| format!("{:04} · {:04} · {:04}", v[0], v[1], v[2]))
        .unwrap_or_default();
    (!emoji.is_empty() || !decimals.is_empty()).then_some((emoji, decimals))
}
fn may_confirm(has_values: bool, already_confirmed: bool, busy: bool, compared: bool) -> bool {
    has_values && !already_confirmed && !busy && compared
}
fn next_row_key(last: &mut i32) -> Option<i32> {
    let next = last.checked_add(1)?;
    *last = next;
    Some(next)
}
fn eligible(ui: &AppWindow) -> bool {
    ui.get_live_session()
        && ui.get_page() == 1
        && !ui.get_peer_verification_protected()
        && !ui.get_application_closing()
}
fn fixed_error(error: Error) -> &'static str {
    match error {
        Error::Unknown => {
            "This step may have reached this person. Refresh status; it was not sent again."
        }
        Error::RequestUncertain => {
            "An earlier request may have reached this person. Check with them before explicitly requesting again."
        }
        Error::Unavailable => UNAVAILABLE,
        Error::RequestInProgress => {
            "There are active or pending verification requests. Finish a request, or wait for it to expire before requesting again."
        }
        Error::SecurityBusy => "Finish the account or security step before continuing.",
        Error::NotReady => {
            "The other device has not reached this step yet. Keep both conversations open, then refresh."
        }
        Error::Unsupported => {
            "This request does not offer emoji or number comparison. It cannot continue here."
        }
        Error::Gone => {
            "This request is no longer available here. This does not confirm the person’s identity."
        }
        Error::TimedOut => "No answer in time. Refresh status before trying another action.",
        Error::AccountChanged | Error::Cancelled => "",
    }
}
fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::IdentityVerified => "This person's signing identity is verified on this device.",
        Outcome::DeviceOnly => {
            "The comparison completed, but this person's signing identity could not be verified. It may have verified a device only. Recover your account's signing keys if needed."
        }
        Outcome::IdentityChanged => {
            "This person's signing identity changed. This comparison does not verify the new identity. Check with them, then start again."
        }
        Outcome::Unconfirmed => {
            "The confirmation is no longer available here. This person's identity could not be confirmed."
        }
        Outcome::Pending => "Waiting for the other device to finish this exact comparison.",
    }
}

// A previous successful read is not a fresh identity receipt. In particular,
// an expired retained session or failed refresh cannot leave a verified claim
// on screen alongside an error saying that the comparison is unavailable.
fn withdraw_failed_completion(shown: &mut PeerVerificationView, error: Error) {
    if shown.stage == 5 || matches!(error, Error::Gone | Error::Unsupported) {
        shown.stage = 5;
        shown.detail = outcome_text(Outcome::Unconfirmed).into();
        shown.emoji = "".into();
        shown.decimals = "".into();
    }
}

// The complete error presentation used by finish_reply after its original
// account/view/operation admission. It changes no SDK or owner capabilities.
fn apply_error_presentation(shown: &mut PeerVerificationView, action: Action, error: Error) {
    withdraw_failed_completion(shown, error);
    shown.error = fixed_error(error).into();
    if matches!(error, Error::Unknown | Error::RequestUncertain) {
        shown.uncertain = true;
    }
    if matches!(action, Action::Confirm) {
        shown.emoji = "".into();
        shown.decimals = "".into();
        if error == Error::Unknown {
            shown.stage = 4;
        }
    }
    if matches!(action, Action::Accept) && error == Error::Unknown {
        shown.stage = 2;
    }
    if matches!(action, Action::Cancel) && error == Error::Unknown {
        shown.stage = 5;
    }
    if matches!(error, Error::Gone | Error::Unsupported) {
        shown.stage = 5;
        shown.emoji = "".into();
        shown.decimals = "".into();
    }
    if matches!(action, Action::Request(_)) {
        shown.stage = 0;
    }
}

pub(crate) fn clear_presentation(ui: &AppWindow) {
    let peer = ui.global::<PeerVerification>();
    peer.set_view(PeerVerificationView::default());
    peer.set_requests(ModelRc::new(VecModel::<PeerVerificationRow>::default()));
    peer.set_peer_input("".into());
    peer.set_compared(false);
    peer.set_checked_uncertain(false);
    peer.set_inbox_note("".into());
    peer.invoke_protect();
}

impl PeerVerificationUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            state: Mutex::new(State::default()),
        });
        let global = ui.global::<PeerVerification>();
        macro_rules! action {
            ($setter:ident, $action:expr) => {{
                let this = owner.clone();
                let weak = ui.as_weak();
                global.$setter(move || {
                    if let Some(ui) = weak.upgrade() {
                        this.act(&ui, $action);
                    }
                });
            }};
        }
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_request(move || {
            if let Some(ui) = weak.upgrade() {
                this.review_outgoing(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_send(move || {
            if let Some(ui) = weak.upgrade() {
                let global = ui.global::<PeerVerification>();
                let ack = global.get_checked_uncertain();
                // One deliberate request consumes one acknowledgement. An
                // error or another omitted peer never inherits a checked box.
                global.set_checked_uncertain(false);
                this.act(&ui, Action::Request(ack));
            }
        });
        action!(on_refresh, Action::Refresh);
        action!(on_accept, Action::Accept);
        action!(on_advance, Action::Advance);
        action!(on_confirm, Action::Confirm);
        action!(on_cancel, Action::Cancel);
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_review(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.act(&ui, Action::Review(key));
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                this.retire(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_protect(move || {
            if let Some(ui) = weak.upgrade() {
                this.retire(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_withdraw_account(move |account| {
            weak.upgrade()
                .is_some_and(|ui| this.retire_account(&ui, account.as_str()))
        });
        let weak = ui.as_weak();
        global.on_open_conversations(move || {
            if let Some(ui) = weak.upgrade()
                && eligible(&ui)
            {
                ui.invoke_navigate_page(0);
            }
        });
        owner
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
    fn retire(&self, ui: &AppWindow) {
        self.lock().view.take();
        let peer = ui.global::<PeerVerification>();
        peer.set_view(PeerVerificationView::default());
        peer.set_requests(ModelRc::new(VecModel::<PeerVerificationRow>::default()));
        peer.set_peer_input("".into());
        peer.set_compared(false);
        peer.set_checked_uncertain(false);
        peer.set_inbox_note("".into());
    }
    fn retire_account(&self, ui: &AppWindow, account: &str) -> bool {
        let removed = {
            let mut state = self.lock();
            if state
                .view
                .as_ref()
                .is_some_and(|view| view.scope.selection.account_id() == account)
            {
                state.view.take();
                true
            } else {
                false
            }
        };
        if removed {
            let peer = ui.global::<PeerVerification>();
            peer.set_view(PeerVerificationView::default());
            peer.set_requests(ModelRc::new(VecModel::<PeerVerificationRow>::default()));
            peer.set_peer_input("".into());
            peer.set_compared(false);
            peer.set_checked_uncertain(false);
            peer.set_inbox_note("".into());
        }
        !self
            .lock()
            .view
            .as_ref()
            .is_some_and(|view| view.scope.selection.account_id() == account)
    }
    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.lock().closed = true;
        self.retire(ui);
    }
    fn scope(&self, ui: &AppWindow) -> Option<Scope> {
        if !eligible(ui)
            || self.live.security_gate.blocked()
            || self.live.action_admission_closed()
            || !self.live.is_live.load(Ordering::Acquire)
        {
            self.retire(ui);
            return None;
        }
        let Some(selection) = self
            .live
            .manager
            .account_selection(ui.get_active_account_local_id().as_str())
        else {
            self.retire(ui);
            return None;
        };
        {
            let mut state = self.lock();
            if state.closed {
                return None;
            }
            if let Some(view) = &state.view
                && view.scope.selection.is_current()
                && view.scope.selection.account_id() == selection.account_id()
                && !view.scope.cancellation.is_cancelled()
                && view.scope.lease.is_current()
            {
                return Some(view.scope.clone());
            }
            if state.view.take().is_some() {
                let peer = ui.global::<PeerVerification>();
                peer.set_view(PeerVerificationView::default());
                peer.set_requests(ModelRc::new(VecModel::<PeerVerificationRow>::default()));
                peer.set_compared(false);
                peer.set_checked_uncertain(false);
                peer.set_inbox_note("".into());
            }
        }
        let cancellation = VerificationCancellation::new();
        let lease = self
            .live
            .verification_work_lease(&selection, &cancellation)?;
        let scope = Scope {
            identity: Arc::new(()),
            selection,
            cancellation,
            lease,
        };
        let mut state = self.lock();
        if state.closed {
            scope.cancellation.cancel();
            return None;
        }
        state.view = Some(View {
            scope: scope.clone(),
            operation: Arc::new(()),
            active: None,
            proposed: None,
            inbox: Vec::new(),
            busy: false,
            has_values: false,
            confirmed: false,
        });
        Some(scope)
    }
    fn publishable(&self, ui: &AppWindow, scope: &Scope, operation: &Arc<()>) -> bool {
        eligible(ui)
            && !self.live.security_gate.blocked()
            && !self.live.action_admission_closed()
            && scope.selection.is_current()
            && !scope.cancellation.is_cancelled()
            && scope.lease.is_current()
            && ui.get_active_account_local_id().as_str() == scope.selection.account_id()
            && self.lock().view.as_ref().is_some_and(|view| {
                Arc::ptr_eq(&view.scope.identity, &scope.identity)
                    && Arc::ptr_eq(&view.operation, operation)
            })
    }
    fn review_outgoing(&self, ui: &AppWindow) {
        if self.scope(ui).is_none() {
            return;
        }
        let peer = ui.global::<PeerVerification>();
        let target = PeerVerificationTarget::parse(
            peer.get_peer_input().as_str(),
            ui.get_active_room_id().as_str(),
        );
        let mut state = self.lock();
        let Some(view) = state.view.as_mut() else {
            return;
        };
        if view.busy || view.active.is_some() {
            return;
        }
        let mut shown = peer.get_view();
        match target {
            Ok(target) => {
                shown.stage = 6;
                shown.peer = exact_label(target.user_id.as_str()).into();
                shown.room = exact_label(target.room_id.as_str()).into();
                shown.detail = "Send one SAS request to this exact person in this exact existing direct conversation? No room or invitation will be created.".into();
                shown.error = "".into();
                view.proposed = Some(target);
            }
            Err(error) => shown.error = fixed_error(error).into(),
        }
        peer.set_view(shown);
    }
    fn act(self: &Arc<Self>, ui: &AppWindow, action: Action) {
        let Some(scope) = self.scope(ui) else { return };
        let global = ui.global::<PeerVerification>();
        let (operation, session, reviewed) = {
            let mut state = self.lock();
            let Some(view) = state.view.as_mut() else {
                return;
            };
            if view.busy {
                return;
            }
            let stage = global.get_view().stage;
            if !match action {
                Action::Request(_) => stage == 6,
                Action::Review(_) => stage == 0 && view.active.is_none(),
                Action::Accept => stage == 1,
                Action::Advance => stage == 2,
                Action::Confirm => stage == 3,
                Action::Cancel => (1..5).contains(&stage),
                Action::Refresh => stage != 6,
            } {
                return;
            }
            if matches!(action, Action::Confirm)
                && !may_confirm(
                    view.has_values,
                    view.confirmed,
                    view.busy,
                    global.get_compared(),
                )
            {
                return;
            }
            if matches!(action, Action::Request(_))
                && (view.active.is_some() || view.proposed.is_none())
            {
                return;
            }
            if matches!(action, Action::Refresh) && view.proposed.is_some() {
                return;
            }
            let reviewed = if let Action::Review(key) = action {
                let Some((_, info)) = view.inbox.iter().find(|(known, _)| *known == key) else {
                    return;
                };
                Some(info.clone())
            } else {
                None
            };
            let session = view.active.clone();
            if matches!(
                action,
                Action::Accept | Action::Advance | Action::Confirm | Action::Cancel
            ) && session.is_none()
            {
                return;
            }
            view.operation = Arc::new(());
            view.busy = true;
            (view.operation.clone(), session, reviewed)
        };
        let target = if matches!(action, Action::Request(_)) {
            let proposed = self
                .lock()
                .view
                .as_ref()
                .and_then(|view| view.proposed.clone());
            match proposed
                .filter(|target| target.room_id.as_str() == ui.get_active_room_id().as_str())
            {
                Some(target) => Some(target),
                None => {
                    self.finish_reply(ui, &scope, &operation, action, Err(Error::Gone));
                    return;
                }
            }
        } else {
            None
        };
        let mut shown = global.get_view();
        shown.busy = true;
        shown.error = "".into();
        global.set_view(shown);
        let gate = self.live.security_gate.clone();
        let generation = gate.ringing_generation();
        let current = scope.clone();
        let admitted = move || {
            current.selection.is_current()
                && !current.cancellation.is_cancelled()
                && current.lease.is_current()
                && gate.ringing_allowed(generation)
        };
        let this = self.clone();
        let manager = self.live.manager.clone();
        let task_scope = scope.clone();
        let op = operation.clone();
        let task = async move {
            let result = async {
                match action {
                    Action::Request(ack) => {
                        let target = target.ok_or(Error::Unavailable)?;
                        let result = manager
                            .request_peer_verification_selected(
                                &task_scope.selection,
                                &task_scope.cancellation,
                                &target,
                                &admitted,
                                ack,
                            )
                            .await?;
                        let (session, sent) = match result {
                            PeerVerificationRequest::Sent(session) => (session, true),
                            PeerVerificationRequest::AlreadyOpen(session) => (session, false),
                        };
                        let info = session.info().await?;
                        let mut reply = Reply::session(session, info);
                        reply.request_sent = Some(sent);
                        Ok(reply)
                    }
                    Action::Review(_) => {
                        let info = reviewed.ok_or(Error::Gone)?;
                        let session = manager
                            .open_peer_verification_selected(
                                &task_scope.selection,
                                &task_scope.cancellation,
                                &info.target,
                                &info.flow.flow_id,
                            )
                            .await?;
                        let info = session.info().await?;
                        Ok(Reply::session(session, info))
                    }
                    Action::Refresh if session.is_none() => Ok(Reply::listing(
                        manager
                            .pending_peer_verifications_selected(
                                &task_scope.selection,
                                &task_scope.cancellation,
                            )
                            .await?,
                    )),
                    _ => {
                        let session = session.ok_or(Error::Gone)?;
                        match action {
                            Action::Accept => session.accept(&admitted).await?,
                            Action::Advance => {
                                session.advance(&admitted).await?;
                            }
                            Action::Confirm => session.confirm(&admitted).await?,
                            Action::Cancel => session.cancel(&admitted).await?,
                            _ => {}
                        }
                        let info = session.info().await?;
                        let mut reply = Reply::session(session.clone(), info.clone());
                        if matches!(action, Action::Refresh) {
                            if info.flow.phase == Phase::Comparing {
                                reply.values = session.values().await?;
                            }
                            if info.flow.phase == Phase::Done {
                                reply.outcome = Some(session.outcome().await?);
                            }
                        }
                        Ok(reply)
                    }
                }
            }
            .await;
            let slot = Arc::new(Publication(Mutex::new(Some(result))));
            let pending = PendingPublication(slot.clone());
            let (ack, acknowledged) = tokio::sync::oneshot::channel();
            let callback_slot = slot.clone();
            let callback_scope = task_scope.clone();
            let callback_op = op.clone();
            let callback_owner = Arc::downgrade(&this);
            if this
                .ui
                .upgrade_in_event_loop(move |ui| {
                    let Some(callback_owner) = callback_owner.upgrade() else {
                        callback_slot.take();
                        let _ = ack.send(());
                        return;
                    };
                    // Eligibility is checked on the UI thread before transferring
                    // the SDK session from the revocable producer-owned slot.
                    if callback_owner.publishable(&ui, &callback_scope, &callback_op) {
                        if let Some(result) = callback_slot.take() {
                            callback_owner.finish_reply(
                                &ui,
                                &callback_scope,
                                &callback_op,
                                action,
                                result,
                            );
                        }
                    } else {
                        callback_slot.take();
                    }
                    let _ = ack.send(());
                })
                .is_err()
            {
                return; // pending withdraws the payload before task completion
            }
            let timed_out = tokio::select! {
                biased;
                _ = task_scope.cancellation.cancelled() => false,
                _ = task_scope.selection.revoked() => false,
                result = tokio::time::timeout(std::time::Duration::from_secs(10), acknowledged) => result.is_err(),
            };
            drop(pending);
            if timed_out {
                // Only a fixed error is queued after withdrawal; late callbacks
                // see an empty slot and cannot reattach the expired payload.
                let weak = this.ui.clone();
                let owner = Arc::downgrade(&this);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if let Some(owner) = owner.upgrade() {
                        owner.finish_reply(&ui, &task_scope, &op, action, Err(Error::TimedOut));
                    }
                });
            }
        };
        if !self
            .live
            .spawn_verification_work(&self.runtime, &scope.lease, task)
        {
            self.finish_reply(ui, &scope, &operation, action, Err(Error::SecurityBusy));
        }
    }
    fn finish_reply(
        &self,
        ui: &AppWindow,
        scope: &Scope,
        operation: &Arc<()>,
        action: Action,
        result: Result<Reply, Error>,
    ) {
        if !self.publishable(ui, scope, operation) {
            return;
        }
        let global = ui.global::<PeerVerification>();
        let mut shown = global.get_view();
        shown.busy = false;
        self.lock().view.as_mut().unwrap().busy = false;
        match result {
            Err(error) => {
                apply_error_presentation(&mut shown, action, error);
                if matches!(action, Action::Confirm) {
                    self.lock().view.as_mut().unwrap().has_values = false;
                    global.set_compared(false);
                    // A handed-over confirm is never offered again blindly.
                    if error == Error::Unknown {
                        self.lock().view.as_mut().unwrap().confirmed = true;
                    }
                }
                if matches!(error, Error::Gone | Error::Unsupported) {
                    global.set_compared(false);
                    self.lock().view.as_mut().unwrap().has_values = false;
                }
                if matches!(action, Action::Request(_)) {
                    self.lock().view.as_mut().unwrap().proposed = None;
                }
            }
            Ok(reply) => {
                if let Some(listing) = reply.listing {
                    let mut state = self.lock();
                    let mut rows = Vec::new();
                    let mut inbox = Vec::new();
                    for info in listing
                        .requests
                        .into_iter()
                        .take(morpheus_matrix::MAX_PEER_REQUESTS)
                    {
                        // Never reuse a row key for a different peer/room/flow.
                        let Some(key) = next_row_key(&mut state.next_row) else {
                            state.closed = true;
                            break;
                        };
                        rows.push(PeerVerificationRow {
                            key,
                            peer: exact_label(info.target.user_id.as_str()).into(),
                            room: exact_label(info.target.room_id.as_str()).into(),
                        });
                        inbox.push((key, info));
                    }
                    state.view.as_mut().unwrap().inbox = inbox;
                    global.set_requests(ModelRc::new(VecModel::from(rows)));
                    global.set_inbox_note(
                        if listing.more_open > 0 || listing.older_may_be_omitted {
                            "Showing a bounded list; older or further open requests may be omitted."
                        } else {
                            "Only supported requests received by this device are listed."
                        }
                        .into(),
                    );
                }
                if let Some(session) = reply.session {
                    let mut state = self.lock();
                    let view = state.view.as_mut().unwrap();
                    view.active = Some(session);
                    view.proposed = None;
                }
                if let Some(info) = reply.info {
                    shown.peer = exact_label(info.target.user_id.as_str()).into();
                    shown.room = exact_label(info.target.room_id.as_str()).into();
                    shown.error = "".into();
                    match info.flow.phase {
                        Phase::Requested => {
                            shown.stage = 1;
                            shown.detail = "Review this exact Matrix ID and room before accepting. Nothing has been accepted here.".into();
                        }
                        Phase::Created => {
                            shown.stage = 2;
                            shown.detail = "Request sent. Ask this person to accept it in this exact conversation, then refresh.".into();
                        }
                        Phase::Ready => {
                            shown.stage = 2;
                            shown.detail =
                                "Request accepted. Choose Continue comparison on both devices."
                                    .into();
                        }
                        Phase::Comparing => {
                            if shown.stage != 4 {
                                shown.stage = 2;
                            }
                            shown.detail = "Comparison is starting. Continue once on the receiving device, then refresh to read the values.".into();
                        }
                        Phase::Cancelled { .. } | Phase::Expired => {
                            shown.stage = 5;
                            shown.detail = "This verification was canceled or expired. This does not confirm the person’s identity.".into();
                        }
                        Phase::Done => {
                            shown.stage = 5;
                            shown.detail =
                                outcome_text(reply.outcome.unwrap_or(Outcome::Pending)).into();
                        }
                    }
                    if info.comparison_confirmed && info.flow.phase != Phase::Done {
                        let mut state = self.lock();
                        let view = state.view.as_mut().unwrap();
                        view.confirmed = true;
                        view.has_values = false;
                        shown.stage = 4;
                        shown.emoji = "".into();
                        shown.decimals = "".into();
                        global.set_compared(false);
                        shown.detail = "You already confirmed these values. Waiting for the other device; confirmation was not sent again.".into();
                    }
                    if info.flow.offers_sas == Some(false) {
                        shown.stage = 5;
                        shown.error = fixed_error(Error::Unsupported).into();
                    }
                }
                if let Some(presentation) = reply.values
                    && let Some((emoji, decimals)) = values(presentation)
                {
                    let mut state = self.lock();
                    let view = state.view.as_mut().unwrap();
                    if !view.confirmed {
                        if shown.emoji.as_str() != emoji || shown.decimals.as_str() != decimals {
                            global.set_compared(false);
                        }
                        view.has_values = true;
                        shown.stage = 3;
                        shown.emoji = emoji.into();
                        shown.decimals = decimals.into();
                        shown.detail = "Compare every value with this exact person using another trusted channel.".into();
                    }
                }
                if reply.request_sent == Some(false) {
                    shown.detail = format!("Continuing an existing request for this person. No new request was sent. {}", shown.detail).into();
                }
                if matches!(action, Action::Confirm) {
                    let mut state = self.lock();
                    let view = state.view.as_mut().unwrap();
                    view.has_values = false;
                    view.confirmed = true;
                    shown.stage = 4;
                    shown.emoji = "".into();
                    shown.decimals = "".into();
                    global.set_compared(false);
                    shown.detail = "Values confirmed here. Waiting for the other device and confirmation of this person's identity.".into();
                }
                if matches!(action, Action::Cancel) {
                    shown.stage = 5;
                    shown.detail = "Cancellation requested for this exact flow. No trust was granted by canceling.".into();
                }
            }
        }
        if shown.stage == 5 {
            shown.emoji = "".into();
            shown.decimals = "".into();
            global.set_compared(false);
            self.lock().view.as_mut().unwrap().has_values = false;
        }
        global.set_view(shown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_peer_ids_are_visible_and_never_reused_as_sanitized_routing() {
        assert_eq!(exact_label("@alice:example.org"), "@alice:example.org");
        assert_eq!(
            exact_label("@a\u{202e}b:example.org"),
            "@a\\u{202e}b:example.org"
        );
        assert_eq!(
            exact_label("@a\n\u{2066}b:example.org"),
            "@a\\u{a}\\u{2066}b:example.org"
        );
        let mut key = 0;
        let stale = next_row_key(&mut key).unwrap();
        let current = next_row_key(&mut key).unwrap();
        assert_ne!(stale, current);
        key = i32::MAX;
        assert_eq!(next_row_key(&mut key), None);
        assert_eq!(key, i32::MAX);
    }
    #[test]
    fn unclaimed_ui_payload_is_withdrawn_before_producer_completion_and_late_callback() {
        struct Payload(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = Arc::new(Publication(Mutex::new(Some(Payload(dropped.clone())))));
        let queued_callback_slot = slot.clone();
        {
            let _producer = PendingPublication(slot);
            // Cancellation/failed queue returns or unwinds this same guard.
        }
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        assert!(queued_callback_slot.take().is_none());

        let slot = Arc::new(Publication(Mutex::new(Some(Payload(dropped.clone())))));
        let producer = PendingPublication(slot.clone());
        let claimed_view_payload = slot.take().unwrap();
        drop(producer);
        // A task receipt cannot stand in for withdrawing an already claimed
        // View. The actual View/protect path must relinquish its payload.
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        drop(claimed_view_payload);
        assert_eq!(dropped.load(Ordering::Acquire), 2);
    }
    #[test]
    fn failed_refresh_withdraws_the_previous_verified_identity_claim() {
        for error in [
            Error::Gone,
            Error::Unsupported,
            Error::Unavailable,
            Error::TimedOut,
        ] {
            let mut shown = PeerVerificationView {
                stage: 5,
                peer: "@casey:example.org".into(),
                room: "!comparison:example.org".into(),
                detail: outcome_text(Outcome::IdentityVerified).into(),
                emoji: "old comparison".into(),
                decimals: "1000 · 2000 · 3000".into(),
                ..Default::default()
            };
            apply_error_presentation(&mut shown, Action::Refresh, error);
            assert_eq!(shown.stage, 5);
            assert_eq!(shown.detail.as_str(), outcome_text(Outcome::Unconfirmed));
            assert_eq!(shown.error.as_str(), fixed_error(error));
            assert!(shown.emoji.is_empty() && shown.decimals.is_empty());
            // The exact person and room remain visible; only the stale claim
            // and comparison payload are withdrawn, never another flow opened.
            assert_eq!(shown.peer.as_str(), "@casey:example.org");
            assert_eq!(shown.room.as_str(), "!comparison:example.org");
        }
        // A transient failure while comparing does not fabricate a terminal
        // outcome. A lost or unsupported flow does withdraw those values.
        let mut comparing = PeerVerificationView {
            stage: 3,
            detail: "Compare every value.".into(),
            emoji: "current comparison".into(),
            ..Default::default()
        };
        apply_error_presentation(&mut comparing, Action::Refresh, Error::TimedOut);
        assert_eq!(comparing.stage, 3);
        assert_eq!(comparing.emoji.as_str(), "current comparison");
        apply_error_presentation(&mut comparing, Action::Refresh, Error::Gone);
        assert_eq!(comparing.stage, 5);
        assert_eq!(comparing.detail.as_str(), outcome_text(Outcome::Unconfirmed));
        assert!(comparing.emoji.is_empty());
    }

    #[test]
    fn confirmation_needs_an_actual_current_presentation_and_explicit_comparison() {
        // A checked box alone must not authorize a missing, retired or
        // in-flight presentation. Confirmation is offered once only.
        assert!(!may_confirm(false, false, false, true));
        assert!(!may_confirm(true, false, true, true));
        assert!(!may_confirm(true, true, false, true));
        assert!(!may_confirm(true, false, false, false));
        assert!(may_confirm(true, false, false, true));
        assert_ne!(
            outcome_text(Outcome::DeviceOnly),
            outcome_text(Outcome::IdentityVerified)
        );
        assert_ne!(
            outcome_text(Outcome::IdentityChanged),
            outcome_text(Outcome::IdentityVerified)
        );
    }
}
