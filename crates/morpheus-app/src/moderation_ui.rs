//! Member actions (invite, kick, ban, unban, a power level) from the member
//! list and message redaction from its dialog, sent through the core's
//! bounded moderation requests (`morpheus_matrix::moderation`).
//!
//! An action belongs to what was shown when it was confirmed: the account
//! selection, the navigation epoch (account and room), the security epoch
//! and, for a redaction, its open dialog. All are captured on the UI thread
//! before any await. Its cancellation is owned here, per kind: closing the
//! redaction dialog, any account or room change, or a newer action of the
//! same kind cancels it. Right before the request is handed to the
//! homeserver the core asks the admission (`Guard::admitted`), which also
//! requires the same navigation and security epochs: a security step that
//! began, even one that already ended, refuses the action. Before the
//! hand-off nothing is sent; after it the result is unknown, never resent.

use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use morpheus_matrix::{
    AccountManager, AccountSelection, MemberAction, ModerationCancellation, ModerationError,
    ModerationOutcome, ModerationScope,
};
use slint::ComponentHandle;
use tokio::runtime::Handle;

use crate::{AppWindow, live::LiveUi};

const SECURITY_BUSY: &str =
    "Finish the account or security step in progress first. Nothing was sent.";

/// One moderation request, as confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Member {
        room: String,
        user: String,
        action: MemberAction,
    },
    Power {
        room: String,
        user: String,
        level: i64,
    },
    Redact {
        room: String,
        event: String,
    },
}

/// One request's answer, as the core gives it.
pub(crate) type Pending<'a> =
    Pin<Box<dyn Future<Output = Result<ModerationOutcome, ModerationError>> + Send + 'a>>;

/// What the callbacks ask of the account core: the selection of the account
/// shown, captured on the UI thread, and one bounded request under a guard.
pub(crate) trait Moderator: Send + Sync + 'static {
    type Selection: Send + Sync + 'static;
    fn selection(&self, account: &str) -> Option<Self::Selection>;
    fn act<'a>(
        &'a self,
        selection: &'a Self::Selection,
        guard: &'a Guard,
        request: &'a Request,
    ) -> Pending<'a>;
}

impl Moderator for AccountManager {
    type Selection = AccountSelection;

    fn selection(&self, account: &str) -> Option<AccountSelection> {
        self.account_selection(account)
    }

    fn act<'a>(
        &'a self,
        selection: &'a AccountSelection,
        guard: &'a Guard,
        request: &'a Request,
    ) -> Pending<'a> {
        Box::pin(async move {
            let admitted = || guard.admitted();
            let scope = ModerationScope {
                selection,
                cancellation: &guard.cancellation,
                admission: &admitted,
            };
            match request {
                Request::Member { room, user, action } => {
                    let action = action.clone();
                    self.moderate_member_selected(&scope, room, user, action)
                        .await
                }
                Request::Power { room, user, level } => {
                    self.set_member_power_level_selected(&scope, room, user, *level)
                        .await
                }
                Request::Redact { room, event } => {
                    self.redact_event_selected(&scope, room, event, None).await
                }
            }
        })
    }
}

/// What an action belongs to, captured on the UI thread before any await.
pub(crate) struct Guard {
    live: Arc<LiveUi>,
    navigation: u64,
    security: u64,
    cancellation: ModerationCancellation,
}

impl Guard {
    /// Asked right before the request is handed to the homeserver: the
    /// action was not cancelled (its dialog is open and its account and room
    /// unchanged), the navigation is the one it was confirmed in, and no
    /// security step began since.
    pub(crate) fn admitted(&self) -> bool {
        !self.cancellation.is_cancelled()
            && self.live.navigation_is_current(self.navigation)
            && self.live.security_gate.ringing_allowed(self.security)
    }
}

/// The action of one kind in flight, by number, with its cancellation.
#[derive(Default)]
struct Slot(Mutex<Option<(u64, ModerationCancellation)>>);

impl Slot {
    fn lock(&self) -> MutexGuard<'_, Option<(u64, ModerationCancellation)>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// `id` is now the action of this kind; the one before it is cancelled.
    fn begin(&self, id: u64, cancellation: &ModerationCancellation) {
        let previous = self.lock().replace((id, cancellation.clone()));
        if let Some((_, previous)) = previous {
            previous.cancel();
        }
    }

    /// Cancel the action in flight; whether there was one.
    fn cancel(&self) -> bool {
        let current = self.lock().take();
        match current {
            Some((_, cancellation)) => {
                cancellation.cancel();
                true
            }
            None => false,
        }
    }

    fn finish(&self, id: u64) {
        let mut current = self.lock();
        if current.as_ref().is_some_and(|(current, _)| *current == id) {
            *current = None;
        }
    }
}

type SaveText = dyn Fn(&AppWindow, i32, String) + 'static;

pub(crate) struct ModerationUi<M: Moderator> {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    moderator: Arc<M>,
    member: Slot,
    redaction: Slot,
    next: AtomicU64,
}

impl<M: Moderator> ModerationUi<M> {
    /// `save_text` handles the room name and topic of the shared admin
    /// callback (the room settings form).
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: &Handle,
        moderator: Arc<M>,
        save_text: impl Fn(&AppWindow, i32, String) + 'static,
    ) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            moderator,
            member: Slot::default(),
            redaction: Slot::default(),
            next: AtomicU64::new(0),
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_submit_member_action(move |kind, user, value| {
            if let Some(ui) = weak.upgrade() {
                this.member(&ui, kind, user.as_str(), value.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        let save_text: Box<SaveText> = Box::new(save_text);
        ui.on_submit_room_admin(move |kind, value, _reason| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match kind {
                0 | 1 => save_text(&ui, kind, value.to_string()),
                2 => this.redact(&ui, value.as_str()),
                _ => {}
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_moderation_context_changed(move |kind| {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui, kind);
            }
        });
        owner
    }

    /// Ends every action in flight.
    pub(crate) fn shutdown(&self) {
        self.member.cancel();
        self.redaction.cancel();
    }

    /// 0: the account or room changed, which ends both kinds; 2: the
    /// redaction dialog closed or now shows something else; 3: its visible
    /// Close/Escape control, which cancels before hiding the dialog.
    fn context_changed(&self, ui: &AppWindow, kind: i32) {
        if kind == 0 && self.member.cancel() {
            ui.set_member_action_busy(false);
        }
        let cancelled = self.redaction.cancel();
        // A completed core action may already have retired its slot while
        // its UI receipt is still queued. Closing that dialog must retire
        // its busy widget too; it does not change the core's known outcome.
        if (kind == 3 && ui.get_room_admin_mode() == 2) || (cancelled && !ui.get_room_admin_open())
        {
            ui.set_room_admin_busy(false);
        }
    }

    /// The guard of an action confirmed now, and its number.
    fn guard(&self) -> (u64, Guard) {
        let id = self.next.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let guard = Guard {
            live: self.live.clone(),
            navigation: self.live.current_navigation_epoch(),
            security: self.live.security_gate.ringing_generation(),
            cancellation: ModerationCancellation::new(),
        };
        (id, guard)
    }

    fn member(self: &Arc<Self>, window: &AppWindow, kind: i32, user_id: &str, value: &str) {
        if crate::room_addresses::is_open(window) {
            return;
        }
        if !self.live.is_live.load(Ordering::Acquire) || window.get_member_action_busy() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = window.get_active_room_id().to_string();
        let user_id = user_id.trim().to_owned();
        if account_id.is_empty() || room_id.is_empty() || user_id.is_empty() {
            window.set_member_action_error("Choose a room and enter a Matrix user ID".into());
            return;
        }
        let label = match kind {
            0 => "invite",
            1 => "kick",
            2 => "ban",
            3 => "unban",
            4 => "change power for",
            _ => return,
        };
        let power_level = if kind == 4 {
            match value.trim().parse::<i64>() {
                Ok(level) => Some(level),
                Err(_) => {
                    window.set_member_action_error("Enter a whole-number power level".into());
                    return;
                }
            }
        } else {
            None
        };
        if self.live.security_gate.blocked() {
            window.set_member_action_error(SECURITY_BUSY.into());
            return;
        }
        // The exact account selection, captured before any await.
        let Some(selection) = self.moderator.selection(&account_id) else {
            window.set_member_action_error("The selected account changed. Try again.".into());
            return;
        };
        let room = room_id.clone();
        let user = user_id.clone();
        let request = match (kind, power_level) {
            (4, Some(level)) => Request::Power { room, user, level },
            (0, _) => Request::Member {
                room,
                user,
                action: MemberAction::Invite,
            },
            (1, _) => Request::Member {
                room,
                user,
                action: MemberAction::Kick { reason: None },
            },
            (2, _) => Request::Member {
                room,
                user,
                action: MemberAction::Ban { reason: None },
            },
            (3, _) => Request::Member {
                room,
                user,
                action: MemberAction::Unban { reason: None },
            },
            _ => return,
        };
        let (id, guard) = self.guard();
        self.member.begin(id, &guard.cancellation);
        let operation = window.get_member_action_operation().wrapping_add(1);
        window.set_member_action_operation(operation);
        window.set_member_action_busy(true);
        window.set_member_action_error("".into());
        window.set_member_action_notice("".into());
        let this = self.clone();
        self.runtime.spawn(async move {
            // Refused here before any read when its context already ended;
            // the completion still clears the busy state it set.
            let result = if guard.admitted() {
                this.moderator.act(&selection, &guard, &request).await
            } else {
                Err(ModerationError::Cancelled)
            };
            this.member.finish(id);
            let live = this.live.clone();
            let weak = this.ui.clone();
            let navigation = guard.navigation;
            if result.is_ok() && live.navigation_is_current(navigation) {
                // The SDK member snapshot may still contain the previous
                // sync state immediately after the homeserver accepts a
                // change. Wait briefly for the target state before repainting.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
                loop {
                    if !live.navigation_is_current(navigation) {
                        break;
                    }
                    let settled = live
                        .manager
                        .room_members_for(&account_id, &room_id)
                        .await
                        .ok()
                        .is_some_and(|members| {
                            let member = members.iter().find(|member| member.user_id == user_id);
                            match kind {
                                0 => member.is_some_and(|member| {
                                    member.membership == "invite" || member.membership == "join"
                                }),
                                1 | 3 => member.is_none(),
                                2 => member.is_some_and(|member| member.membership == "ban"),
                                4 => member.is_some_and(|member| member.power_level == power_level),
                                _ => false,
                            }
                        });
                    if settled || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                live.refresh_members_for(account_id.clone(), room_id.clone(), weak.clone(), true)
                    .await;
            }
            let _ = weak.upgrade_in_event_loop(move |window| {
                if window.get_active_account_local_id().as_str() != account_id
                    || window.get_active_room_id().as_str() != room_id
                    || window.get_member_action_operation() != operation
                {
                    return;
                }
                window.set_member_action_busy(false);
                match result {
                    Ok(outcome) => {
                        if kind == 0 && window.get_member_invite_user_id().as_str() == user_id {
                            window.set_member_invite_user_id("".into());
                        }
                        if kind != 0 && kind != 4 {
                            window.set_member_selected_user_id("".into());
                        }
                        let notice = member_notice(outcome, label, &user_id);
                        let notice = crate::directory_plain_text(&notice, 240);
                        window.set_member_action_notice(notice.into());
                        window.set_member_action_error("".into());
                    }
                    Err(error) => {
                        let text = format!("Could not {label} {user_id}: {error}");
                        let text = crate::directory_plain_text(&text, 240);
                        window.set_member_action_error(text.into());
                    }
                }
            });
        });
    }

    fn redact(self: &Arc<Self>, window: &AppWindow, value: &str) {
        if crate::room_addresses::is_open(window) {
            return;
        }
        if !window.get_room_admin_open()
            || window.get_room_admin_mode() != 2
            || !self.live.is_live.load(Ordering::Acquire)
            || window.get_room_admin_busy()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = window.get_active_room_id().to_string();
        if account_id.is_empty()
            || room_id.is_empty()
            || window.get_room_admin_account_id().as_str() != account_id
            || window.get_room_admin_room_id().as_str() != room_id
        {
            window
                .set_room_admin_error("Room or account changed. Reopen message redaction.".into());
            return;
        }
        if value.is_empty() || window.get_room_admin_event_id().as_str() != value {
            window.set_room_admin_error("Choose a message to redact".into());
            return;
        }
        if self.live.security_gate.blocked() {
            window.set_room_admin_error(SECURITY_BUSY.into());
            return;
        }
        // The exact account selection, captured before any await.
        let Some(selection) = self.moderator.selection(&account_id) else {
            window
                .set_room_admin_error("Room or account changed. Reopen message redaction.".into());
            return;
        };
        let value = value.to_owned();
        let request = Request::Redact {
            room: room_id.clone(),
            event: value.clone(),
        };
        let (id, guard) = self.guard();
        self.redaction.begin(id, &guard.cancellation);
        let operation = window.get_room_admin_operation().wrapping_add(1);
        window.set_room_admin_operation(operation);
        window.set_room_admin_busy(true);
        window.set_room_admin_error("".into());
        window.set_room_admin_notice("".into());
        let this = self.clone();
        self.runtime.spawn(async move {
            let result = if guard.admitted() {
                this.moderator.act(&selection, &guard, &request).await
            } else {
                Err(ModerationError::Cancelled)
            };
            this.redaction.finish(id);
            let live = this.live.clone();
            let weak = this.ui.clone();
            let navigation = guard.navigation;
            let refresh = result.is_ok() && live.navigation_is_current(navigation);
            let error = result.err().map(|error| {
                let text = format!("Could not redact message: {error}");
                crate::directory_plain_text(&text, 240)
            });
            let completion = live.clone();
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !window.get_room_admin_open()
                    || window.get_room_admin_mode() != 2
                    || !completion.navigation_is_current(navigation)
                    || window.get_active_account_local_id().as_str() != account_id
                    || window.get_active_room_id().as_str() != room_id
                    || window.get_room_admin_account_id().as_str() != account_id
                    || window.get_room_admin_room_id().as_str() != room_id
                    || window.get_room_admin_event_id().as_str() != value
                    || window.get_room_admin_operation() != operation
                {
                    return;
                }
                window.set_room_admin_busy(false);
                if let Some(error) = error {
                    window.set_room_admin_error(error.into());
                } else {
                    window.set_room_admin_notice("Message redacted".into());
                    window.set_room_admin_event_id("".into());
                }
            });
            if refresh {
                let _ = live.refresh(weak).await;
            }
        });
    }
}

/// What a member action did, said plainly.
fn member_notice(outcome: ModerationOutcome, label: &str, user_id: &str) -> String {
    match outcome {
        ModerationOutcome::Done => format!("Room member action completed: {label} {user_id}"),
        ModerationOutcome::AlreadyDone => {
            format!("{user_id} already has that power level; nothing was sent.")
        }
        ModerationOutcome::AcceptedUnconfirmed => format!(
            "The homeserver accepted the change for {user_id}; the room does not show it yet."
        ),
    }
}

/// A core stand-in for the production-callback regression in
/// `account_access_ui_tests`: its permission read waits until released,
/// then it hands off only while the guard still admits the action, as the
/// bounded moderation requests do at their real hand-off.
#[cfg(test)]
pub(crate) struct HeldModerator {
    held: tokio::sync::Semaphore,
    reads: std::sync::atomic::AtomicUsize,
    answered: std::sync::atomic::AtomicUsize,
    sent: Mutex<Vec<&'static str>>,
}

#[cfg(test)]
impl HeldModerator {
    pub(crate) fn new() -> Self {
        Self {
            held: tokio::sync::Semaphore::new(0),
            reads: std::sync::atomic::AtomicUsize::new(0),
            answered: std::sync::atomic::AtomicUsize::new(0),
            sent: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn release(&self) {
        self.held.add_permits(1);
    }

    pub(crate) fn sent(&self) -> Vec<&'static str> {
        self.sent.lock().unwrap().clone()
    }

    pub(crate) async fn wait_reads(&self, count: usize) {
        while self.reads.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub(crate) async fn wait_answered(&self, count: usize) {
        while self.answered.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

#[cfg(test)]
impl Moderator for HeldModerator {
    type Selection = ();

    fn selection(&self, _: &str) -> Option<()> {
        Some(())
    }

    fn act<'a>(&'a self, _: &'a (), guard: &'a Guard, request: &'a Request) -> Pending<'a> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Ok(permit) = self.held.acquire().await {
                permit.forget();
            }
            let result = if guard.admitted() {
                let label = match request {
                    Request::Member { .. } => "member",
                    Request::Power { .. } => "power",
                    Request::Redact { .. } => "redact",
                };
                self.sent.lock().unwrap().push(label);
                Ok(ModerationOutcome::Done)
            } else {
                Err(ModerationError::Cancelled)
            };
            self.answered.fetch_add(1, Ordering::SeqCst);
            result
        })
    }
}
