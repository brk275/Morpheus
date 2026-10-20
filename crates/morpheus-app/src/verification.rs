//! Own-device SAS verification in Settings › Security & recovery: an inbox
//! of the requests this account's other devices sent, and one active flow.
//! The UI never confirms a SAS flow until the person checks the values on
//! both devices and opts in explicitly.
//!
//! Every action captures the exact account selection, this view's
//! cancellation and a new operation number; the core (`own_verification`)
//! releases the manager's state before any SDK await, so nothing here holds
//! the account transition and an abandoned step never blocks navigation. A
//! step that sends an event carries the security gate's generation from the
//! click, and the core checks it again right before the SDK hand-off; every
//! result is published only while the same account, selection, operation
//! and (for a flow) flow are shown and the gate is open. A step that may
//! have left is reported as uncertain and never resent.
//!
//! The SAS values are cleared from the window the moment the view closes
//! (leaving Settings), the account changes, or protection starts (a
//! security or account-access step, sign-in, a pending switch).
//!
//! Inbox rows are built on the UI thread from plain core data only.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use morpheus_matrix::{
    AccountSelection, MAX_PENDING_VERIFICATIONS, OwnVerification, OwnVerificationError,
    OwnVerificationPhase, OwnVerificationRequest, PendingOwnVerifications, SasPresentation,
    VERIFICATION_REQUEST_LIFETIME, VerificationCancellation,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, VerificationRequestItem, live::LiveUi};

const IDLE: i32 = 0;
const WAITING_FOR_REQUEST: i32 = 1;
const READY_TO_START: i32 = 2;
const WAITING_FOR_VALUES: i32 = 3;
const COMPARING: i32 = 4;
const WAITING_FOR_COMPLETION: i32 = 5;
/// The other device moved the flow to a method Morpheus does not support
/// (such as a QR code): nothing is polled; only Cancel remains.
const UNSUPPORTED: i32 = 6;

const MAX_TITLE_CHARS: usize = 80;
/// Clock skew allowed for a request's own timestamp, as Matrix allows.
const FUTURE_SKEW_MS: u64 = 5 * 60 * 1000;
const FLOW_LABEL_CHARS: usize = 8;

/// The view every action of this window belongs to.
struct View {
    account: String,
    selection: AccountSelection,
    cancellation: VerificationCancellation,
}

#[derive(Default)]
struct State {
    view: Option<View>,
    /// The listed requests, as the core last reported them.
    inbox: Vec<OwnVerification>,
    /// Open requests the core found but did not list, and whether older
    /// ones may be missing from its bounded record.
    more_open: usize,
    older_may_be_omitted: bool,
    /// The active flow and its other device, as last reported.
    target: Option<(String, Option<String>)>,
    /// When this app first listed each request (account, flow), for the
    /// expiry estimate: the SDK does not expose a request's age. Pruned to
    /// the listed requests on every paint, so it never outgrows the inbox.
    seen: HashMap<(String, String), Instant>,
}

thread_local! {
    // Slint callbacks run on the UI thread only.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| f(&mut state.borrow_mut()))
}

fn next_operation(ui: &AppWindow) -> i32 {
    let next = ui.get_sas_operation().wrapping_add(1);
    ui.set_sas_operation(next);
    next
}

/// Ends the current view: its waits stop at once (a step already handed to
/// the SDK may still reach the other device).
fn close_view() {
    with(|state| {
        if let Some(view) = state.view.take() {
            view.cancellation.cancel();
        }
    });
}

/// Verification may act or show anything: a live session, the Settings
/// page open, and no protection (a security or account-access step,
/// sign-in, a pending switch, a recovery key on screen). Timers, actions
/// and results all check this, so a timer can never recreate the view or
/// accept anything while protected.
fn eligible(ui: &AppWindow) -> bool {
    may_act(ui.get_live_session(), ui.get_page(), ui.get_sas_protected())
}

fn may_act(live_session: bool, page: i32, protected: bool) -> bool {
    const SETTINGS_PAGE: i32 = 1;
    live_session && page == SETTINGS_PAGE && !protected
}

/// The view for the visible live account, kept while its selection is
/// current; a new one otherwise. None while not eligible.
fn current_view(
    ui: &AppWindow,
    live: &LiveUi,
) -> Option<(String, AccountSelection, VerificationCancellation)> {
    if !eligible(ui) {
        return None;
    }
    let account = ui.get_active_account_local_id().to_string();
    if account.is_empty() {
        return None;
    }
    with(|state| {
        let reusable = state.view.as_ref().is_some_and(|view| {
            view.account == account
                && view.selection.is_current()
                && !view.cancellation.is_cancelled()
        });
        if !reusable {
            if let Some(old) = state.view.take() {
                old.cancellation.cancel();
            }
            let selection = live.manager.account_selection(&account)?;
            state.view = Some(View {
                account: account.clone(),
                selection,
                cancellation: VerificationCancellation::new(),
            });
        }
        let view = state.view.as_ref()?;
        Some((
            view.account.clone(),
            view.selection.clone(),
            view.cancellation.clone(),
        ))
    })
}

/// A result may be shown: still eligible, same visible account, selection
/// and operation, and no security or account-access step holds the gate.
fn publishable(
    ui: &AppWindow,
    live: &LiveUi,
    account_id: &str,
    selection: &AccountSelection,
    operation: i32,
) -> bool {
    eligible(ui)
        && ui.get_active_account_local_id().as_str() == account_id
        && ui.get_sas_operation() == operation
        && selection.is_current()
        && !live.security_gate.blocked()
}

fn same_flow(
    ui: &AppWindow,
    live: &LiveUi,
    account_id: &str,
    selection: &AccountSelection,
    operation: i32,
    flow_id: &str,
) -> bool {
    publishable(ui, live, account_id, selection, operation)
        && ui.get_sas_flow_id().as_str() == flow_id
}

/// The security gate's generation now, as a fence the core rechecks right
/// before handing a step to the SDK.
fn admission(live: &LiveUi) -> impl Fn() -> bool + Send + Sync + 'static {
    let gate = live.security_gate.clone();
    let generation = gate.ringing_generation();
    move || gate.ringing_allowed(generation)
}

/// Fixed text for a step that did not complete; `None` for a closed view
/// or another account, which show nothing.
fn step_error(error: OwnVerificationError) -> Option<String> {
    match error {
        OwnVerificationError::Cancelled | OwnVerificationError::AccountChanged => None,
        other => Some(other.to_string()),
    }
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

fn may_confirm(stage: i32, compared: bool, busy: bool, recovery_blocked: bool) -> bool {
    stage == COMPARING && compared && !busy && !recovery_blocked
}

/// After this device confirmed, the SDK no longer has the flow. That does
/// not prove this exact flow with this exact device completed (this
/// device's own status says nothing about the other device), so it is
/// reported as unconfirmed, naming the device when known.
fn completion_unconfirmed(device: Option<&str>) -> String {
    let device = device.map_or_else(
        || "the other device".to_owned(),
        |id| format!("device {}", plain_label(id, MAX_TITLE_CHARS)),
    );
    format!(
        "This verification with {device} is no longer known here, so whether it completed \
         cannot be confirmed. Check that device and Refresh status."
    )
}

/// The open flow moved to a method other than SAS: it cannot continue here.
fn unsupported_method(info: &OwnVerification) -> bool {
    info.is_open() && info.offers_sas == Some(false)
}

/// A short, plain label of a flow ID for telling requests apart.
fn flow_label(flow_id: &str) -> String {
    let short: String = flow_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(FLOW_LABEL_CHARS)
        .collect();
    if short.is_empty() {
        "request".to_owned()
    } else {
        format!("request {short}")
    }
}

fn now_ms() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

/// A request's age line and whether it is past its lifetime. Its own
/// timestamp (the sending device's clock) is used when plausible; otherwise
/// only how long it has been listed here is known, and its expiry is not.
/// Nothing here ever restarts a request's lifetime.
fn age_text(sent_at_ms: Option<u64>, now_ms: u64, listed_for: Duration) -> (String, bool) {
    let lifetime_ms = u64::try_from(VERIFICATION_REQUEST_LIFETIME.as_millis()).unwrap_or(u64::MAX);
    match sent_at_ms {
        Some(sent) if sent <= now_ms.saturating_add(FUTURE_SKEW_MS) => {
            let age = now_ms.saturating_sub(sent);
            let minutes = age / 60_000;
            if age >= lifetime_ms {
                (
                    format!("expired: sent {minutes} min ago by its clock"),
                    true,
                )
            } else {
                let left = (lifetime_ms - age).div_ceil(60_000);
                let text =
                    format!("sent {minutes} min ago by its clock; expires in about {left} min");
                (text, false)
            }
        }
        _ => {
            let listed = listed_for.as_secs() / 60;
            (
                format!("listed here for {listed} min; actual expiry unknown"),
                false,
            )
        }
    }
}

/// One inbox row: the device's own label and ID, the flow, its status and
/// its age. Only a request still waiting here, whose device offers SAS and
/// that is not past its lifetime by its own timestamp, can be reviewed.
fn row(info: &OwnVerification, listed_for: Duration, now_ms: u64) -> VerificationRequestItem {
    let name = info
        .device_name
        .as_deref()
        .map(|name| plain_label(name, MAX_TITLE_CHARS))
        .unwrap_or_else(|| "Unnamed device".to_owned());
    let device = info.device_id.as_deref().map_or_else(
        || "device not identified yet".to_owned(),
        |id| format!("device {}", plain_label(id, MAX_TITLE_CHARS)),
    );
    let (age, expired) = age_text(info.sent_at_ms, now_ms, listed_for);
    let (status, reviewable) = match (info.phase, info.offers_sas) {
        (_, Some(false)) => ("Does not offer emoji or number comparison", false),
        (OwnVerificationPhase::Requested, _) => ("Waiting for your answer", true),
        (OwnVerificationPhase::Ready, _) => ("Accepted here; continue to compare", true),
        (OwnVerificationPhase::Comparing, _) => ("Comparison started; continue", true),
        _ => ("No longer open", false),
    };
    VerificationRequestItem {
        flow_id: info.flow_id.as_str().into(),
        title: format!("{name} · {device}").into(),
        detail: format!("{status} · {} · {age}", flow_label(&info.flow_id)).into(),
        can_review: reviewable && !expired,
    }
}

/// What the inbox does not show, in plain words; empty when it shows all.
fn inbox_note(listed: usize, more_open: usize, older_may_be_omitted: bool) -> String {
    let mut note = String::new();
    if more_open > 0 {
        note = format!(
            "Showing {listed} of {} open requests. Answer or decline these to see the rest.",
            listed + more_open
        );
    }
    if older_may_be_omitted {
        if !note.is_empty() {
            note.push(' ');
        }
        note.push_str("Older requests may not be listed.");
    }
    note
}

/// Keep only the first-seen times of the requests listed for `account`.
fn prune_seen(
    seen: &mut HashMap<(String, String), Instant>,
    account: &str,
    inbox: &[OwnVerification],
) {
    seen.retain(|(owner, flow), _| {
        owner == account && inbox.iter().any(|info| info.flow_id == *flow)
    });
}

/// Store one bounded listing; the inbox never holds more than the core's
/// bound, whatever it is handed.
fn adopt_listing(state: &mut State, listing: PendingOwnVerifications) {
    let mut requests = listing.requests;
    let over = requests.len().saturating_sub(MAX_PENDING_VERIFICATIONS);
    requests.truncate(MAX_PENDING_VERIFICATIONS);
    state.inbox = requests;
    state.more_open = listing.more_open + over;
    state.older_may_be_omitted = listing.older_may_be_omitted;
}

/// Rebuild the inbox rows from `State::inbox` (UI thread only), keeping the
/// selected request selected while it is listed.
fn paint_inbox(ui: &AppWindow) {
    let selected = usize::try_from(ui.get_sas_inbox_index())
        .ok()
        .and_then(|index| slint::Model::row_data(&ui.get_sas_inbox_items(), index))
        .map(|item| item.flow_id.to_string());
    let account = ui.get_active_account_local_id().to_string();
    let now = Instant::now();
    let wall = now_ms();
    let (rows, note): (Vec<VerificationRequestItem>, String) = with(|state| {
        let inbox = state.inbox.clone();
        prune_seen(&mut state.seen, &account, &inbox);
        let rows: Vec<_> = inbox
            .iter()
            .map(|info| {
                let key = (account.clone(), info.flow_id.clone());
                let seen = *state.seen.entry(key).or_insert(now);
                row(info, now.saturating_duration_since(seen), wall)
            })
            .collect();
        let note = inbox_note(rows.len(), state.more_open, state.older_may_be_omitted);
        (rows, note)
    });
    ui.set_sas_inbox_note(note.into());
    let index = selected
        .and_then(|flow| rows.iter().position(|item| item.flow_id == flow.as_str()))
        .or_else(|| (!rows.is_empty()).then_some(0))
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1);
    ui.set_sas_inbox_items(ModelRc::new(VecModel::from(rows)));
    ui.set_sas_inbox_index(index);
}

fn clear_inbox(ui: &AppWindow) {
    with(|state| {
        state.inbox.clear();
        state.more_open = 0;
        state.older_may_be_omitted = false;
        state.seen.clear();
    });
    ui.set_sas_inbox_note("".into());
    ui.set_sas_inbox_items(ModelRc::new(VecModel::<VerificationRequestItem>::default()));
    ui.set_sas_inbox_index(-1);
}

/// Invalidate all late callbacks before changing the visible Matrix account.
pub(crate) fn clear_ui(ui: &AppWindow) {
    close_view();
    next_operation(ui);
    clear_inbox(ui);
    ui.set_sas_stage(IDLE);
    ui.set_sas_busy(false);
    ui.set_sas_incoming_loading(false);
    ui.set_sas_request_uncertain(false);
    with(|state| state.target = None);
    ui.set_sas_compared(false);
    ui.set_sas_request_initiator(false);
    ui.set_sas_flow_id("".into());
    ui.set_sas_other_user_id("".into());
    ui.set_sas_emoji("".into());
    ui.set_sas_decimals("".into());
    ui.set_sas_error("".into());
    ui.set_sas_notice("".into());
}

/// The view closed or protection started: the SAS values leave the window
/// now, every wait of this view ends, and a comparison resumes only by
/// reading the values again. A step already handed to the SDK may still
/// arrive; its result is no longer shown.
pub(crate) fn hide_sensitive(ui: &AppWindow) {
    close_view();
    let was_busy = ui.get_sas_busy();
    next_operation(ui);
    ui.set_sas_emoji("".into());
    ui.set_sas_decimals("".into());
    ui.set_sas_compared(false);
    ui.set_sas_busy(false);
    ui.set_sas_incoming_loading(false);
    if ui.get_sas_stage() == COMPARING {
        ui.set_sas_stage(WAITING_FOR_VALUES);
    }
    if was_busy {
        ui.set_sas_notice(
            "This view closed while a step was being sent. Check the status before continuing."
                .into(),
        );
    }
}

/// Ask the account's other devices to verify this one. An open request this
/// device sent is continued instead; after an uncertain attempt nothing new
/// is sent until `acknowledged`.
fn request_own(window: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, acknowledged: bool) {
    if window.get_sas_stage() != IDLE
        || window.get_sas_busy()
        || live.security_gate.blocked()
        || window.get_security_device_status().as_str() != "Unverified"
        || (window.get_sas_request_uncertain() && !acknowledged)
    {
        return;
    }
    let Some((account_id, selection, cancellation)) = current_view(window, live) else {
        return;
    };
    let operation = next_operation(window);
    window.set_sas_busy(true);
    window.set_sas_incoming_loading(false);
    window.set_sas_error("".into());
    window.set_sas_notice("Sending a request to your other device…".into());
    let admitted = admission(live);
    let live = live.clone();
    let weak = window.as_weak();
    handle.spawn(async move {
        let manager = live.manager.clone();
        let result = manager
            .request_own_verification_selected(&selection, &cancellation, &admitted, acknowledged)
            .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            if !publishable(&window, &live, &account_id, &selection, operation) {
                return;
            }
            window.set_sas_busy(false);
            match result {
                Ok(OwnVerificationRequest::Sent(info)) => {
                    window.set_sas_request_uncertain(false);
                    adopt(&window, &info);
                    window.set_sas_notice(
                        "Approve this request on your other verified device.".into(),
                    );
                }
                Ok(OwnVerificationRequest::AlreadyOpen(info)) => {
                    window.set_sas_request_uncertain(false);
                    adopt(&window, &info);
                    window.set_sas_notice(
                        "A request this device sent earlier is still open; continuing it. \
                         Nothing new was sent."
                            .into(),
                    );
                }
                Err(error)
                    if matches!(
                        error,
                        OwnVerificationError::Unknown | OwnVerificationError::RequestUncertain
                    ) =>
                {
                    // It may have reached the other devices: no new flow
                    // until the person checked and says so.
                    window.set_sas_request_uncertain(true);
                    window.set_sas_notice("".into());
                    window.set_sas_error(error.to_string().into());
                }
                Err(error) => {
                    window.set_sas_notice("".into());
                    if let Some(text) = step_error(error) {
                        window.set_sas_error(text.into());
                    }
                }
            }
        });
    });
}

/// Make `info` the active flow without sending anything: it was already
/// answered here (or elsewhere on this device), so it continues from where
/// the SDK has it.
fn adopt(ui: &AppWindow, info: &OwnVerification) {
    with(|state| state.target = Some((info.flow_id.clone(), info.device_id.clone())));
    ui.set_sas_flow_id(info.flow_id.as_str().into());
    ui.set_sas_other_user_id(info.own_user_id.as_str().into());
    ui.set_sas_request_initiator(info.we_started);
    ui.set_sas_compared(false);
    ui.set_sas_stage(match info.phase {
        OwnVerificationPhase::Comparing => WAITING_FOR_VALUES,
        _ => WAITING_FOR_REQUEST,
    });
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    ui.on_verification_protected(move || {
        if let Some(window) = weak.upgrade() {
            hide_sensitive(&window);
        }
    });

    let weak = ui.as_weak();
    let incoming_live = live.clone();
    let incoming_handle = handle.clone();
    ui.on_refresh_incoming_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if window.get_sas_stage() != IDLE
            || window.get_sas_busy()
            || window.get_sas_incoming_loading()
            || incoming_live.security_gate.blocked()
        {
            return;
        }
        let Some((account_id, selection, cancellation)) = current_view(&window, &incoming_live)
        else {
            return;
        };
        let operation = next_operation(&window);
        window.set_sas_incoming_loading(true);
        let live = incoming_live.clone();
        let weak = weak.clone();
        incoming_handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager
                .pending_own_verifications_selected(&selection, &cancellation)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !publishable(&window, &live, &account_id, &selection, operation)
                    || window.get_sas_stage() != IDLE
                {
                    return;
                }
                window.set_sas_incoming_loading(false);
                // A failed read keeps the rows shown; the next one reconciles.
                if let Ok(listing) = result {
                    with(|state| adopt_listing(state, listing));
                    paint_inbox(&window);
                }
            });
        });
    });

    let weak = ui.as_weak();
    let accept_live = live.clone();
    let accept_handle = handle.clone();
    ui.on_accept_incoming_verification(move |flow_id| {
        let Some(window) = weak.upgrade() else { return };
        if window.get_sas_stage() != IDLE
            || window.get_sas_busy()
            || accept_live.security_gate.blocked()
        {
            return;
        }
        let flow_id = flow_id.to_string();
        let listed = with(|state| {
            state
                .inbox
                .iter()
                .find(|info| info.flow_id == flow_id)
                .cloned()
        });
        let Some(listed) = listed else { return };
        let reviewable = usize::try_from(window.get_sas_inbox_index())
            .ok()
            .and_then(|index| slint::Model::row_data(&window.get_sas_inbox_items(), index))
            .is_some_and(|item| item.flow_id == flow_id.as_str() && item.can_review);
        if !reviewable {
            return;
        }
        let Some((account_id, selection, cancellation)) = current_view(&window, &accept_live)
        else {
            return;
        };
        let operation = next_operation(&window);
        window.set_sas_error("".into());
        if listed.phase != OwnVerificationPhase::Requested {
            // Already answered here: continue it, send nothing again.
            adopt(&window, &listed);
            window.set_sas_notice("This request was already accepted here. Continuing it.".into());
            return;
        }
        window.set_sas_busy(true);
        window.set_sas_incoming_loading(false);
        let admitted = admission(&accept_live);
        let live = accept_live.clone();
        let weak = weak.clone();
        accept_handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager
                .accept_own_verification_selected(&selection, &cancellation, &flow_id, &admitted)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !publishable(&window, &live, &account_id, &selection, operation)
                    || window.get_sas_stage() != IDLE
                {
                    return;
                }
                window.set_sas_busy(false);
                match result {
                    Ok(info) => {
                        adopt(&window, &info);
                        with(|state| state.inbox.retain(|listed| listed.flow_id != info.flow_id));
                        paint_inbox(&window);
                        window.set_sas_notice(
                            "Request accepted. Waiting for the requesting device to start comparison."
                                .into(),
                        );
                    }
                    Err(OwnVerificationError::Unknown) => {
                        // It may have been accepted: never offer to accept
                        // it again blindly; the next listing shows its state.
                        with(|state| state.inbox.retain(|listed| listed.flow_id != flow_id));
                        paint_inbox(&window);
                        window.set_sas_error(OwnVerificationError::Unknown.to_string().into());
                    }
                    Err(error) => {
                        if let Some(text) = step_error(error) {
                            window.set_sas_error(text.into());
                        }
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let decline_live = live.clone();
    let decline_handle = handle.clone();
    ui.on_decline_incoming_verification(move |flow_id| {
        let Some(window) = weak.upgrade() else { return };
        if window.get_sas_stage() != IDLE || window.get_sas_busy() {
            return;
        }
        let flow_id = flow_id.to_string();
        if !with(|state| state.inbox.iter().any(|info| info.flow_id == flow_id)) {
            return;
        }
        let Some((account_id, selection, cancellation)) = current_view(&window, &decline_live)
        else {
            return;
        };
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_error("".into());
        window.set_sas_notice("".into());
        let live = decline_live.clone();
        let weak = weak.clone();
        decline_handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager
                .cancel_own_verification_selected(&selection, &cancellation, &flow_id)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !publishable(&window, &live, &account_id, &selection, operation) {
                    return;
                }
                window.set_sas_busy(false);
                match result {
                    Ok(()) | Err(OwnVerificationError::Gone) => {
                        with(|state| state.inbox.retain(|info| info.flow_id != flow_id));
                        paint_inbox(&window);
                        window.set_sas_notice("Request declined.".into());
                    }
                    Err(error) => {
                        if let Some(text) = step_error(error) {
                            window.set_sas_error(text.into());
                        }
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let request_live = live.clone();
    let request_handle = handle.clone();
    ui.on_request_own_verification(move || {
        if let Some(window) = weak.upgrade() {
            request_own(&window, &request_live, &request_handle, false);
        }
    });

    // After an uncertain request, only this explicit acknowledgement (the
    // person checked their other devices) may start another flow.
    let weak = ui.as_weak();
    let anyway_live = live.clone();
    let anyway_handle = handle.clone();
    ui.on_request_own_verification_anyway(move || {
        if let Some(window) = weak.upgrade()
            && window.get_sas_request_uncertain()
        {
            request_own(&window, &anyway_live, &anyway_handle, true);
        }
    });

    let weak = ui.as_weak();
    let refresh_live = live.clone();
    let refresh_handle = handle.clone();
    ui.on_refresh_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        let stage = window.get_sas_stage();
        if window.get_sas_busy()
            || refresh_live.security_gate.blocked()
            || !matches!(
                stage,
                WAITING_FOR_REQUEST | WAITING_FOR_VALUES | WAITING_FOR_COMPLETION
            )
        {
            return;
        }
        let flow_id = window.get_sas_flow_id().to_string();
        if flow_id.is_empty() {
            return;
        }
        let Some((account_id, selection, cancellation)) = current_view(&window, &refresh_live)
        else {
            return;
        };
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        let admitted = admission(&refresh_live);
        let live = refresh_live.clone();
        let weak = weak.clone();
        refresh_handle.spawn(async move {
            let manager = live.manager.clone();
            let info = manager
                .own_verification_selected(&selection, &cancellation, &flow_id)
                .await;
            let open = info.as_ref().is_ok_and(OwnVerification::is_open);
            // Nothing is polled or accepted for a flow on another method.
            let open = open && !info.as_ref().is_ok_and(unsupported_method);
            // Only the device that sent the request starts SAS. After the
            // responder accepted that request, it waits for the incoming SAS
            // start and accepts the method without confirming the values.
            let incoming_sas = if stage == WAITING_FOR_REQUEST
                && open
                && info.as_ref().is_ok_and(|info| !info.we_started)
            {
                Some(
                    manager
                        .accept_own_sas_selected(&selection, &cancellation, &flow_id, &admitted)
                        .await,
                )
            } else {
                None
            };
            let values = if stage == WAITING_FOR_VALUES && open {
                Some(
                    manager
                        .own_sas_values_selected(&selection, &cancellation, &flow_id)
                        .await,
                )
            } else {
                None
            };
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_flow(&window, &live, &account_id, &selection, operation, &flow_id) {
                    return;
                }
                window.set_sas_busy(false);
                if let Ok(info) = &info
                    && info.device_id.is_some()
                {
                    let target = (info.flow_id.clone(), info.device_id.clone());
                    with(|state| state.target = Some(target));
                }
                match info {
                    Ok(info) if unsupported_method(&info) => {
                        window.set_sas_stage(UNSUPPORTED);
                        window.set_sas_notice("".into());
                        window.set_sas_error(OwnVerificationError::Unsupported.to_string().into());
                    }
                    Ok(info) => match info.phase {
                        OwnVerificationPhase::Cancelled { by_us } => {
                            clear_ui(&window);
                            window.set_sas_error(
                                if by_us {
                                    "Verification was canceled on this device."
                                } else {
                                    "The other device canceled verification."
                                }
                                .into(),
                            );
                        }
                        OwnVerificationPhase::Expired => {
                            clear_ui(&window);
                            window.set_sas_error("The verification request expired.".into());
                        }
                        OwnVerificationPhase::Done => {
                            clear_ui(&window);
                            window.set_sas_notice("Device verification completed.".into());
                            window.invoke_refresh_security();
                        }
                        OwnVerificationPhase::Ready
                            if stage == WAITING_FOR_REQUEST && info.we_started =>
                        {
                            window.set_sas_stage(READY_TO_START);
                            window.set_sas_notice(
                                "The other device accepted. Start comparison.".into(),
                            );
                            window.set_sas_error("".into());
                        }
                        _ if stage == WAITING_FOR_REQUEST && !info.we_started => match incoming_sas
                        {
                            Some(Ok(true)) => {
                                window.set_sas_stage(WAITING_FOR_VALUES);
                                window.set_sas_notice(
                                    "Comparison started. Waiting for values on both devices."
                                        .into(),
                                );
                                window.set_sas_error("".into());
                            }
                            Some(Err(error)) => {
                                if let Some(text) = step_error(error) {
                                    window.set_sas_error(text.into());
                                }
                            }
                            _ => {}
                        },
                        _ if stage == WAITING_FOR_VALUES => match values {
                            Some(Ok(Some(presentation))) => {
                                // The values are shown only on the open
                                // Settings page, outside any protection.
                                if window.get_page() != 1 || window.get_sas_protected() {
                                    return;
                                }
                                if let Some((emoji, decimals)) = comparison_labels(presentation) {
                                    window.set_sas_emoji(emoji.into());
                                    window.set_sas_decimals(decimals.into());
                                    window.set_sas_compared(false);
                                    window.set_sas_stage(COMPARING);
                                    window.set_sas_notice("".into());
                                    window.set_sas_error("".into());
                                }
                            }
                            Some(Err(error)) => {
                                if let Some(text) = step_error(error) {
                                    window.set_sas_error(text.into());
                                }
                            }
                            _ => {}
                        },
                        _ => window.set_sas_error("".into()),
                    },
                    Err(OwnVerificationError::Gone) if stage == WAITING_FOR_COMPLETION => {
                        // Never inferred from this device's own status.
                        let device = with(|state| {
                            state
                                .target
                                .as_ref()
                                .filter(|(flow, _)| *flow == flow_id)
                                .and_then(|(_, device)| device.clone())
                        });
                        clear_ui(&window);
                        window.set_sas_error(completion_unconfirmed(device.as_deref()).into());
                        window.invoke_refresh_security();
                    }
                    Err(error) => {
                        if let Some(text) = step_error(error) {
                            window.set_sas_error(text.into());
                        }
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let start_live = live.clone();
    let start_handle = handle.clone();
    ui.on_start_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if window.get_sas_busy()
            || window.get_sas_stage() != READY_TO_START
            || !window.get_sas_request_initiator()
            || start_live.security_gate.blocked()
        {
            return;
        }
        let flow_id = window.get_sas_flow_id().to_string();
        if flow_id.is_empty() {
            return;
        }
        let Some((account_id, selection, cancellation)) = current_view(&window, &start_live) else {
            return;
        };
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_error("".into());
        let admitted = admission(&start_live);
        let live = start_live.clone();
        let weak = weak.clone();
        start_handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager
                .start_own_sas_selected(&selection, &cancellation, &flow_id, &admitted)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_flow(&window, &live, &account_id, &selection, operation, &flow_id) {
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
                    Err(error) => {
                        if let Some(text) = step_error(error) {
                            window.set_sas_error(text.into());
                        }
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let confirm_live = live.clone();
    let confirm_handle = handle.clone();
    ui.on_confirm_sas_verification(move || {
        let Some(window) = weak.upgrade() else { return };
        if !may_confirm(
            window.get_sas_stage(),
            window.get_sas_compared(),
            window.get_sas_busy(),
            confirm_live.security_gate.blocked(),
        ) || window.get_sas_protected()
        {
            return;
        }
        let flow_id = window.get_sas_flow_id().to_string();
        if flow_id.is_empty() {
            return;
        }
        let Some((account_id, selection, cancellation)) = current_view(&window, &confirm_live)
        else {
            return;
        };
        let operation = next_operation(&window);
        window.set_sas_busy(true);
        window.set_sas_error("".into());
        let admitted = admission(&confirm_live);
        let live = confirm_live.clone();
        let weak = weak.clone();
        confirm_handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager
                .confirm_own_sas_selected(&selection, &cancellation, &flow_id, &admitted)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !same_flow(&window, &live, &account_id, &selection, operation, &flow_id) {
                    return;
                }
                window.set_sas_busy(false);
                // The values leave the window whatever the outcome.
                window.set_sas_compared(false);
                window.set_sas_emoji("".into());
                window.set_sas_decimals("".into());
                match result {
                    Ok(()) => {
                        window.set_sas_stage(WAITING_FOR_COMPLETION);
                        window.set_sas_notice(
                            "Comparison confirmed here. Waiting for the other device to finish."
                                .into(),
                        );
                    }
                    Err(OwnVerificationError::Unknown) => {
                        // It may have been confirmed: wait for the outcome,
                        // never confirm again.
                        window.set_sas_stage(WAITING_FOR_COMPLETION);
                        window.set_sas_error(OwnVerificationError::Unknown.to_string().into());
                    }
                    Err(error) => {
                        window.set_sas_stage(WAITING_FOR_VALUES);
                        if let Some(text) = step_error(error) {
                            window.set_sas_error(text.into());
                        }
                    }
                }
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
        let flow_id = window.get_sas_flow_id().to_string();
        let account_id = window.get_active_account_local_id().to_string();
        let selection = (!account_id.is_empty() && window.get_live_session())
            .then(|| cancel_live.manager.account_selection(&account_id))
            .flatten();
        clear_ui(&window);
        let Some(selection) = selection.filter(|_| !flow_id.is_empty()) else {
            return;
        };
        window.set_sas_notice(
            "Verification dismissed here. Morpheus will ask the other device to cancel.".into(),
        );
        let operation = window.get_sas_operation();
        // Its own cancellation: the view just closed, but the cancel must
        // still be sent for this exact account.
        let cancellation = VerificationCancellation::new();
        let live = cancel_live.clone();
        let weak = weak.clone();
        handle.spawn(async move {
            let manager = live.manager.clone();
            let result = manager
                .cancel_own_verification_selected(&selection, &cancellation, &flow_id)
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !publishable(&window, &live, &account_id, &selection, operation)
                    || window.get_sas_stage() != IDLE
                {
                    return;
                }
                window.set_sas_notice(
                    match result {
                        Ok(()) | Err(OwnVerificationError::Gone) => "Verification canceled.",
                        Err(_) => {
                            "Could not confirm the cancel reached the other device. The request \
                             ends when it expires."
                        }
                    }
                    .into(),
                );
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
    fn confirmation_requires_explicit_human_comparison() {
        assert!(!may_confirm(COMPARING, false, false, false));
        assert!(!may_confirm(WAITING_FOR_VALUES, true, false, false));
        assert!(!may_confirm(COMPARING, true, true, false));
        assert!(!may_confirm(COMPARING, true, false, true));
        assert!(may_confirm(COMPARING, true, false, false));
    }

    /// A flow the SDK forgot after this device confirmed is unconfirmed,
    /// never "verified" from this device's own status; and a flow moved to
    /// another method is unsupported, never polled forever.
    #[test]
    fn a_forgotten_flow_is_unconfirmed_and_another_method_is_unsupported() {
        let text = completion_unconfirmed(Some("OTHER\u{202e}DEVICE"));
        assert!(text.contains("cannot be confirmed") && text.contains("device OTHER DEVICE"));
        assert!(!text.to_lowercase().contains("verified"));
        assert!(completion_unconfirmed(None).contains("the other device"));
        let qr = request(OwnVerificationPhase::Comparing, Some(false));
        assert!(unsupported_method(&qr));
        assert!(!unsupported_method(&request(
            OwnVerificationPhase::Comparing,
            Some(true)
        )));
        assert!(!unsupported_method(&request(
            OwnVerificationPhase::Comparing,
            None
        )));
        assert!(!unsupported_method(&request(
            OwnVerificationPhase::Done,
            Some(false)
        )));
    }

    fn request(phase: OwnVerificationPhase, offers_sas: Option<bool>) -> OwnVerification {
        OwnVerification {
            flow_id: "AbC-123\u{202e}xyz-long-flow".into(),
            own_user_id: "@casey:example.org".into(),
            device_id: Some("DEVICE\u{7}ID".into()),
            device_name: Some("Laptop\u{202e}spoof".into()),
            we_started: false,
            phase,
            offers_sas,
            sent_at_ms: None,
        }
    }

    /// Rows name the device and the exact flow in plain text; only a request
    /// still waiting here, offering SAS and not past its lifetime by its own
    /// timestamp, can be reviewed.
    #[test]
    fn inbox_rows_are_plain_exact_and_only_reviewable_while_open() {
        let now = 10_000_000_000;
        let waiting = request(OwnVerificationPhase::Requested, Some(true));
        let fresh = row(&waiting, Duration::ZERO, now);
        assert!(fresh.can_review);
        assert!(!fresh.title.contains('\u{202e}') && !fresh.title.contains('\u{7}'));
        assert!(fresh.title.contains("Laptop") && fresh.title.contains("DEVICE"));
        assert!(fresh.detail.contains("request AbC123xy"));
        assert!(fresh.detail.contains("actual expiry unknown"));
        let no_sas = request(OwnVerificationPhase::Requested, Some(false));
        let no_sas = row(&no_sas, Duration::ZERO, now);
        assert!(!no_sas.can_review);
        let unnamed = OwnVerification {
            device_name: None,
            device_id: None,
            ..request(OwnVerificationPhase::Ready, Some(true))
        };
        let unnamed = row(&unnamed, Duration::ZERO, now);
        assert!(unnamed.title.contains("Unnamed device"));
        assert!(unnamed.title.contains("not identified"));
        assert!(unnamed.can_review, "accepted here: it can be continued");
        assert_eq!(flow_label("\u{202e}"), "request");
    }

    /// The age comes from the request's own timestamp, so a request first
    /// listed late keeps its real deadline and an old one shows as expired;
    /// without a plausible timestamp the expiry is said to be unknown, never
    /// invented from when it was listed.
    #[test]
    fn expiry_comes_from_the_request_and_is_never_restarted() {
        let now: u64 = 10_000_000_000;
        let minute = 60_000;
        let (text, expired) = age_text(Some(now - 2 * minute), now, Duration::ZERO);
        assert!(!expired && text.contains("sent 2 min ago") && text.contains("about 8 min"));
        // First listed just now, but sent 12 minutes ago: expired, not 10
        // fresh minutes.
        let old = OwnVerification {
            sent_at_ms: Some(now - 12 * minute),
            ..request(OwnVerificationPhase::Requested, Some(true))
        };
        let old = row(&old, Duration::ZERO, now);
        assert!(!old.can_review && old.detail.contains("expired: sent 12 min ago"));
        // Listed for 30 minutes without a timestamp: age only, expiry
        // unknown, still reviewable (the SDK refuses an expired one).
        let (text, expired) = age_text(None, now, Duration::from_secs(30 * 60));
        assert!(!expired && text.contains("listed here for 30 min") && text.contains("unknown"));
        // A timestamp far in the future is not believed.
        let (text, expired) = age_text(Some(now + 60 * minute), now, Duration::ZERO);
        assert!(!expired && text.contains("unknown"));
        // Within the allowed skew it counts as just sent.
        let (text, _) = age_text(Some(now + minute), now, Duration::ZERO);
        assert!(text.contains("sent 0 min ago"));
    }

    /// Timers, actions and results act only on the open Settings page of a
    /// live session outside protection: a refresh timer firing while a
    /// switch, sign-in or security step is pending creates no view, so it
    /// cannot accept a SAS start or show values.
    #[test]
    fn verification_acts_only_on_the_unprotected_settings_page() {
        assert!(may_act(true, 1, false));
        assert!(!may_act(true, 1, true), "protected");
        assert!(!may_act(true, 0, false), "chat page");
        assert!(!may_act(true, 2, false), "call stage");
        assert!(!may_act(false, 1, false), "no live session");
    }

    /// The inbox and its first-seen times never outgrow the core's bound:
    /// times of requests no longer listed, or of another account, are
    /// dropped on every paint, and what is not shown is said.
    #[test]
    fn the_inbox_and_its_seen_times_stay_bounded_and_say_what_is_omitted() {
        let mut state = State::default();
        let many: Vec<OwnVerification> = (0..MAX_PENDING_VERIFICATIONS + 3)
            .map(|flow| OwnVerification {
                flow_id: format!("flow-{flow}"),
                ..request(OwnVerificationPhase::Requested, Some(true))
            })
            .collect();
        adopt_listing(
            &mut state,
            PendingOwnVerifications {
                requests: many,
                more_open: 2,
                older_may_be_omitted: true,
            },
        );
        assert_eq!(state.inbox.len(), MAX_PENDING_VERIFICATIONS);
        assert_eq!(state.more_open, 5);
        let now = Instant::now();
        for flow in 0..100 {
            state
                .seen
                .insert(("account".into(), format!("flow-{flow}")), now);
        }
        state.seen.insert(("other".into(), "flow-0".into()), now);
        let inbox = state.inbox.clone();
        prune_seen(&mut state.seen, "account", &inbox);
        assert_eq!(state.seen.len(), MAX_PENDING_VERIFICATIONS);
        assert!(
            !state
                .seen
                .contains_key(&("other".to_owned(), "flow-0".to_owned()))
        );
        prune_seen(&mut state.seen, "account", &[]);
        assert!(state.seen.is_empty());
        let note = inbox_note(16, 5, true);
        assert!(note.contains("Showing 16 of 21") && note.contains("Older requests"));
        assert_eq!(inbox_note(3, 0, false), "");
        assert_eq!(inbox_note(3, 0, true), "Older requests may not be listed.");
    }

    /// Only a closed view or another account stays silent; every other
    /// failure shows its fixed text, and an uncertain step says so.
    #[test]
    fn step_errors_are_fixed_and_uncertainty_is_stated() {
        assert_eq!(step_error(OwnVerificationError::Cancelled), None);
        assert_eq!(step_error(OwnVerificationError::AccountChanged), None);
        let unknown = step_error(OwnVerificationError::Unknown).unwrap();
        assert!(unknown.contains("may or may not") && unknown.contains("not sent again"));
        assert!(step_error(OwnVerificationError::SecurityBusy).is_some());
        assert!(step_error(OwnVerificationError::Gone).is_some());
    }
}
