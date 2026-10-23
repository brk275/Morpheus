//! Account-scoped security controls for the native Settings view.
//! Recovery material is shown only in the current UI session and is never
//! logged or written to Morpheus configuration, drafts, or the account store.
//!
//! Status, setup and recovery go through `morpheus_matrix::account_recovery`
//! for the exact account selection; the manager's state is never held across
//! a request. Setup and recovery reserve the security gate first and keep
//! the account transition for their whole run, as before, and are never
//! cancelled or timed out once they start. The status is read again after
//! every outcome and described as the SDK reports it: a request that
//! returned does not mean every message's key came back. A generated key is
//! never dropped: if its account is not shown when it arrives, it is held
//! until that account's Security page shows it, and the gate stays reserved
//! until the person confirms they saved it.

use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};

use morpheus_matrix::{
    AccountRecoveryError, BackupStatus, DeviceVerificationStatus, RecoveryKey, RecoveryOverview,
    RecoveryProgress, RecoveryStatus, SecurityInfo, SetupConfirmation,
};
use slint::{CloseRequestResponse, ComponentHandle};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{AppWindow, live::LiveUi};

const SECURITY_IDLE: u8 = 0;
const SECURITY_BUSY: u8 = 1;
const SECURITY_KEY_PENDING: u8 = 2;
const SECURITY_RESTART_PENDING: u8 = 3;
const SECURITY_SHUTDOWN: u8 = 5;
const SECURITY_ACCOUNT_ACCESS: u8 = 4;

/// Mirrors the account-sensitive UI state across the asynchronous transition
/// lock. A queued account switch must recheck this after it acquires the lock.
#[derive(Default)]
pub(crate) struct SecurityGate(
    AtomicU8,
    AtomicU64,
    // A restart reservation is an actual retained Rust owner, not a state
    // snapshot or generation another controller can reuse. Never held across
    // async work; updater callers take their own inner lock before this lock.
    std::sync::Mutex<Option<std::sync::Weak<()>>>,
);

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
    /// Atomically close ordinary admission BEFORE the UI yields to another
    /// event. Busy recovery, unsaved keys and credential owners always refuse.
    /// Existing restart/account/recovery lease Drop cannot reset this state.
    #[cfg(all(windows, feature = "native-calls"))]
    pub(crate) fn begin_shutdown(&self, restart: Option<&RestartLease>) -> bool {
        let Ok(owner) = self.2.lock() else {
            return false;
        };
        loop {
            let current = self.0.load(Ordering::Acquire);
            if current == SECURITY_SHUTDOWN {
                return true;
            }
            if current == SECURITY_RESTART_PENDING {
                let Some(restart) = restart else {
                    return false;
                };
                if !std::ptr::eq(self, Arc::as_ptr(&restart.gate))
                    || !owner
                        .as_ref()
                        .and_then(std::sync::Weak::upgrade)
                        .is_some_and(|identity| Arc::ptr_eq(&identity, &restart.identity))
                {
                    return false;
                }
            } else if current != SECURITY_IDLE {
                return false;
            }
            if self
                .0
                .compare_exchange(
                    current,
                    SECURITY_SHUTDOWN,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.advance_ring_epoch();
                return true;
            }
        }
    }

    #[cfg(all(windows, feature = "native-calls"))]
    pub(crate) fn admit_shutdown(
        &self,
        restart: Option<&RestartLease>,
    ) -> Option<ShutdownAdmission<'_>> {
        self.begin_shutdown(restart)
            .then_some(ShutdownAdmission { _gate: self })
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

    /// Whether the recovery reservation taken at `generation` still holds:
    /// reserved for recovery, and no other security step began since.
    fn recovery_owned(&self, generation: u64) -> bool {
        self.0.load(Ordering::Acquire) == SECURITY_BUSY && self.ringing_generation() == generation
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
        let mut owner = self.2.lock().ok()?;
        let identity = Arc::new(());
        self.0
            .compare_exchange(
                SECURITY_IDLE,
                SECURITY_RESTART_PENDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        self.advance_ring_epoch();
        *owner = Some(Arc::downgrade(&identity));
        Some(RestartLease {
            gate: self.clone(),
            identity,
        })
    }
}

/// Produced only by the actual gate reducer; no caller can forge an owned
/// Pending reservation from a snapshot. Dropping it never reopens admission.
#[cfg(all(windows, feature = "native-calls"))]
pub(crate) struct ShutdownAdmission<'a> {
    _gate: &'a SecurityGate,
}

pub(crate) struct RestartLease {
    gate: Arc<SecurityGate>,
    identity: Arc<()>,
}

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
        let Ok(mut owner) = self.gate.2.lock() else {
            return;
        };
        if !owner
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|identity| Arc::ptr_eq(&identity, &self.identity))
        {
            return;
        }
        *owner = None;
        // A terminal shutdown reservation survives this owner's Drop.
        let _ = self.gate.0.compare_exchange(
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
        SECURITY_BUSY | SECURITY_KEY_PENDING | SECURITY_ACCOUNT_ACCESS | SECURITY_SHUTDOWN
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

/// A generated recovery key whose account was not shown when it arrived:
/// kept (zeroized on drop), with how far its setup is known to have got,
/// until that account's Security page shows both.
type PendingKey = (String, RecoveryKey, SetupConfirmation);
static PENDING_KEY: std::sync::Mutex<Option<PendingKey>> = std::sync::Mutex::new(None);

/// Status lines for one overview: device, recovery, key backup (with
/// whether one exists on the server), identity keys, and whether setup is
/// offered. A part that is not known says so.
fn overview_labels(info: &RecoveryOverview) -> (String, String, String, String, bool) {
    let (device, recovery, backup, can_enable) = status_labels(SecurityInfo {
        device_verification: info.device,
        recovery: info.recovery,
        backup: info.backup,
    });
    let server = match info.backup_on_server {
        Some(true) => "a backup exists on the server",
        Some(false) => "no backup on the server",
        None => "server backup not known",
    };
    let identity = match info.cross_signing {
        Some(keys) => format!("Cross-signing keys on this device: {} of 3", keys.count()),
        None => "Cross-signing keys on this device: not known yet".to_owned(),
    };
    (
        device.to_owned(),
        recovery.to_owned(),
        format!("{backup} ({server})"),
        identity,
        can_enable,
    )
}

fn apply_overview(ui: &AppWindow, info: &RecoveryOverview) {
    let (device, recovery, backup, identity, can_enable) = overview_labels(info);
    ui.set_security_device_status(device.into());
    ui.set_security_recovery_status(recovery.into());
    ui.set_security_backup_status(backup.into());
    ui.set_security_identity_status(identity.into());
    ui.set_security_can_enable(can_enable);
}

fn progress_text(progress: RecoveryProgress) -> String {
    match progress {
        RecoveryProgress::Starting => "Starting recovery setup…".to_owned(),
        RecoveryProgress::CreatingBackup => "Creating a key backup on your homeserver…".to_owned(),
        RecoveryProgress::CreatingRecoveryKey => {
            "Creating your recovery key and storing your encrypted secrets…".to_owned()
        }
        RecoveryProgress::BackingUp { backed_up, total } => {
            format!("Backing up message keys: {backed_up} of {total}…")
        }
        RecoveryProgress::UploadIncomplete => {
            "Some message keys are not backed up yet; they are uploaded later.".to_owned()
        }
        RecoveryProgress::Finishing => "Finishing recovery setup…".to_owned(),
    }
}

/// The notice and the error for a recovery outcome, from the state read
/// after it. Never claims that every message's key came back.
fn recovery_outcome(
    result: &Result<(), AccountRecoveryError>,
    overview: Option<&RecoveryOverview>,
) -> (String, String) {
    let state = overview.map(|info| {
        let (_, recovery, backup, identity, _) = overview_labels(info);
        format!("{identity} · Recovery: {recovery} · Key backup: {backup}.")
    });
    let complete = overview.is_some_and(|info| {
        info.recovery == RecoveryStatus::Enabled
            && info.cross_signing.is_some_and(|keys| keys.complete())
            && matches!(
                info.backup,
                BackupStatus::Enabled | BackupStatus::Downloading
            )
    });
    match (result, state) {
        (Ok(()), Some(state)) if complete => (
            format!(
                "Recovery finished. {state} Older messages decrypt as their keys download; \
                 messages whose keys were never backed up stay unreadable."
            ),
            String::new(),
        ),
        (Ok(()), Some(state)) => (
            String::new(),
            format!(
                "Recovery finished but is not complete yet. {state} Refresh in a moment; if it \
                 stays incomplete, enter your recovery key again."
            ),
        ),
        (Ok(()), None) => (
            String::new(),
            "Recovery returned, but the resulting state could not be read. Refresh status \
             before trying again."
                .to_owned(),
        ),
        (Err(error), state) => {
            let retry = match error {
                AccountRecoveryError::KeyNotAccepted | AccountRecoveryError::NotSetUp => {
                    " You can try again with another key or passphrase."
                }
                AccountRecoveryError::SecurityBusy | AccountRecoveryError::AccountChanged => "",
                _ => " Check the state below, then try again only if it is still incomplete.",
            };
            let state = state.map(|state| format!(" {state}")).unwrap_or_default();
            (String::new(), format!("{error}{retry}{state}"))
        }
    }
}

/// What an unconfirmed setup's outcome says; `None` when setup is
/// confirmed. Never claims the key is in use, nor that it is not.
fn setup_outcome(confirmation: SetupConfirmation) -> Option<&'static str> {
    match confirmation {
        SetupConfirmation::Confirmed => None,
        SetupConfirmation::FinalStepFailed => Some(
            "Setup created your recovery key and then reported an error, so it is unconfirmed. \
             Save the key first; then check the status, which is read again after you refresh.",
        ),
        SetupConfirmation::StorageUnknown => Some(
            "Setup created your recovery key but failed while storing it, so whether your \
             account now depends on this key is not known. Save the key first: your homeserver \
             may already use it. Then refresh and check the status; set up recovery again only \
             after the key is saved, because a new setup replaces it.",
        ),
    }
}

/// Put a generated key in the one-time key dialog. The gate is already
/// reserved for it; only "I saved this key" releases it.
fn show_key(ui: &AppWindow, key: &RecoveryKey) {
    ui.set_security_busy(false);
    ui.set_security_progress("".into());
    ui.set_security_key_revealed(false);
    crate::withdraw_call_bar(ui);
    ui.set_security_recovery_key(key.reveal().into());
    ui.set_security_error("".into());
    ui.set_security_notice("Reveal and save this key now. It cannot be shown again.".into());
}

/// Show a held key once its account is the one shown.
fn show_pending_key(ui: &AppWindow) {
    if !ui.get_security_recovery_key().is_empty() {
        return;
    }
    let account = ui.get_active_account_local_id().to_string();
    let mut held = PENDING_KEY
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if held.as_ref().is_some_and(|(owner, _, _)| *owner == account)
        && let Some((_, key, confirmation)) = held.take()
    {
        show_key(ui, &key);
        // Kept with the key, and through refreshes (see `set_outcome`).
        if let Some(text) = setup_outcome(confirmation) {
            set_outcome(ui, text, true);
        }
    }
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
    // The session list, any sign-out step and its challenge end with the account.
    crate::device_sessions::clear_presentation(ui);
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
    ui.set_security_identity_status("".into());
    ui.set_security_progress("".into());
    set_outcome(ui, "", false);
}

/// The last setup or recovery outcome. Status reads never touch it; it
/// stays until the person dismisses it, a new setup or recovery starts, or
/// the account changes, so an automatic refresh cannot erase an unresolved
/// failure or uncertainty.
fn set_outcome(ui: &AppWindow, text: &str, is_error: bool) {
    ui.set_security_outcome(text.into());
    ui.set_security_outcome_error(is_error);
}

/// A recovery operation must finish and a newly generated key must be saved
/// before this account can be left. The guard runs before navigation is
/// invalidated or an account transition is spawned.
pub(crate) fn block_account_change(ui: &AppWindow, gate: &SecurityGate) -> bool {
    if gate.0.load(Ordering::Acquire) == SECURITY_SHUTDOWN {
        ui.set_account_access_notice("Morpheus is closing. Wait for cleanup to finish.".into());
        return true;
    }
    if gate.0.load(Ordering::Acquire) == SECURITY_ACCOUNT_ACCESS {
        ui.set_account_access_notice("Wait for the account operation to finish.".into());
        return true;
    }
    if gate.0.load(Ordering::Acquire) == SECURITY_RESTART_PENDING {
        crate::thread_browser::withdraw(ui);
        ui.invoke_navigate_page(1);
        ui.set_security_error("Wait for the application restart before changing accounts.".into());
        return true;
    }
    if gate.0.load(Ordering::Acquire) == SECURITY_BUSY {
        crate::thread_browser::withdraw(ui);
        ui.invoke_navigate_page(1);
        ui.set_security_error(
            "Wait for the security operation to finish before changing accounts.".into(),
        );
        return true;
    }
    if gate.0.load(Ordering::Acquire) == SECURITY_KEY_PENDING {
        crate::thread_browser::withdraw(ui);
        ui.invoke_navigate_page(1);
        ui.set_security_error(
            "Save and acknowledge the new recovery key before changing accounts.".into(),
        );
        return true;
    }
    false
}

pub(crate) fn register(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    handle: Handle,
    #[cfg(all(windows, feature = "native-calls"))]
    updates: crate::update_controller::UpdateController,
) {
    crate::verification::register(ui, live.clone(), handle.clone());
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
        if response != CloseRequestResponse::KeepWindowShown
            && let Some(window) = close_weak.upgrade()
        {
            window.invoke_stop_attachment_audio();
        }
        if response == CloseRequestResponse::KeepWindowShown
            && let Some(window) = close_weak.upgrade()
        {
            if close_gate.0.load(Ordering::Acquire) == SECURITY_SHUTDOWN {
                window.set_update_status("Morpheus is closing. Wait for cleanup to finish.".into());
            } else if close_gate.0.load(Ordering::Acquire) == SECURITY_ACCOUNT_ACCESS {
                window.set_account_access_notice(
                    "Wait for the account operation to finish before closing Morpheus.".into(),
                );
            } else if close_gate.0.load(Ordering::Acquire) == SECURITY_BUSY {
                crate::thread_browser::withdraw(&window);
                window.invoke_navigate_page(1);
                window.set_security_error(
                    "Recovery is still running. Keep Morpheus open until it finishes.".into(),
                );
            } else {
                window.set_security_notice(
                    "Save this key and choose ‘I saved this key’ before closing Morpheus.".into(),
                );
            }
        }
        #[cfg(all(windows, feature = "native-calls"))]
        if response != CloseRequestResponse::KeepWindowShown {
            if !updates.request_windows_shutdown()
                && let Some(window) = close_weak.upgrade()
            {
                window.set_security_notice(
                    "Wait for the account or security operation before closing Morpheus.".into(),
                );
            }
            return CloseRequestResponse::KeepWindowShown;
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
        show_pending_key(&window);
        if !window.get_live_session() || window.get_security_busy() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let Some(selection) = refresh_live.manager.account_selection(&account_id) else {
            return;
        };
        let operation = next_operation(&window);
        window.set_security_loading(true);
        // Clears only an earlier status-read error, never an operation's
        // outcome (see `set_outcome`).
        window.set_security_error("".into());
        let live = refresh_live.clone();
        let weak = weak.clone();
        // A read: no account transition is held, so it never delays a switch.
        refresh_handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager.recovery_overview_selected(&selection).await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_view(&window, &account_id, operation) || !selection.is_current() {
                    return;
                }
                window.set_security_loading(false);
                match result {
                    Ok(info) => apply_overview(&window, &info),
                    Err(AccountRecoveryError::AccountChanged) => {}
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
        let Some(selection) = enable_live.manager.account_selection(&account_id) else {
            return;
        };
        if !enable_gate.begin() {
            return;
        }
        // The reservation's generation: setup starts only while it holds.
        let generation = enable_gate.ringing_generation();
        let operation = next_operation(&window);
        window.invoke_withdraw_verification();
        crate::withdraw_call_bar(&window);
        window.set_security_busy(true);
        set_outcome(&window, "", false);
        window.set_security_error("".into());
        window.set_security_progress("".into());
        window.set_security_notice("Keep Morpheus open while recovery is set up.".into());
        let live = enable_live.clone();
        let gate = enable_gate.clone();
        let weak = weak.clone();
        enable_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            let admitted = {
                let gate = gate.clone();
                move || gate.recovery_owned(generation)
            };
            let progress_weak = weak.clone();
            let progress_account = account_id.clone();
            let report = move |progress: RecoveryProgress| {
                let account = progress_account.clone();
                let _ = progress_weak.upgrade_in_event_loop(move |window| {
                    if same_view(&window, &account, operation) && window.get_security_busy() {
                        window.set_security_progress(progress_text(progress).into());
                    }
                });
            };
            // The SDK returns the key only at the end. Once started this is
            // never cancelled or timed out, so the key cannot be lost.
            let manager = live.manager.clone();
            let result = manager
                .enable_recovery_selected(&selection, &admitted, &report)
                .await;
            // The key is held for its account before anything else, so no UI
            // path (a closed window, another account shown) can lose it; the
            // gate stays reserved until "I saved this key".
            // An unconfirmed setup's key is held the same way: the server
            // may already depend on it.
            let result = result.map(|enabled| {
                gate.key_pending();
                let mut held = PENDING_KEY
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                *held = Some((account_id.clone(), enabled.key, enabled.confirmation));
                enabled.overview
            });
            let gate_for_ui = gate.clone();
            let _ = weak.upgrade_in_event_loop(move |window| {
                match result {
                    Ok(overview) => {
                        // Shown now for its account, with any uncertainty;
                        // otherwise when that account's Security page next
                        // refreshes.
                        if window.get_active_account_local_id().as_str() == account_id {
                            if let Some(info) = overview.as_ref() {
                                apply_overview(&window, info);
                            }
                            show_pending_key(&window);
                        }
                    }
                    Err(error) => {
                        gate_for_ui.reset();
                        if !same_view(&window, &account_id, operation) {
                            return;
                        }
                        window.set_security_busy(false);
                        window.set_security_progress("".into());
                        window.set_security_notice("".into());
                        let text = match error {
                            AccountRecoveryError::Unconfirmed => {
                                "Recovery setup did not finish and may have partly applied. The \
                                 status is being read again; once it shows recovery still \
                                 disabled, you can retry setup."
                                    .to_owned()
                            }
                            other => other.to_string(),
                        };
                        // Kept through the refresh below (see `set_outcome`).
                        set_outcome(&window, &text, true);
                        // What the account has now, not what was attempted.
                        window.invoke_refresh_security();
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
        // clear it again for callers outside that widget. The typed value
        // goes straight into zeroizing memory.
        window.set_security_recovery_input("".into());
        let secret = Zeroizing::new(value.to_string());
        drop(value);
        if window.get_security_busy()
            || !window.get_live_session()
            || window.get_sas_stage() != 0
            || window.get_sas_busy()
            || !window.get_security_recovery_key().is_empty()
        {
            return;
        }
        if secret.trim().is_empty() {
            window.set_security_error("Enter a recovery key or passphrase.".into());
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let Some(selection) = recover_live.manager.account_selection(&account_id) else {
            return;
        };
        if !recover_gate.begin() {
            return;
        }
        let generation = recover_gate.ringing_generation();
        let operation = next_operation(&window);
        window.invoke_withdraw_verification();
        crate::withdraw_call_bar(&window);
        window.set_security_busy(true);
        set_outcome(&window, "", false);
        window.set_security_error("".into());
        window.set_security_progress("Importing your encrypted secrets…".into());
        window.set_security_notice("Keep Morpheus open while recovery runs.".into());
        let live = recover_live.clone();
        let gate = recover_gate.clone();
        let weak = weak.clone();
        recover_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            let admitted = {
                let gate = gate.clone();
                move || gate.recovery_owned(generation)
            };
            // Never cancelled or timed out once started; never retried here.
            let manager = live.manager.clone();
            let (result, overview) = manager
                .recover_selected(&selection, &admitted, secret)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                gate.reset();
                if !same_view(&window, &account_id, operation) {
                    return;
                }
                window.set_security_busy(false);
                window.set_security_progress("".into());
                if let Some(info) = overview.as_ref() {
                    apply_overview(&window, info);
                }
                let (notice, error) = recovery_outcome(&result, overview.as_ref());
                window.set_security_notice("".into());
                if error.is_empty() {
                    set_outcome(&window, &notice, false);
                } else {
                    set_outcome(&window, &error, true);
                }
            });
        });
    });

    let weak = ui.as_weak();
    ui.on_dismiss_security_outcome(move || {
        if let Some(window) = weak.upgrade() {
            set_outcome(&window, "", false);
        }
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

    #[cfg(all(windows, feature = "native-calls"))]
    #[test]
    fn shutdown_gate_stays_closed_after_restart_owner_and_async_cleanup_release() {
        let gate = Arc::new(SecurityGate::default());
        let epoch = gate.ringing_generation();
        let restart = gate.try_restart().expect("ordinary restart reservation");
        let admitted = gate.begin_shutdown(Some(&restart));
        drop(restart);
        gate.reset();
        assert!(admitted);
        assert!(gate.blocked());
        assert!(!gate.ringing_allowed(epoch));
        assert!(gate.try_account_access().is_none());
        assert!(gate.try_restart().is_none());
        assert!(!gate.begin());
    }

    #[cfg(all(windows, feature = "native-calls"))]
    #[test]
    fn restart_shutdown_requires_the_exact_retained_reservation() {
        let gate = Arc::new(SecurityGate::default());
        let foreign = Arc::new(SecurityGate::default());
        let restart = gate.try_restart().expect("restart owner");
        let other = foreign.try_restart().expect("foreign owner");
        assert!(!gate.begin_shutdown(None));
        assert!(!gate.begin_shutdown(Some(&other)));
        assert_eq!(gate.0.load(Ordering::Acquire), SECURITY_RESTART_PENDING);
        drop(other);
        assert!(gate.begin_shutdown(Some(&restart)));
        drop(restart);
        assert_eq!(gate.0.load(Ordering::Acquire), SECURITY_SHUTDOWN);
        assert!(gate.begin_shutdown(None));
        assert!(gate.try_restart().is_none());
    }

    #[cfg(all(windows, feature = "native-calls"))]
    #[test]
    fn shutdown_admission_never_retires_recovery_key_or_credential_owner() {
        let gate = Arc::new(SecurityGate::default());
        assert!(gate.begin());
        assert!(!gate.begin_shutdown(None));
        gate.key_pending();
        assert!(!gate.begin_shutdown(None));
        assert_eq!(gate.0.load(Ordering::Acquire), SECURITY_KEY_PENDING);
        gate.acknowledge_key();
        let account = gate.try_account_access().expect("credential owner");
        assert!(!gate.begin_shutdown(None));
        assert_eq!(gate.0.load(Ordering::Acquire), SECURITY_ACCOUNT_ACCESS);
        drop(account);
        assert!(gate.begin_shutdown(None));
        assert!(gate.blocked());
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

    fn overview(
        recovery: RecoveryStatus,
        backup: BackupStatus,
        keys: Option<u8>,
    ) -> RecoveryOverview {
        RecoveryOverview {
            device: DeviceVerificationStatus::Verified,
            recovery,
            backup,
            backup_on_server: Some(true),
            cross_signing: keys.map(|count| morpheus_matrix::CrossSigningKeys {
                master: count >= 1,
                self_signing: count >= 2,
                user_signing: count >= 3,
            }),
        }
    }

    /// A recovery that returned is described by the state read after it:
    /// complete only when recovery, all three identity keys and the backup
    /// are in place, and even then never "all history".
    #[test]
    fn recovery_outcomes_never_claim_more_than_the_state_shows() {
        let complete = overview(RecoveryStatus::Enabled, BackupStatus::Downloading, Some(3));
        let (notice, error) = recovery_outcome(&Ok(()), Some(&complete));
        assert!(error.is_empty());
        assert!(notice.contains("3 of 3") && notice.contains("never backed up stay unreadable"));
        assert!(!notice.to_lowercase().contains("all "));
        for partial in [
            overview(RecoveryStatus::Incomplete, BackupStatus::Enabled, Some(3)),
            overview(RecoveryStatus::Enabled, BackupStatus::Unknown, Some(3)),
            overview(RecoveryStatus::Enabled, BackupStatus::Enabled, Some(2)),
            overview(RecoveryStatus::Enabled, BackupStatus::Enabled, None),
        ] {
            let (notice, error) = recovery_outcome(&Ok(()), Some(&partial));
            assert!(notice.is_empty(), "{partial:?}");
            assert!(error.contains("not complete"), "{partial:?}");
        }
        let (notice, error) = recovery_outcome(&Ok(()), None);
        assert!(notice.is_empty() && error.contains("could not be read"));
        // A wrong key invites another try; an uncertain outcome points at
        // the state first; neither claims success.
        let (notice, error) =
            recovery_outcome(&Err(AccountRecoveryError::KeyNotAccepted), Some(&complete));
        assert!(notice.is_empty() && error.contains("try again"));
        let (_, error) = recovery_outcome(&Err(AccountRecoveryError::Unconfirmed), None);
        assert!(error.contains("Check the state"));
        let (_, error) = recovery_outcome(
            &Err(AccountRecoveryError::BackupNotRestored),
            Some(&complete),
        );
        assert!(error.contains("Nothing was deleted") && error.contains("3 of 3"));
    }

    #[test]
    fn status_lines_say_what_is_not_known() {
        let mut unknown = overview(RecoveryStatus::Unknown, BackupStatus::Unknown, None);
        unknown.backup_on_server = None;
        let (_, recovery, backup, identity, can_enable) = overview_labels(&unknown);
        assert_eq!(recovery, "Checking");
        assert!(backup.contains("not known") && identity.contains("not known"));
        assert!(!can_enable);
        let disabled = overview(RecoveryStatus::Disabled, BackupStatus::Unknown, Some(0));
        let (_, _, backup, identity, can_enable) = overview_labels(&disabled);
        assert!(can_enable && identity.contains("0 of 3") && backup.contains("exists"));
        let counted = progress_text(RecoveryProgress::BackingUp {
            backed_up: 3,
            total: 9,
        });
        assert!(counted.contains("3 of 9"));
        assert!(progress_text(RecoveryProgress::UploadIncomplete).contains("later"));
    }

    /// A key from a setup that failed while storing it is never presented
    /// as in use, nor as unused: the outcome says it is not known, asks for
    /// the key to be saved first, and warns that a new setup replaces it.
    #[test]
    fn an_unconfirmed_setup_keeps_its_uncertainty_with_the_key() {
        assert_eq!(setup_outcome(SetupConfirmation::Confirmed), None);
        let final_step = setup_outcome(SetupConfirmation::FinalStepFailed).unwrap();
        assert!(final_step.contains("unconfirmed") && final_step.contains("Save the key first"));
        let storage = setup_outcome(SetupConfirmation::StorageUnknown).unwrap();
        assert!(storage.contains("is not known") && storage.contains("Save the key first"));
        assert!(storage.contains("may already use it") && storage.contains("replaces it"));
        for claim in ["was saved", "is in use", "is set up", "nothing"] {
            assert!(!storage.to_lowercase().contains(claim), "{claim}");
        }
        // The key and its outcome stay reserved until acknowledged: no new
        // setup can start meanwhile, and an error path never releases it.
        let gate = SecurityGate::default();
        assert!(gate.begin());
        gate.key_pending();
        gate.reset();
        assert!(!gate.begin(), "no new setup while the key is pending");
        gate.acknowledge_key();
        assert!(gate.begin());
    }

    /// A recovery that failed turning the key backup on, after the identity
    /// came back, is described by the state read afterwards and never says
    /// nothing was imported.
    #[test]
    fn a_late_backup_failure_shows_the_recovered_identity() {
        let identity = overview(RecoveryStatus::Incomplete, BackupStatus::Unknown, Some(3));
        let late = Err(AccountRecoveryError::BackupNotEnabled);
        let (notice, error) = recovery_outcome(&late, Some(&identity));
        assert!(notice.is_empty());
        assert!(error.contains("may have been recovered") && error.contains("3 of 3"));
        assert!(error.contains("Check the state below"));
        assert!(!error.to_lowercase().contains("nothing was imported"));
        let unread = Err(AccountRecoveryError::RecoveryDataUnreadable);
        let (_, error) = recovery_outcome(&unread, None);
        assert!(error.contains("could not be read") && error.contains("Nothing was imported"));
    }

    /// Setup and recovery start only under their own reservation; a pending
    /// key keeps the gate reserved through any error path until the person
    /// acknowledges it, and a newer reservation never admits an older run.
    #[test]
    fn the_recovery_reservation_fences_only_its_own_run() {
        let gate = Arc::new(SecurityGate::default());
        assert!(gate.begin());
        let generation = gate.ringing_generation();
        assert!(gate.recovery_owned(generation));
        gate.key_pending();
        assert!(!gate.recovery_owned(generation));
        gate.reset();
        assert!(gate.blocked(), "an error path never releases a pending key");
        assert_eq!(close_response(&gate), CloseRequestResponse::KeepWindowShown);
        gate.acknowledge_key();
        assert!(!gate.blocked());
        assert!(gate.begin());
        assert!(
            !gate.recovery_owned(generation),
            "an older run is not admitted"
        );
        assert!(gate.recovery_owned(gate.ringing_generation()));
        gate.reset();
        assert!(!gate.blocked());
    }
}
