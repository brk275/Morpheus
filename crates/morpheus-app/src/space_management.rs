//! Manage which existing rooms one joined Space lists, from the Space
//! explorer's "Manage rooms".
//!
//! Opening captures the exact account selection and reads the Space's view
//! (`AccountManager::space_rooms_for`): the account's joined rooms, which of
//! them this Space lists, and whether the account may change that (fresh
//! power levels). Add or Remove first reviews the change
//! (`review_space_link_for`, reads only) and shows what it means, including
//! a public Space listing a room that is not public. Only Confirm writes, and
//! it writes once (`apply_space_link_guarded`) under its captured context.
//! A write whose reply never came is
//! reported as possibly made: its row reads as not known, and that one link
//! is read fresh (`space_link_state_for`) before anything says what it is.
//!
//! What Morpheus knows newer than the account's synced state (a write the
//! homeserver accepted, a fresh read, or a write without a reply) is kept
//! in `MARKS`, across closing and reopening the panel and account switches,
//! until a fresh read of that link replaces it. Synced cache agreement alone
//! never retires a receipt (`overlay`). Opening and Read again read bounded
//! batches of marked links fresh. Full receipt storage refuses new mutations
//! instead of forgetting uncertainty. It never outlives its account:
//! logging out or removing an account drops
//! that account's marks (`forget_account`), and detaching drops all of them
//! (`forget_all`), each at once.
//!
//! Each open, review, confirm, refresh and clear takes a new operation
//! number; a result for an earlier one, or for a departed selection, is
//! dropped. Clearing on account change, detach and close is synchronous.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, LinkIntent, LinkWarning, RoomSettingsCancellation, SpaceLinkChange,
    SpaceManageError, SpaceRoomCandidate, SpaceRoomsView,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, SpaceRoomEntry, SpaceRooms, live::LiveUi};

#[cfg(test)]
mod tests;

const READ_TIMEOUT: Duration = Duration::from_secs(40);
/// Longer than the write's own bound and the reads around it; when it
/// passes, the write may have been sent, so it reads as unknown.
const WRITE_TIMEOUT: Duration = Duration::from_secs(90);
/// Review intents from the panel (SpaceRooms.review).
pub(crate) const REVIEW_ADD: i32 = 0;
pub(crate) const REVIEW_REMOVE: i32 = 1;
pub(crate) const REVIEW_CHECK: i32 = 2;
const MAX_QUERY_CHARS: usize = 64;
const MAX_LABEL_CHARS: usize = 120;
/// Marked links read fresh per open or Read again, at most.
const MAX_FRESH_READS: usize = 16;
/// Receipts are never evicted on the strength of an older synced cache.
/// Saturation refuses new mutations before they are handed to the core.
const MAX_MARKS: usize = 1024;
const MAX_ACCOUNT_MARKS: usize = 256;
const MAX_SPACE_MARKS: usize = 128;
const MAX_SCOPE_ID_BYTES: usize = 1024;
const RECEIPTS_FULL: &str = "Morpheus is tracking its limit of recent Space changes. No new \
    link was sent. Existing links can still be read and checked.";
static NEXT_MARK: AtomicU64 = AtomicU64::new(0);

/// The review shown before Confirm, in plain sentences.
pub(crate) fn review_text(
    add: bool,
    warnings: &[LinkWarning],
    room_name: &str,
    space_name: &str,
) -> (String, Vec<String>, &'static str) {
    let room = label(room_name);
    let space = label(space_name);
    if add {
        let mut lines = vec![format!(
            "“{space}” will list “{room}”, so its members can find it there."
        )];
        lines.push(format!(
            "Only this Space changes: “{room}” stays in any other Space that lists it, keeps its members, and you stay in it."
        ));
        for warning in warnings {
            lines.push(match warning {
                LinkWarning::PublicSpaceListsPrivateRoom => format!(
                    "“{space}” is public: anyone who can see it will see that it lists “{room}”, with its name. That does not let them read or join “{room}” unless its own settings allow it."
                ),
                LinkWarning::MayContainThisSpace => format!(
                    "Morpheus could not check far enough to rule out that “{room}” already contains “{space}”. If it does, the two Spaces would contain each other."
                ),
            });
        }
        (format!("Add “{room}” to “{space}”?"), lines, "Add to Space")
    } else {
        let lines = vec![
            format!("“{space}” will stop listing “{room}”."),
            format!(
                "You stay in “{room}”, nothing is deleted, and any other Space that lists it keeps it."
            ),
        ];
        (
            format!("Remove “{room}” from “{space}”?"),
            lines,
            "Remove from Space",
        )
    }
}

/// What follows a finished write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Then {
    /// Nothing was changed; the view stays.
    Stay,
    /// The Space is read again.
    Read,
    /// The change may or may not have been made: the row reads as not known
    /// and that one link is read fresh.
    Reconcile,
}

/// What a finished write leaves on screen: a notice, an error, and what
/// follows.
pub(crate) fn written(
    result: &Result<bool, SpaceManageError>,
    add: bool,
    room_name: &str,
    space_name: &str,
) -> (String, String, Then) {
    let room = label(room_name);
    let space = label(space_name);
    match result {
        Ok(confirmed) => {
            let mut notice = if add {
                format!("Added “{room}” to “{space}”.")
            } else {
                format!("Removed “{room}” from “{space}”.")
            };
            if !confirmed {
                notice.push_str(" Your homeserver accepted it; it could not be read back yet.");
            }
            (notice, String::new(), Then::Read)
        }
        // Says the link is being read again: Then::Reconcile starts it.
        Err(error) if error.may_have_changed() => {
            (error.text().to_owned(), String::new(), Then::Reconcile)
        }
        Err(error) => (String::new(), error.text().to_owned(), Then::Stay),
    }
}

/// What a fresh read of the one link says after a write without a reply.
pub(crate) fn reconciled(
    state: &Result<bool, SpaceManageError>,
    room_name: &str,
    space_name: &str,
) -> String {
    let room = label(room_name);
    let space = label(space_name);
    match state {
        Ok(true) => format!("Read again just now: “{space}” lists “{room}”."),
        Ok(false) => format!("Read again just now: “{space}” does not list “{room}”."),
        Err(_) => format!(
            "No reply arrived, and the link could not be read again: “{space}” may or may not list “{room}”."
        ),
    }
}

/// The review a row asks for.
pub(crate) fn intent(value: i32) -> Option<LinkIntent> {
    match value {
        REVIEW_ADD => Some(LinkIntent::Add),
        REVIEW_REMOVE => Some(LinkIntent::Remove),
        REVIEW_CHECK => Some(LinkIntent::Check),
        _ => None,
    }
}

/// One row of the search: ID, name, detail, link state (`None`: not known
/// yet) and whether it is a Space.
pub(crate) type VisibleRow = (String, String, String, Option<bool>, bool);

/// The rows the search shows, in name order.
pub(crate) fn visible(candidates: &[SpaceRoomCandidate], query: &str) -> Vec<VisibleRow> {
    let query = query
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .trim()
        .to_lowercase();
    candidates
        .iter()
        .filter(|room| {
            query.is_empty()
                || room.name.to_lowercase().contains(&query)
                || room.room_id.to_lowercase().contains(&query)
        })
        .map(|room| {
            let mut detail = match room.linked {
                Some(true) => "In this Space".to_owned(),
                Some(false) => "Not in this Space".to_owned(),
                // Never shown as absent: a fresh read decides at review.
                None => "Not known yet whether this Space lists it".to_owned(),
            };
            if room.is_space {
                detail.push_str(" · A Space");
            }
            if room.public {
                detail.push_str(" · Public");
            }
            (
                room.room_id.clone(),
                room.name.clone(),
                detail,
                room.linked,
                room.is_space,
            )
        })
        .collect()
}

pub(crate) fn summary(view: &SpaceRoomsView) -> String {
    let linked = view
        .candidates
        .iter()
        .filter(|room| room.linked == Some(true))
        .count();
    let mut text = format!(
        "{} joined rooms · {linked} in this Space",
        view.candidates.len()
    );
    if view.other_children > 0 {
        text.push_str(&format!(
            " · {} more listed that you have not joined",
            view.other_children
        ));
    }
    if view.truncated {
        text.push_str(" · more joined rooms than can be shown");
    }
    if !view.links_complete {
        text.push_str(
            " · some of this Space's links could not be read; rooms marked not known are checked fresh before any change",
        );
    }
    text
}

/// Link states Morpheus knows newer than an account's synced state, by
/// account and Space, then room: `None` after a write without a reply (not
/// known until a fresh read of that link), `Some` once the homeserver
/// accepted a write or a fresh read showed it. Memory only.
type LinkKey = (String, String, String);

#[derive(Clone, Debug)]
struct Mark {
    linked: Option<bool>,
    writing: bool,
    revision: u64,
}

#[derive(Clone, Debug)]
struct ReadMark {
    key: LinkKey,
    revision: u64,
}

#[derive(Clone, Debug)]
struct WriteMark {
    read: ReadMark,
    before: Option<Option<bool>>,
}

#[derive(Debug, Default)]
pub(crate) struct Marks(HashMap<LinkKey, Mark>);

impl Marks {
    /// Reserve uncertainty BEFORE starting work. No empty scope buckets or
    /// unknown/known receipt evictions: full storage refuses new mutations.
    fn reserve(&mut self, account: &str, space: &str, room: &str) -> Option<WriteMark> {
        if [account, space, room]
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_SCOPE_ID_BYTES)
        {
            return None;
        }
        let key = (account.to_owned(), space.to_owned(), room.to_owned());
        let before = self.0.get(&key);
        if before.is_some_and(|mark| mark.writing) {
            return None;
        }
        if before.is_none()
            && (self.0.len() >= MAX_MARKS
                || self
                    .0
                    .keys()
                    .filter(|(owner, _, _)| owner == account)
                    .count()
                    >= MAX_ACCOUNT_MARKS
                || self
                    .0
                    .keys()
                    .filter(|(owner, parent, _)| owner == account && parent == space)
                    .count()
                    >= MAX_SPACE_MARKS)
        {
            return None;
        }
        let revision = next_mark()?;
        let before = before.map(|mark| mark.linked);
        self.0.insert(
            key.clone(),
            Mark {
                linked: None,
                writing: true,
                revision,
            },
        );
        Some(WriteMark {
            read: ReadMark { key, revision },
            before,
        })
    }

    /// Forget every mark of one account.
    pub(crate) fn forget_account(&mut self, account: &str) {
        self.0.retain(|(owner, _, _), _| owner != account);
    }

    fn state(&self, account: &str, space: &str, room: &str) -> Option<Option<bool>> {
        if [account, space, room]
            .iter()
            .any(|id| id.len() > MAX_SCOPE_ID_BYTES)
        {
            return None;
        }
        self.0
            .get(&(account.to_owned(), space.to_owned(), room.to_owned()))
            .map(|mark| mark.linked)
    }

    fn begin_read(&mut self, key: &LinkKey) -> Option<ReadMark> {
        let mark = self.0.get_mut(key)?;
        if mark.writing {
            return None;
        }
        mark.revision = next_mark()?;
        Some(ReadMark {
            key: key.clone(),
            revision: mark.revision,
        })
    }

    /// At most 16 selected keys retained, over an already bounded store. Each new
    /// read generation prevents an earlier read from replacing newer work.
    fn pending(&mut self, account: &str, space: &str) -> Vec<ReadMark> {
        let mut selected = Vec::new();
        for (key, mark) in &self.0 {
            if key.0 != account || key.1 != space || mark.writing {
                continue;
            }
            let candidate = (mark.linked.is_some(), key.clone());
            let index = selected.partition_point(|entry| entry < &candidate);
            if index < MAX_FRESH_READS {
                selected.insert(index, candidate);
                selected.truncate(MAX_FRESH_READS);
            }
        }
        selected
            .into_iter()
            .filter_map(|(_, key)| self.begin_read(&key))
            .collect()
    }

    /// Updates an existing reservation only. A logout/forget can remove it;
    /// a late completion can never recreate it or another selection's key.
    fn finish(&mut self, receipt: &WriteMark, add: bool, result: &Result<bool, SpaceManageError>) {
        let Some(mark) = self.0.get_mut(&receipt.read.key) else {
            return;
        };
        if mark.revision != receipt.read.revision || !mark.writing {
            return;
        }
        let linked = match result {
            Ok(true) => Some(Some(add)),
            Ok(false) => Some(None),
            Err(error) if error.may_have_changed() => Some(None),
            Err(_) => receipt.before,
        };
        match linked {
            Some(linked) => {
                mark.linked = linked;
                mark.writing = false;
            }
            None => {
                self.0.remove(&receipt.read.key);
            }
        }
    }

    fn fresh(&mut self, read: &ReadMark, linked: bool) {
        if let Some(mark) = self.0.get_mut(&read.key)
            && mark.revision == read.revision
            && !mark.writing
        {
            mark.linked = Some(linked);
        }
    }
}

fn next_mark() -> Option<u64> {
    NEXT_MARK
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .ok()
        .and_then(|value| value.checked_add(1))
}

/// Overlay authoritative/uncertain receipts; matching synced values alone
/// never retire knowledge newer than that cache. No read allocates a scope.
pub(crate) fn overlay(view: &mut SpaceRoomsView, marks: &Marks, account: &str) {
    for room in &mut view.candidates {
        if let Some(linked) = marks.state(account, &view.space_id, &room.room_id) {
            room.linked = linked;
        }
    }
}

/// The link state a review's fresh read showed, when it showed one.
pub(crate) fn reviewed_link(result: &Result<SpaceLinkChange, SpaceManageError>) -> Option<bool> {
    match result {
        // An Add is prepared only for an absent link, a Remove for a live one.
        Ok(change) => Some(!change.add),
        Err(SpaceManageError::AlreadyLinked) => Some(true),
        Err(SpaceManageError::NotLinked) => Some(false),
        Err(_) => None,
    }
}

fn label(text: &str) -> String {
    crate::directory_plain_text(text, MAX_LABEL_CHARS)
}

struct Session {
    selection: AccountSelection,
    space_id: String,
    view: Option<SpaceRoomsView>,
    change: Option<SpaceLinkChange>,
}

/// Outlives the session (closing the panel keeps what it learned), never its
/// account. Process-wide, so the account paths (on Tokio) drop it at once.
static MARKS: OnceLock<Mutex<Marks>> = OnceLock::new();

fn marks() -> MutexGuard<'static, Marks> {
    MARKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn reserve_write(
    selection: &AccountSelection,
    space: &str,
    room: &str,
) -> Result<WriteMark, &'static str> {
    let mut store = marks();
    if !selection.is_current() {
        return Err("The selected account changed. Nothing was sent.");
    }
    if [selection.account_id(), space, room]
        .iter()
        .any(|id| id.is_empty() || id.len() > MAX_SCOPE_ID_BYTES)
    {
        return Err("This room or Space cannot be used as a link target. Nothing was sent.");
    }
    let key = (
        selection.account_id().to_owned(),
        space.to_owned(),
        room.to_owned(),
    );
    if store.0.get(&key).is_some_and(|mark| mark.writing) {
        return Err(
            "A previous change to this link is still waiting for a reply. Nothing new was sent.",
        );
    }
    store
        .reserve(selection.account_id(), space, room)
        .ok_or(RECEIPTS_FULL)
}

fn fresh_reads(selection: &AccountSelection, space: &str) -> Vec<ReadMark> {
    let mut store = marks();
    if !selection.is_current() {
        return Vec::new();
    }
    store.pending(selection.account_id(), space)
}

fn fresh_read(selection: &AccountSelection, space: &str, room: &str) -> Option<ReadMark> {
    let mut store = marks();
    if !selection.is_current()
        || [selection.account_id(), space, room]
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_SCOPE_ID_BYTES)
    {
        return None;
    }
    store.begin_read(&(
        selection.account_id().to_owned(),
        space.to_owned(),
        room.to_owned(),
    ))
}

fn record_fresh(selection: &AccountSelection, read: &ReadMark, linked: bool) {
    let mut store = marks();
    // The check shares the cleanup lock; a revoked completion never creates
    // state after logout/removal. Existing-key revision also rejects a read
    // that started before a newer write or read.
    if selection.is_current() && selection.account_id() == read.key.0 {
        store.fresh(read, linked);
    }
}

fn record_write(
    selection: &AccountSelection,
    receipt: &WriteMark,
    add: bool,
    result: &Result<bool, SpaceManageError>,
) {
    let mut store = marks();
    if selection.account_id() != receipt.read.key.0 {
        return;
    }
    let owned = store
        .0
        .get(&receipt.read.key)
        .is_some_and(|mark| mark.revision == receipt.read.revision && mark.writing);
    // A switch may retire the selection after the request was admitted:
    // finish that already-reserved receipt, including known success, while
    // paused. No insertion is possible. Forget removes the capability, and
    // process-unique revisions prevent a later login reusing it.
    if selection.is_current() || owned {
        store.finish(receipt, add, result);
    }
}

/// Drop everything the panel learned about one account's Spaces: it was
/// logged out or removed.
pub(crate) fn forget_account(account_id: &str) {
    marks().forget_account(account_id);
}

/// Drop every account's marks: no account is shown any more (detached).
pub(crate) fn forget_all() {
    *marks() = Marks::default();
}

thread_local! {
    // Slint callbacks and every publication run on the UI thread only.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    static OPERATION: Cell<u64> = const { Cell::new(0) };
    static WRITING: RefCell<Option<WriteOwner>> = const { RefCell::new(None) };
    static ROWS: Rc<VecModel<SpaceRoomEntry>> = Rc::new(VecModel::default());
}

/// The exact operation owns cancellation independently of its Tokio task.
/// Replacing/clearing it, or dropping the UI thread, revokes pre-send work.
struct WriteOwner {
    operation: u64,
    cancellation: RoomSettingsCancellation,
}
impl Drop for WriteOwner {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn own_write(operation: u64) -> RoomSettingsCancellation {
    let cancellation = RoomSettingsCancellation::new();
    WRITING.with(|writing| {
        *writing.borrow_mut() = Some(WriteOwner {
            operation,
            cancellation: cancellation.clone(),
        })
    });
    cancellation
}

fn finish_owner(operation: u64) {
    WRITING.with(|writing| {
        let mut writing = writing.borrow_mut();
        if writing
            .as_ref()
            .is_some_and(|owner| owner.operation == operation)
        {
            *writing = None;
        }
    });
}

/// Captured synchronously at Confirm, never reconstructed after an await.
struct WriteAdmission {
    live: Arc<LiveUi>,
    navigation: u64,
    security: u64,
    cancellation: RoomSettingsCancellation,
}
impl WriteAdmission {
    fn admitted(&self) -> bool {
        !self.cancellation.is_cancelled()
            && self.live.is_live.load(Ordering::Acquire)
            && self.live.navigation_is_current(self.navigation)
            && self.live.security_gate.ringing_allowed(self.security)
    }
}

fn next_operation() -> u64 {
    WRITING.with(|writing| *writing.borrow_mut() = None);
    OPERATION.with(|operation| {
        let next = operation.get().wrapping_add(1);
        operation.set(next);
        next
    })
}

fn current_operation() -> u64 {
    OPERATION.with(Cell::get)
}

/// Forget the panel and every name it showed, now.
pub(crate) fn clear(ui: &AppWindow) {
    clear_session();
    ROWS.with(|rows| rows.set_vec(Vec::new()));
    let state = ui.global::<SpaceRooms>();
    state.set_open(false);
    state.set_space_id("".into());
    state.set_title("".into());
    state.set_can_manage(false);
    state.set_permission("".into());
    state.set_summary("".into());
    state.set_query("".into());
    state.set_loading(false);
    state.set_error("".into());
    state.set_notice("".into());
    state.set_reviewing(false);
    state.set_review_title("".into());
    state.set_review_lines(ModelRc::new(VecModel::<SharedString>::default()));
    state.set_confirm_label("".into());
    state.set_busy(false);
}

fn clear_session() {
    next_operation();
    SESSION.with(|session| *session.borrow_mut() = None);
}

pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<SpaceRooms>().get_open()
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let state = ui.global::<SpaceRooms>();
    state.set_rooms(ROWS.with(|rows| ModelRc::from(rows.clone())));

    let weak = ui.as_weak();
    let (show_live, show_handle) = (live.clone(), handle.clone());
    state.on_show(move |space_id, name| {
        if let Some(ui) = weak.upgrade() {
            show(&ui, &show_live, &show_handle, &space_id, &name);
        }
    });

    let weak = ui.as_weak();
    state.on_filter(move |_| {
        if let Some(ui) = weak.upgrade() {
            publish(&ui);
        }
    });

    let weak = ui.as_weak();
    let (review_live, review_handle) = (live.clone(), handle.clone());
    state.on_review(move |room_id, asked| {
        if let (Some(ui), Some(intent)) = (weak.upgrade(), intent(asked)) {
            review(&ui, &review_live, &review_handle, &room_id, intent);
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
            && !ui.global::<SpaceRooms>().get_busy()
        {
            next_operation();
            SESSION.with(|session| {
                if let Some(session) = session.borrow_mut().as_mut() {
                    session.change = None;
                }
            });
            ui.global::<SpaceRooms>().set_reviewing(false);
        }
    });

    let weak = ui.as_weak();
    state.on_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            // A refresh supersedes any pending read, so its notice goes too.
            let state = ui.global::<SpaceRooms>();
            state.set_notice("".into());
            state.set_error("".into());
            read(&ui, &live, &handle);
        }
    });

    let weak = ui.as_weak();
    state.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            clear(&ui);
        }
    });
    let weak = ui.as_weak();
    state.on_context_changed(move || {
        if let Some(ui) = weak.upgrade() {
            clear(&ui);
        }
    });
}

/// No form opens or confirms behind account access or a protected native
/// presentation. Context hooks retire its operation owner on later changes.
fn surface_allowed(ui: &AppWindow) -> bool {
    !crate::room_addresses::is_open(ui)
        && ui.get_live_session()
        && ui.get_page() == 0
        && !ui.get_show_sign_in()
        && !ui.get_access_open()
        && !ui.get_manage_accounts_open()
        && !ui.get_account_switch_pending()
        && !ui.get_security_busy()
        && ui.get_security_recovery_key().is_empty()
        && !ui.get_sas_busy()
        && ui.get_sas_stage() == 0
        && !crate::member_privacy::is_open(ui)
        && !ui.get_people_open()
        && !ui.get_account_profile_open()
        && !ui.get_account_menu_open()
        && !ui.get_quick_switcher_open()
        && !ui.get_room_actions_open()
        && !ui.get_room_admin_open()
        && !ui.get_member_confirm_open()
        && !ui.get_native_capture_picker_open()
        && !ui.get_leave_call_dialog()
        && !ui.get_image_preview_open()
        && !ui.get_poll_dialog_open()
        && !ui.global::<crate::RoomNotificationState>().get_open()
        && !ui.global::<crate::RoomActivity>().get_seen_open()
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
    let selection = SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .map(|session| session.selection.clone())
    })?;
    let on_screen = ui.get_active_account_local_id().as_str() == selection.account_id();
    if selection.is_current() && on_screen {
        Some(selection)
    } else {
        clear(ui);
        None
    }
}

fn show(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, space_id: &str, name: &str) {
    clear(ui);
    let Some(selection) = current_selection(ui, live) else {
        return;
    };
    if !space_id.starts_with('!') || space_id.len() > MAX_SCOPE_ID_BYTES {
        return;
    }
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            selection,
            space_id: space_id.to_owned(),
            view: None,
            change: None,
        });
    });
    crate::matrix_links::clear(ui);
    let state = ui.global::<SpaceRooms>();
    state.set_space_id(space_id.into());
    state.set_title(label(name).into());
    state.set_open(true);
    read(ui, live, handle);
}

/// Read the Space's view and a bounded batch of existing marked links fresh.
/// A write still in flight is never reconciled by a concurrent read.
fn read(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let Some(space_id) = SESSION.with(|session| Some(session.borrow().as_ref()?.space_id.clone()))
    else {
        return;
    };
    let account = selection.account_id().to_owned();
    let pending = fresh_reads(&selection, &space_id);
    let operation = next_operation();
    let state = ui.global::<SpaceRooms>();
    state.set_loading(true);
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let reading = manager.space_rooms_for(&selection, &space_id);
        let result = tokio::time::timeout(READ_TIMEOUT, reading)
            .await
            .unwrap_or(Err(SpaceManageError::Unavailable));
        // One bound for all the fresh reads; those not done keep their mark.
        if result.is_ok() {
            let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
            for receipt in pending {
                let reading = manager.space_link_state_for(&selection, &space_id, &receipt.key.2);
                let Ok(state) = tokio::time::timeout_at(deadline, reading).await else {
                    break;
                };
                if let Ok(linked) = state {
                    record_fresh(&selection, &receipt, linked);
                }
            }
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<SpaceRooms>();
            state.set_loading(false);
            match result {
                Ok(mut view) => {
                    {
                        let store = marks();
                        if !selection.is_current() {
                            return;
                        }
                        overlay(&mut view, &store, &account);
                    }
                    state.set_title(label(&view.space_name).into());
                    state.set_can_manage(view.can_manage);
                    state.set_permission(
                        if view.can_manage {
                            "You can add your joined rooms to this Space and remove them."
                        } else {
                            "You can see which of your rooms this Space lists, but not change it."
                        }
                        .into(),
                    );
                    state.set_summary(summary(&view).into());
                    SESSION.with(|session| {
                        if let Some(session) = session.borrow_mut().as_mut() {
                            session.view = Some(view);
                        }
                    });
                    publish(&ui);
                }
                Err(error) => state.set_error(error.text().into()),
            }
        });
    });
}

fn review(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, room_id: &str, intent: LinkIntent) {
    let state = ui.global::<SpaceRooms>();
    if state.get_busy() || state.get_reviewing() || !state.get_can_manage() {
        return;
    }
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let Some(space_id) = SESSION.with(|session| Some(session.borrow().as_ref()?.space_id.clone()))
    else {
        return;
    };
    let operation = next_operation();
    state.set_busy(true);
    state.set_error("".into());
    state.set_notice("".into());
    let room_name = room_name(room_id);
    let space_name: String = state.get_title().into();
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    let room_id = room_id.to_owned();
    let receipt = fresh_read(&selection, &space_id, &room_id);
    handle.spawn(async move {
        let reviewing = manager.review_space_link_for(&selection, &space_id, &room_id, intent);
        let result = tokio::time::timeout(READ_TIMEOUT, reviewing)
            .await
            .unwrap_or(Err(SpaceManageError::Unavailable));
        if let (Some(receipt), Some(linked)) = (receipt.as_ref(), reviewed_link(&result)) {
            record_fresh(&selection, receipt, linked);
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<SpaceRooms>();
            state.set_busy(false);
            if let Some(linked) = reviewed_link(&result) {
                // Read fresh just now, so the row shows it.
                mark_link(&ui, &room_id, Some(linked));
            }
            match result {
                Ok(change) => {
                    let (title, lines, confirm) =
                        review_text(change.add, &change.warnings, &room_name, &space_name);
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
                Err(error) => state.set_error(error.text().into()),
            }
        });
    });
}

fn confirm(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    // Capture before any receipt/selection lock or task spawn.
    let navigation = live.current_navigation_epoch();
    let security = live.security_gate.ringing_generation();
    let state = ui.global::<SpaceRooms>();
    if state.get_busy() || !state.get_reviewing() {
        return;
    }
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let Some(change) = SESSION.with(|session| session.borrow().as_ref()?.change.clone()) else {
        return;
    };
    if !live.security_gate.ringing_allowed(security)
        || !live.navigation_is_current(navigation)
        || !surface_allowed(ui)
        || !state.get_open()
    {
        state.set_error(SpaceManageError::SecurityBusy.text().into());
        return;
    }
    let receipt = match reserve_write(&selection, &change.space_id, &change.room_id) {
        Ok(receipt) => receipt,
        Err(message) => {
            state.set_error(message.into());
            return;
        }
    };
    mark_link(ui, &change.room_id, None);
    let operation = next_operation();
    let guard = WriteAdmission {
        live: live.clone(),
        navigation,
        security,
        cancellation: own_write(operation),
    };
    state.set_busy(true);
    let room_name = room_name(&change.room_id);
    let space_name: String = state.get_title().into();
    let (live, task) = (live.clone(), handle.clone());
    let weak = ui.as_weak();
    handle.spawn(async move {
        let writing = async {
            live.manager
                .apply_space_link_guarded(&selection, &change, &guard.cancellation, &|| {
                    guard.admitted()
                })
                .await
                .map(|written| written.confirmed)
        };
        let result = settle_write(writing, |result| {
            record_write(&selection, &receipt, change.add, result)
        })
        .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_owner(operation);
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<SpaceRooms>();
            state.set_busy(false);
            state.set_reviewing(false);
            SESSION.with(|session| {
                if let Some(session) = session.borrow_mut().as_mut() {
                    session.change = None;
                }
            });
            let (notice, error, then) = written(&result, change.add, &room_name, &space_name);
            state.set_notice(notice.into());
            state.set_error(error.into());
            match then {
                Then::Stay => {}
                Then::Read => {
                    // Accepted: shown as written, not as the synced state
                    // from before it, until sync shows it or a fresh read.
                    mark_link(
                        &ui,
                        &change.room_id,
                        result
                            .as_ref()
                            .ok()
                            .and_then(|confirmed| confirmed.then_some(change.add)),
                    );
                    read(&ui, &live, &task);
                }
                Then::Reconcile => {
                    mark_link(&ui, &change.room_id, None);
                    reconcile(&ui, &live, &task, &change.room_id, &room_name, &space_name);
                }
            }
        });
    });
}

/// Finish operation knowledge before returning to presentation ownership.
/// A close may discard that presentation, never this already-reserved result.
async fn settle_write(
    writing: impl std::future::Future<Output = Result<bool, SpaceManageError>>,
    record: impl FnOnce(&Result<bool, SpaceManageError>),
) -> Result<bool, SpaceManageError> {
    let result = tokio::time::timeout(WRITE_TIMEOUT, writing)
        .await
        .unwrap_or(Err(SpaceManageError::OutcomeUnknown));
    record(&result);
    result
}

/// After a write without a reply: read that one link fresh and say what it
/// shows, or that it could not be read.
fn reconcile(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    room_id: &str,
    room_name: &str,
    space_name: &str,
) {
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let Some(space_id) = SESSION.with(|session| Some(session.borrow().as_ref()?.space_id.clone()))
    else {
        return;
    };
    // A later open, review, refresh or close supersedes it; each of those
    // replaces or clears the pending notice.
    let operation = next_operation();
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    let (room_id, room_name, space_name) = (
        room_id.to_owned(),
        room_name.to_owned(),
        space_name.to_owned(),
    );
    let receipt = fresh_read(&selection, &space_id, &room_id);
    handle.spawn(async move {
        let reading = manager.space_link_state_for(&selection, &space_id, &room_id);
        let state = tokio::time::timeout(READ_TIMEOUT, reading)
            .await
            .unwrap_or(Err(SpaceManageError::Unavailable));
        if let (Some(receipt), Ok(linked)) = (receipt.as_ref(), &state) {
            record_fresh(&selection, receipt, *linked);
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            if let Ok(linked) = state {
                mark_link(&ui, &room_id, Some(linked));
            }
            let notice = reconciled(&state, &room_name, &space_name);
            ui.global::<SpaceRooms>().set_notice(notice.into());
        });
    });
}

/// Project one link after its operation knowledge was recorded independently.
/// An outstanding write's Unknown always overrides an older read result.
fn mark_link(ui: &AppWindow, room_id: &str, linked: Option<bool>) {
    let text = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let session = session.as_mut()?;
        let linked = {
            let store = marks();
            if !session.selection.is_current() {
                return None;
            }
            store
                .state(session.selection.account_id(), &session.space_id, room_id)
                .unwrap_or(linked)
        };
        let view = session.view.as_mut()?;
        if let Some(room) = view
            .candidates
            .iter_mut()
            .find(|room| room.room_id == room_id)
        {
            room.linked = linked;
        }
        Some(summary(view))
    });
    if let Some(text) = text {
        ui.global::<SpaceRooms>().set_summary(text.into());
    }
    publish(ui);
}

/// The shown name of a candidate, else its ID.
fn room_name(room_id: &str) -> String {
    SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .and_then(|session| session.view.as_ref())
            .and_then(|view| view.candidates.iter().find(|room| room.room_id == room_id))
            .map_or_else(|| room_id.to_owned(), |room| room.name.clone())
    })
}

/// Show the rows the search matches. Rows update in place, so a refresh
/// keeps the list's position.
fn publish(ui: &AppWindow) {
    let query: String = ui.global::<SpaceRooms>().get_query().into();
    let rows = SESSION.with(|session| {
        let session = session.borrow();
        let view = session.as_ref()?.view.as_ref()?;
        Some(visible(&view.candidates, &query))
    });
    let rows = rows
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name, detail, linked, is_space)| SpaceRoomEntry {
            initials: initials(&name),
            id: id.into(),
            name: name.into(),
            detail: detail.into(),
            linked: linked == Some(true),
            link_known: linked.is_some(),
            is_space,
        })
        .collect::<Vec<_>>();
    ROWS.with(|model| sync(model, rows));
}

fn initials(name: &str) -> SharedString {
    name.chars()
        .find(|ch| ch.is_alphanumeric())
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "#".into())
        .into()
}

/// Make `model` hold `rows`, changing only the rows that differ.
fn sync(model: &VecModel<SpaceRoomEntry>, rows: Vec<SpaceRoomEntry>) {
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
