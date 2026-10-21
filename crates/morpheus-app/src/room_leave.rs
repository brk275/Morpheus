//! Leaving the open room (`morpheus_matrix::room_leave`).
//!
//! "Leave this room…" in the room header opens a modal that names the exact
//! room (its name as this device has it, which is not verified, and its
//! Matrix room ID) and what leaving does. The dialog keeps the account
//! selection and room ID it was opened for, captured before any await, and
//! closes with that selection. Confirming sends one leave request for that
//! room ID alone; no earlier or later version of the room is left. Nothing
//! local changes until the account's sync shows the room left, and the room
//! list then follows sync on its own.
//!
//! One dialog sends at most one leave. Unresolved receipts outlive the view
//! and account selection in this process, bounded without eviction; only a
//! new persisted Left sync update or removal of that local account retires one.
//! A request that may have reached the
//! homeserver without an answer is reported as unknown, never as done or not
//! done, and is never sent again; the next sync shows the truth. Leaving is
//! refused while a call of this account is starting, joined, recovering or
//! ending in that room, or ringing there, so a call is never torn down by
//! leaving its room, and while account access or a security step holds the
//! security gate. Error text is fixed and nothing here is logged.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, OnceLock, atomic::Ordering},
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, RoomLeaveCancellation, RoomLeaveError, RoomLeaveObservation, RoomLeaveReview,
};
use slint::ComponentHandle;
use tokio::runtime::Handle;

use crate::{AppWindow, RoomLeave, RoomLeaveView, live::LiveUi};

const MAX_NAME_CHARS: usize = 120;
const MAX_ID_CHARS: usize = 260;
const MAX_PENDING_LEAVES: usize = 64;
const ACCOUNT_LIST_WAIT: Duration = Duration::from_secs(10);
const PENDING: &str = "An earlier leave for this room is still unresolved. It is not sent again; \
    wait for this account's sync to show that you left.";
const SATURATED: &str = "Earlier leaves are still unresolved. Wait for their accounts to sync \
    before leaving another room. Nothing was sent.";

const SECURITY_BUSY: &str =
    "Finish the account or security step in progress first. Nothing was sent.";
const CALL_ACTIVE: &str = "A call in this room is in progress for this account. Leave the call \
                           first; leaving the room never ends it. Nothing was sent.";
const CALL_RINGING: &str =
    "A call is ringing in this room. Answer or decline it first. Nothing was sent.";

/// Stages of the dialog, as `RoomLeaveView::stage`.
const READING: i32 = 0;
const REVIEW: i32 = 1;
const SENDING: i32 = 2;
const AWAITING: i32 = 3;
const FINISHED: i32 = 4;
const FAILED: i32 = 5;

/// A call in the room being left, as the call controller and the incoming
/// ring see it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomCall {
    None,
    /// This account's call there is starting, joined, recovering or ending.
    Active,
    /// A call there is ringing for this account.
    Ringing,
}

impl RoomCall {
    pub(crate) fn of(active: bool, ringing: bool) -> Self {
        if active {
            Self::Active
        } else if ringing {
            Self::Ringing
        } else {
            Self::None
        }
    }

    fn refusal(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Active => Some(CALL_ACTIVE),
            Self::Ringing => Some(CALL_RINGING),
        }
    }
}

/// The calls of an account (local ID) in a room (exact room ID), now.
pub(crate) type CallFence = Arc<dyn Fn(&str, &str) -> RoomCall + Send + Sync>;

/// One opened dialog.
struct Dialog {
    id: u64,
    selection: AccountSelection,
    room: String,
    cancellation: RoomLeaveCancellation,
    navigation: u64,
    security_generation: u64,
    review: Option<RoomLeaveReview>,
    /// Set when confirmed: one dialog sends at most one leave.
    sent: bool,
    /// An existing receipt this view presents, including after reopening.
    waiting_for: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingStage {
    Queued,
    Admitted,
    Accepted,
    Unknown,
}

struct PendingLeave {
    id: u64,
    stage: PendingStage,
    sync_left: bool,
    observation: RoomLeaveCancellation,
}

type LeaveKey = (String, String);
type CheckedLeaves = Vec<(LeaveKey, u64)>;

fn key(selection: &AccountSelection, room: &str) -> LeaveKey {
    (selection.account_id().to_owned(), room.to_owned())
}

#[derive(Default)]
struct State {
    dialog: Option<Dialog>,
    next: u64,
    // No TTL or eviction of unresolved work. Closing a view and A-B-A do
    // not grant permission to repeat a non-idempotent membership write.
    pending: HashMap<LeaveKey, PendingLeave>,
}

impl State {
    fn reserve(&mut self, key: LeaveKey, id: u64) -> Result<(), &'static str> {
        if self.pending.contains_key(&key) {
            return Err(PENDING);
        }
        if self.pending.len() >= MAX_PENDING_LEAVES {
            return Err(SATURATED);
        }
        self.pending.insert(
            key,
            PendingLeave {
                id,
                stage: PendingStage::Queued,
                sync_left: false,
                observation: RoomLeaveCancellation::new(),
            },
        );
        Ok(())
    }

    fn finish(&mut self, key: &LeaveKey, id: u64, result: Result<(), RoomLeaveError>) -> bool {
        if !self
            .pending
            .get(key)
            .is_some_and(|pending| pending.id == id)
        {
            return false;
        }
        let stage = match result {
            Ok(()) => PendingStage::Accepted,
            Err(RoomLeaveError::Unknown) => PendingStage::Unknown,
            Err(_) => {
                if let Some(pending) = self.pending.remove(key) {
                    pending.observation.cancel();
                }
                return false;
            }
        };
        let pending = self.pending.get_mut(key).unwrap();
        pending.stage = stage;
        let settled = pending.sync_left;
        if settled {
            self.pending.remove(key);
        }
        settled
    }

    fn observed_left(&mut self, key: &LeaveKey, id: u64) -> bool {
        let Some(pending) = self.pending.get_mut(key).filter(|pending| pending.id == id) else {
            return false;
        };
        pending.sync_left = true;
        if matches!(
            pending.stage,
            PendingStage::Accepted | PendingStage::Unknown
        ) {
            self.pending.remove(key);
            true
        } else {
            // The HTTP attempt may still be in flight. Never release its
            // exclusion until its own outcome has been classified as well.
            false
        }
    }

    fn checked_leaves(&self) -> CheckedLeaves {
        self.pending
            .iter()
            .map(|(key, pending)| (key.clone(), pending.id))
            .collect()
    }

    fn prune_removed(&mut self, known_accounts: &HashSet<String>, checked: &CheckedLeaves) {
        self.pending.retain(|key, pending| {
            if known_accounts.contains(&key.0)
                || !checked
                    .iter()
                    .any(|(own, id)| own == key && *id == pending.id)
            {
                true
            } else {
                pending.observation.cancel();
                false
            }
        });
    }

    /// Complete local removal only. Neither late outcomes nor sync observers
    /// can recreate a removed receipt: both require its exact retained id.
    fn forget_account(&mut self, account: &str) -> bool {
        self.pending.retain(|(owner, _), pending| {
            if owner == account {
                pending.observation.cancel();
                false
            } else {
                true
            }
        });
        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.selection.account_id() == account)
        {
            self.dialog.take().unwrap().cancellation.cancel();
            true
        } else {
            false
        }
    }
}

pub(crate) struct RoomLeaveUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    fence: CallFence,
    state: Mutex<State>,
}

static OWNER: OnceLock<std::sync::Weak<RoomLeaveUi>> = OnceLock::new();

/// Metadata only, after complete local removal or successful selected logout.
/// A switch, view close, lock or partial removal keeps unresolved receipts.
pub(crate) fn forget_account(account: &str) {
    let Some(owner) = OWNER.get().and_then(std::sync::Weak::upgrade) else {
        return;
    };
    let forgot_dialog = owner.lock().forget_account(account);
    if forgot_dialog {
        let weak = owner.ui.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            // A newer dialog owns its own presentation.
            if owner.lock().dialog.is_none() {
                ui.global::<RoomLeave>().set_view(RoomLeaveView::default());
            }
        });
    }
}

/// Whether the dialog is open; shortcuts pause while it is.
pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<RoomLeave>().get_view().open
}

/// Account change, logout, detach and shutdown: the dialog and its room go
/// now, without waiting for a task.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    let leave = ui.global::<RoomLeave>();
    leave.set_view(RoomLeaveView::default());
    leave.invoke_reset();
}

/// A display label: control and direction characters replaced, bounded.
fn label(value: &str, max_chars: usize) -> String {
    crate::directory_plain_text(value, max_chars)
}

/// What leaving does, in fixed sentences chosen from this device's copy of
/// the room.
fn impact(review: &RoomLeaveReview) -> String {
    let mut lines = vec![
        "You stop receiving its messages and calls on every device of this account. Its \
         members can see that you left, and your messages stay in the room.",
        "To come back you may need a new invitation, unless anyone can join.",
    ];
    if review.is_space {
        lines.push("The rooms in this Space are not left.");
    }
    if review.has_predecessor || review.has_successor {
        lines.push(
            "Only this room is left. The room it replaced, or the room that replaced it, \
             stays as it is.",
        );
    }
    // Zero means the count is not known on this device.
    if review.joined_members == 1 {
        lines.push(
            "As far as this device knows, you are its only member: once you leave, nobody \
             may be able to invite you back.",
        );
    }
    lines.join("\n\n")
}

fn title(review: Option<&RoomLeaveReview>) -> &'static str {
    match review {
        Some(review) if review.is_space => "Leave this Space?",
        Some(review) if review.is_direct => "Leave this direct chat?",
        _ => "Leave this room?",
    }
}

fn leave_error(error: RoomLeaveError) -> String {
    match error {
        RoomLeaveError::Unknown => format!(
            "{error}. It is not sent again; the room list follows the next sync, which \
             shows whether you left."
        ),
        RoomLeaveError::NotJoined => format!("{error}. Nothing was sent."),
        _ => error.to_string() + ".",
    }
}

impl RoomLeaveUi {
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: &Handle,
        fence: CallFence,
    ) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            fence,
            state: Mutex::new(State::default()),
        });
        let _ = OWNER.set(Arc::downgrade(&owner));
        let leave = ui.global::<RoomLeave>();
        let this = owner.clone();
        let weak = ui.as_weak();
        leave.on_start(move || {
            if let Some(ui) = weak.upgrade() {
                this.start(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        leave.on_confirm(move || {
            if let Some(ui) = weak.upgrade() {
                this.confirm(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        leave.on_close(move || {
            if let Some(ui) = weak.upgrade() {
                this.close(&ui);
            }
        });
        let this = owner.clone();
        leave.on_reset(move || this.retire());
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        clear_presentation(ui);
        self.retire();
        let mut state = self.lock();
        for (_, pending) in state.pending.drain() {
            pending.observation.cancel();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Cancels and forgets the dialog.
    fn retire(&self) {
        if let Some(dialog) = self.lock().dialog.take() {
            dialog.cancellation.cancel();
        }
    }

    /// The selection of the account shown now, if it is live.
    fn live_account(&self, ui: &AppWindow) -> Option<AccountSelection> {
        if !ui.get_live_session() || !self.live.is_live.load(Ordering::Acquire) {
            return None;
        }
        let account = ui.get_active_account_local_id();
        self.live.manager.account_selection(account.as_str())
    }

    /// The dialog's own selection is current and its account is the one
    /// shown. The room may have changed since: the dialog names its own.
    fn shows(&self, ui: &AppWindow, dialog: &Dialog) -> bool {
        dialog.selection.is_current()
            && ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_active_account_local_id().as_str() == dialog.selection.account_id()
    }

    /// Why leaving this room is not possible right now, if it is not.
    fn refusal(&self, selection: &AccountSelection, room: &str) -> Option<&'static str> {
        if self.live.security_gate.blocked() {
            return Some(SECURITY_BUSY);
        }
        (self.fence)(selection.account_id(), room).refusal()
    }

    fn start(self: &Arc<Self>, ui: &AppWindow) {
        // Capture before admission: a security begin/end while this click
        // is being admitted never grants it a fresh generation.
        let security_generation = self.live.security_gate.ringing_generation();
        let navigation = self.live.current_navigation_epoch();
        if is_open(ui) || ui.get_room_leave_blocked() {
            return;
        }
        // Captured now, before any await: the dialog never moves to another
        // account selection or room.
        let room = ui.get_active_room_id().to_string();
        let Some(selection) = self.live_account(ui) else {
            return;
        };
        if room.is_empty() {
            return;
        }
        // Withdraw a held unread jump, for good, and the link's held
        // navigation before Leave owns the window.
        crate::unread_navigation::withdraw_jump(ui);
        crate::matrix_links::clear(ui);
        // Admission refuses an open browser. Retire any stale page/task as
        // well, before this dialog is shown; ordinary clear never calls back.
        crate::thread_browser::clear(ui);
        // The dialog protects the call bar: a bar click still waiting for the
        // call controller is refused for good, synchronously, before the
        // view opens. Only an admitted start gets here, so a refused one
        // leaves every other surface's pending work as it was.
        crate::withdraw_call_bar(ui);
        // Visible identity details were refused by room-leave-blocked.
        // An admitted Leave also retires their older focus return intent.
        crate::identity_warnings::withdraw(ui);
        let cancellation = RoomLeaveCancellation::new();
        let id = {
            let mut state = self.lock();
            if let Some(previous) = state.dialog.take() {
                previous.cancellation.cancel();
            }
            state.next = state.next.wrapping_add(1);
            state.dialog = Some(Dialog {
                id: state.next,
                selection: selection.clone(),
                room: room.clone(),
                cancellation: cancellation.clone(),
                navigation,
                security_generation,
                review: None,
                sent: false,
                waiting_for: None,
            });
            state.next
        };
        ui.global::<RoomLeave>().set_view(RoomLeaveView {
            open: true,
            stage: READING,
            title: title(None).into(),
            room_id: label(&room, MAX_ID_CHARS).into(),
            notice: "Reading this room on this device…".into(),
            error: self.refusal(&selection, &room).unwrap_or("").into(),
            ..RoomLeaveView::default()
        });
        // The dialog ends with its account selection.
        let this = self.clone();
        let watched = (selection.clone(), cancellation.clone());
        self.runtime.spawn(async move {
            let (selection, cancellation) = watched;
            tokio::select! {
                () = selection.revoked() => {}
                () = cancellation.cancelled() => {}
            }
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.end(&ui, id));
        });
        let this = self.clone();
        let checked = self.lock().checked_leaves();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            // Only a fresh registry absence can forget a removed account's
            // receipts. A selection change alone never retires them.
            let known = tokio::select! {
                biased;
                () = cancellation.cancelled() => None,
                accounts = selection.guard(tokio::time::timeout(ACCOUNT_LIST_WAIT, manager.accounts())) => {
                    accounts.and_then(Result::ok).map(|accounts| {
                        let accounts = accounts.into_iter().map(|account| account.id).collect();
                        (accounts, checked)
                    })
                }
            };
            let review = manager
                .room_leave_review_selected(&selection, &room, &cancellation)
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.finish_review(&ui, id, review, known));
        });
    }

    /// Closes dialog `id` if it is still the one shown.
    fn end(&self, ui: &AppWindow, id: u64) {
        let mut state = self.lock();
        if state.dialog.as_ref().is_some_and(|dialog| dialog.id == id) {
            if let Some(dialog) = state.dialog.take() {
                dialog.cancellation.cancel();
            }
            ui.global::<RoomLeave>().set_view(RoomLeaveView::default());
        }
    }

    fn close(&self, ui: &AppWindow) {
        if let Some(dialog) = self.lock().dialog.take() {
            dialog.cancellation.cancel();
        }
        ui.global::<RoomLeave>().set_view(RoomLeaveView::default());
    }

    fn finish_review(
        &self,
        ui: &AppWindow,
        id: u64,
        review: Result<RoomLeaveReview, RoomLeaveError>,
        known_accounts: Option<(HashSet<String>, CheckedLeaves)>,
    ) {
        let mut state = self.lock();
        let Some(dialog) = state.dialog.as_ref().filter(|dialog| dialog.id == id) else {
            return;
        };
        if !self.shows(ui, dialog) {
            return;
        }
        if let Some((known, checked)) = known_accounts {
            state.prune_removed(&known, &checked);
        }
        let pending = state.dialog.as_ref().and_then(|dialog| {
            state
                .pending
                .get(&key(&dialog.selection, &dialog.room))
                .map(|pending| (pending.id, pending.stage))
        });
        let Some(dialog) = state.dialog.as_mut().filter(|dialog| dialog.id == id) else {
            return;
        };
        if !self.shows(ui, dialog) {
            return;
        }
        let leave = ui.global::<RoomLeave>();
        let mut view = leave.get_view();
        view.notice = "".into();
        dialog.waiting_for = pending.map(|(id, _)| id);
        match review {
            Err(RoomLeaveError::Cancelled | RoomLeaveError::AccountChanged) => return,
            Err(error) => {
                view.stage = FAILED;
                view.title = "This room can't be left here".into();
                view.error = leave_error(error).into();
            }
            Ok(review) => {
                view.stage = REVIEW;
                view.title = title(Some(&review)).into();
                view.name = label(&review.name, MAX_NAME_CHARS).into();
                view.room_id = label(&review.room_id, MAX_ID_CHARS).into();
                view.impact = impact(&review).into();
                view.error = self
                    .refusal(&dialog.selection, &dialog.room)
                    .unwrap_or("")
                    .into();
                dialog.review = Some(review);
            }
        }
        if let Some((_, pending)) = pending {
            view.stage = if pending == PendingStage::Accepted {
                AWAITING
            } else {
                FAILED
            };
            view.title = if pending == PendingStage::Accepted {
                "Leave accepted"
            } else {
                "Earlier leave unresolved"
            }
            .into();
            view.notice = PENDING.into();
            view.error = "".into();
        }
        leave.set_view(view);
    }

    fn confirm(self: &Arc<Self>, ui: &AppWindow) {
        let leave = ui.global::<RoomLeave>();
        let mut view = leave.get_view();
        if view.stage != REVIEW {
            return;
        }
        let started = {
            let mut state = self.lock();
            let Some(dialog) = state.dialog.as_ref() else {
                return;
            };
            if dialog.sent || dialog.review.is_none() || !self.shows(ui, dialog) {
                return;
            }
            if let Some(refusal) = self.refusal(&dialog.selection, &dialog.room) {
                view.error = refusal.into();
                leave.set_view(view);
                return;
            }
            let started = (
                dialog.id,
                dialog.selection.clone(),
                dialog.room.clone(),
                dialog.cancellation.clone(),
            );
            if let Err(error) = state.reserve(key(&started.1, &started.2), started.0) {
                view.error = error.into();
                leave.set_view(view);
                return;
            }
            let dialog = state.dialog.as_mut().unwrap();
            dialog.sent = true;
            dialog.waiting_for = Some(started.0);
            started
        };
        let (id, selection, room, cancellation) = started;
        view.stage = SENDING;
        view.title = "Leaving…".into();
        view.notice = "Preparing this room's leave. You can close now; a request that has \
                       already reached your homeserver may still take effect."
            .into();
        view.error = "".into();
        leave.set_view(view);
        let this = self.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let admission_owner = this.clone();
            let admitted_selection = selection.clone();
            let admitted_room = room.clone();
            let accepted = manager
                .leave_room_exactly_selected(&selection, &room, &cancellation, move |observation| {
                    admission_owner.admit(id, &admitted_selection, &admitted_room, observation)
                })
                .await;
            let result = accepted.as_ref().map(|_| ()).map_err(|error| *error);
            // The retained receipt contains no SDK store handle. Release the
            // short-lived accepted response before posting any UI callback.
            drop(accepted);
            let key = key(&selection, &room);
            let settled = this.lock().finish(&key, id, result);
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let Some(dialog_id) = this.receipt_dialog(&ui, &key, id) else {
                    return;
                };
                if settled {
                    this.finished(&ui, dialog_id);
                } else {
                    match result {
                        Ok(()) => this.accepted(&ui, dialog_id),
                        Err(RoomLeaveError::Cancelled | RoomLeaveError::AccountChanged) => {
                            this.update(&ui, dialog_id, |dialog, view| {
                                dialog.sent = false;
                                dialog.waiting_for = None;
                                view.stage = if dialog.review.is_some() {
                                    REVIEW
                                } else {
                                    FAILED
                                };
                                view.title = title(dialog.review.as_ref()).into();
                                view.notice = "The earlier leave was withdrawn before admission. \
                                               Nothing was sent."
                                    .into();
                                view.error = "".into();
                            });
                        }
                        Err(error) => this.failed(&ui, dialog_id, error),
                    }
                }
            });
        });
    }

    /// The actual SDK admission point, after core acquisition. Closing the
    /// dialog takes this same lock, so a withdrawn view cannot be admitted.
    fn admit(
        self: &Arc<Self>,
        id: u64,
        selection: &AccountSelection,
        room: &str,
        observation: RoomLeaveObservation,
    ) -> bool {
        let key = key(selection, room);
        let cancelled = {
            let mut state = self.lock();
            let Some(dialog) = state.dialog.as_ref().filter(|dialog| dialog.id == id) else {
                return false;
            };
            if dialog.selection.account_id() != selection.account_id()
                || dialog.room != room
                || !dialog.selection.is_current()
                || !selection.is_current()
                || dialog.cancellation.is_cancelled()
                || !self.live.navigation_is_current(dialog.navigation)
                || !self
                    .live
                    .security_gate
                    .ringing_allowed(dialog.security_generation)
                || self.refusal(selection, room).is_some()
            {
                return false;
            }
            let Some(pending) = state
                .pending
                .get_mut(&key)
                .filter(|pending| pending.id == id && pending.stage == PendingStage::Queued)
            else {
                return false;
            };
            pending.stage = PendingStage::Admitted;
            pending.observation.clone()
        };
        let owner = Arc::downgrade(self);
        self.runtime.spawn(async move {
            if !observation.left_from_sync(&cancelled).await {
                return;
            }
            let Some(owner) = owner.upgrade() else {
                return;
            };
            let settled = owner.lock().observed_left(&key, id);
            if settled {
                let weak = owner.ui.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if let Some(dialog_id) = owner.receipt_dialog(&ui, &key, id) {
                        owner.finished(&ui, dialog_id);
                    }
                });
            }
        });
        true
    }

    /// A reopened view can show its own old receipt. A completion for that
    /// receipt cannot change a later explicit attempt, another room/account,
    /// or an account selection that has since departed.
    fn receipt_dialog(&self, ui: &AppWindow, key: &LeaveKey, id: u64) -> Option<u64> {
        self.lock()
            .dialog
            .as_ref()
            .filter(|dialog| {
                dialog.waiting_for == Some(id)
                    && dialog.selection.account_id() == key.0.as_str()
                    && dialog.room.as_str() == key.1.as_str()
                    && self.shows(ui, dialog)
            })
            .map(|dialog| dialog.id)
    }

    /// The view of dialog `id`, if it is still shown, to change in place.
    fn update(
        &self,
        ui: &AppWindow,
        id: u64,
        change: impl FnOnce(&mut Dialog, &mut RoomLeaveView),
    ) {
        let mut state = self.lock();
        let Some(dialog) = state.dialog.as_mut().filter(|dialog| dialog.id == id) else {
            return;
        };
        if !self.shows(ui, dialog) {
            return;
        }
        let leave = ui.global::<RoomLeave>();
        let mut view = leave.get_view();
        change(dialog, &mut view);
        leave.set_view(view);
    }

    fn failed(&self, ui: &AppWindow, id: u64, error: RoomLeaveError) {
        if matches!(
            error,
            RoomLeaveError::Cancelled | RoomLeaveError::AccountChanged
        ) {
            // Nothing was sent and the dialog is closing with its account or
            // by choice.
            return;
        }
        self.update(ui, id, |_, view| {
            if view.stage == FINISHED {
                return;
            }
            view.stage = FAILED;
            view.title = if error == RoomLeaveError::Unknown {
                "Result unknown".into()
            } else {
                "Not left".into()
            };
            view.notice = "".into();
            view.error = leave_error(error).into();
        });
    }

    fn accepted(&self, ui: &AppWindow, id: u64) {
        self.update(ui, id, |_, view| {
            if view.stage == FINISHED {
                return;
            }
            view.stage = AWAITING;
            view.title = "Leave accepted".into();
            view.notice = "Your homeserver accepted the leave. The room stays in your list \
                           until this device's sync shows it; you can close this now."
                .into();
        });
    }

    fn finished(&self, ui: &AppWindow, id: u64) {
        self.update(ui, id, |_, view| {
            view.stage = FINISHED;
            view.title = "Left".into();
            view.notice = "This account's sync showed that you left. The room list follows \
                           its latest membership."
                .into();
            view.error = "".into();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> RoomLeaveReview {
        RoomLeaveReview {
            room_id: "!r:example.org".into(),
            name: "Plans".into(),
            is_space: false,
            is_direct: false,
            has_predecessor: false,
            has_successor: false,
            joined_members: 0,
        }
    }

    #[test]
    fn the_impact_names_only_what_this_device_knows_and_never_claims_more() {
        let plain = impact(&review());
        assert!(plain.contains("on every device of this account"));
        assert!(!plain.contains("Only this room is left"));
        assert!(
            !plain.contains("only member"),
            "zero members means not known"
        );
        let replaced = RoomLeaveReview {
            has_predecessor: true,
            joined_members: 1,
            ..review()
        };
        let replaced = impact(&replaced);
        assert!(replaced.contains("Only this room is left"));
        assert!(replaced.contains("only member"));
        let space = RoomLeaveReview {
            is_space: true,
            ..review()
        };
        assert!(impact(&space).contains("rooms in this Space are not left"));
        assert_eq!(title(Some(&space)), "Leave this Space?");
        assert_eq!(title(None), "Leave this room?");
    }

    #[test]
    fn calls_in_the_room_refuse_the_leave_and_an_unknown_result_says_so() {
        assert_eq!(RoomCall::of(false, false).refusal(), None);
        assert_eq!(RoomCall::of(true, true).refusal(), Some(CALL_ACTIVE));
        assert_eq!(RoomCall::of(false, true).refusal(), Some(CALL_RINGING));
        let unknown = leave_error(RoomLeaveError::Unknown);
        assert!(unknown.contains("may or may not have left"));
        assert!(unknown.contains("not sent again"));
        assert!(leave_error(RoomLeaveError::Refused).contains("refused this leave request"));
    }

    #[test]
    fn unresolved_leaves_survive_closed_views_and_other_accounts_until_sync_converges() {
        let mut state = State::default();
        let a = ("account-a".into(), "!same:example.org".into());
        let b = ("account-b".into(), "!same:example.org".into());
        state.reserve(a.clone(), 1).unwrap();
        state.pending.get_mut(&a).unwrap().stage = PendingStage::Admitted;
        assert!(!state.finish(&a, 1, Err(RoomLeaveError::Unknown)));
        assert!(
            state.dialog.is_none(),
            "exclusion belongs to the account/room, not a view"
        );
        assert_eq!(state.reserve(a.clone(), 2), Err(PENDING));
        state.reserve(b.clone(), 3).unwrap();
        assert!(
            !state.finish(&b, 3, Ok(())),
            "an acknowledgment is not sync"
        );
        assert_eq!(state.reserve(b.clone(), 4), Err(PENDING));
        assert!(
            !state.observed_left(&a, 99),
            "a stale owner cannot release another attempt"
        );
        assert!(state.observed_left(&a, 1));
        assert!(
            state.pending.contains_key(&b),
            "one account's sync cannot settle another's"
        );
        state.reserve(a.clone(), 5).unwrap();
        state.pending.get_mut(&a).unwrap().stage = PendingStage::Admitted;
        assert!(
            !state.observed_left(&a, 5),
            "an in-flight attempt stays excluded"
        );
        assert_eq!(state.reserve(a.clone(), 6), Err(PENDING));
        assert!(state.finish(&a, 5, Ok(())));
        assert!(!state.pending.contains_key(&a));
    }

    #[test]
    fn saturation_never_evicts_uncertainty_and_only_the_removed_account_is_forgotten() {
        let mut state = State::default();
        for id in 0..MAX_PENDING_LEAVES {
            let key = (format!("account-{id}"), "!r:example.org".into());
            state.reserve(key.clone(), id as u64).unwrap();
            state.finish(&key, id as u64, Err(RoomLeaveError::Unknown));
        }
        let first = ("account-0".into(), "!r:example.org".into());
        let refused = ("new-account".into(), "!r:example.org".into());
        assert_eq!(state.reserve(refused.clone(), 100), Err(SATURATED));
        assert!(state.pending.contains_key(&first));
        let mut known = (0..MAX_PENDING_LEAVES)
            .map(|id| format!("account-{id}"))
            .collect::<HashSet<_>>();
        known.remove("account-0");
        let checked = state.checked_leaves();
        state.prune_removed(&known, &checked);
        assert!(!state.pending.contains_key(&first));
        assert_eq!(state.pending.len(), MAX_PENDING_LEAVES - 1);
        state.reserve(refused, 100).unwrap();
        assert!(
            state
                .pending
                .contains_key(&("account-1".into(), "!r:example.org".into()))
        );
        // A delayed listing cannot delete work created after its snapshot.
        state.prune_removed(&HashSet::new(), &checked);
        assert!(
            state
                .pending
                .contains_key(&("new-account".into(), "!r:example.org".into()))
        );
    }

    #[test]
    fn complete_removal_cancels_only_its_observer_and_late_outcomes_cannot_restore_it() {
        let mut state = State::default();
        let a = ("a".into(), "!same:example.org".into());
        let b = ("b".into(), "!same:example.org".into());
        state.reserve(a.clone(), 1).unwrap();
        state.reserve(b.clone(), 2).unwrap();
        let observation = state.pending[&a].observation.clone();
        state.finish(&a, 1, Err(RoomLeaveError::Unknown));
        state.forget_account("a");
        assert!(observation.is_cancelled());
        assert!(!state.pending.contains_key(&a));
        assert!(state.pending.contains_key(&b));
        assert!(!state.finish(&a, 1, Err(RoomLeaveError::Unknown)));
        assert!(!state.observed_left(&a, 1));
        assert!(!state.pending.contains_key(&a));
        state.reserve(a.clone(), 3).unwrap();
        assert!(!state.finish(&a, 1, Ok(())));
        assert_eq!(
            state.pending[&a].id, 3,
            "a late prior account lifetime cannot settle new work"
        );
    }
}
