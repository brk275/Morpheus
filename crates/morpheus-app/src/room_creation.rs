//! The "Rooms and Spaces" dialog's Create room and Create Space modes.
//!
//! Opening a mode captures the exact account selection and the Space shown in
//! the room list. It then reads what the account and homeserver allow
//! (`AccountManager::creation_context_for`): identity, server, the default
//! room version, and whether the account may list rooms in that Space.
//! Every change revalidates the choices into a `CreationPlan`, whose preview
//! is shown as it would be applied.
//!
//! Create runs four steps, each with its own bound:
//! 1. the create request (`LiveUi::create_room_for`; never sent again by
//!    Morpheus, see the core on the token-refresh replay). Waiting for its
//!    turn (`LiveUi::creation_allowed`: an account change holds it) has a
//!    bound of its own, and passing that sends nothing;
//! 2. listing the room in its Space;
//! 3. waiting for it to sync;
//! 4. opening it as an owned reveal (`open_created`), which keeps drafts.
//!    It navigates only for the click that asked for it: the navigation
//!    and the security gate are captured at that click on the UI thread
//!    (`LiveUi::creation_asked`), and the open takes a navigation only while
//!    nothing navigated since, the exact selection is current and no
//!    security step began since. It counts as open only when its own
//!    snapshot was admitted on the UI thread showing exactly that room or
//!    Space, the admission milestone, told before the follow-ups after it
//!    (members, the feed). Its bound withdraws it rather than dropping it.
//!    The withdrawal is linearized with the UI's claim of the admission, so
//!    a snapshot the UI claimed is never reported as refused; an unclaimed
//!    one is never shown, its commit is given back and the reveal stops
//!    every wait at once.
//!
//! A room the homeserver confirmed is shown as created, with its name, as
//! soon as step 1 returns. Nothing in steps 2-4 can turn it into an unknown
//! outcome, and the dialog closes only once the room is placed as planned
//! and open. Otherwise it stays open and says what is true:
//! - created, but its Space placement failed or is unconfirmed (Add to
//!   Space again retries only the idempotent link);
//! - created, but not open yet (not synced, not admitted, or the person
//!   navigated, switched or a security step began since the click). The
//!   room ID is kept, and Open tries steps 3-4 again for that room, under
//!   that new click; nothing is created again;
//! - no reply to the create request: the room may exist, so Create stays
//!   disabled for that account until the person says they checked. This is
//!   kept apart from the dialog, in the bounded journal (Journal):
//!   closing it, switching modes or accounts and opening it again never
//!   forgets it.
//!
//! The journal holds at most one entry per local account and MAX_JOURNAL
//! entries in all. A create reserves its account's entry before anything
//! can be sent, so every outcome that may exist has one; when it cannot
//! (that account's earlier request is still on its way, its unknown
//! outcome is not acknowledged, or the journal is full) nothing is sent.
//! An entry is released when its request settles definitely, by the
//! acknowledgement of an unknown outcome on that still-registered account,
//! or when that account is removed or logged out (forget_account). Nothing
//! is evicted, and a settlement arriving after its account was forgotten
//! changes nothing (it names its reservation's ticket).
//!
//! The create request and every Space link of it are handed to the
//! homeserver only while no security or account step began since their
//! click (`LiveUi::creation_admits`, asked by the core right before each
//! write, as a join's hand-off is). A step running at Create's click
//! refuses it at once; Add to Space again is refused at its hand-off.
//! Refused there, nothing was sent. While the dialog's shared action (an
//! invitation reply or a join) is under way, Create sends nothing, as
//! before.
//!
//! Each open, submit, retry and clear takes a new operation number; a result
//! for an earlier one, or for a departed selection, is dropped. Clearing on
//! account change and detach is synchronous; a close clears once the
//! dialog's open state changes (`RoomCreation.closed`).
//!
//! Protected surfaces (Thread, Leave, the invitations center, More, the
//! unread jump, identity details, the call bar and the rest): the
//! room-actions dialog's one entry, the room list's "+" (app-window's
//! room-action-trigger), is admitted only by room-creation-entry-blocked:
//! never while its own dialog is up or a creation step is under way (begin
//! would clear that attempt: a new operation, its session and its latent
//! open gone, so its late result and room ID would be dropped), and never
//! while an incompatible visible owner is shown (room-creation-blocked:
//! Leave, the thread browser, identity details, the invitations center, a
//! link card, More and the account, security and modal surfaces). The
//! button is disabled then, for its accessibility default action too, and
//! its handler refuses first, before any reset, withdrawal or `begin`, so
//! a refused entry changes nothing. The admission reads no page and no
//! session: the offline preview and the Settings page keep the entry. An
//! admitted entry first withdraws a waiting call bar click, a held link
//! with the unread jump, identity details and the thread browser (with a
//! latent Leave), as the window's other entries do, and only then resets
//! its own values and begins. A created room's open is
//! latent owned navigation of its own (OPEN, held on the UI thread from the
//! click until its attempt finishes). It is held as the window's owned
//! automatic open too (matrix_links::own_automatic_open, as an accepted
//! invitation's open is), so every protected surface's actual entry
//! withdraws it synchronously, before that surface's flag changes, through
//! the link withdrawal each of them already makes (matrix_links::clear).
//! Its authority also refuses, for good, an admission while a newer
//! protected surface is up (`surface_blocked`). So it never navigates under
//! such a surface and never revives when that surface closes; the creation
//! itself keeps its outcome (created, not opened). A finished attempt is
//! released from both, never withdrawn, and lifecycle clears stay `clear`.

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Mutex, MutexGuard, atomic::Ordering},
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, CreatedRoom, CreationAccess, CreationChoices, CreationContext, CreationError,
    CreationKind, CreationPlan, LinkFailure, Placement,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::{
    AppWindow, RoomCreation, RoomCreationLine,
    live::{CreationAsked, LiveUi, NavigationAuthority},
};

// The window assertions (phase 91) run from the real-window suite.
#[cfg(test)]
pub(crate) mod tests;

const CONTEXT_TIMEOUT: Duration = Duration::from_secs(40);
/// Bounds waiting for the create request's turn (LiveUi::creation_allowed);
/// passing it sends nothing.
const ADMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds the create request and the checks before it, nothing after. When
/// it passes, the request may still have reached the homeserver, so it
/// reads as an unknown outcome.
const CREATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Bounds listing a confirmed room in its Space; passing it leaves the
/// placement unconfirmed, never the room.
const LINK_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds waiting for a confirmed room to sync (the core waits 15 s).
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds opening a confirmed, synced room; when it passes, the open is
/// withdrawn (see open_created).
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LABEL_CHARS: usize = 120;

/// The outcome states of RoomCreation.outcome.
pub(crate) const OUTCOME_NONE: i32 = 0;
/// Created; listing it in its Space failed or is unconfirmed.
pub(crate) const OUTCOME_NOT_LINKED: i32 = 1;
/// No reply to the create request: it may exist.
pub(crate) const OUTCOME_UNKNOWN: i32 = 2;
/// Listed in its Space after another try.
pub(crate) const OUTCOME_LINKED: i32 = 3;
/// Created and placed as planned, but not open.
pub(crate) const OUTCOME_NOT_OPENED: i32 = 4;
/// Created; placing, syncing and opening it are still under way.
pub(crate) const OUTCOME_CONFIRMED: i32 = 5;

/// What the dialog shows for the current choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Evaluation {
    pub(crate) preview: Vec<(String, String)>,
    pub(crate) problem: String,
    pub(crate) valid: bool,
}

pub(crate) fn evaluate(choices: &CreationChoices, context: Option<&CreationContext>) -> Evaluation {
    let Some(context) = context else {
        return Evaluation {
            preview: Vec::new(),
            problem: String::new(),
            valid: false,
        };
    };
    match CreationPlan::new(choices, context) {
        Ok(plan) => Evaluation {
            preview: plan
                .preview()
                .into_iter()
                .map(|line| (line.label.to_owned(), line.value))
                .collect(),
            problem: String::new(),
            valid: true,
        },
        Err(error) => Evaluation {
            preview: Vec::new(),
            problem: error.text().to_owned(),
            valid: false,
        },
    }
}

/// What a finished attempt leaves on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    /// Created, placed as planned and open: the dialog closes.
    Done,
    /// The dialog keeps an outcome: its state, text, whether the Space link
    /// may be tried again, and whether the created room may be opened again.
    Outcome {
        state: i32,
        text: String,
        can_relink: bool,
        can_open: bool,
    },
    /// Nothing was created; the choices stay editable.
    Failed(&'static str),
}

/// A link failure that another explicit try may fix.
pub(crate) fn retryable(failure: LinkFailure) -> bool {
    matches!(
        failure,
        LinkFailure::OutcomeUnknown
            | LinkFailure::RateLimited
            | LinkFailure::Unavailable
            | LinkFailure::SecurityBusy
    )
}

/// What a confirmed creation came to, step by step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Confirmed {
    pub(crate) created: CreatedRoom,
    pub(crate) placement: Placement,
    /// It arrived through sync in time to be opened.
    pub(crate) synced: bool,
    /// It was opened in the room list.
    pub(crate) opened: bool,
}

/// How the owned reveal of a created room answered (see
/// LiveUi::open_created_room).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Its own snapshot was admitted on the UI thread showing exactly that
    /// room or Space, and the reveal said so. What follows it (members, the
    /// feed) runs on and does not change this.
    Admitted,
    /// Its bound passed after the UI had claimed the admission: the snapshot
    /// is shown, or being applied right now.
    Claimed,
    /// It is never shown: it ended without an admission (it did not
    /// navigate, or was not admitted), or was withdrawn before the UI
    /// claimed one, and its commit is given back.
    Refused,
}

/// Whether the created room was opened: only by its own snapshot, admitted
/// or claimed by the UI. Matching IDs afterwards, or the follow-ups'
/// results, never count.
pub(crate) fn shows_created(answer: Admission) -> bool {
    matches!(answer, Admission::Admitted | Admission::Claimed)
}

/// What Open tries again for a confirmed room that is not open: that exact
/// room and its current placement. Never anything to create.
pub(crate) fn reopening(done: &Confirmed) -> Option<(CreatedRoom, Placement)> {
    (!done.opened).then(|| (done.created.clone(), done.placement.clone()))
}

/// A create request that did not confirm a room.
pub(crate) fn failed(error: CreationError) -> Finish {
    if error.may_exist() {
        Finish::Outcome {
            state: OUTCOME_UNKNOWN,
            text: format!("{} Your choices are kept.", error.text()),
            can_relink: false,
            can_open: false,
        }
    } else {
        Finish::Failed(error.text())
    }
}

/// A confirmed room: created, whatever happened to its placement or opening.
pub(crate) fn confirmed(done: &Confirmed, space_name: &str) -> Finish {
    let name = label(&done.created.name);
    let unopened = if done.synced {
        "It is not open; Open shows it, or find it in your room list."
    } else {
        "It has not arrived through sync yet; Open tries again, and it appears in your room list when it arrives."
    };
    let can_open = !done.opened;
    match &done.placement {
        Placement::Home | Placement::Linked { .. } if done.opened => Finish::Done,
        Placement::Home => Finish::Outcome {
            state: OUTCOME_NOT_OPENED,
            text: format!("“{name}” was created. {unopened}"),
            can_relink: false,
            can_open,
        },
        Placement::Linked { .. } => Finish::Outcome {
            state: OUTCOME_NOT_OPENED,
            text: format!("“{name}” was created in “{space_name}”. {unopened}"),
            can_relink: false,
            can_open,
        },
        Placement::NotLinked { failure, .. } => Finish::Outcome {
            state: OUTCOME_NOT_LINKED,
            text: format!(
                "“{name}” was created, but {}. {} {}",
                placement_text(*failure, space_name),
                failure.text(),
                if done.opened {
                    "It is open in Home."
                } else {
                    unopened
                }
            ),
            can_relink: retryable(*failure),
            can_open,
        },
    }
}

/// A link that got no reply may or may not have listed the room.
fn placement_text(failure: LinkFailure, space_name: &str) -> String {
    if failure == LinkFailure::OutcomeUnknown {
        format!("its placement in “{space_name}” is unconfirmed")
    } else {
        format!("it was not added to “{space_name}”")
    }
}

/// The outcome of another explicit try at listing the room; `opened`: the
/// room is open already, else Open stays offered.
pub(crate) fn relinked(
    result: Result<(), LinkFailure>,
    name: &str,
    space_name: &str,
    opened: bool,
) -> Finish {
    let name = label(name);
    match result {
        Ok(()) => Finish::Outcome {
            state: OUTCOME_LINKED,
            text: format!("Added “{name}” to “{space_name}”."),
            can_relink: false,
            can_open: !opened,
        },
        Err(failure) => Finish::Outcome {
            state: OUTCOME_NOT_LINKED,
            text: if failure == LinkFailure::OutcomeUnknown {
                format!(
                    "The placement of “{name}” in “{space_name}” is still unconfirmed. {}",
                    failure.text()
                )
            } else {
                format!(
                    "“{name}” is still not in “{space_name}”. {}",
                    failure.text()
                )
            },
            can_relink: retryable(failure),
            can_open: !opened,
        },
    }
}

fn label(text: &str) -> String {
    crate::directory_plain_text(text, MAX_LABEL_CHARS)
}

struct Unlinked {
    space_id: String,
    room_id: String,
    name: String,
    suggested: bool,
}

struct Session {
    selection: AccountSelection,
    /// The Space shown when the mode opened.
    parent: Option<String>,
    context: Option<CreationContext>,
    unlinked: Option<Unlinked>,
    /// The confirmed room once an attempt finished: its exact ID, placement
    /// and whether it opened. Open and Add to Space again use it.
    confirmed: Option<Confirmed>,
}

thread_local! {
    // Slint callbacks and every publication run on the UI thread only.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    static OPERATION: Cell<u64> = const { Cell::new(0) };
    /// The latent open of a creation (submit, reopen): its operation and the
    /// authority it navigates under, from its click until it finishes.
    static OPEN: RefCell<Option<(u64, NavigationAuthority)>> = const { RefCell::new(None) };
}

/// The most accounts the creation journal holds entries for at once.
pub(crate) const MAX_JOURNAL: usize = 32;
/// The journal is full: nothing is evicted, so no other account may create
/// until one of those entries is released.
const JOURNAL_FULL: &str = "Earlier room creations on other accounts are still unresolved. \
    Open Create room on those accounts and confirm you checked their room lists first. \
    Nothing was sent.";
/// This account's earlier create request has not settled yet.
const JOURNAL_IN_FLIGHT: &str =
    "An earlier create request of this account is still on its way. Nothing was sent.";

/// Why a create may not reserve its journal entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The account's last create got no reply, and that was not
    /// acknowledged.
    Unknown,
    /// The account's create request may still be on its way.
    InFlight,
    /// MAX_JOURNAL accounts hold entries; none is evicted.
    Full,
}

#[derive(Debug)]
struct Entry {
    account: String,
    ticket: u64,
    /// No reply arrived: a room may exist. Otherwise a reservation: the
    /// request may be on its way.
    unknown: bool,
}

/// The creation journal: per local account, at most one entry, either a
/// reservation taken before a create request can be handed off and held
/// until that request settles, or an unknown outcome held until the person
/// acknowledges it or the account is forgotten. At most MAX_JOURNAL
/// entries: a create that cannot reserve one is refused before anything is
/// sent, and no entry is ever evicted. A settlement names its
/// reservation's ticket, so one that arrives after its account was
/// forgotten (or for an entry of a newer request) changes nothing and
/// leaves no tombstone. Process memory only: a restart forgets it.
#[derive(Debug)]
pub(crate) struct Journal {
    entries: Vec<Entry>,
    next: u64,
}

impl Journal {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 0,
        }
    }

    /// Reserve `account`'s entry for one create request: its ticket.
    pub(crate) fn reserve(&mut self, account: &str) -> Result<u64, Refusal> {
        if let Some(entry) = self.entries.iter().find(|entry| entry.account == account) {
            return Err(if entry.unknown {
                Refusal::Unknown
            } else {
                Refusal::InFlight
            });
        }
        if self.entries.len() >= MAX_JOURNAL {
            return Err(Refusal::Full);
        }
        self.next = self.next.wrapping_add(1);
        self.entries.push(Entry {
            account: account.to_owned(),
            ticket: self.next,
            unknown: false,
        });
        Ok(self.next)
    }

    /// The request of `ticket` settled: one that may have created a room
    /// keeps the entry as unknown, any other releases it. Whether its
    /// reservation was still held (false once its account was forgotten).
    pub(crate) fn settle(&mut self, account: &str, ticket: u64, may_exist: bool) -> bool {
        let reserved =
            |entry: &Entry| entry.account == account && entry.ticket == ticket && !entry.unknown;
        let Some(index) = self.entries.iter().position(reserved) else {
            return false;
        };
        if may_exist {
            self.entries[index].unknown = true;
        } else {
            self.entries.remove(index);
        }
        true
    }

    /// The person checked `account`'s room list: its unknown outcome is
    /// released. A reservation still on its way never is.
    pub(crate) fn acknowledge(&mut self, account: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !(entry.account == account && entry.unknown));
        self.entries.len() != before
    }

    /// `account` was removed or logged out: its entry goes, whatever it is.
    pub(crate) fn forget(&mut self, account: &str) {
        self.entries.retain(|entry| entry.account != account);
    }

    pub(crate) fn unknown(&self, account: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.account == account && entry.unknown)
    }

    /// How many entries it holds, reserved or unknown.
    pub(crate) fn held(&self) -> usize {
        self.entries.len()
    }
}

/// Process-wide: accounts are forgotten from account tasks, and creations
/// settle in their own tasks.
static JOURNAL: Mutex<Journal> = Mutex::new(Journal::new());

/// Held only for one call; never across another journal call.
fn journal() -> MutexGuard<'static, Journal> {
    JOURNAL.lock().unwrap_or_else(|error| error.into_inner())
}

/// A create request's journal entry, held by its task until the request
/// settles. Dropped unsettled (the task ended early), it settles as
/// unknown: the request may have gone.
struct Reservation {
    account: String,
    ticket: u64,
    settled: bool,
}

impl Reservation {
    fn settle(mut self, may_exist: bool) {
        self.settled = true;
        journal().settle(&self.account, self.ticket, may_exist);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            journal().settle(&self.account, self.ticket, true);
        }
    }
}

/// Complete local removal or a successful logout of `account`, the hooks
/// Leave's records use: its journal entry goes, reserved or unknown, and a
/// request of it still on its way later settles into nothing. A switch, a
/// lock, a closed dialog or a partial removal keeps it.
pub(crate) fn forget_account(account: &str) {
    journal().forget(account);
}

/// Hold `authority` as the latent open of `operation` until it finishes.
/// An older one still held is withdrawn: one open at a time. It is also the
/// window's owned automatic open from now on, so every protected surface's
/// entry (matrix_links::clear) withdraws it for good.
fn install_open(operation: u64, authority: NavigationAuthority) {
    crate::matrix_links::own_automatic_open(authority.clone());
    let older = OPEN.with(|open| open.borrow_mut().replace((operation, authority)));
    if let Some((_, older)) = older {
        older.withdraw();
    }
}

/// `operation` finished (publish_finish): forget its open without
/// withdrawing it, here and as the owned automatic open, so no later entry
/// takes back the follow-ups of a room it showed.
fn release_open(operation: u64) {
    let finished = OPEN.with(|open| {
        let mut open = open.borrow_mut();
        if open.as_ref().is_some_and(|(held, _)| *held == operation) {
            open.take()
        } else {
            None
        }
    });
    if let Some((_, authority)) = finished {
        crate::matrix_links::release_automatic_open(&authority);
    }
}

/// Withdraw the latent open, if there is one; whether there was one still
/// latent. The same authority held as the owned automatic open is withdrawn
/// with it (one shared decision), so a later link withdrawal only finds it
/// withdrawn. Callers: clear, and a close under a step still running. A
/// protected surface's entry reaches the same authority through
/// matrix_links::clear; the slot then holds it already withdrawn, which is
/// no latent open any more.
fn withdraw_open() -> bool {
    let held = OPEN.with(|open| open.borrow_mut().take());
    held.is_some_and(|(_, authority)| {
        let latent = !authority.withdrawn();
        authority.withdraw();
        latent
    })
}

/// Whether a newer protected surface is up, so a created room's open may
/// not be admitted now (LiveUi::creation_authority's blocked_by). It reads
/// app-window's room-creation-blocked, so Rust and Slint use the one
/// predicate: everything Leave counts (the thread browser, identity details
/// and the invitations center among them) except the page and the
/// room-actions dialog the creation runs in, plus Leave itself, a Matrix
/// link card and the header's More menu.
pub(crate) fn surface_blocked(ui: &AppWindow) -> bool {
    ui.get_room_creation_blocked()
}

fn unknown_for(account_id: &str) -> bool {
    journal().unknown(account_id)
}

/// Show an unknown outcome the journal already holds (its request's task
/// settled it first), whatever became of the dialog meanwhile.
fn show_unknown_later(weak: &slint::Weak<AppWindow>) {
    let _ = weak.upgrade_in_event_loop(move |ui| {
        show_unknown(&ui);
    });
}

/// Show the session's account's unknown outcome, unless something else is
/// on screen or under way; whether it has one.
fn show_unknown(ui: &AppWindow) -> bool {
    let account = SESSION.with(|session| {
        let session = session.borrow();
        Some(session.as_ref()?.selection.account_id().to_owned())
    });
    if !account.is_some_and(|account| unknown_for(&account)) {
        return false;
    }
    let state = ui.global::<RoomCreation>();
    if !state.get_busy() && !state.get_relinking() && state.get_outcome() == OUTCOME_NONE {
        state.set_outcome(OUTCOME_UNKNOWN);
        state.set_outcome_text(CreationError::OutcomeUnknown.text().into());
        state.set_can_relink(false);
        state.set_can_open(false);
    }
    true
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

/// Forget the session, its typed choices, context and outcome, and end its
/// latent open, now.
pub(crate) fn clear(ui: &AppWindow) {
    next_operation();
    withdraw_open();
    SESSION.with(|session| *session.borrow_mut() = None);
    reset(&ui.global::<RoomCreation>(), CreationKind::Room);
    ui.global::<RoomCreation>().set_error("".into());
}

fn reset(state: &RoomCreation, kind: CreationKind) {
    let defaults = CreationChoices::new(kind, None);
    state.set_kind(i32::from(kind == CreationKind::Space));
    state.set_name("".into());
    state.set_topic("".into());
    state.set_alias("".into());
    state.set_access(0);
    state.set_listed(defaults.listed);
    state.set_encrypted(defaults.encrypted);
    state.set_in_space(false);
    state.set_suggested(false);
    state.set_context_ready(false);
    state.set_identity("".into());
    state.set_server_name("".into());
    state.set_parent_name("".into());
    state.set_can_place(false);
    state.set_space_members_supported(false);
    state.set_preview(ModelRc::new(VecModel::<RoomCreationLine>::default()));
    state.set_problem("".into());
    state.set_valid(false);
    state.set_busy(false);
    state.set_outcome(OUTCOME_NONE);
    state.set_outcome_text("".into());
    state.set_can_relink(false);
    state.set_relinking(false);
    state.set_can_open(false);
}

fn choices(state: &RoomCreation, parent: Option<&String>) -> CreationChoices {
    let kind = if state.get_kind() == 1 {
        CreationKind::Space
    } else {
        CreationKind::Room
    };
    CreationChoices {
        kind,
        name: state.get_name().into(),
        topic: state.get_topic().into(),
        alias: state.get_alias().into(),
        access: match state.get_access() {
            1 => CreationAccess::SpaceMembers,
            2 => CreationAccess::Public,
            _ => CreationAccess::InviteOnly,
        },
        listed: state.get_listed(),
        encrypted: state.get_encrypted(),
        parent: parent.filter(|_| state.get_in_space()).cloned(),
        suggested: state.get_suggested(),
    }
}

/// The selection of the account on screen, if it may still act.
fn current_selection(ui: &AppWindow, live: &LiveUi) -> Option<AccountSelection> {
    if !live.is_live.load(Ordering::Acquire) || !ui.get_live_session() {
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
/// departed selection clears the session.
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

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let state = ui.global::<RoomCreation>();

    let weak = ui.as_weak();
    let (begin_live, begin_handle) = (live.clone(), handle.clone());
    state.on_begin(move |kind| {
        if let Some(ui) = weak.upgrade() {
            begin(&ui, &begin_live, &begin_handle, kind);
        }
    });

    let weak = ui.as_weak();
    state.on_changed(move || {
        if let Some(ui) = weak.upgrade() {
            refresh(&ui);
        }
    });

    let weak = ui.as_weak();
    let (submit_live, submit_handle) = (live.clone(), handle.clone());
    state.on_submit(move || {
        if let Some(ui) = weak.upgrade() {
            submit(&ui, &submit_live, &submit_handle);
        }
    });

    let weak = ui.as_weak();
    let (relink_live, relink_handle) = (live.clone(), handle.clone());
    state.on_relink(move || {
        if let Some(ui) = weak.upgrade() {
            relink(&ui, &relink_live, &relink_handle);
        }
    });

    let weak = ui.as_weak();
    state.on_open_created(move || {
        if let Some(ui) = weak.upgrade() {
            reopen(&ui, &live, &handle);
        }
    });

    let weak = ui.as_weak();
    state.on_acknowledge(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<RoomCreation>();
            // Only once the form can take the keyboard back (its context
            // was read), so the acknowledgement never removes the focused
            // control with nowhere for it to go.
            if state.get_outcome() == OUTCOME_UNKNOWN
                && !state.get_busy()
                && state.get_context_ready()
            {
                // The person checked, on this still-registered account: its
                // journal entry is released.
                if let Some(selection) = session_selection(&ui) {
                    journal().acknowledge(selection.account_id());
                }
                state.set_outcome(OUTCOME_NONE);
                state.set_outcome_text("".into());
                refresh(&ui);
            }
        }
    });

    let weak = ui.as_weak();
    state.on_done(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let state = ui.global::<RoomCreation>();
        // Never while a step of a creation or a relink is still under way.
        if !state.get_relinking() && !state.get_busy() {
            ui.set_room_actions_open(false);
            clear(&ui);
            ui.invoke_room_creation_finished();
        }
    });

    let weak = ui.as_weak();
    state.on_closed(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.global::<RoomCreation>().get_busy() {
            // Closed under a step still running (an account change, a
            // detach): its session keeps the outcome, but its open, still
            // to come, must not move the window now.
            withdraw_open();
        } else {
            clear(&ui);
        }
    });
}

fn begin(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, kind: i32) {
    let kind = if kind == 1 {
        CreationKind::Space
    } else {
        CreationKind::Room
    };
    clear(ui);
    let state = ui.global::<RoomCreation>();
    reset(&state, kind);
    let Some(selection) = current_selection(ui, live) else {
        state.set_error("Sign in to a Matrix account to create rooms and Spaces.".into());
        return;
    };
    let space = ui.get_active_space_id();
    let parent = (!space.is_empty()).then(|| space.to_string());
    if parent.is_some() {
        state.set_parent_name(label(&ui.get_active_space_name()).into());
    }
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            selection: selection.clone(),
            parent: parent.clone(),
            context: None,
            unlinked: None,
            confirmed: None,
        });
    });
    // An earlier create request of this account got no reply: shown again
    // until the person says they checked.
    show_unknown(ui);
    let operation = next_operation();
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let read = manager.creation_context_for(&selection, parent.as_deref());
        let result = tokio::time::timeout(CONTEXT_TIMEOUT, read)
            .await
            .unwrap_or(Err(CreationError::Unavailable));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<RoomCreation>();
            match result {
                Ok(context) => {
                    state.set_identity(
                        format!("{} on {}", label(&context.user_id), context.server_name).into(),
                    );
                    state.set_server_name(context.server_name.as_str().into());
                    if let Some(parent) = &context.parent {
                        // Display only, under the app's own label policy too;
                        // the session keeps the exact Space ID for actions.
                        state.set_parent_name(label(&parent.name).into());
                        state.set_can_place(parent.can_link);
                        // A room starts in the shown Space when it may.
                        state.set_in_space(parent.can_link);
                    }
                    state.set_space_members_supported(context.space_members_supported);
                    state.set_context_ready(true);
                    // The name field is enabled only now: focus it for this
                    // open and mode alone (a stale context never gets here).
                    state.set_focus_request(state.get_focus_request().wrapping_add(1));
                    SESSION.with(|session| {
                        if let Some(session) = session.borrow_mut().as_mut() {
                            session.context = Some(context);
                        }
                    });
                    refresh(&ui);
                }
                Err(error) => state.set_error(error.text().into()),
            }
        });
    });
}

/// Revalidate the choices and show their preview.
fn refresh(ui: &AppWindow) {
    let state = ui.global::<RoomCreation>();
    SESSION.with(|session| {
        let session = session.borrow();
        let Some(session) = session.as_ref() else {
            return;
        };
        let evaluation = evaluate(
            &choices(&state, session.parent.as_ref()),
            session.context.as_ref(),
        );
        let lines = evaluation
            .preview
            .into_iter()
            .map(|(label, value)| RoomCreationLine {
                label: label.into(),
                value: value.into(),
            })
            .collect::<Vec<_>>();
        state.set_preview(ModelRc::new(VecModel::from(lines)));
        state.set_problem(evaluation.problem.into());
        state.set_valid(evaluation.valid);
    });
}

/// Create what the dialog shows. Also reached from the dialog's original
/// submit callback for modes 0 and 2. Runs on the UI thread: the navigation
/// and security generations of this click are captured here.
pub(crate) fn submit(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let state = ui.global::<RoomCreation>();
    // After an unknown outcome nothing is sent until the person says they
    // checked; a busy attempt is never doubled. The dialog's shared action
    // (an invitation reply or a join) holds Create too, also from the name
    // field's Enter, as the dialog's own submit did for every mode.
    if state.get_busy() || state.get_outcome() != OUTCOME_NONE || ui.get_room_action_busy() {
        return;
    }
    let Some(selection) = session_selection(ui) else {
        state.set_error("Sign in to a Matrix account to create rooms and Spaces.".into());
        return;
    };
    if show_unknown(ui) {
        return;
    }
    // A security or account step running now refuses it before anything
    // is spawned; one beginning later is refused at the hand-off.
    if live.security_gate.blocked() {
        state.set_error(CreationError::SecurityBusy.text().into());
        return;
    }
    let plan = SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        let context = session.context.as_ref()?;
        Some(CreationPlan::new(
            &choices(&state, session.parent.as_ref()),
            context,
        ))
    });
    let plan = match plan {
        Some(Ok(plan)) => plan,
        Some(Err(error)) => {
            state.set_problem(error.text().into());
            return;
        }
        None => return,
    };
    // Its journal entry, reserved before anything can be sent, so an
    // outcome that may exist always has one (Journal). Refused, nothing is
    // sent. From here to the spawn nothing returns: the task settles it.
    let reserved = journal().reserve(selection.account_id());
    let reservation = match reserved {
        Ok(ticket) => Reservation {
            account: selection.account_id().to_owned(),
            ticket,
            settled: false,
        },
        Err(Refusal::Unknown) => {
            show_unknown(ui);
            return;
        }
        Err(Refusal::InFlight) => {
            state.set_error(JOURNAL_IN_FLIGHT.into());
            return;
        }
        Err(Refusal::Full) => {
            state.set_error(JOURNAL_FULL.into());
            return;
        }
    };
    // Its writes and its open go only for this click (see CreationAsked).
    // The open's authority is built and held here, on the UI thread, so a
    // protected entry from now on withdraws it (install_open).
    let asked = live.creation_asked();
    let operation = next_operation();
    let authority = live.creation_authority(asked, &selection);
    install_open(operation, authority.clone());
    state.set_busy(true);
    state.set_error("".into());
    let space_name: String = state.get_parent_name().into();
    let live = live.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        // 1. The create request, once it may go (its own bound: passing it
        // sends nothing). The request's bound covers nothing after it:
        // when it passes, the request may have reached the homeserver.
        let allowed = live.creation_allowed(&selection, asked);
        let allowed = tokio::time::timeout(ADMIT_TIMEOUT, allowed)
            .await
            .unwrap_or(Err(CreationError::Unavailable));
        if let Err(error) = allowed {
            // Nothing was sent: its entry is released.
            reservation.settle(false);
            return publish_finish(&weak, operation, failed(error), None);
        }
        let creating = live.create_room_for(&selection, &plan, asked);
        let outcome = match tokio::time::timeout(CREATE_TIMEOUT, creating).await {
            Ok(result) => result,
            Err(_) => Err(CreationError::OutcomeUnknown),
        };
        // Settled in the journal before anything shows it: an outcome that
        // may exist keeps the entry, as unknown, whatever became of the
        // dialog; a confirmed room or a definite refusal releases it.
        let created = match outcome {
            Ok(created) => {
                reservation.settle(false);
                created
            }
            Err(error) => {
                let may_exist = error.may_exist();
                reservation.settle(may_exist);
                if may_exist {
                    show_unknown_later(&weak);
                }
                return publish_finish(&weak, operation, failed(error), None);
            }
        };
        // Confirmed: shown at once, and nothing below can change that.
        let progress = created.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<RoomCreation>();
            state.set_outcome(OUTCOME_CONFIRMED);
            state.set_outcome_text(
                format!("“{}” was created. Finishing…", label(&progress.name)).into(),
            );
        });
        // 2. Its Space placement, under this click's admission; without a
        // reply it is unconfirmed.
        let admit = live.creation_admits(asked);
        let placing = live
            .manager
            .place_created_room_for(&selection, &created, admit);
        let placement = tokio::time::timeout(LINK_TIMEOUT, placing)
            .await
            .unwrap_or_else(|_| unconfirmed(&created));
        // 3. Its arrival through sync.
        let syncing = live
            .manager
            .await_created_room_for(&selection, &created.room_id);
        let synced = tokio::time::timeout(SYNC_TIMEOUT, syncing)
            .await
            .unwrap_or(false);
        // 4. Opening it, only once it is known locally, as an owned reveal
        // under this click's own navigation.
        let opened = synced
            && open_created(
                &live, &selection, &created, &placement, asked, authority, &weak,
            )
            .await;
        let done = Confirmed {
            created,
            placement,
            synced,
            opened,
        };
        let finish = confirmed(&done, &space_name);
        publish_finish(&weak, operation, finish, Some(done));
    });
}

/// Open a confirmed room as an owned reveal (`LiveUi::open_created_room`)
/// for the click that asked for it (`asked`), under the authority built at
/// that click (LiveUi::creation_authority, held as OPEN), and say whether
/// its snapshot was admitted showing exactly that room or Space.
async fn open_created(
    live: &Arc<LiveUi>,
    selection: &AccountSelection,
    created: &CreatedRoom,
    placement: &Placement,
    asked: CreationAsked,
    authority: NavigationAuthority,
    ui: &slint::Weak<AppWindow>,
) -> bool {
    let (admitted, admission) = tokio::sync::oneshot::channel();
    let reveal = live.clone().open_created_room(
        selection.clone(),
        created.clone(),
        placement.clone(),
        asked,
        authority.clone(),
        admitted,
        ui.clone(),
    );
    shows_created(bounded_reveal(reveal, admission, &authority, OPEN_TIMEOUT).await)
}

/// Run `reveal` as its own task, so no bound can drop it between its commit
/// and its give-back and its follow-ups run on, and await its admission
/// milestone (`admission`: told when its snapshot is admitted, dropped
/// unsent when it never is) for `bound`. Past `bound` the reveal is
/// withdrawn, from this thread. The withdrawal is linearized with the UI's
/// claim of the admission (NavigationAuthority), so its answer decides:
/// already claimed means the snapshot is shown or being applied (Claimed,
/// never Refused); otherwise it is never shown, and the reveal stops every
/// wait and gives its commit back on its own.
pub(crate) async fn bounded_reveal<F>(
    reveal: F,
    mut admission: tokio::sync::oneshot::Receiver<()>,
    authority: &NavigationAuthority,
    bound: Duration,
) -> Admission
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(reveal);
    if let Ok(told) = tokio::time::timeout(bound, &mut admission).await {
        return if told.is_ok() {
            Admission::Admitted
        } else {
            Admission::Refused
        };
    }
    if authority.withdraw() {
        Admission::Claimed
    } else {
        Admission::Refused
    }
}

/// Open the confirmed room again, on request: wait for it to sync if it had
/// not, then the owned reveal, for this new click. Never creates or links
/// anything.
fn reopen(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let state = ui.global::<RoomCreation>();
    if state.get_busy() || state.get_relinking() || !state.get_can_open() {
        return;
    }
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let Some(done) = SESSION.with(|session| session.borrow().as_ref()?.confirmed.clone()) else {
        return;
    };
    let Some((created, placement)) = reopening(&done) else {
        return;
    };
    // The open may navigate only for this click (see CreationAsked), under
    // an authority held from here (see submit).
    let asked = live.creation_asked();
    let operation = next_operation();
    let authority = live.creation_authority(asked, &selection);
    install_open(operation, authority.clone());
    state.set_busy(true);
    state.set_error("".into());
    let space_name: String = state.get_parent_name().into();
    let live = live.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let synced = done.synced || {
            let syncing = live
                .manager
                .await_created_room_for(&selection, &created.room_id);
            tokio::time::timeout(SYNC_TIMEOUT, syncing)
                .await
                .unwrap_or(false)
        };
        let opened = synced
            && open_created(
                &live, &selection, &created, &placement, asked, authority, &weak,
            )
            .await;
        let done = Confirmed {
            created,
            placement,
            synced,
            opened,
        };
        let finish = confirmed(&done, &space_name);
        publish_finish(&weak, operation, finish, Some(done));
    });
}

/// The placement of a room whose link got no reply in time.
fn unconfirmed(created: &CreatedRoom) -> Placement {
    match &created.parent {
        Some(space_id) => Placement::NotLinked {
            space_id: space_id.clone(),
            failure: LinkFailure::OutcomeUnknown,
        },
        None => Placement::Home,
    }
}

/// Show how an attempt finished, if it is still the dialog's attempt.
fn publish_finish(
    weak: &slint::Weak<AppWindow>,
    operation: u64,
    finish: Finish,
    done: Option<Confirmed>,
) {
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if current_operation() != operation || session_selection(&ui).is_none() {
            return;
        }
        // Finished: its open is no longer latent (shown or not), so no entry
        // or close from now on withdraws it.
        release_open(operation);
        let state = ui.global::<RoomCreation>();
        state.set_busy(false);
        match finish {
            Finish::Done => {
                ui.set_room_actions_open(false);
                clear(&ui);
                ui.invoke_room_creation_finished();
            }
            Finish::Outcome {
                state: outcome,
                text,
                can_relink,
                can_open,
            } => {
                if let Some(done) = done {
                    let unlinked = match &done.placement {
                        Placement::NotLinked { space_id, .. } => Some(Unlinked {
                            space_id: space_id.clone(),
                            room_id: done.created.room_id.clone(),
                            name: done.created.name.clone(),
                            suggested: done.created.suggested,
                        }),
                        Placement::Home | Placement::Linked { .. } => None,
                    };
                    // The confirmed room's ID is kept for Open; it is never
                    // created again.
                    SESSION.with(|session| {
                        if let Some(session) = session.borrow_mut().as_mut() {
                            session.unlinked = unlinked;
                            session.confirmed = Some(done);
                        }
                    });
                }
                state.set_outcome(outcome);
                state.set_outcome_text(text.into());
                state.set_can_relink(can_relink);
                state.set_can_open(can_open);
            }
            Finish::Failed(text) => {
                state.set_error(text.into());
                refresh(&ui);
            }
        }
    });
}

/// List the created room in its Space again, on request.
fn relink(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let state = ui.global::<RoomCreation>();
    if state.get_relinking() || state.get_busy() || state.get_outcome() != OUTCOME_NOT_LINKED {
        return;
    }
    let Some(selection) = session_selection(ui) else {
        return;
    };
    let target = SESSION.with(|session| {
        let session = session.borrow();
        let unlinked = session.as_ref()?.unlinked.as_ref()?;
        Some((
            unlinked.space_id.clone(),
            unlinked.room_id.clone(),
            unlinked.name.clone(),
            unlinked.suggested,
        ))
    });
    let Some((space_id, room_id, name, suggested)) = target else {
        return;
    };
    // The write goes only while no security or account step began since
    // this click.
    let admit = live.creation_admits(live.creation_asked());
    let operation = next_operation();
    state.set_relinking(true);
    let space_name: String = state.get_parent_name().into();
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let linking =
            manager.link_created_room_for(&selection, &space_id, &room_id, suggested, admit);
        let result = tokio::time::timeout(LINK_TIMEOUT, linking)
            .await
            .unwrap_or(Err(LinkFailure::OutcomeUnknown));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if current_operation() != operation || session_selection(&ui).is_none() {
                return;
            }
            let state = ui.global::<RoomCreation>();
            state.set_relinking(false);
            // Listed now: Open shows it in its Space.
            let opened = SESSION.with(|session| {
                let mut session = session.borrow_mut();
                let session = session.as_mut()?;
                if result.is_ok() {
                    session.unlinked = None;
                }
                let done = session.confirmed.as_mut()?;
                if result.is_ok() {
                    done.placement = Placement::Linked {
                        space_id: space_id.clone(),
                    };
                }
                Some(done.opened)
            });
            if let Finish::Outcome {
                state: outcome,
                text,
                can_relink,
                can_open,
            } = relinked(result, &name, &space_name, opened.unwrap_or(true))
            {
                state.set_outcome(outcome);
                state.set_outcome_text(text.into());
                state.set_can_relink(can_relink);
                state.set_can_open(can_open);
            }
        });
    });
}
