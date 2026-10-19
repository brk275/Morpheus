//! Account-scoped security controls for the native Settings view.
//! Recovery material is shown only in the current UI session and is never
//! logged or written to Morpheus configuration, drafts, or the account store.

use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};

use morpheus_matrix::{BackupStatus, DeviceVerificationStatus, RecoveryStatus, SecurityInfo};
use slint::{CloseRequestResponse, ComponentHandle};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{AppWindow, live::LiveUi};

const SECURITY_IDLE: u8 = 0;
const SECURITY_BUSY: u8 = 1;
const SECURITY_KEY_PENDING: u8 = 2;
const SECURITY_RESTART_PENDING: u8 = 3;
const SECURITY_ACCOUNT_ACCESS: u8 = 4;

/// Mirrors the account-sensitive UI state across the asynchronous transition
/// lock. A queued account switch must recheck this after it acquires the lock.
#[derive(Default)]
pub(crate) struct SecurityGate(AtomicU8, AtomicU64);

impl SecurityGate {
    pub(crate) fn try_account_access(self: &Arc<Self>) -> Option<AccountAccessLease> {
        self.0
            .compare_exchange(
                SECURITY_IDLE,
                SECURITY_ACCOUNT_ACCESS,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| {
                self.advance_ring_epoch();
                AccountAccessLease(self.clone())
            })
    }
    pub(crate) fn blocked(&self) -> bool {
        self.0.load(Ordering::Acquire) != SECURITY_IDLE
    }

    fn begin(&self) -> bool {
        let reserved = self
            .0
            .compare_exchange(
                SECURITY_IDLE,
                SECURITY_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if reserved {
            self.advance_ring_epoch();
        }
        reserved
    }

    pub(crate) fn ringing_generation(&self) -> u64 {
        self.1.load(Ordering::Acquire)
    }
    pub(crate) fn ringing_allowed(&self, generation: u64) -> bool {
        generation != u64::MAX && !self.blocked() && self.ringing_generation() == generation
    }
    fn advance_ring_epoch(&self) {
        let _ = self
            .1
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_add(1))
            });
    }

    fn key_pending(&self) {
        self.0.store(SECURITY_KEY_PENDING, Ordering::Release);
    }

    fn reset(&self) {
        // Ordinary completion/reset cannot acknowledge a recovery key or
        // release the updater's reservation.
        let _ = self.0.compare_exchange(
            SECURITY_BUSY,
            SECURITY_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn acknowledge_key(&self) {
        let _ = self.0.compare_exchange(
            SECURITY_KEY_PENDING,
            SECURITY_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn can_clear_account_view(&self) -> bool {
        // A queued account snapshot owns neither the recovery reservation nor
        // its resulting key. It may clear stale busy widgets for a new account,
        // but cannot change the gate; the old owner will resolve its mismatch.
        // An actual returned key must remain visible until acknowledged.
        self.0.load(Ordering::Acquire) != SECURITY_KEY_PENDING
    }

    /// Reserve restart synchronously, before any preparation worker starts.
    /// Recovery uses the same CAS before spawning work, so neither operation
    /// can slip between a status check and the other's reservation.
    pub(crate) fn try_restart(self: &Arc<Self>) -> Option<RestartLease> {
        self.0
            .compare_exchange(
                SECURITY_IDLE,
                SECURITY_RESTART_PENDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| {
                self.advance_ring_epoch();
                RestartLease(self.clone())
            })
    }
}

pub(crate) struct RestartLease(Arc<SecurityGate>);

pub(crate) struct AccountAccessLease(Arc<SecurityGate>);
impl Drop for AccountAccessLease {
    fn drop(&mut self) {
        let _ = self.0.0.compare_exchange(
            SECURITY_ACCOUNT_ACCESS,
            SECURITY_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl Drop for RestartLease {
    fn drop(&mut self) {
        let _ = self.0.0.compare_exchange(
            SECURITY_RESTART_PENDING,
            SECURITY_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

fn close_response(gate: &SecurityGate) -> CloseRequestResponse {
    if matches!(
        gate.0.load(Ordering::Acquire),
        SECURITY_BUSY | SECURITY_KEY_PENDING | SECURITY_ACCOUNT_ACCESS
    ) {
        CloseRequestResponse::KeepWindowShown
    } else {
        // Closing while a restart is preparing drains that authorized worker
        // through normal shutdown. Also leaves a manual close path if the UI's
        // quit request fails after the helper has signalled readiness.
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

/// Only a fully set-up recovery on a verified device holds every secret the
/// new key must protect.
fn can_change_key(info: SecurityInfo) -> bool {
    info.recovery == RecoveryStatus::Enabled
        && info.device_verification == DeviceVerificationStatus::Verified
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

/// The SDK reporting success means the secrets were imported; whether this
/// device can now read encrypted history is decided by the refreshed state.
/// Returns whether recovery is complete and the message to show.
fn recovery_result(status: Option<SecurityInfo>) -> (bool, &'static str) {
    let Some(status) = status else {
        return (
            false,
            "Recovery ran, but status could not be refreshed. Choose Refresh to confirm it.",
        );
    };
    match (status.recovery, status.device_verification) {
        (RecoveryStatus::Enabled, DeviceVerificationStatus::Verified) => (
            true,
            "Recovery complete. This device is verified and can read encrypted history.",
        ),
        (RecoveryStatus::Enabled, _) => (
            false,
            "Keys restored, but this device is not verified yet. Verify it with another device.",
        ),
        _ => (
            false,
            "Recovery ran but is still incomplete. Refresh, or check key backup elsewhere.",
        ),
    }
}

fn apply_status(ui: &AppWindow, info: SecurityInfo) {
    ui.set_security_can_change_key(can_change_key(info));
    let (device, recovery, backup, can_enable) = status_labels(info);
    ui.set_security_device_status(device.into());
    ui.set_security_recovery_status(recovery.into());
    ui.set_security_backup_status(backup.into());
    ui.set_security_can_enable(can_enable);
}

/// Clear all account-specific status and secret fields on account change,
/// logout, or entry into the offline preview.
pub(crate) fn clear_ui(ui: &AppWindow, gate: &SecurityGate) {
    // Account transitions are guarded too; this is the final defence against
    // an ordinary delayed refresh hiding an unacknowledged recovery key.
    if !gate.can_clear_account_view() {
        return;
    }
    crate::verification::clear_ui(ui);
    crate::encryption_center::clear_ui(ui);
    next_operation(ui);
    ui.set_security_device_status("Unknown".into());
    ui.set_security_recovery_status("Unknown".into());
    ui.set_security_backup_status("Unknown".into());
    ui.set_security_can_enable(false);
    ui.set_security_can_change_key(false);
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
    if gate.0.load(Ordering::Acquire) == SECURITY_ACCOUNT_ACCESS {
        ui.set_account_access_notice("Wait for the account operation to finish.".into());
        return true;
    }
    if gate.0.load(Ordering::Acquire) == SECURITY_RESTART_PENDING {
        ui.set_page(1);
        ui.set_security_error("Wait for the application restart before changing accounts.".into());
        return true;
    }
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
    crate::encryption_center::register(ui, live.clone(), handle.clone());
    let close_gate = live.security_gate.clone();
    let close_manager = live.manager.clone();
    let close_weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if close_manager.account_access_busy() {
            if let Some(window) = close_weak.upgrade() {
                window.set_account_access_notice(
                    "The credential store is still working. Keep Morpheus open until it finishes."
                        .into(),
                );
            }
            return CloseRequestResponse::KeepWindowShown;
        }
        let response = close_response(&close_gate);
        if response == CloseRequestResponse::KeepWindowShown
            && let Some(window) = close_weak.upgrade()
        {
            if close_gate.0.load(Ordering::Acquire) == SECURITY_ACCOUNT_ACCESS {
                window.set_account_access_notice(
                    "Wait for the account operation to finish before closing Morpheus.".into(),
                );
            } else if close_gate.0.load(Ordering::Acquire) == SECURITY_BUSY {
                window.set_page(1);
                window.set_security_error(
                    "Recovery is still running. Keep Morpheus open until it finishes.".into(),
                );
            } else {
                window.set_security_notice(
                    "Save this key and choose ‘I saved this key’ before closing Morpheus.".into(),
                );
            }
        }
        if response == CloseRequestResponse::HideWindow
            && let Some(window) = close_weak.upgrade()
        {
            // A hidden window keeps running; it must not keep a typed secret.
            window.set_security_recovery_input("".into());
            crate::device_sessions::clear_secret(&window);
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
            ui.invoke_refresh_encryption_devices();
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
    let change_live = live.clone();
    let change_handle = handle.clone();
    let change_gate = live.security_gate.clone();
    ui.on_change_recovery_key(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if window.get_security_busy()
            || !window.get_live_session()
            || window.get_sas_stage() != 0
            || window.get_sas_busy()
            || !window.get_security_can_change_key()
            || !window.get_security_recovery_key().is_empty()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        // The same reservation as recovery setup: the account cannot change
        // and the window cannot close until the new key is saved.
        if !change_gate.begin() {
            return;
        }
        let operation = next_operation(&window);
        window.set_security_busy(true);
        window.set_security_error("".into());
        window.set_security_notice("Keep Morpheus open while the key is replaced.".into());
        let live = change_live.clone();
        let gate = change_gate.clone();
        let weak = weak.clone();
        change_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                gate.reset();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if same_view(&window, &account_id, operation) {
                        window.set_security_busy(false);
                        window.set_security_notice("".into());
                        window.set_security_error(
                            "Account changed before the key was replaced. Nothing changed.".into(),
                        );
                    }
                });
                return;
            }
            // The new key exists only in this result; never cancel this
            // future after the server may have accepted the new store.
            let result = live.manager.change_recovery_key_for(&account_id).await;
            let status = live.manager.security_info().await.ok();
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
                            "Reveal and save this new key now. The previous key no longer works."
                                .into(),
                        );
                    }
                    Err(_) => {
                        gate_for_ui.reset();
                        window.set_security_notice("".into());
                        window.set_security_error(
                            "Could not replace the recovery key. Refresh status, then try again."
                                .into(),
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
                        let (complete, message) = recovery_result(status);
                        if complete {
                            window.set_security_notice(message.into());
                        } else {
                            window.set_security_notice("".into());
                            window.set_security_error(message.into());
                        }
                        window.invoke_refresh_encryption_devices();
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
            dismiss_gate.acknowledge_key();
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
    fn account_password_work_excludes_restart_recovery_and_ringing() {
        let gate = Arc::new(SecurityGate::default());
        let old_generation = gate.ringing_generation();
        let lease = gate.try_account_access().unwrap();
        assert!(gate.blocked());
        assert!(gate.try_restart().is_none());
        assert!(!gate.begin());
        assert!(!gate.ringing_allowed(old_generation));
        assert_eq!(close_response(&gate), CloseRequestResponse::KeepWindowShown);
        gate.reset();
        assert!(gate.blocked());
        drop(lease);
        assert!(!gate.blocked());
        assert!(!gate.ringing_allowed(old_generation));
        assert!(gate.try_restart().is_some());
    }

    #[test]
    fn recovery_success_is_claimed_only_for_a_verified_enabled_device() {
        let status = |recovery, device| {
            Some(SecurityInfo {
                device_verification: device,
                recovery,
                backup: BackupStatus::Enabled,
            })
        };
        let verified = DeviceVerificationStatus::Verified;
        let unverified = DeviceVerificationStatus::Unverified;
        assert!(recovery_result(status(RecoveryStatus::Enabled, verified)).0);
        assert!(!recovery_result(status(RecoveryStatus::Enabled, unverified)).0);
        assert!(!recovery_result(status(RecoveryStatus::Incomplete, verified)).0);
        assert!(!recovery_result(None).0);
    }

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
    fn only_complete_recovery_on_a_verified_device_can_change_the_key() {
        use DeviceVerificationStatus::{Unverified, Verified};
        use RecoveryStatus::{Disabled, Enabled, Incomplete};
        let info = |recovery, device_verification| SecurityInfo {
            device_verification,
            recovery,
            backup: BackupStatus::Enabled,
        };
        assert!(can_change_key(info(Enabled, Verified)));
        for (recovery, device) in [
            (Enabled, Unverified),
            (Incomplete, Verified),
            (Disabled, Verified),
        ] {
            assert!(!can_change_key(info(recovery, device)));
        }
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
        assert!(gate.blocked());
        gate.acknowledge_key();
        assert!(!gate.blocked());
        assert_eq!(close_response(&gate), CloseRequestResponse::HideWindow);
    }

    #[test]
    fn restart_and_recovery_reservations_exclude_each_other_and_reset_preserves_restart() {
        let gate = Arc::new(SecurityGate::default());
        assert!(gate.begin());
        assert!(gate.try_restart().is_none());
        gate.key_pending();
        assert!(gate.try_restart().is_none());
        gate.reset();
        assert!(gate.try_restart().is_none());
        gate.acknowledge_key();
        let lease = gate.try_restart().unwrap();
        assert!(gate.blocked());
        assert!(!gate.begin());
        assert!(gate.try_restart().is_none());
        gate.reset();
        assert!(gate.blocked());
        assert!(!gate.begin());
        assert_eq!(close_response(&gate), CloseRequestResponse::HideWindow);
        drop(lease);
        assert!(!gate.blocked());
        assert!(gate.begin());
    }

    #[test]
    fn queued_account_clear_cannot_release_an_owned_recovery_or_erase_its_key() {
        let gate = Arc::new(SecurityGate::default());
        assert!(gate.begin());
        // This is the same decision clear_ui makes before touching fields.
        assert!(gate.can_clear_account_view());
        assert!(
            gate.blocked(),
            "clearing stale widgets does not release ownership"
        );
        assert!(
            !gate.begin(),
            "a second recovery cannot acquire the old operation's reservation"
        );
        assert!(gate.try_restart().is_none());
        // Successful recovery keeps ownership until the user saves the key.
        gate.key_pending();
        assert!(!gate.can_clear_account_view());
        assert!(gate.try_restart().is_none());
        gate.reset();
        assert!(!gate.can_clear_account_view());
        gate.acknowledge_key();
        assert!(gate.can_clear_account_view());
        assert!(gate.try_restart().is_some());
    }
    #[test]
    fn recovery_or_restart_roundtrip_cannot_revive_an_existing_ring() {
        let gate = Arc::new(SecurityGate::default());
        let initial = gate.ringing_generation();
        assert!(gate.ringing_allowed(initial));
        assert!(gate.begin());
        gate.reset();
        assert!(!gate.ringing_allowed(initial));
        let current = gate.ringing_generation();
        let lease = gate.try_restart().unwrap();
        drop(lease);
        assert!(!gate.ringing_allowed(current));
    }
}
