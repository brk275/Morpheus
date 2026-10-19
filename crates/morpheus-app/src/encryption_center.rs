//! Encryption center: this account's signed-in devices and their trust.
//! Results are fenced by the visible account and a per-view operation, and
//! cleared on every account change. A device row can only start an explicit
//! SAS request through `verification`; nothing here trusts a device itself.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use morpheus_matrix::{AccountManager, DeviceTrust, OwnDeviceInfo, VerificationSnapshot};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use tokio::runtime::Handle;

use crate::{AppWindow, TrustedDeviceItem, live::LiveUi};

const DAY_MS: u64 = 86_400_000;
/// Bound for one wait on a pending request's change streams.
const RETIRE_CHECK: Duration = Duration::from_secs(60);

fn next_operation(ui: &AppWindow) -> i32 {
    let operation = ui.get_encryption_devices_operation().wrapping_add(1);
    ui.set_encryption_devices_operation(operation);
    operation
}

fn same_view(ui: &AppWindow, account_id: &str, operation: i32) -> bool {
    ui.get_live_session()
        && ui.get_active_account_local_id().as_str() == account_id
        && ui.get_encryption_devices_operation() == operation
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn trust_label(trust: DeviceTrust) -> &'static str {
    match trust {
        DeviceTrust::Verified => "Verified",
        DeviceTrust::LocallyTrusted => "Trusted on this device only",
        DeviceTrust::Unverified => "Not verified",
        DeviceTrust::Blocked => "Blocked",
        DeviceTrust::NoKeys => "Encryption keys not available yet",
    }
}

fn activity_label(last_seen_ms: Option<u64>, now_ms: u64) -> String {
    let Some(seen) = last_seen_ms else {
        return "Last activity not reported".into();
    };
    match now_ms.saturating_sub(seen) / DAY_MS {
        0 => "Active in the last day".into(),
        1 => "Last active yesterday".into(),
        days => format!("Last active {days} days ago"),
    }
}

/// Another device may be verified when it is not cross-signed yet, or, when
/// this device is the unverified one, a verified device may vouch for it.
fn may_verify(device: &OwnDeviceInfo, current_verified: bool) -> bool {
    if device.is_current {
        return false;
    }
    match device.trust {
        DeviceTrust::Unverified | DeviceTrust::LocallyTrusted => true,
        DeviceTrust::Verified => !current_verified,
        DeviceTrust::Blocked | DeviceTrust::NoKeys => false,
    }
}

fn device_item(device: &OwnDeviceInfo, current_verified: bool, now_ms: u64) -> TrustedDeviceItem {
    let name = device
        .display_name
        .as_deref()
        .map(|name| crate::directory_plain_text(name, 60))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Unnamed device".into());
    TrustedDeviceItem {
        device_id: device.device_id.as_str().into(),
        shown_id: crate::directory_plain_text(&device.device_id, 64).into(),
        name: name.into(),
        trust: trust_label(device.trust).into(),
        detail: activity_label(device.last_seen_ms, now_ms).into(),
        verified: device.trust == DeviceTrust::Verified,
        current: device.is_current,
        can_verify: may_verify(device, current_verified),
    }
}

fn device_items(devices: &[OwnDeviceInfo], now_ms: u64) -> Vec<TrustedDeviceItem> {
    let current_verified = devices
        .iter()
        .any(|device| device.is_current && device.trust == DeviceTrust::Verified);
    devices
        .iter()
        .map(|device| device_item(device, current_verified, now_ms))
        .collect()
}

/// Forget this account's device list and request notice. Called on every
/// account change, logout and entry into the offline preview.
pub(crate) fn clear_ui(ui: &AppWindow) {
    next_operation(ui);
    let empty: Vec<TrustedDeviceItem> = Vec::new();
    ui.set_encryption_devices(ModelRc::new(VecModel::from(empty)));
    ui.set_encryption_devices_loading(false);
    ui.set_encryption_devices_error("".into());
    ui.set_encryption_incoming_flow_id("".into());
    ui.set_encryption_incoming_dismissed("".into());
}

fn publish_incoming(weak: &Weak<AppWindow>, account_id: &str, flow_id: String) {
    let account_id = account_id.to_owned();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if ui.get_live_session() && ui.get_active_account_local_id().as_str() == account_id {
            ui.set_encryption_incoming_flow_id(flow_id.into());
        }
    });
}

/// Resolves once the SDK reports this request completed, expired or
/// cancelled. Never resolves after an error: the caller's account watch
/// owns that case, so a failing lookup cannot spin.
async fn request_retired(
    manager: &AccountManager,
    account_id: &str,
    user_id: &str,
    flow_id: &str,
) -> bool {
    let mut seen = VerificationSnapshot::default();
    loop {
        let next = manager
            .wait_verification_change_for(account_id, user_id, flow_id, seen, RETIRE_CHECK)
            .await;
        match next {
            Ok(next) if next.finished() => return true,
            Ok(next) => seen = next,
            Err(_) => return std::future::pending().await,
        }
    }
}

/// Point to verification requests from this account's other devices
/// anywhere in the app, not only while Settings polls. Only the newest
/// pending flow ID is shown; reviewing still happens in the SAS panel. The
/// newest request is followed through the SDK's change streams, so one that
/// completes, expires or is cancelled retires the notice at once. Each
/// receiver belongs to one account's client, so resubscribe on every change.
fn watch_incoming_requests(ui: &AppWindow, live: Arc<LiveUi>, handle: &Handle) {
    let weak = ui.as_weak();
    handle.spawn(async move {
        let manager = &live.manager;
        let mut active = manager.subscribe_active_account();
        loop {
            let mut account_changed = false;
            let subscription = manager.subscribe_incoming_verification_requests().await;
            if let Ok((account_id, mut requests)) = subscription {
                // A retired request is never followed again, even if a stale
                // list still names it, so retiring cannot loop.
                let mut retired: Option<String> = None;
                loop {
                    let newest = requests
                        .borrow_and_update()
                        .last()
                        .map(|request| (request.flow_id.clone(), request.other_user_id.clone()));
                    let flow = newest.as_ref().map(|(flow, _)| flow.clone());
                    publish_incoming(&weak, &account_id, flow.clone().unwrap_or_default());
                    let follow = newest.filter(|(flow, _)| retired.as_ref() != Some(flow));
                    let retirement = async {
                        match &follow {
                            Some((flow, user)) => {
                                request_retired(manager, &account_id, user, flow).await
                            }
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        changed = requests.changed() => {
                            if changed.is_err() {
                                break;
                            }
                        }
                        changed = active.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            account_changed = true;
                            break;
                        }
                        _ = retirement => {
                            retired = flow;
                            // Prune the finished request; the list update
                            // wakes this loop and republishes.
                            if manager.account_selection(&account_id).is_some() {
                                let _ = manager.pending_own_verification_requests().await;
                            }
                        }
                    }
                }
            }
            // A closed receiver for the same account would otherwise spin;
            // wait for the next account change before resubscribing.
            if !account_changed && active.changed().await.is_err() {
                return;
            }
        }
    });
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    watch_incoming_requests(ui, live.clone(), &handle);
    let weak = ui.as_weak();
    let refresh_live = live.clone();
    let refresh_handle = handle.clone();
    ui.on_refresh_encryption_devices(move || {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session() || window.get_encryption_devices_loading() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_encryption_devices_loading(true);
        window.set_encryption_devices_error("".into());
        let live = refresh_live.clone();
        let weak = weak.clone();
        refresh_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            let result = live.manager.own_devices_for(&account_id).await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_view(&window, &account_id, operation) {
                    return;
                }
                window.set_encryption_devices_loading(false);
                match result {
                    Ok(devices) => {
                        let items = device_items(&devices, now_ms());
                        window.set_encryption_devices(ModelRc::new(VecModel::from(items)));
                    }
                    Err(_) => window.set_encryption_devices_error(
                        "Could not load this account's devices. Try Refresh.".into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    ui.on_verify_encryption_device(move |device_id| {
        let Some(window) = weak.upgrade() else { return };
        // Only a verifiable row from the list loaded for this account can
        // start a request; a stale or forged ID does nothing.
        let devices = window.get_encryption_devices();
        let Some(item) = devices
            .iter()
            .find(|item| item.device_id == device_id && item.can_verify)
        else {
            return;
        };
        crate::verification::request_device(
            &window,
            live.clone(),
            &handle,
            item.device_id.to_string(),
            item.name.to_string(),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, current: bool, trust: DeviceTrust) -> OwnDeviceInfo {
        OwnDeviceInfo {
            device_id: id.into(),
            display_name: Some(format!("{id}\u{202e}name")),
            is_current: current,
            trust,
            last_seen_ms: None,
        }
    }

    #[test]
    fn only_other_devices_that_need_or_can_give_trust_offer_verify() {
        let verified_here = [
            device("SELF", true, DeviceTrust::Verified),
            device("NEW", false, DeviceTrust::Unverified),
            device("OK", false, DeviceTrust::Verified),
            device("BLOCKED", false, DeviceTrust::Blocked),
            device("NOKEYS", false, DeviceTrust::NoKeys),
        ];
        let offers: Vec<_> = device_items(&verified_here, 0)
            .into_iter()
            .map(|item| (item.device_id.to_string(), item.can_verify))
            .collect();
        assert_eq!(
            offers,
            [
                ("SELF".to_owned(), false),
                ("NEW".to_owned(), true),
                ("OK".to_owned(), false),
                ("BLOCKED".to_owned(), false),
                ("NOKEYS".to_owned(), false),
            ]
        );
        // When this device is the unverified one, a verified device can vouch.
        let unverified_here = [
            device("SELF", true, DeviceTrust::Unverified),
            device("OK", false, DeviceTrust::Verified),
        ];
        let items = device_items(&unverified_here, 0);
        assert!(!items[0].can_verify && items[1].can_verify);
        assert_eq!(items[1].name, "OK name");
    }

    #[test]
    fn activity_is_reported_in_whole_days_or_as_unknown() {
        for (seen, now, label) in [
            (None, 10, "Last activity not reported"),
            (Some(5), DAY_MS - 1, "Active in the last day"),
            (Some(0), DAY_MS, "Last active yesterday"),
            (Some(0), 3 * DAY_MS, "Last active 3 days ago"),
            (Some(DAY_MS), 0, "Active in the last day"),
        ] {
            assert_eq!(activity_label(seen, now), label);
        }
    }
}
