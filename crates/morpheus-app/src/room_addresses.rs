//! A joined room's addresses (room_settings' RoomAddressesSession): the
//! aliases the account's homeserver keeps for the room and the addresses the
//! room publishes, for the exact account selection and room captured when
//! the panel opened (from Room settings or the Settings search).
//!
//! Every change is reviewed first, naming the exact address and room, and
//! sent only on Confirm, once:
//! - Add creates `#localpart:<own server>` for this room; it does not
//!   publish it;
//! - Remove deletes an address on the account's own server only, after the
//!   core checked it still points to this room. That check is not a
//!   compare-and-swap; a published address stays published until that is
//!   changed too;
//! - Make main, Publish and Unpublish edit the published addresses, keeping
//!   every other stored field.
//! Each is its own outcome: nothing is retried or rolled back, and nothing
//! claims two changes happened together. An unanswered outcome is retained
//! outside the view for this process; observing current state never proves
//! that an earlier request finished.
//!
//! Each open, review, confirm, refresh and clear takes a new operation
//! number; a result for an earlier one, or for a departed selection, is
//! dropped. Closing, an account change and detaching cancel the form's
//! requests that have not been handed over, synchronously. Slint values are
//! made and read on the UI thread only; tasks carry plain strings.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use morpheus_matrix::{
    AccountSelection, PublishEdit, RoomAddress, RoomAddressError, RoomAddressesSession,
    RoomAddressesSnapshot, RoomSettingsCancellation, new_local_address,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, RoomAddressEntry, RoomAddresses, live::LiveUi};

#[cfg(test)]
mod tests;

/// RoomAddresses.act actions.
pub(crate) const ACTION_MAKE_MAIN: i32 = 0;
pub(crate) const ACTION_PUBLISH: i32 = 1;
pub(crate) const ACTION_UNPUBLISH: i32 = 2;
pub(crate) const ACTION_UNPUBLISH_MAIN: i32 = 3;
pub(crate) const ACTION_REMOVE: i32 = 4;
const MAX_QUERY_CHARS: usize = 64;
const MAX_LABEL_CHARS: usize = 120;
const MAX_RECEIPTS: usize = 256;
const MAX_ACCOUNT_RECEIPTS: usize = 64;
const MAX_SCOPE_ID_BYTES: usize = 1024;
const WAITING: &str = "An earlier address change for this room is still waiting for an answer. No new change was sent.";
const UNKNOWN: &str = "An earlier change may or may not have been made. Address changes for this room are paused. Read again shows current state only; it cannot prove that the earlier request finished.";
const ACCEPTED: &str = "The homeserver accepted an earlier change, but its state could not be read back. Read again before making another change.";
static NEXT_RECEIPT: AtomicU64 = AtomicU64::new(0);
type ReceiptKey = (String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Writing,
    Unknown,
    Accepted,
}
#[derive(Clone, Debug)]
struct Receipt {
    id: u64,
    pending: Pending,
}
#[derive(Clone, Debug)]
struct Ticket {
    key: ReceiptKey,
    id: u64,
    accepted_at_start: bool,
}
#[derive(Default)]
struct Receipts(HashMap<ReceiptKey, Receipt>);
impl Receipts {
    fn reserve(&mut self, account: &str, room: &str) -> Result<Ticket, &'static str> {
        if [account, room]
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_SCOPE_ID_BYTES)
        {
            return Err("This address target is unavailable. Nothing was sent.");
        }
        let key = (account.to_owned(), room.to_owned());
        if self.0.contains_key(&key) {
            return Err(WAITING);
        }
        if self.0.len() >= MAX_RECEIPTS
            || self.0.keys().filter(|key| key.0 == account).count() >= MAX_ACCOUNT_RECEIPTS
        {
            return Err(
                "Morpheus is tracking its limit of unresolved address changes. Nothing new was sent.",
            );
        }
        let id = NEXT_RECEIPT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| "Address changes are unavailable in this session. Nothing was sent.")?
            + 1;
        self.0.insert(
            key.clone(),
            Receipt {
                id,
                pending: Pending::Writing,
            },
        );
        Ok(Ticket {
            key,
            id,
            accepted_at_start: false,
        })
    }
    fn finish(
        &mut self,
        ticket: &Ticket,
        result: &Result<RoomAddressesSnapshot, RoomAddressError>,
    ) {
        let Some(receipt) = self.0.get_mut(&ticket.key).filter(|r| r.id == ticket.id) else {
            return;
        };
        match result {
            Err(RoomAddressError::Unknown) => receipt.pending = Pending::Unknown,
            Err(RoomAddressError::SavedNotRead) => receipt.pending = Pending::Accepted,
            _ => {
                self.0.remove(&ticket.key);
            }
        }
    }
    fn observe(&mut self, ticket: &Ticket) {
        // A successful response was already received. Only a later fresh
        // read can release this accepted write. An uncertain request is
        // NEVER settled by a snapshot, including one with the desired state.
        if ticket.accepted_at_start
            && self
                .0
                .get(&ticket.key)
                .is_some_and(|r| r.id == ticket.id && r.pending == Pending::Accepted)
        {
            self.0.remove(&ticket.key);
        }
    }
    fn ticket(&self, account: &str, room: &str) -> Option<Ticket> {
        let key = (account.to_owned(), room.to_owned());
        self.0.get(&key).map(|r| Ticket {
            key,
            id: r.id,
            accepted_at_start: r.pending == Pending::Accepted,
        })
    }
    fn pending(&self, account: &str, room: &str) -> Option<Pending> {
        self.0
            .get(&(account.to_owned(), room.to_owned()))
            .map(|r| r.pending)
    }
    fn forget(&mut self, account: &str) {
        self.0.retain(|key, _| key.0 != account);
    }
}
static RECEIPTS: OnceLock<Mutex<Receipts>> = OnceLock::new();
fn receipts() -> MutexGuard<'static, Receipts> {
    RECEIPTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
// Memory-only for this process: no restart durability is claimed. An
// unanswered server request has no safe GET-based completion oracle; keep
// it paused here rather than promising that reopening/restarting undoes it.
// Called only after actual local account removal/logout, never by view close,
// ordinary account switches, selection revocation, or a transient detach.
pub(crate) fn forget_account(account: &str) {
    receipts().forget(account);
}

/// A dropped worker cannot silently release a reserved uncertain action.
/// The ledger contains strings/numeric metadata only: no Client/Room/receiver
/// or event handler that could retain an encrypted store on Windows.
struct Completion(Option<Ticket>);
impl Completion {
    fn finish(&mut self, result: &Result<RoomAddressesSnapshot, RoomAddressError>) {
        if let Some(ticket) = self.0.take() {
            receipts().finish(&ticket, result);
        }
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(ticket) = self.0.take() {
            receipts().finish(&ticket, &Err(RoomAddressError::Unknown));
        }
    }
}

struct ViewAuthority {
    live: Arc<LiveUi>,
    selection: AccountSelection,
    navigation: u64,
    security: u64,
    cancellation: RoomSettingsCancellation,
}
impl ViewAuthority {
    fn admitted(&self) -> bool {
        !self.cancellation.is_cancelled()
            && self.selection.is_current()
            && self.live.is_live.load(Ordering::Acquire)
            && self.live.navigation_is_current(self.navigation)
            && self.live.security_gate.ringing_allowed(self.security)
    }
}

/// A reviewed change waiting for Confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Add(String),
    Remove(String),
    Publish(PublishEdit),
}

/// The change an action asks for on one address.
pub(crate) fn change_for(alias: &str, action: i32) -> Option<Change> {
    Some(match action {
        ACTION_MAKE_MAIN => Change::Publish(PublishEdit::SetMain(alias.to_owned())),
        ACTION_PUBLISH => Change::Publish(PublishEdit::AddAlternative(alias.to_owned())),
        ACTION_UNPUBLISH => Change::Publish(PublishEdit::RemoveAlternative(alias.to_owned())),
        ACTION_UNPUBLISH_MAIN => Change::Publish(PublishEdit::ClearMain),
        ACTION_REMOVE => Change::Remove(alias.to_owned()),
        _ => return None,
    })
}

/// The review of a change: title, plain sentences and the Confirm label.
/// Addresses are Matrix IDs shown as plain text; `main` is the current main
/// address, for unpublishing it.
pub(crate) fn review_text(
    change: &Change,
    room_name: &str,
    main: Option<&str>,
) -> (String, Vec<String>, &'static str) {
    let room = label(room_name);
    match change {
        Change::Add(alias) => (
            format!("Add {} to “{room}”?", label(alias)),
            vec![
                format!("{} will lead to “{room}” on your homeserver.", label(alias)),
                "It is not published yet: the room's published addresses stay as they are."
                    .to_owned(),
            ],
            "Add address",
        ),
        Change::Remove(alias) => (
            format!("Remove {} from your homeserver?", label(alias)),
            vec![
                format!(
                    "Morpheus first checks that {} still points to “{room}”, then removes it. The check is not a lock: if it changes in between, your homeserver decides.",
                    label(alias)
                ),
                "It stops leading here for everyone. The room, its members and its messages stay."
                    .to_owned(),
                "If it is published, it stays listed as published until you unpublish it."
                    .to_owned(),
            ],
            "Remove address",
        ),
        Change::Publish(edit) => {
            let (title, line) = match edit {
                PublishEdit::SetMain(alias) => (
                    format!("Make {} the main address of “{room}”?", label(alias)),
                    "Clients show the main address for the room. The previous main address stays published as an alternative."
                        .to_owned(),
                ),
                PublishEdit::AddAlternative(alias) => (
                    format!("Publish {} as an address of “{room}”?", label(alias)),
                    "It is listed with the room's state for everyone in it.".to_owned(),
                ),
                PublishEdit::RemoveAlternative(alias) => (
                    format!("Unpublish {} from “{room}”?", label(alias)),
                    "It is no longer listed with the room. It still leads here until it is removed from its server."
                        .to_owned(),
                ),
                PublishEdit::ClearMain => (
                    format!(
                        "Unpublish the main address {} of “{room}”?",
                        label(main.unwrap_or(""))
                    ),
                    "The room has no main address afterwards. The address still leads here until it is removed from its server."
                        .to_owned(),
                ),
            };
            let mut lines = vec![line];
            if let PublishEdit::SetMain(_) | PublishEdit::AddAlternative(_) = edit {
                lines.push(
                    "Morpheus first checks that the address points to this room. Other fields of the published addresses are kept as they are."
                        .to_owned(),
                );
            }
            (title, lines, "Save published addresses")
        }
    }
}

/// What a finished change leaves: a notice, an error, and whether it may or
/// may not have happened (the addresses are then read again at once).
pub(crate) fn outcome_text(
    result: &Result<RoomAddressesSnapshot, RoomAddressError>,
    change: &Change,
) -> (String, String, bool) {
    match result {
        Ok(_) => (
            match change {
                Change::Add(alias) => format!("Added {}.", label(alias)),
                Change::Remove(alias) => format!("Removed {} from your homeserver.", label(alias)),
                Change::Publish(_) => "Saved the published addresses.".to_owned(),
            },
            String::new(),
            false,
        ),
        Err(error) => (String::new(), error.to_string(), error.may_have_changed()),
    }
}

/// The rows the search shows: alias, detail, and its flags.
pub(crate) fn visible(addresses: &[RoomAddress], query: &str) -> Vec<(RoomAddress, String)> {
    let query = query
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .trim()
        .to_lowercase();
    addresses
        .iter()
        .filter(|address| query.is_empty() || address.alias.to_lowercase().contains(&query))
        .map(|address| (address.clone(), detail(address)))
        .collect()
}

/// One row's detail: published as what, listed or not, and whose server.
pub(crate) fn detail(address: &RoomAddress) -> String {
    let mut parts = Vec::new();
    if address.main {
        parts.push("Main address".to_owned());
    } else if address.alternative {
        parts.push("Published".to_owned());
    } else {
        parts.push("Not published".to_owned());
    }
    if address.local {
        parts.push(if address.listed {
            "On your homeserver".to_owned()
        } else {
            "On your homeserver, not in its list for this room".to_owned()
        });
    } else {
        parts.push(format!("On {}; not changed here", label(&address.server)));
    }
    parts.join(" · ")
}

pub(crate) fn summary(snapshot: &RoomAddressesSnapshot) -> String {
    let mut text = format!(
        "{} addresses · new ones are added on {}",
        snapshot.addresses.len(),
        label(&snapshot.own_server)
    );
    if snapshot.truncated {
        text.push_str(" · more than can be shown");
    }
    if !snapshot.local_list_known {
        text.push_str(
            " · your homeserver did not list its addresses for this room; only published ones are shown",
        );
    }
    if !snapshot.can_publish {
        text.push_str(" · you cannot change the published addresses");
    } else if !snapshot.publish_editable {
        text.push_str(" · the published addresses use data Morpheus cannot edit");
    }
    text
}

fn label(text: &str) -> String {
    crate::directory_plain_text(text, MAX_LABEL_CHARS)
}

struct Session {
    selection: AccountSelection,
    room_id: String,
    cancellation: RoomSettingsCancellation,
    authority: Arc<ViewAuthority>,
    form: Option<Arc<RoomAddressesSession>>,
    snapshot: Option<RoomAddressesSnapshot>,
    change: Option<Change>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

thread_local! {
    // Slint callbacks and every publication run on the UI thread only.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    static OPERATION: Cell<u64> = const { Cell::new(0) };
    static ROWS: Rc<VecModel<RoomAddressEntry>> = Rc::new(VecModel::default());
}

fn next_operation() -> u64 {
    OPERATION.with(|operation| {
        let next = operation.get().wrapping_add(1);
        operation.set(next);
        next
    })
}

fn current_operation() -> u64 {
    OPERATION.with(Cell::get)
}

/// Forget the panel and every address it showed, now; requests not yet
/// handed over are cancelled.
pub(crate) fn clear(ui: &AppWindow) {
    next_operation();
    if let Some(session) = SESSION.with(|session| session.borrow_mut().take()) {
        session.cancellation.cancel();
    }
    ROWS.with(|rows| rows.set_vec(Vec::new()));
    let state = ui.global::<RoomAddresses>();
    state.set_rows(ROWS.with(|rows| ModelRc::from(rows.clone())));
    state.set_open(false);
    state.set_room_name("".into());
    state.set_own_server("".into());
    state.set_summary("".into());
    state.set_query("".into());
    state.set_new_local("".into());
    state.set_can_publish(false);
    state.set_publish_editable(false);
    state.set_loading(false);
    state.set_busy(false);
    state.set_error("".into());
    state.set_notice("".into());
    state.set_unknown(false);
    state.set_uncertainty_note("".into());
    state.set_reviewing(false);
    state.set_review_title("".into());
    state.set_review_lines(ModelRc::new(VecModel::<SharedString>::default()));
    state.set_confirm_label("".into());
}

pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<RoomAddresses>().get_open()
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let state = ui.global::<RoomAddresses>();
    state.set_rows(ROWS.with(|rows| ModelRc::from(rows.clone())));
    let weak = ui.as_weak();
    let (show_live, show_handle) = (live.clone(), handle.clone());
    state.on_show(move |room_id, name| {
        if let Some(ui) = weak.upgrade() {
            show(&ui, &show_live, &show_handle, &room_id, &name);
        }
    });
    let weak = ui.as_weak();
    state.on_filter(move |_| {
        if let Some(ui) = weak.upgrade() {
            publish(&ui);
        }
    });
    let weak = ui.as_weak();
    state.on_add(move |localpart| {
        if let Some(ui) = weak.upgrade() {
            review_add(&ui, &localpart);
        }
    });
    let weak = ui.as_weak();
    state.on_act(move |alias, action| {
        if let (Some(ui), Some(change)) = (weak.upgrade(), change_for(&alias, action)) {
            review(&ui, change);
        }
    });
    let weak = ui.as_weak();
    let (confirm_live, confirm_handle) = (live.clone(), handle.clone());
    state.on_confirm(move || {
        if let Some(ui) = weak.upgrade() {
            confirm(&ui, &confirm_live, &confirm_handle);
        }
    });
    let weak = ui.as_weak();
    state.on_cancel_review(move || {
        if let Some(ui) = weak.upgrade()
            && !ui.global::<RoomAddresses>().get_busy()
        {
            next_operation();
            SESSION.with(|session| {
                if let Some(session) = session.borrow_mut().as_mut() {
                    session.change = None;
                }
            });
            ui.global::<RoomAddresses>().set_reviewing(false);
        }
    });
    let weak = ui.as_weak();
    let (refresh_live, refresh_handle) = (live.clone(), handle.clone());
    state.on_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<RoomAddresses>();
            state.set_notice("".into());
            state.set_error("".into());
            read(&ui, &refresh_live, &refresh_handle);
        }
    });
    let weak = ui.as_weak();
    state.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            let return_focus = session_selection(&ui).is_some();
            clear(&ui); // cancel synchronously BEFORE hiding/focus return
            if return_focus {
                let state = ui.global::<RoomAddresses>();
                state.set_closed(state.get_closed().wrapping_add(1));
            }
        }
    });
    let weak = ui.as_weak();
    state.on_context_changed(move || {
        if let Some(ui) = weak.upgrade() {
            clear(&ui);
        }
    });
}

// Room settings (mode0) is an allowed predecessor: Manage addresses opens
// over it. All other protected native presentations withdraw this form.
fn surface_allowed(ui: &AppWindow) -> bool {
    !crate::room_leave::is_open(ui)
        && ui.get_live_session()
        && (ui.get_page() == 0 || ui.get_page() == 1)
        && !ui.get_show_sign_in()
        && !ui.get_access_open()
        && !ui.get_manage_accounts_open()
        && !ui.get_account_switch_pending()
        && !ui.get_security_busy()
        && ui.get_security_recovery_key().is_empty()
        && !ui.get_sas_busy()
        && ui.get_sas_stage() == 0
        && !ui.get_account_menu_open()
        && !ui.get_quick_switcher_open()
        && !ui.get_room_actions_open()
        && (!ui.get_room_admin_open()
            || (ui.get_room_admin_mode() == 0
                && !ui.get_room_admin_busy()
                && ui.get_room_settings_confirmation() == 0
                && ui.get_room_admin_account_id() == ui.get_active_account_local_id()
                && ui.get_room_admin_room_id() == ui.get_active_room_id()
                // Access and visibility, over the same form, keeps it.
                && !ui.global::<crate::RoomAccessState>().get_open()))
        && !crate::member_privacy::is_open(ui)
        && !ui.get_people_open()
        && !ui.get_account_profile_open()
        && !crate::space_explorer::is_open(ui)
        && !crate::space_management::is_open(ui)
        && !ui.global::<crate::RoomNotificationState>().get_open()
        && !ui.global::<crate::RoomActivity>().get_seen_open()
        && !ui.global::<crate::MemberPrivacy>().get_ignored().busy
        && ui.global::<crate::MemberPrivacy>().get_ignored().stage == 0
        && !ui.get_member_confirm_open()
        && !ui.get_native_capture_picker_open()
        && !ui.get_leave_call_dialog()
        && !ui.get_image_preview_open()
        && !ui.get_poll_dialog_open()
}

/// The selection of the account on screen, if it may still act.
fn current_selection(ui: &AppWindow, live: &LiveUi) -> Option<AccountSelection> {
    if !live.is_live.load(Ordering::Acquire) || !surface_allowed(ui) || live.security_gate.blocked()
    {
        return None;
    }
    let account_id = ui.get_active_account_local_id();
    if account_id.is_empty() {
        return None;
    }
    live.manager
        .account_selection(&account_id)
        .filter(AccountSelection::is_current)
}

/// The session's selection while it is still the account on screen. A
/// departed selection clears the panel.
fn session_selection(ui: &AppWindow) -> Option<AccountSelection> {
    let (selection, admitted) = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        Some((
            session.selection.clone(),
            session.authority.admitted() && session.room_id == ui.get_active_room_id().as_str(),
        ))
    })?;
    let on_screen = ui.get_active_account_local_id().as_str() == selection.account_id();
    if admitted && on_screen && surface_allowed(ui) && ui.global::<RoomAddresses>().get_open() {
        Some(selection)
    } else {
        clear(ui);
        None
    }
}

fn show(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, room_id: &str, name: &str) {
    // Capture the user-click context before any manager lock/task/await.
    let navigation = live.current_navigation_epoch();
    let security = live.security_gate.ringing_generation();
    clear(ui);
    let Some(selection) = current_selection(ui, live) else {
        return;
    };
    if !room_id.starts_with('!')
        || room_id.len() > MAX_SCOPE_ID_BYTES
        || room_id != ui.get_active_room_id().as_str()
    {
        return;
    }
    let cancellation = RoomSettingsCancellation::new();
    let authority = Arc::new(ViewAuthority {
        live: live.clone(),
        selection: selection.clone(),
        navigation,
        security,
        cancellation: cancellation.clone(),
    });
    if !authority.admitted() {
        return;
    }
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            selection,
            room_id: room_id.to_owned(),
            cancellation,
            authority,
            form: None,
            snapshot: None,
            change: None,
        });
    });
    crate::matrix_links::clear(ui);
    crate::thread_browser::withdraw(ui);
    let state = ui.global::<RoomAddresses>();
    state.set_room_name(label(name).into());
    state.set_open(true);
    publish_pending(ui);
    read(ui, live, handle);
}

/// Read the addresses fresh: the first read opens the form.
fn read(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    if ui.global::<RoomAddresses>().get_busy() || ui.global::<RoomAddresses>().get_loading() {
        return;
    }
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let Some((room_id, cancellation, form, authority)) = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        Some((
            session.room_id.clone(),
            session.cancellation.clone(),
            session.form.clone(),
            session.authority.clone(),
        ))
    }) else {
        return;
    };
    let operation = next_operation();
    ui.global::<RoomAddresses>().set_loading(true);
    let manager = live.manager.clone();
    let receipt = receipts().ticket(selection.account_id(), &room_id);
    let weak = ui.as_weak();
    handle.spawn(async move {
        let result = match form {
            Some(form) => form.refresh().await.map(|snapshot| (form, snapshot)),
            None => {
                let opened = manager
                    .open_room_addresses_for(&selection, &room_id, cancellation, Arc::new(move || authority.admitted()))
                    .await
                    .map(Arc::new);
                opened.map(|form| {
                    let snapshot = form.snapshot();
                    (form, snapshot)
                })
            }
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<RoomAddresses>();
            state.set_loading(false);
            match result {
                Ok((form, snapshot)) => {
                    if let Some(receipt) = receipt { receipts().observe(&receipt); }
                    state.set_error("".into());
                    state.set_notice("Read just now: this is the observed state, not proof that an unanswered request finished.".into());
                    show_snapshot(&ui, &snapshot);
                    SESSION.with(|session| {
                        if let Some(session) = session.borrow_mut().as_mut() {
                            session.form = Some(form);
                            session.snapshot = Some(snapshot);
                        }
                    });
                    publish(&ui);
                }
                Err(error) => state.set_error(error.to_string().into()),
            }
            // A departed worker may have finished its retained receipt while
            // this view's refresh failed. Never leave a stale Waiting label.
            publish_pending(&ui);
        });
    });
}

fn show_snapshot(ui: &AppWindow, snapshot: &RoomAddressesSnapshot) {
    let state = ui.global::<RoomAddresses>();
    state.set_own_server(label(&snapshot.own_server).into());
    state.set_summary(summary(snapshot).into());
    state.set_can_publish(snapshot.can_publish);
    state.set_publish_editable(snapshot.publish_editable);
}

fn review_add(ui: &AppWindow, localpart: &str) {
    // The server the core read, not its shown label.
    let Some(own_server) = SESSION.with(|session| {
        let session = session.borrow();
        Some(session.as_ref()?.snapshot.as_ref()?.own_server.clone())
    }) else {
        return;
    };
    match new_local_address(localpart, &own_server) {
        Ok(alias) => review(ui, Change::Add(alias)),
        Err(error) => ui
            .global::<RoomAddresses>()
            .set_error(error.to_string().into()),
    }
}

fn review(ui: &AppWindow, change: Change) {
    let state = ui.global::<RoomAddresses>();
    if state.get_busy() || state.get_unknown() || state.get_reviewing() || state.get_loading() {
        return;
    }
    if session_selection(ui).is_none() {
        return;
    }
    next_operation();
    let main = SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .and_then(|session| session.snapshot.as_ref())
            .and_then(|snapshot| snapshot.addresses.iter().find(|address| address.main))
            .map(|address| address.alias.clone())
    });
    let room_name: String = state.get_room_name().into();
    let (title, lines, confirm) = review_text(&change, &room_name, main.as_deref());
    state.set_error("".into());
    state.set_notice("".into());
    state.set_review_title(title.into());
    let lines = lines
        .into_iter()
        .map(SharedString::from)
        .collect::<Vec<_>>();
    state.set_review_lines(ModelRc::new(VecModel::from(lines)));
    state.set_confirm_label(confirm.into());
    SESSION.with(|session| {
        if let Some(session) = session.borrow_mut().as_mut() {
            session.change = Some(change);
        }
    });
    state.set_reviewing(true);
}

fn confirm(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let state = ui.global::<RoomAddresses>();
    if state.get_busy() || !state.get_reviewing() {
        return;
    }
    if session_selection(ui).is_none() {
        return;
    }
    let Some((form, change)) = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        Some((session.form.clone()?, session.change.clone()?))
    }) else {
        return;
    };
    // The original open authority and the current Confirm click must agree.
    // Captured now, not first sampled after an awaited preflight.
    let navigation = live.current_navigation_epoch();
    let security = live.security_gate.ringing_generation();
    if !surface_allowed(ui)
        || !live.navigation_is_current(navigation)
        || !live.security_gate.ringing_allowed(security)
    {
        clear(ui);
        return;
    }
    let Some((selection, room_id)) = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        (session.authority.navigation == navigation && session.authority.security == security)
            .then(|| (session.selection.clone(), session.room_id.clone()))
    }) else {
        clear(ui);
        return;
    };
    let ticket = {
        let mut store = receipts();
        if !selection.is_current() {
            return;
        }
        match store.reserve(selection.account_id(), &room_id) {
            Ok(ticket) => ticket,
            Err(message) => {
                state.set_error(message.into());
                return;
            }
        }
    };
    let operation = next_operation();
    state.set_busy(true);
    publish_pending(ui);
    let weak = ui.as_weak();
    let mut completion = Completion(Some(ticket));
    handle.spawn(async move {
        let result = match &change {
            Change::Add(alias) => {
                let localpart = alias
                    .trim_start_matches('#')
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                form.add_address(&localpart).await
            }
            Change::Remove(alias) => form.remove_address(alias).await,
            Change::Publish(edit) => form.edit_published(edit.clone()).await,
        };
        completion.finish(&result);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<RoomAddresses>();
            state.set_busy(false);
            state.set_reviewing(false);
            SESSION.with(|session| {
                if let Some(session) = session.borrow_mut().as_mut() {
                    session.change = None;
                }
            });
            let (notice, error, unknown) = outcome_text(&result, &change);
            state.set_notice(notice.into());
            state.set_error(if unknown { "".into() } else { error.into() });
            if let Ok(snapshot) = &result {
                show_snapshot(&ui, snapshot);
                SESSION.with(|session| {
                    if let Some(session) = session.borrow_mut().as_mut() {
                        session.snapshot = Some(snapshot.clone());
                    }
                });
                if let Change::Add(_) = change {
                    state.set_new_local("".into());
                }
                publish(&ui);
            }
            // Never auto-read away an unresolved write. The ledger survives
            // this view and its selection. Read again is observation only.
            publish_pending(&ui);
        });
    });
}

fn publish_pending(ui: &AppWindow) {
    let pending = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        receipts().pending(session.selection.account_id(), &session.room_id)
    });
    let state = ui.global::<RoomAddresses>();
    state.set_unknown(pending.is_some());
    state.set_uncertainty_note(
        match pending {
            Some(Pending::Writing) => WAITING,
            Some(Pending::Unknown) => UNKNOWN,
            Some(Pending::Accepted) => ACCEPTED,
            None => "",
        }
        .into(),
    );
}

/// Show the rows the search matches. Rows update in place, so a refresh
/// keeps the list's position.
fn publish(ui: &AppWindow) {
    let query: String = ui.global::<RoomAddresses>().get_query().into();
    let rows = SESSION.with(|session| {
        let session = session.borrow();
        let snapshot = session.as_ref()?.snapshot.as_ref()?;
        Some(visible(&snapshot.addresses, &query))
    });
    let rows = rows
        .unwrap_or_default()
        .into_iter()
        .map(|(address, detail)| RoomAddressEntry {
            // The exact alias goes back with an action; the label is shown.
            shown: label(&address.alias).into(),
            alias: address.alias.as_str().into(),
            detail: detail.into(),
            local: address.local,
            listed: address.listed,
            main: address.main,
            alternative: address.alternative,
        })
        .collect::<Vec<_>>();
    ROWS.with(|model| sync(model, rows));
}

/// Make `model` hold `rows`, changing only the rows that differ.
fn sync(model: &VecModel<RoomAddressEntry>, rows: Vec<RoomAddressEntry>) {
    let shared = rows.len().min(model.row_count());
    for (index, row) in rows.iter().enumerate().take(shared) {
        if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for row in rows.into_iter().skip(shared) {
        model.push(row);
    }
}
