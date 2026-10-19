//! Own-device SAS verification in Settings. The UI never confirms a SAS flow
//! until the person checks the values on both devices and opts in explicitly.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use morpheus_matrix::{
    DeviceVerificationStatus, MatrixCoreError, SasPresentation, VerificationCancel,
    VerificationCancelKind, VerificationInfo, VerificationSnapshot,
};
use slint::ComponentHandle;
use tokio::runtime::Handle;

use crate::{AppWindow, live::LiveUi};

const IDLE: i32 = 0;
const WAITING_FOR_REQUEST: i32 = 1;
const READY_TO_START: i32 = 2;
const WAITING_FOR_VALUES: i32 = 3;
const COMPARING: i32 = 4;
const WAITING_FOR_COMPLETION: i32 = 5;
/// Bound for one wait on the SDK's change streams; a quiet flow is still
/// checked this often as a fallback.
const FLOW_FALLBACK: Duration = Duration::from_secs(30);
const ENDED_NOTICE: &str = "Verification ended. Check this device's status above.";
const ACCEPTED_NOTICE: &str =
    "Request accepted. Waiting for the requesting device to start comparison.";

/// The one SDK-driven watcher of the visible flow. A new flow replaces it and
/// every clear of the flow UI aborts it, so no late update outlives its flow.
static FLOW_WATCHER: Mutex<Option<tokio::task::AbortHandle>> = Mutex::new(None);

fn stop_flow_watcher() {
    let previous = FLOW_WATCHER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(previous) = previous {
        previous.abort();
    }
}

/// Follow one flow through the SDK's request and SAS change streams while it
/// is shown, so progress, cancellation and expiry arrive without polling and
/// while Settings is closed. Comparison and confirmation stay explicit.
fn start_flow_watcher(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    account_id: String,
    user_id: String,
    flow_id: String,
) {
    let weak = window.as_weak();
    let watch_live = live.clone();
    let refresh_handle = handle.clone();
    let task = handle.spawn(async move {
        let mut seen = VerificationSnapshot::default();
        loop {
            let next = watch_live
                .manager
                .wait_verification_change_for(
                    &account_id,
                    &user_id,
                    &flow_id,
                    seen,
                    FLOW_FALLBACK,
                )
                .await;
            // An account change ends this watcher; it also clears the flow UI.
            let Ok(next) = next else { break };
            seen = next;
            let finished = next.finished();
            let live = watch_live.clone();
            let handle = refresh_handle.clone();
            let account = account_id.clone();
            let flow = flow_id.clone();
            let _ = weak.upgrade_in_event_loop(move |window| {
                if window.get_active_account_local_id().as_str() != account
                    || window.get_sas_flow_id().as_str() != flow
                {
                    return;
                }
                let finished = finished.then_some(flow.as_str());
                refresh_flow(&window, &live, &handle, finished);
            });
            if finished {
                break;
            }
        }
    });
    let previous = FLOW_WATCHER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .replace(task.abort_handle());
    if let Some(previous) = previous {
        previous.abort();
    }
}

/// Show a flow that this device requested or accepted and start following it.
fn begin_flow(
    window: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    info: VerificationInfo,
    notice: String,
) {
    let account_id = window.get_active_account_local_id().to_string();
    window.set_sas_flow_id(info.flow_id.as_str().into());
    window.set_sas_other_user_id(info.other_user_id.as_str().into());
    window.set_sas_request_initiator(info.we_started);
    window.set_sas_stage(WAITING_FOR_REQUEST);
    window.set_sas_error("".into());
    window.set_sas_notice(notice.into());
    start_flow_watcher(
        window,
        live,
        handle,
        account_id,
        info.other_user_id,
        info.flow_id,
    );
}

fn next_operation(ui: &AppWindow) -> i32 {
    let next = ui.get_sas_operation().wrapping_add(1);
    ui.set_sas_operation(next);
    next
}

fn same_account(ui: &AppWindow, account_id: &str, operation: i32) -> bool {
    ui.get_live_session()
        && ui.get_active_account_local_id().as_str() == account_id
        && ui.get_sas_operation() == operation
}

fn same_flow(ui: &AppWindow, account_id: &str, operation: i32, flow_id: &str) -> bool {
    same_account(ui, account_id, operation) && ui.get_sas_flow_id().as_str() == flow_id
}

async fn active_account_is(live: &LiveUi, account_id: &str) -> bool {
    live.manager
        .active_account()
        .await
        .is_some_and(|account| account.id == account_id)
}

fn plain_label(value: &str, limit: usize) -> String {
    value
        .chars()
        .take(limit)
        .map(|ch| {
            if ch.is_control()
                || matches!(
                    ch,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
            {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn comparison_labels(presentation: SasPresentation) -> Option<(String, String)> {
    let emoji = presentation
        .emoji
        .unwrap_or_default()
        .into_iter()
        .take(7)
        .map(|item| {
            format!(
                "{} {}",
                plain_label(&item.symbol, 4),
                plain_label(&item.description, 32)
            )
        })
        .collect::<Vec<_>>()
        .join("  ·  ");
    let decimals = presentation.decimals.map_or_else(String::new, |values| {
        format!("{:04} · {:04} · {:04}", values[0], values[1], values[2])
    });
    if emoji.is_empty() && decimals.is_empty() {
        None
    } else {
        Some((emoji, decimals))
    }
}

/// Why a flow stopped, for the person who has to decide whether to retry.
/// Every stop leaves trust unchanged.
fn cancel_message(cancel: Option<VerificationCancel>) -> &'static str {
    let Some(cancel) = cancel else {
        return "Verification was canceled. Nothing was trusted.";
    };
    match (cancel.kind, cancel.by_us) {
        (VerificationCancelKind::Timeout, _) => {
            "Verification timed out before both devices finished. Nothing was trusted."
        }
        (VerificationCancelKind::Mismatch, false) => {
            "The other device reported that the values differ. Nothing was trusted."
        }
        (VerificationCancelKind::Mismatch, true) => {
            "Verification stopped because the values differ. Nothing was trusted."
        }
        (VerificationCancelKind::User, false) => "The other device canceled verification.",
        (VerificationCancelKind::User, true) => "Verification was canceled on this device.",
        (VerificationCancelKind::AcceptedElsewhere, _) => {
            "Another of your devices answered this request."
        }
        (VerificationCancelKind::Other, _) => "Verification was canceled. Nothing was trusted.",
    }
}

fn may_confirm(stage: i32, compared: bool, busy: bool, recovery_blocked: bool) -> bool {
    stage == COMPARING && compared && !busy && !recovery_blocked
}

fn request_was_pruned(result: &morpheus_matrix::Result<VerificationInfo>) -> bool {
    matches!(
        result,
        Err(MatrixCoreError::Sdk(message)) if message == "verification request was not found"
    )
}

fn verified_after_pruned_completion(
    stage: i32,
    result: &morpheus_matrix::Result<VerificationInfo>,
    device_status: Option<DeviceVerificationStatus>,
) -> bool {
    stage == WAITING_FOR_COMPLETION
        && request_was_pruned(result)
        && device_status == Some(DeviceVerificationStatus::Verified)
}

/// Invalidate all late callbacks before changing the visible Matrix account.
pub(crate) fn clear_ui(ui: &AppWindow) {
    stop_flow_watcher();
    next_operation(ui);
    ui.set_sas_stage(IDLE);
    ui.set_sas_busy(false);
    ui.set_sas_incoming_loading(false);
    ui.set_sas_incoming_flow_id("".into());
    ui.set_sas_incoming_user_id("".into());
    ui.set_sas_compared(false);
    ui.set_sas_request_initiator(false);
    ui.set_sas_flow_id("".into());
    ui.set_sas_other_user_id("".into());
    ui.set_sas_emoji("".into());
    ui.set_sas_decimals("".into());
    ui.set_sas_error("".into());
    ui.set_sas_notice("".into());
}

/// Start an outgoing SAS request to one other device of this account, chosen
/// from the encryption center's device list. The flow then continues like a
/// request from this device: nothing is trusted until both people compare
/// the values and confirm on both devices.
pub(crate) fn request_device(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    handle: &Handle,
    device_id: String,
    device_name: String,
) {
    if !ui.get_live_session()
        || ui.get_sas_stage() != IDLE
        || ui.get_sas_busy()
        || ui.get_security_busy()
        || !ui.get_security_recovery_key().is_empty()
        || live.security_gate.blocked()
    {
        return;
    }
    let account_id = ui.get_active_account_local_id().to_string();
    if account_id.is_empty() || device_id.is_empty() {
        return;
    }
    let operation = next_operation(ui);
    ui.set_sas_busy(true);
    ui.set_sas_incoming_loading(false);
    ui.set_sas_error("".into());
    ui.set_sas_notice(format!("Sending a verification request to {device_name}…").into());
    let weak = ui.as_weak();
    let flow_live = live.clone();
    let flow_handle = handle.clone();
    handle.spawn(async move {
        let _transition = live.account_transition.lock().await;
        let result = live
            .manager
            .request_device_verification_for(&account_id, &device_id)
            .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            if !same_account(&window, &account_id, operation) {
                return;
            }
            window.set_sas_busy(false);
            match result {
                Ok(info) => {
                    let notice =
                        format!("Approve the request on {device_name}, then compare the values.");
                    begin_flow(&window, &flow_live, &flow_handle, info, notice);
                }
                Err(_) => {
                    window.set_sas_notice("".into());
                    window.set_sas_error(
                        "Could not send a request to that device. Check that it is signed in, then retry."
                            .into(),
                    );
                }
            }
        });
    });
}

/// Check the visible flow's progress. The watcher passes the flow it saw
/// finish: that check runs even while another action is pending, because a
/// finished flow must retire at once; every other check waits for pending
/// actions so it never overtakes an explicit comparison or confirmation.
fn refresh_flow(window: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, finished: Option<&str>) {
    let stage = window.get_sas_stage();
    let allowed = match finished {
        Some(flow_id) => stage != IDLE && window.get_sas_flow_id().as_str() == flow_id,
        None => {
            !window.get_sas_busy()
                && !live.security_gate.blocked()
                && matches!(
                    stage,
                    WAITING_FOR_REQUEST | WAITING_FOR_VALUES | WAITING_FOR_COMPLETION
                )
        }
    };
    if !window.get_live_session() || !allowed {
        return;
    }
    let retiring = finished.is_some();
    let account_id = window.get_active_account_local_id().to_string();
    let user_id = window.get_sas_other_user_id().to_string();
    let flow_id = window.get_sas_flow_id().to_string();
    if account_id.is_empty() || user_id.is_empty() || flow_id.is_empty() {
        return;
    }
    let operation = next_operation(window);
    window.set_sas_busy(true);
    let live = live.clone();
    let weak = window.as_weak();
    handle.spawn(async move {
        let _transition = live.account_transition.lock().await;
        if !active_account_is(&live, &account_id).await {
            return;
        }
        let outcome = live
            .manager
            .verification_outcome_for(&account_id, &user_id, &flow_id)
            .await;
        let cancel = outcome.as_ref().ok().and_then(|outcome| outcome.cancel);
        let info = outcome.map(|outcome| outcome.info);
        // Only the device that sent the request starts SAS. After the
        // responder accepted that request, it waits for the incoming SAS
        // start and accepts the method without confirming the values.
        let incoming_sas = if stage == WAITING_FOR_REQUEST
            && info
                .as_ref()
                .is_ok_and(|info| !info.we_started && !info.is_cancelled && !info.is_done)
        {
            Some(
                live.manager
                    .accept_sas_if_available(&user_id, &flow_id)
                    .await,
            )
        } else {
            None
        };
        let values = if stage == WAITING_FOR_VALUES
            && info
                .as_ref()
                .is_ok_and(|info| !info.is_cancelled && !info.is_done)
        {
            Some(live.manager.sas_presentation(&user_id, &flow_id).await)
        } else {
            None
        };
        // The SDK can remove a finished request before this poll sees
        // `is_done`. In that case, trust status is the durable result.
        let device_status = if stage == WAITING_FOR_COMPLETION && request_was_pruned(&info) {
            live.manager
                .security_info()
                .await
                .ok()
                .map(|status| status.device_verification)
        } else {
            None
        };
        let pruned_and_verified = verified_after_pruned_completion(stage, &info, device_status);
        let _ = weak.upgrade_in_event_loop(move |window| {
            if !same_flow(&window, &account_id, operation, &flow_id) {
                return;
            }
            window.set_sas_busy(false);
            match info {
                Ok(info) if info.is_cancelled => {
                    clear_ui(&window);
                    window.set_sas_error(cancel_message(cancel).into());
                }
                Ok(info) if info.is_done => {
                    clear_ui(&window);
                    window.set_sas_notice("Device verification completed.".into());
                    window.invoke_refresh_security();
                    window.invoke_refresh_encryption_devices();
                }
                Err(_) if pruned_and_verified => {
                    clear_ui(&window);
                    window.set_sas_notice("This device is Verified.".into());
                    window.invoke_refresh_security();
                    window.invoke_refresh_encryption_devices();
                }
                Ok(info) if stage == WAITING_FOR_REQUEST && info.we_started && info.is_ready => {
                    window.set_sas_stage(READY_TO_START);
                    window
                        .set_sas_notice("The other device accepted. Start comparison.".into());
                    window.set_sas_error("".into());
                }
                Ok(info) if stage == WAITING_FOR_REQUEST && !info.we_started => {
                    match incoming_sas {
                        Some(Ok(true)) => {
                            window.set_sas_stage(WAITING_FOR_VALUES);
                            window.set_sas_notice(
                                "Comparison started. Waiting for values on both devices.".into(),
                            );
                            window.set_sas_error("".into());
                        }
                        Some(Err(_)) => window.set_sas_error(
                            "Could not accept comparison. Keep both devices open and retry."
                                .into(),
                        ),
                        _ => {}
                    }
                }
                Ok(_) if stage == WAITING_FOR_VALUES => match values {
                    Some(Ok(Some(presentation))) => {
                        if let Some((emoji, decimals)) = comparison_labels(presentation) {
                            window.set_sas_emoji(emoji.into());
                            window.set_sas_decimals(decimals.into());
                            window.set_sas_compared(false);
                            window.set_sas_stage(COMPARING);
                            window.set_sas_notice("".into());
                            window.set_sas_error("".into());
                        }
                    }
                    Some(Err(_)) => window.set_sas_error(
                        "Comparison values are not available yet. Keep both devices open.".into(),
                    ),
                    _ => {}
                },
                Err(_) if retiring => {
                    // The watcher saw this flow end and the SDK forgot it.
                    clear_ui(&window);
                    window.set_sas_error(ENDED_NOTICE.into());
                    window.invoke_refresh_security();
                    window.invoke_refresh_encryption_devices();
                }
                Ok(_) => window.set_sas_error("".into()),
                Err(_) => window.set_sas_error(
                    "Could not check verification status. Try Check status again.".into(),
                ),
            }
        });
    });
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    let incoming_live = live.clone();
    let incoming_handle = handle.clone();
    ui.on_refresh_incoming_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || window.get_sas_stage() != IDLE
            || window.get_sas_busy()
            || window.get_sas_incoming_loading()
            || incoming_live.security_gate.blocked()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_sas_incoming_loading(true);
        let live = incoming_live.clone();
        let weak = weak.clone();
        incoming_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live.manager.pending_own_verification_requests().await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_account(&window, &account_id, operation) || window.get_sas_stage() != IDLE
                {
                    return;
                }
                window.set_sas_incoming_loading(false);
                if let Ok(requests) = result {
                    if let Some(request) = requests.into_iter().next() {
                        window.set_sas_incoming_flow_id(request.flow_id.into());
                        window.set_sas_incoming_user_id(request.other_user_id.into());
                    } else {
                        window.set_sas_incoming_flow_id("".into());
                        window.set_sas_incoming_user_id("".into());
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let accept_live = live.clone();
    let accept_handle = handle.clone();
    ui.on_accept_incoming_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || window.get_sas_stage() != IDLE
            || window.get_sas_busy()
            || accept_live.security_gate.blocked()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let user_id = window.get_sas_incoming_user_id().to_string();
        let flow_id = window.get_sas_incoming_flow_id().to_string();
        if account_id.is_empty() || user_id.is_empty() || flow_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_incoming_loading(false);
        window.set_sas_error("".into());
        let live = accept_live.clone();
        let weak = weak.clone();
        let flow_live = accept_live.clone();
        let flow_handle = accept_handle.clone();
        accept_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live
                .manager
                .accept_verification_request(&user_id, &flow_id)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_account(&window, &account_id, operation)
                    || window.get_sas_incoming_flow_id().as_str() != flow_id
                {
                    return;
                }
                window.set_sas_busy(false);
                match result {
                    Ok(()) => {
                        window.set_sas_incoming_flow_id("".into());
                        window.set_sas_incoming_user_id("".into());
                        let info = VerificationInfo {
                            flow_id,
                            other_user_id: user_id,
                            we_started: false,
                            is_ready: true,
                            is_done: false,
                            is_cancelled: false,
                        };
                        let notice = ACCEPTED_NOTICE.to_owned();
                        begin_flow(&window, &flow_live, &flow_handle, info, notice);
                    }
                    Err(_) => window.set_sas_error(
                        "Could not accept this request. It may have expired; refresh Settings."
                            .into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let request_live = live.clone();
    let request_handle = handle.clone();
    ui.on_request_own_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || window.get_sas_stage() != IDLE
            || window.get_sas_busy()
            || request_live.security_gate.blocked()
            || window.get_security_device_status().as_str() != "Unverified"
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_incoming_loading(false);
        window.set_sas_error("".into());
        window.set_sas_notice("Sending a request to your other device…".into());
        let live = request_live.clone();
        let weak = weak.clone();
        let flow_live = request_live.clone();
        let flow_handle = request_handle.clone();
        request_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live.manager.request_own_verification().await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_account(&window, &account_id, operation) {
                    return;
                }
                window.set_sas_busy(false);
                match result {
                    Ok(info) => {
                        let notice = "Approve this request on your other verified device.";
                        begin_flow(&window, &flow_live, &flow_handle, info, notice.into());
                    }
                    Err(_) => {
                        window.set_sas_notice("".into());
                        window.set_sas_error(
                            "Could not request verification. Check that another verified device is signed in, then retry."
                                .into(),
                        );
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let refresh_live = live.clone();
    let refresh_handle = handle.clone();
    ui.on_refresh_sas_verification(move || {
        if let Some(window) = weak.upgrade() {
            refresh_flow(&window, &refresh_live, &refresh_handle, None);
        }
    });

    let weak = ui.as_weak();
    let start_live = live.clone();
    let start_handle = handle.clone();
    ui.on_start_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || window.get_sas_busy()
            || window.get_sas_stage() != READY_TO_START
            || !window.get_sas_request_initiator()
            || start_live.security_gate.blocked()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let user_id = window.get_sas_other_user_id().to_string();
        let flow_id = window.get_sas_flow_id().to_string();
        if account_id.is_empty() || user_id.is_empty() || flow_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_error("".into());
        let live = start_live.clone();
        let weak = weak.clone();
        start_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live
                .manager
                .start_sas_verification(&user_id, &flow_id)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_flow(&window, &account_id, operation, &flow_id) {
                    return;
                }
                window.set_sas_busy(false);
                match result {
                    Ok(()) => {
                        window.set_sas_stage(WAITING_FOR_VALUES);
                        window.set_sas_notice(
                            "Waiting for the other device to show comparison values.".into(),
                        );
                    }
                    Err(_) => window.set_sas_error(
                        "Could not start SAS comparison. Keep both devices open and retry.".into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let confirm_live = live.clone();
    let confirm_handle = handle.clone();
    ui.on_confirm_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_live_session()
            || !may_confirm(
                window.get_sas_stage(),
                window.get_sas_compared(),
                window.get_sas_busy(),
                confirm_live.security_gate.blocked(),
            )
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let user_id = window.get_sas_other_user_id().to_string();
        let flow_id = window.get_sas_flow_id().to_string();
        if account_id.is_empty() || user_id.is_empty() || flow_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_error("".into());
        let live = confirm_live.clone();
        let weak = weak.clone();
        confirm_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live
                .manager
                .confirm_sas_verification(&user_id, &flow_id)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_flow(&window, &account_id, operation, &flow_id) {
                    return;
                }
                window.set_sas_busy(false);
                match result {
                    Ok(()) => {
                        window.set_sas_compared(false);
                        window.set_sas_emoji("".into());
                        window.set_sas_decimals("".into());
                        window.set_sas_stage(WAITING_FOR_COMPLETION);
                        window.set_sas_notice(
                            "Comparison confirmed here. Waiting for the other device to finish."
                                .into(),
                        );
                    }
                    Err(_) => window.set_sas_error(
                        "Could not confirm comparison. Check the other device and retry.".into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let mismatch_live = live.clone();
    let mismatch_handle = handle.clone();
    ui.on_mismatch_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        // Reporting a difference is always allowed while values are shown; it
        // can only stop a flow, never trust a device.
        if !window.get_live_session()
            || window.get_sas_stage() != COMPARING
            || window.get_sas_busy()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let user_id = window.get_sas_other_user_id().to_string();
        let flow_id = window.get_sas_flow_id().to_string();
        if account_id.is_empty() || user_id.is_empty() || flow_id.is_empty() {
            return;
        }
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_compared(false);
        window.set_sas_error("".into());
        let live = mismatch_live.clone();
        let weak = weak.clone();
        mismatch_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            let mut reported = live
                .manager
                .sas_mismatch_for(&account_id, &user_id, &flow_id)
                .await;
            if reported.is_err() && active_account_is(&live, &account_id).await {
                // Fall back to a plain cancel so the other device stops too.
                reported = live.manager.cancel_verification(&user_id, &flow_id).await;
            }
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_flow(&window, &account_id, operation, &flow_id) {
                    return;
                }
                clear_ui(&window);
                let message = if reported.is_ok() {
                    "Values reported as different. Verification stopped; nothing was trusted."
                } else {
                    "Could not reach the other device. Nothing was trusted; cancel there too."
                };
                window.set_sas_error(message.into());
            });
        });
    });

    let weak = ui.as_weak();
    let cancel_live = live.clone();
    ui.on_cancel_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if window.get_sas_stage() == IDLE || window.get_sas_busy() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let user_id = window.get_sas_other_user_id().to_string();
        let flow_id = window.get_sas_flow_id().to_string();
        clear_ui(&window);
        window.set_sas_notice(
            "Verification dismissed here. Morpheus will ask the other device to cancel.".into(),
        );
        let operation = window.get_sas_operation();
        let live = cancel_live.clone();
        let weak = weak.clone();
        handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if !active_account_is(&live, &account_id).await {
                return;
            }
            let result = live.manager.cancel_verification(&user_id, &flow_id).await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_account(&window, &account_id, operation) || window.get_sas_stage() != IDLE
                {
                    return;
                }
                if result.is_ok() {
                    window.set_sas_notice("Verification canceled.".into());
                } else {
                    window.set_sas_notice(
                        "Could not cancel remotely. The request may remain until it expires."
                            .into(),
                    );
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::SasEmojiInfo;

    #[test]
    fn comparison_requires_values_and_sanitizes_labels() {
        assert_eq!(
            comparison_labels(SasPresentation {
                emoji: None,
                decimals: None
            }),
            None
        );
        let labels = comparison_labels(SasPresentation {
            emoji: Some(vec![SasEmojiInfo {
                symbol: "🐶".into(),
                description: "Dog\u{202e}spoof".into(),
            }]),
            decimals: Some([1234, 567, 9999]),
        })
        .unwrap();
        assert_eq!(labels.0, "🐶 Dog spoof");
        assert_eq!(labels.1, "1234 · 0567 · 9999");
    }

    #[test]
    fn stopped_flows_say_why_and_never_claim_trust() {
        let cancel = |kind, by_us| Some(VerificationCancel { kind, by_us });
        let timeout = cancel_message(cancel(VerificationCancelKind::Timeout, false));
        assert!(timeout.contains("timed out"));
        let theirs = cancel_message(cancel(VerificationCancelKind::Mismatch, false));
        assert!(theirs.contains("other device"));
        let ours = cancel_message(cancel(VerificationCancelKind::Mismatch, true));
        assert!(ours.contains("differ"));
        assert!(cancel_message(None).contains("Nothing was trusted"));
        for kind in [
            VerificationCancelKind::User,
            VerificationCancelKind::Timeout,
            VerificationCancelKind::Mismatch,
            VerificationCancelKind::AcceptedElsewhere,
            VerificationCancelKind::Other,
        ] {
            for by_us in [false, true] {
                assert!(!cancel_message(cancel(kind, by_us)).contains("completed"));
            }
        }
    }

    #[test]
    fn confirmation_requires_explicit_human_comparison() {
        assert!(!may_confirm(COMPARING, false, false, false));
        assert!(!may_confirm(WAITING_FOR_VALUES, true, false, false));
        assert!(!may_confirm(COMPARING, true, true, false));
        assert!(!may_confirm(COMPARING, true, false, true));
        assert!(may_confirm(COMPARING, true, false, false));
    }

    #[test]
    fn completed_request_pruned_by_sdk_uses_verified_trust_state() {
        let pruned = Err(MatrixCoreError::Sdk(
            "verification request was not found".into(),
        ));
        assert!(verified_after_pruned_completion(
            WAITING_FOR_COMPLETION,
            &pruned,
            Some(DeviceVerificationStatus::Verified),
        ));
        assert!(!verified_after_pruned_completion(
            WAITING_FOR_VALUES,
            &pruned,
            Some(DeviceVerificationStatus::Verified),
        ));
        assert!(!verified_after_pruned_completion(
            WAITING_FOR_COMPLETION,
            &pruned,
            Some(DeviceVerificationStatus::Unverified),
        ));
        assert!(!verified_after_pruned_completion(
            WAITING_FOR_COMPLETION,
            &Err(MatrixCoreError::Sdk("network unavailable".into())),
            Some(DeviceVerificationStatus::Verified),
        ));
    }
}
