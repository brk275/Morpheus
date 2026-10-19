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
//! answering, opening the saved account when no account is open, and
//! pointing an invitation banner at the room list of its account (another
//! account's invitation only switches to that account).

use std::sync::Arc;

use slint::{ComponentHandle, Model, Weak};
use tokio::{runtime::Handle, sync::mpsc};

use crate::{AppWindow, navigation_target::NavigationTarget};

const DIALOG_OPEN: &str = "Finish the open account dialog, then open the notification again.";
const INVITE_OPEN: &str = "Accept or decline this invitation in the room list.";
const ACCOUNT_GONE: &str = "This notification's account is no longer available.";

pub(crate) struct Activator {
    ui: Weak<AppWindow>,
}

impl Activator {
    pub(crate) fn register(
        ui: &AppWindow,
        runtime: Handle,
        activations: Option<mpsc::Receiver<NavigationTarget>>,
    ) -> Arc<Self> {
        let activator = Arc::new(Self { ui: ui.as_weak() });
        if let Some(mut activations) = activations {
            let weak = Arc::downgrade(&activator);
            runtime.spawn(async move {
                while let Some(target) = activations.recv().await {
                    let Some(activator) = weak.upgrade() else {
                        return;
                    };
                    if activator
                        .ui
                        .upgrade_in_event_loop(move |ui| open(&ui, target))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        activator
    }
}

/// Open one clicked banner. UI thread only.
fn open(ui: &AppWindow, target: NavigationTarget) {
    if ui.window().is_minimized() {
        ui.window().set_minimized(false);
    }
    let live = ui.get_live_session();
    let visible = live && ui.get_active_account_local_id().as_str() == target.account_id();
    // Another account goes through the account switch; it must not replace
    // an app-password or leave-call dialog the user is already answering.
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
        // No account is open (locked, or signed out of the last one): the
        // saved account's protected activation. The room opens from the
        // room list once the account is open.
        ui.invoke_activate_saved_account(target.account_id().into());
        return;
    }
    if !visible && invitation {
        // Another account's invitation: only its protected account switch;
        // the invitation then waits in that account's room list.
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
