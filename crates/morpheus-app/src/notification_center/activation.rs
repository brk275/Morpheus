//! Opening a clicked banner.
//!
//! A clicked banner's target arrives from `DesktopNotifications` already
//! resolved against its bounded record and the saved accounts. It is opened
//! through the window's single navigation seam, the `open-conversation`
//! callback (message search's `open_conversation`, branch
//! claude/overnight-messaging-20261018 at 9ecbe90): the exact account, room,
//! event and thread, with another account made visible only through the
//! window's own protected switch (security gate, leave-call confirmation,
//! local app password) and every later step fenced by that account's
//! selection. This module adds only what is specific to banners: restoring a
//! minimized window, never replacing an account dialog the user is already
//! answering, opening the saved account when no account is open (and the
//! banner's room once that account is open, within two minutes), and
//! pointing an invitation banner at the room list of its account (another
//! account's invitation only switches to that account).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use slint::{ComponentHandle, Model, Weak};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot, watch},
};

use crate::{
    AppWindow,
    navigation_target::NavigationTarget,
    notifications::{
        DesktopNotifications,
        owned::{Close, Role},
    },
};

const DIALOG_OPEN: &str = "Finish the open account dialog, then open the notification again.";
const INVITE_OPEN: &str = "Accept or decline this invitation in the room list.";
const ACCOUNT_GONE: &str = "This notification's account is no longer available.";
/// A click made while no account was open waits this long for its account.
const OPEN_WAIT: Duration = Duration::from_secs(2 * 60);
const OPEN_POLL: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct Pending {
    target: NavigationTarget,
    click: u64,
    deadline: Instant,
}

pub(crate) struct Activator {
    ui: Weak<AppWindow>,
    closing: Close,
    pending: watch::Sender<Option<Pending>>,
    /// Counts clicks; a click waiting for its account is dropped by a newer
    /// one.
    clicks: AtomicU64,
    #[cfg(test)]
    executed: AtomicU64,
    #[cfg(test)]
    admitted: AtomicU64,
}

impl Activator {
    pub(crate) fn register(
        ui: &AppWindow,
        runtime: &Handle,
        notifications: &DesktopNotifications,
        activations: Option<mpsc::Receiver<NavigationTarget>>,
    ) -> Arc<Self> {
        let (pending, pending_receiver) = watch::channel(None);
        let activator = Arc::new(Self {
            ui: ui.as_weak(),
            closing: notifications.cancellation(),
            pending,
            clicks: AtomicU64::new(0),
            #[cfg(test)]
            executed: AtomicU64::new(0),
            #[cfg(test)]
            admitted: AtomicU64::new(0),
        });
        if let Some(mut activations) = activations {
            let weak = Arc::downgrade(&activator);
            let closing = activator.closing.clone();
            notifications.spawn_app_task(Role::Activations, runtime, async move {
                while let Some(Some(target)) = closing.guard(activations.recv()).await {
                    let Some(activator) = weak.upgrade() else {
                        return;
                    };
                    if !activator.queue_target(target) {
                        return;
                    }
                }
            });
        }
        notifications.spawn_app_task(
            Role::PendingOpen,
            runtime,
            wait_for_account(
                Arc::downgrade(&activator),
                pending_receiver,
                activator.closing.clone(),
            ),
        );
        activator
    }

    /// The same actual callback is used by the retained receiver and the
    /// disposable generated-window regression. No SDK/controller owner is queued.
    fn queue_target(self: &Arc<Self>, target: NavigationTarget) -> bool {
        let opener = Arc::downgrade(self);
        self.ui
            .upgrade_in_event_loop(move |ui| {
                if let Some(opener) = opener.upgrade() {
                    #[cfg(test)]
                    opener.executed.fetch_add(1, Ordering::AcqRel);
                    opener.open(&ui, target);
                }
            })
            .is_ok()
    }

    /// Open one clicked banner. UI thread only.
    fn open(self: &Arc<Self>, ui: &AppWindow, target: NavigationTarget) {
        if self.closing.is_closed() || ui.get_application_closing() {
            return;
        }
        #[cfg(test)]
        self.admitted.fetch_add(1, Ordering::AcqRel);
        let click = self.clicks.fetch_add(1, Ordering::AcqRel) + 1;
        self.pending.send_replace(None);
        if ui.window().is_minimized() {
            ui.window().set_minimized(false);
        }
        let live = ui.get_live_session();
        let visible = live && ui.get_active_account_local_id().as_str() == target.account_id();
        // Another account goes through the account switch; it must not
        // replace an app-password or leave-call dialog the user is already
        // answering.
        if !visible && (ui.get_access_open() || ui.get_leave_call_dialog()) {
            ui.set_session_error(DIALOG_OPEN.into());
            return;
        }
        // An invitation banner has no event; its room is not joined yet and
        // waits in the room list rather than opening as a conversation.
        let invitation = target.event_id().is_none();
        if visible
            && invitation
            && ui
                .get_invitations()
                .iter()
                .any(|invite| invite.id.as_str() == target.room_id())
        {
            ui.set_transfer_notice(INVITE_OPEN.into());
            return;
        }
        if !live {
            // No account is open (locked, or signed out of the last one):
            // the saved account's protected activation, then the banner's
            // room once that account is open.
            ui.invoke_activate_saved_account(target.account_id().into());
            self.open_when_live(target, click);
            return;
        }
        if !visible && invitation {
            // Another account's invitation: only its protected account
            // switch; the invitation then waits in that account's room list.
            let Some(index) = ui
                .get_account_options()
                .iter()
                .find(|option| option.local_id.as_str() == target.account_id())
                .map(|option| option.index)
            else {
                ui.set_session_error(ACCOUNT_GONE.into());
                return;
            };
            ui.invoke_switch_account(index);
            ui.set_transfer_notice(INVITE_OPEN.into());
            return;
        }
        ui.invoke_open_conversation(
            target.account_id().into(),
            target.room_id().into(),
            target.event_id().unwrap_or_default().into(),
            target.thread_root().unwrap_or_default().into(),
        );
    }

    /// Open `target` once its account is open; activating it may first ask
    /// for the app password. A newer click, another account becoming open or
    /// `OPEN_WAIT` drops it.
    fn open_when_live(&self, target: NavigationTarget, click: u64) {
        if !self.closing.is_closed() {
            // One finite latest intent, not one detached task per click.
            self.pending.send_replace(Some(Pending {
                target,
                click,
                deadline: Instant::now() + OPEN_WAIT,
            }));
        }
    }
}

/// Holds only the registered controller while the existing fixture pumps its
/// real queued callback. Counters observe actual callback/body entry, not a
/// second copy of either admission predicate.
#[cfg(test)]
pub(crate) struct QueuedActivationForTest(Arc<Activator>);

#[cfg(test)]
impl QueuedActivationForTest {
    pub(crate) fn observations(&self) -> (u64, u64) {
        (
            self.0.executed.load(Ordering::Acquire),
            self.0.admitted.load(Ordering::Acquire),
        )
    }
}

#[cfg(test)]
pub(crate) fn queue_activation_for_test(
    ui: &AppWindow,
    runtime: &Handle,
    notifications: &DesktopNotifications,
    target: NavigationTarget,
) -> Result<QueuedActivationForTest, &'static str> {
    let activator =
        Activator::register(ui, runtime, notifications, notifications.take_activations());
    if !activator.queue_target(target) {
        return Err("notification activation callback could not be queued");
    }
    Ok(QueuedActivationForTest(activator))
}

async fn wait_for_account(
    activator: std::sync::Weak<Activator>,
    mut pending: watch::Receiver<Option<Pending>>,
    closing: Close,
) {
    loop {
        let intent = pending.borrow_and_update().clone();
        let Some(intent) = intent else {
            if closing
                .guard(pending.changed())
                .await
                .is_none_or(|changed| changed.is_err())
            {
                return;
            }
            continue;
        };
        tokio::select! {
            biased;
            _ = closing.cancelled() => return,
            changed = pending.changed() => {
                if changed.is_err() { return; }
                continue;
            }
            _ = tokio::time::sleep(OPEN_POLL) => {},
        }
        let Some(this) = activator.upgrade() else {
            return;
        };
        if this.clicks.load(Ordering::Acquire) != intent.click || Instant::now() >= intent.deadline
        {
            // A newer value is never cleared by an older observation.
            this.pending.send_if_modified(|current| {
                if current
                    .as_ref()
                    .is_some_and(|current| current.click == intent.click)
                {
                    *current = None;
                    true
                } else {
                    false
                }
            });
            continue;
        }
        let (send, receive) = oneshot::channel();
        let opener = activator.clone();
        let deadline = intent.deadline;
        let click = intent.click;
        let asked = this.ui.upgrade_in_event_loop(move |ui| {
            let Some(opener) = opener.upgrade() else {
                return;
            };
            if opener.closing.is_closed() || ui.get_application_closing() {
                return;
            }
            let current = opener.clicks.load(Ordering::Acquire) == intent.click
                && Instant::now() < intent.deadline;
            let waiting = current && !ui.get_live_session();
            if current
                && !waiting
                && ui.get_active_account_local_id().as_str() == intent.target.account_id()
            {
                opener.open(&ui, intent.target);
            } else if !waiting {
                opener.pending.send_if_modified(|pending| {
                    if pending
                        .as_ref()
                        .is_some_and(|pending| pending.click == intent.click)
                    {
                        *pending = None;
                        true
                    } else {
                        false
                    }
                });
            }
            let _ = send.send(waiting);
        });
        drop(this);
        if asked.is_err() {
            return;
        }
        // Close releases the queued reply, never waiting on a stopped UI loop.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let answer = closing
            .guard(tokio::time::timeout(remaining, receive))
            .await;
        if answer.is_none() {
            return;
        }
        if !matches!(answer, Some(Ok(Ok(true)))) {
            if let Some(this) = activator.upgrade() {
                this.pending.send_if_modified(|pending| {
                    if pending
                        .as_ref()
                        .is_some_and(|pending| pending.click == click)
                    {
                        *pending = None;
                        true
                    } else {
                        false
                    }
                });
            }
        }
    }
}
