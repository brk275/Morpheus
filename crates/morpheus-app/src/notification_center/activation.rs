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
    sync::{mpsc, oneshot},
};

use crate::{AppWindow, navigation_target::NavigationTarget};

const DIALOG_OPEN: &str = "Finish the open account dialog, then open the notification again.";
const INVITE_OPEN: &str = "Accept or decline this invitation in the room list.";
const ACCOUNT_GONE: &str = "This notification's account is no longer available.";
/// A click made while no account was open waits this long for its account.
const OPEN_WAIT: Duration = Duration::from_secs(2 * 60);
const OPEN_POLL: Duration = Duration::from_millis(500);

pub(crate) struct Activator {
    ui: Weak<AppWindow>,
    runtime: Handle,
    /// Counts clicks; a click waiting for its account is dropped by a newer
    /// one.
    clicks: AtomicU64,
}

impl Activator {
    pub(crate) fn register(
        ui: &AppWindow,
        runtime: Handle,
        activations: Option<mpsc::Receiver<NavigationTarget>>,
    ) -> Arc<Self> {
        let activator = Arc::new(Self {
            ui: ui.as_weak(),
            runtime: runtime.clone(),
            clicks: AtomicU64::new(0),
        });
        if let Some(mut activations) = activations {
            let weak = Arc::downgrade(&activator);
            runtime.spawn(async move {
                while let Some(target) = activations.recv().await {
                    let Some(activator) = weak.upgrade() else {
                        return;
                    };
                    let opener = activator.clone();
                    if activator
                        .ui
                        .upgrade_in_event_loop(move |ui| opener.open(&ui, target))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        activator
    }

    /// Open one clicked banner. UI thread only.
    fn open(self: &Arc<Self>, ui: &AppWindow, target: NavigationTarget) {
        let click = self.clicks.fetch_add(1, Ordering::AcqRel) + 1;
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
    fn open_when_live(self: &Arc<Self>, target: NavigationTarget, click: u64) {
        let activator = Arc::downgrade(self);
        self.runtime.spawn(async move {
            let deadline = Instant::now() + OPEN_WAIT;
            while Instant::now() < deadline {
                tokio::time::sleep(OPEN_POLL).await;
                let Some(this) = activator.upgrade() else {
                    return;
                };
                let (send, receive) = oneshot::channel();
                let target = target.clone();
                let opener = this.clone();
                let asked = this.ui.upgrade_in_event_loop(move |ui| {
                    let current = opener.clicks.load(Ordering::Acquire) == click;
                    let waiting = current && !ui.get_live_session();
                    if current
                        && !waiting
                        && ui.get_active_account_local_id().as_str() == target.account_id()
                    {
                        opener.open(&ui, target);
                    }
                    let _ = send.send(waiting);
                });
                if asked.is_err() || !receive.await.unwrap_or(false) {
                    return;
                }
            }
        });
    }
}
