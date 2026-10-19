//! Rename and sign out this account's devices from the encryption center.
//! Each action is bound to the account selection it started under and to a
//! per-view operation, so a late result after an account change (A → B → A
//! included) is dropped. A password is taken only from explicit input, held
//! in Zeroizing memory and cleared on submit, cancel, hide and every account
//! change. This device is never signed out here: its row hands over to the
//! normal guarded logout.

use std::sync::{Arc, Mutex, MutexGuard};

use morpheus_matrix::{
    AccountSelection, DeviceSignOutBlocker, DeviceSignOutChallenge, DeviceSignOutStep,
    MatrixCoreError, bounded_device_name,
};
use slint::{ComponentHandle, Model};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{AppWindow, TrustedDeviceItem, live::LiveUi};

const CLOSED: i32 = 0;
const CONFIRM: i32 = 1;
const WORKING: i32 = 2;
const PASSWORD: i32 = 3;
const BROWSER: i32 = 4;
const ACCOUNT_PAGE: i32 = 5;
const BLOCKED: i32 = 6;
const THIS_DEVICE: i32 = 7;

type StepResult = morpheus_matrix::Result<DeviceSignOutStep>;

/// The one remote sign-out in progress. The UIA challenge and page URL stay
/// here, in Rust; neither is ever shown or logged.
struct PendingSignOut {
    operation: i32,
    selection: AccountSelection,
    device_id: String,
    name: String,
    challenge: Option<DeviceSignOutChallenge>,
    page: Option<String>,
}

static PENDING: Mutex<Option<PendingSignOut>> = Mutex::new(None);

fn pending() -> MutexGuard<'static, Option<PendingSignOut>> {
    PENDING.lock().unwrap_or_else(|error| error.into_inner())
}

fn next_operation(ui: &AppWindow) -> i32 {
    let operation = ui.get_device_session_operation().wrapping_add(1);
    ui.set_device_session_operation(operation);
    operation
}

fn same_view(ui: &AppWindow, account_id: &str, operation: i32) -> bool {
    ui.get_live_session()
        && ui.get_active_account_local_id().as_str() == account_id
        && ui.get_device_session_operation() == operation
}

/// A row of the device list loaded for the visible account.
fn device_row(ui: &AppWindow, device_id: &str) -> Option<TrustedDeviceItem> {
    ui.get_encryption_devices()
        .iter()
        .find(|item| item.device_id.as_str() == device_id)
}

fn blocker_message(blocker: DeviceSignOutBlocker) -> &'static str {
    match blocker {
        DeviceSignOutBlocker::NoAccountManagementPage => {
            "Your account provider offers no page for managing devices. Sign the device out on that device or on your provider's website."
        }
        DeviceSignOutBlocker::UnsupportedAuthentication => {
            "Your homeserver asks for a confirmation Morpheus does not support here. Sign the device out on that device or in another app."
        }
        DeviceSignOutBlocker::InsecurePage => {
            "The confirmation page is not secure, so Morpheus will not open it."
        }
    }
}

fn close_signout(ui: &AppWindow) {
    ui.set_device_signout_stage(CLOSED);
    ui.set_device_signout_name("".into());
    ui.set_device_signout_shown_id("".into());
    ui.set_device_signout_password("".into());
    ui.set_device_signout_message("".into());
    ui.set_device_signout_page_opened(false);
}

/// Forget every rename and sign-out of the previous account view.
pub(crate) fn clear_ui(ui: &AppWindow) {
    pending().take();
    next_operation(ui);
    ui.set_device_rename_id("".into());
    ui.set_device_rename_input("".into());
    ui.set_device_rename_busy(false);
    ui.set_device_session_notice("".into());
    close_signout(ui);
}

/// Drop a typed password without ending the flow, e.g. when the window hides.
pub(crate) fn clear_secret(ui: &AppWindow) {
    ui.set_device_signout_password("".into());
}

/// Apply one server step to the sign-out started by `operation`, if it is
/// still the visible one and its account selection is still current.
fn apply_step(window: &AppWindow, operation: i32, step: StepResult) {
    let mut guard = pending();
    let Some(current) = guard.as_mut() else {
        return;
    };
    if current.operation != operation {
        return;
    }
    if !current.selection.is_current() || window.get_device_session_operation() != operation {
        guard.take();
        drop(guard);
        close_signout(window);
        return;
    }
    let had_page = current.page.is_some();
    window.set_device_signout_password("".into());
    window.set_device_signout_message("".into());
    window.set_device_signout_page_opened(false);
    match step {
        Ok(DeviceSignOutStep::Done) => {
            let name = current.name.clone();
            guard.take();
            drop(guard);
            close_signout(window);
            window.set_device_session_notice(format!("Signed out {name}.").into());
            window.invoke_refresh_encryption_devices();
        }
        Ok(DeviceSignOutStep::Password(challenge)) => {
            if challenge.rejected() {
                window.set_device_signout_message("That password was not accepted.".into());
            }
            current.challenge = Some(challenge);
            current.page = None;
            window.set_device_signout_stage(PASSWORD);
        }
        Ok(DeviceSignOutStep::Browser { challenge, url }) => {
            if had_page {
                window.set_device_signout_message(
                    "Not confirmed yet. Finish the page in your browser, then continue.".into(),
                );
            }
            current.challenge = Some(challenge);
            current.page = Some(url.into());
            window.set_device_signout_stage(BROWSER);
        }
        Ok(DeviceSignOutStep::AccountManagement { url }) => {
            current.challenge = None;
            current.page = Some(url.into());
            window.set_device_signout_stage(ACCOUNT_PAGE);
        }
        Ok(DeviceSignOutStep::Unsupported(blocker)) => {
            current.challenge = None;
            current.page = None;
            window.set_device_signout_message(blocker_message(blocker).into());
            window.set_device_signout_stage(BLOCKED);
        }
        Err(MatrixCoreError::SelectedAccountChanged) => {
            guard.take();
            drop(guard);
            close_signout(window);
        }
        Err(_) => {
            current.challenge = None;
            current.page = None;
            window.set_device_signout_message(
                "The server did not sign out that device. Check your connection and try again."
                    .into(),
            );
            window.set_device_signout_stage(CONFIRM);
        }
    }
}

/// Take the stored challenge of the visible sign-out, if any.
fn take_challenge(operation: i32) -> Option<DeviceSignOutChallenge> {
    pending()
        .as_mut()
        .filter(|current| current.operation == operation)
        .and_then(|current| current.challenge.take())
}

fn spawn_step<F>(window: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, operation: i32, step: F)
where
    F: std::future::Future<Output = StepResult> + Send + 'static,
{
    window.set_device_signout_message("".into());
    window.set_device_signout_stage(WORKING);
    let live = live.clone();
    let weak = window.as_weak();
    handle.spawn(async move {
        let _transition = live.account_transition.lock().await;
        let step = step.await;
        let _ = weak.upgrade_in_event_loop(move |window| apply_step(&window, operation, step));
    });
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    let open_live = live.clone();
    ui.on_open_device_signout(move |device_id| {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || window.get_device_signout_stage() != CLOSED
            || window.get_device_rename_busy()
        {
            return;
        }
        let Some(row) = device_row(&window, device_id.as_str()) else {
            return;
        };
        let account_id = window.get_active_account_local_id().to_string();
        let operation = next_operation(&window);
        pending().take();
        window.set_device_rename_id("".into());
        window.set_device_rename_input("".into());
        window.set_device_session_notice("".into());
        window.set_device_signout_name(row.name.clone());
        window.set_device_signout_shown_id(row.shown_id.clone());
        window.set_device_signout_message("".into());
        if row.current {
            // This device leaves only through the normal guarded logout.
            window.set_device_signout_stage(THIS_DEVICE);
            return;
        }
        let Some(selection) = open_live.account_selection(&account_id) else {
            window.set_device_signout_message("The account changed. Refresh the list.".into());
            window.set_device_signout_stage(BLOCKED);
            return;
        };
        *pending() = Some(PendingSignOut {
            operation,
            selection,
            device_id: row.device_id.to_string(),
            name: row.name.to_string(),
            challenge: None,
            page: None,
        });
        window.set_device_signout_stage(CONFIRM);
    });

    let weak = ui.as_weak();
    let confirm_live = live.clone();
    let confirm_handle = handle.clone();
    ui.on_confirm_device_signout(move || {
        let Some(window) = weak.upgrade() else { return };
        if window.get_device_signout_stage() != CONFIRM {
            return;
        }
        if confirm_live.security_gate.blocked() {
            window.set_device_signout_message("Wait for the security operation to finish.".into());
            return;
        }
        let operation = window.get_device_session_operation();
        let target = pending()
            .as_ref()
            .filter(|current| current.operation == operation)
            .map(|current| (current.selection.clone(), current.device_id.clone()));
        let Some((selection, id)) = target else {
            return;
        };
        let manager = confirm_live.manager.clone();
        let step = async move { manager.begin_device_sign_out_for(&selection, &id).await };
        spawn_step(&window, &confirm_live, &confirm_handle, operation, step);
    });

    let weak = ui.as_weak();
    let password_live = live.clone();
    let password_handle = handle.clone();
    ui.on_submit_device_signout_password(move |value| {
        let Some(window) = weak.upgrade() else { return };
        // The field clears itself before calling; clear it again for callers
        // outside that widget.
        window.set_device_signout_password("".into());
        let secret = Zeroizing::new(value.to_string());
        if window.get_device_signout_stage() != PASSWORD || secret.is_empty() {
            return;
        }
        if password_live.security_gate.blocked() {
            window.set_device_signout_message("Wait for the security operation to finish.".into());
            return;
        }
        let operation = window.get_device_session_operation();
        let Some(challenge) = take_challenge(operation) else {
            return;
        };
        let manager = password_live.manager.clone();
        let step = async move {
            manager
                .sign_out_device_with_password_for(challenge, secret.as_str())
                .await
        };
        spawn_step(&window, &password_live, &password_handle, operation, step);
    });

    let weak = ui.as_weak();
    ui.on_open_device_signout_page(move || {
        let Some(window) = weak.upgrade() else { return };
        let stage = window.get_device_signout_stage();
        if stage != BROWSER && stage != ACCOUNT_PAGE {
            return;
        }
        let operation = window.get_device_session_operation();
        let page = pending()
            .as_ref()
            .filter(|current| current.operation == operation)
            .and_then(|current| current.page.clone());
        let Some(page) = page else {
            return;
        };
        if webbrowser::open(&page).is_ok() {
            window.set_device_signout_page_opened(true);
            window.set_device_signout_message("".into());
        } else {
            window.set_device_signout_message("Could not open your web browser.".into());
        }
    });

    let weak = ui.as_weak();
    let continue_live = live.clone();
    let continue_handle = handle.clone();
    ui.on_continue_device_signout(move || {
        let Some(window) = weak.upgrade() else { return };
        let stage = window.get_device_signout_stage();
        if stage == ACCOUNT_PAGE {
            // The provider's page is external; only the refreshed list can
            // say whether the device is gone.
            pending().take();
            next_operation(&window);
            close_signout(&window);
            window.set_device_session_notice(
                "Refreshing the list to show the result of your provider's page.".into(),
            );
            window.invoke_refresh_encryption_devices();
            return;
        }
        if stage != BROWSER || !window.get_device_signout_page_opened() {
            return;
        }
        let operation = window.get_device_session_operation();
        let Some(challenge) = take_challenge(operation) else {
            return;
        };
        let manager = continue_live.manager.clone();
        let step = async move { manager.sign_out_device_after_browser_for(challenge).await };
        spawn_step(&window, &continue_live, &continue_handle, operation, step);
    });

    let weak = ui.as_weak();
    ui.on_cancel_device_signout(move || {
        let Some(window) = weak.upgrade() else { return };
        let stage = window.get_device_signout_stage();
        if stage == CLOSED {
            return;
        }
        let was_working = stage == WORKING;
        pending().take();
        next_operation(&window);
        close_signout(&window);
        if was_working {
            // The request may still reach the server; never guess its result.
            window.set_device_session_notice(
                "Stopped waiting. Refresh the list to see whether the device was signed out."
                    .into(),
            );
        }
    });

    let weak = ui.as_weak();
    ui.on_start_device_rename(move |device_id| {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || window.get_device_rename_busy()
            || window.get_device_signout_stage() != CLOSED
        {
            return;
        }
        let Some(row) = device_row(&window, device_id.as_str()) else {
            return;
        };
        window.set_device_session_notice("".into());
        window.set_encryption_devices_error("".into());
        window.set_device_rename_input(row.name.clone());
        window.set_device_rename_id(row.device_id);
    });

    let weak = ui.as_weak();
    ui.on_cancel_device_rename(move || {
        if let Some(window) = weak.upgrade()
            && !window.get_device_rename_busy()
        {
            window.set_device_rename_id("".into());
            window.set_device_rename_input("".into());
        }
    });

    let weak = ui.as_weak();
    ui.on_save_device_rename(move |device_id, name| {
        let Some(window) = weak.upgrade() else { return };
        if window.get_device_rename_busy()
            || window.get_device_signout_stage() != CLOSED
            || window.get_device_rename_id() != device_id
            || device_row(&window, device_id.as_str()).is_none()
        {
            return;
        }
        let Some(name) = bounded_device_name(name.as_str()) else {
            window.set_encryption_devices_error("Enter a device name.".into());
            return;
        };
        let account_id = window.get_active_account_local_id().to_string();
        let Some(selection) = live.account_selection(&account_id) else {
            return;
        };
        let operation = next_operation(&window);
        window.set_device_rename_busy(true);
        window.set_encryption_devices_error("".into());
        let live = live.clone();
        let weak = weak.clone();
        handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            let result = live
                .manager
                .rename_device_for(&selection, device_id.as_str(), &name)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_view(&window, &account_id, operation) {
                    return;
                }
                window.set_device_rename_busy(false);
                match result {
                    Ok(()) => {
                        window.set_device_rename_id("".into());
                        window.set_device_rename_input("".into());
                        window.set_device_session_notice(format!("Renamed to {name}.").into());
                        window.invoke_refresh_encryption_devices();
                    }
                    Err(_) => window.set_encryption_devices_error(
                        "Could not rename the device. Try again.".into(),
                    ),
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_blocker_says_what_to_do_instead_and_never_claims_success() {
        for blocker in [
            DeviceSignOutBlocker::NoAccountManagementPage,
            DeviceSignOutBlocker::UnsupportedAuthentication,
            DeviceSignOutBlocker::InsecurePage,
        ] {
            let message = blocker_message(blocker);
            assert!(!message.is_empty());
            assert!(!message.contains("Signed out") && !message.contains("signed out"));
        }
    }
}
