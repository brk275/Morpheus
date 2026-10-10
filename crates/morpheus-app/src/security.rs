//! Account-scoped security controls for the native Settings view.
//! Recovery material is shown only in the current UI session and is never
//! logged or written to Morpheus configuration, drafts, or the account store.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use morpheus_matrix::{BackupStatus, DeviceVerificationStatus, RecoveryStatus, SecurityInfo};
use slint::{CloseRequestResponse, ComponentHandle};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{AppWindow, live::LiveUi};

const SECURITY_IDLE: u8 = 0;
const SECURITY_BUSY: u8 = 1;
const SECURITY_KEY_PENDING: u8 = 2;

/// Mirrors the account-sensitive UI state across the asynchronous transition
/// lock. A queued account switch must recheck this after it acquires the lock.
#[derive(Default)]
pub(crate) struct SecurityGate(AtomicU8);

impl SecurityGate {
    pub(crate) fn blocked(&self) -> bool {
        self.0.load(Ordering::Acquire) != SECURITY_IDLE
    }

    fn begin(&self) -> bool {
        self.0
            .compare_exchange(
                SECURITY_IDLE,
                SECURITY_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn key_pending(&self) {
        self.0.store(SECURITY_KEY_PENDING, Ordering::Release);
    }

    fn reset(&self) {
        self.0.store(SECURITY_IDLE, Ordering::Release);
    }
}

fn close_response(gate: &SecurityGate) -> CloseRequestResponse {
    if gate.blocked() {
        CloseRequestResponse::KeepWindowShown
    } else {
        CloseRequestResponse::HideWindow
    }
}

fn next_operation(ui: &AppWindow) -> i32 {
    let operation = ui.get_security_operation().wrapping_add(1);
    ui.set_security_operation(operation);
    operation
}

fn same_view(ui: &AppWindow, account_id: &str, operation: i32) -> bool {
    ui.get_live_session()
        && ui.get_active_account_local_id().as_str() == account_id
        && ui.get_security_operation() == operation
}

async fn active_account_is(live: &LiveUi, account_id: &str) -> bool {
    live.manager
        .active_account()
        .await
        .is_some_and(|account| account.id == account_id)
}

fn status_labels(info: SecurityInfo) -> (&'static str, &'static str, &'static str, bool) {
    let device = match info.device_verification {
        DeviceVerificationStatus::Unknown => "Checking",
        DeviceVerificationStatus::Verified => "Verified",
        DeviceVerificationStatus::Unverified => "Unverified",
    };
    let recovery = match info.recovery {
        RecoveryStatus::Unknown => "Checking",
        RecoveryStatus::Enabled => "Enabled",
        RecoveryStatus::Disabled => "Disabled",
        RecoveryStatus::Incomplete => "Incomplete",
    };
    let backup = match info.backup {
        BackupStatus::Unknown => "Checking",
        BackupStatus::Creating => "Creating",
        BackupStatus::Enabling => "Enabling",
        BackupStatus::Resuming => "Resuming",
        BackupStatus::Enabled => "Enabled",
        BackupStatus::Downloading => "Downloading",
        BackupStatus::Disabling => "Disabling",
    };
    (
        device,
        recovery,
        backup,
        info.recovery == RecoveryStatus::Disabled,
    )
}

fn apply_status(ui: &AppWindow, info: SecurityInfo) {
    let (device, recovery, backup, can_enable) = status_labels(info);
    ui.set_security_device_status(device.into());
    ui.set_security_recovery_status(recovery.into());
    ui.set_security_backup_status(backup.into());
    ui.set_security_can_enable(can_enable);
}

/// Clear all account-specific status and secret fields on account change,
/// logout, or entry into the offline preview.
pub(crate) fn clear_ui(ui: &AppWindow, gate: &SecurityGate) {
    gate.reset();
    crate::verification::clear_ui(ui);
    next_operation(ui);
    ui.set_security_device_status("Unknown".into());
    ui.set_security_recovery_status("Unknown".into());
    ui.set_security_backup_status("Unknown".into());
    ui.set_security_can_enable(false);
    ui.set_security_loading(false);
    ui.set_security_busy(false);
    ui.set_security_error("".into());
    ui.set_security_notice("".into());
    ui.set_security_recovery_input("".into());
    ui.set_security_recovery_key("".into());
    ui.set_security_key_revealed(false);
}

/// A recovery operation must finish and a newly generated key must be saved
/// before this account can be left. The guard runs before navigation is
/// invalidated or an account transition is spawned.
pub(crate) fn block_account_change(ui: &AppWindow, gate: &SecurityGate) -> bool {
    if gate.0.load(Ordering::Acquire) == SECURITY_BUSY {
        ui.set_page(1);
        ui.set_security_error(
            "Wait for the security operation to finish before changing accounts.".into(),
        );
        return true;
    }
    if gate.0.load(Ordering::Acquire) == SECURITY_KEY_PENDING {
        ui.set_page(1);
        ui.set_security_error(
            "Save and acknowledge the new recovery key before changing accounts.".into(),
        );
        return true;
    }
    false
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    crate::verification::register(ui, live.clone(), handle.clone());
    let close_gate = live.security_gate.clone();
    let close_weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        let response = close_response(&close_gate);
        if response == CloseRequestResponse::KeepWindowShown
            && let Some(window) = close_weak.upgrade()
        {
            window.set_page(1);
            if close_gate.0.load(Ordering::Acquire) == SECURITY_BUSY {
                window.set_security_error(
                    "Recovery is still running. Keep Morpheus open until it finishes.".into(),
                );
            } else {
                window.set_security_notice(
                    "Save this key and choose ‘I saved this key’ before closing Morpheus.".into(),
                );
            }
        }
        response
    });

    let weak = ui.as_weak();
    ui.on_open_security(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_live_session()
        {
            ui.invoke_refresh_security();
            ui.invoke_refresh_incoming_verification();
        }
    });

    let weak = ui.as_weak();
    let refresh_live = live.clone();
    let refresh_handle = handle.clone();
    ui.on_refresh_security(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !window.get_live_session() || window.get_security_busy() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_security_loading(true);
        window.set_security_error("".into());
        let live = refresh_live.clone();
        let weak = weak.clone();
        refresh_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live.manager.security_info().await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_view(&window, &account_id, operation) {
                    return;
                }
                window.set_security_loading(false);
                match result {
                    Ok(info) => apply_status(&window, info),
                    Err(_) => window.set_security_error(
                        "Could not load account security status. Try Refresh.".into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let enable_live = live.clone();
    let enable_handle = handle.clone();
    let enable_gate = live.security_gate.clone();
    ui.on_enable_recovery(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if window.get_security_busy()
            || !window.get_live_session()
            || window.get_sas_stage() != 0
            || window.get_sas_busy()
            || !window.get_security_can_enable()
            || !window.get_security_recovery_key().is_empty()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        if !enable_gate.begin() {
            return;
        }
        let operation = next_operation(&window);
        window.set_security_busy(true);
        window.set_security_error("".into());
        window.set_security_notice("Keep Morpheus open while recovery is set up.".into());
        let live = enable_live.clone();
        let gate = enable_gate.clone();
        let weak = weak.clone();
        enable_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                gate.reset();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if same_view(&window, &account_id, operation) {
                        window.set_security_busy(false);
                        window.set_security_notice("".into());
                        window.set_security_error(
                            "Account changed before recovery setup began. Refresh status.".into(),
                        );
                    }
                });
                return;
            }
            // The SDK returns the key only here. Do not cancel this future
            // with a UI timeout after setup may have started on the server.
            let result = live.manager.enable_recovery(None, false).await;
            let status = if result.is_ok() {
                live.manager.security_info().await.ok()
            } else {
                None
            };
            if result.is_ok() {
                gate.key_pending();
            }
            let gate_for_ui = gate.clone();
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_view(&window, &account_id, operation) {
                    return;
                }
                window.set_security_busy(false);
                if let Some(info) = status {
                    apply_status(&window, info);
                }
                match result {
                    Ok(key) => {
                        window.set_security_key_revealed(false);
                        window.set_security_recovery_key(key.reveal().into());
                        window.set_security_notice(
                            "Reveal and save this key now. It cannot be shown again.".into(),
                        );
                    }
                    Err(_) => {
                        gate_for_ui.reset();
                        window.set_security_notice("".into());
                        window.set_security_error(
                            "Could not set up recovery. Refresh status before trying again.".into(),
                        );
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let recover_live = live.clone();
    let recover_handle = handle.clone();
    let recover_gate = live.security_gate.clone();
    ui.on_recover_account(move |value| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        // The Slint action clears its password field before invoking us;
        // clear it again for callers outside that widget.
        window.set_security_recovery_input("".into());
        if window.get_security_busy()
            || !window.get_live_session()
            || window.get_sas_stage() != 0
            || window.get_sas_busy()
            || !window.get_security_recovery_key().is_empty()
        {
            return;
        }
        let secret = Zeroizing::new(value.to_string());
        if secret.trim().is_empty() {
            window.set_security_error("Enter a recovery key or passphrase.".into());
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        if !recover_gate.begin() {
            return;
        }
        let operation = next_operation(&window);
        window.set_security_busy(true);
        window.set_security_error("".into());
        window.set_security_notice("Keep Morpheus open while recovery runs.".into());
        let live = recover_live.clone();
        let gate = recover_gate.clone();
        let weak = weak.clone();
        recover_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                gate.reset();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if same_view(&window, &account_id, operation) {
                        window.set_security_busy(false);
                        window.set_security_notice("".into());
                        window.set_security_error(
                            "Account changed before recovery began. Refresh status.".into(),
                        );
                    }
                });
                return;
            }
            let result = live.manager.recover_account(secret.as_str()).await;
            let status = if result.is_ok() {
                live.manager.security_info().await.ok()
            } else {
                None
            };
            let gate_for_ui = gate.clone();
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_view(&window, &account_id, operation) {
                    return;
                }
                gate_for_ui.reset();
                window.set_security_busy(false);
                if let Some(info) = status {
                    apply_status(&window, info);
                }
                match result {
                    Ok(()) => {
                        window.set_security_notice(
                            "Recovery completed. Refresh status if key backup is still updating."
                                .into(),
                        );
                    }
                    Err(_) => {
                        window.set_security_notice("".into());
                        window.set_security_error(
                            "Recovery failed. Check the key or passphrase and try again.".into(),
                        );
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    ui.on_copy_recovery_key(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !window.get_security_key_revealed() {
            return;
        }
        let secret = Zeroizing::new(window.get_security_recovery_key().to_string());
        if secret.is_empty() {
            return;
        }
        let copied = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(secret.as_str().to_owned()))
            .is_ok();
        window.set_security_notice(if copied {
            "Copied. Save it privately, then clear your system clipboard.".into()
        } else {
            "Could not access the clipboard. Save the visible key manually.".into()
        });
    });

    let weak = ui.as_weak();
    let dismiss_gate = live.security_gate.clone();
    ui.on_dismiss_recovery_key(move || {
        if let Some(window) = weak.upgrade()
            && window.get_security_key_revealed()
        {
            next_operation(&window);
            window.set_security_recovery_key("".into());
            window.set_security_key_revealed(false);
            dismiss_gate.reset();
            window.set_security_notice(
                "Recovery key hidden. Morpheus cannot display it again.".into(),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_requires_disabled_recovery_and_statuses_are_distinct() {
        let disabled = status_labels(SecurityInfo {
            device_verification: DeviceVerificationStatus::Unverified,
            recovery: RecoveryStatus::Disabled,
            backup: BackupStatus::Unknown,
        });
        assert_eq!(disabled, ("Unverified", "Disabled", "Checking", true));

        let incomplete = status_labels(SecurityInfo {
            device_verification: DeviceVerificationStatus::Verified,
            recovery: RecoveryStatus::Incomplete,
            backup: BackupStatus::Enabling,
        });
        assert_eq!(incomplete, ("Verified", "Incomplete", "Enabling", false));
    }

    #[test]
    fn queued_account_transition_stays_blocked_until_key_is_acknowledged() {
        let gate = SecurityGate::default();
        assert!(!gate.blocked());
        assert_eq!(close_response(&gate), CloseRequestResponse::HideWindow);
        assert!(gate.begin());
        assert!(gate.blocked());
        assert_eq!(close_response(&gate), CloseRequestResponse::KeepWindowShown);
        assert!(!gate.begin());
        gate.key_pending();
        assert!(gate.blocked());
        assert_eq!(close_response(&gate), CloseRequestResponse::KeepWindowShown);
        gate.reset();
        assert!(!gate.blocked());
        assert_eq!(close_response(&gate), CloseRequestResponse::HideWindow);
    }
}
