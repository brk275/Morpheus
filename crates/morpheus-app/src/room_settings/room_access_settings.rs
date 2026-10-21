//! Access and visibility for the open Room settings form: who can read the
//! history, whether guests may join, and whether the account's homeserver
//! lists the room. Every read is fresh and choosing sends nothing. Save,
//! after a review of every change, sends them one at a time on the form's
//! own session, changes that close the room up first and a listing last,
//! and stops at the first that does not succeed, or whose read-back does
//! not show it. Nothing is retried; after any failure the panel needs a
//! refresh, which shows what the room has.
//!
//! A change's result outlives the panel and the form (`Receipts`): closing
//! withdraws a change only before its request is handed over; one already
//! handed over still settles from its own answer. One that ends without a
//! definite answer keeps its setting read-only for as long as Morpheus
//! runs, whatever is hidden, reopened, refreshed or read: no read, not even
//! one showing the requested value, proves that the unanswered request is
//! not still on its way. Only removing the account forgets it. Closing the
//! panel withdraws a read in progress before it hides.
//! Slint values are built on the UI thread only.
use super::{Form, Progress, RoomSettingsUi, settings_error};
use crate::{AppWindow, RoomAccessLine, RoomAccessState};
use morpheus_matrix::{
    CurrentJoinRule, DirectoryListing, GuestAccessChoice, HistoryVisibilityChoice, JoinRuleKind,
    RoomAccessPermissions, RoomAccessSetting, RoomAccessSnapshot, RoomSettingsError,
    RoomSettingsSession,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, MutexGuard},
};

const POISONED: &str = "Room access state poisoned";
const LOADED: &str = "Current settings read from your homeserver.";
const SAVED: &str = "Saved. The settings shown were read back from your homeserver.";
const REFRESH_FIRST: &str = "Refresh before making another change.";
const UNRECOGNIZED: &str =
    "This room uses a value Morpheus does not recognize, so it is read-only.";
const DENIED: &str = " Your permissions in this room do not allow changing it.";
const DIRECTORY_DENIED: &str =
    " Changing it needs permission to change this room's addresses, which you do not have here.";
const WAITING: &str = " An earlier change of this setting is still waiting for its answer. \
     Nothing more is sent for it until that answer is known.";
const UNSETTLED: &str = " An earlier change of this setting got no answer, so it may still \
     apply. Morpheus sends no other change of it until Morpheus restarts.";
/// Receipts kept at most. Each is one account, room and setting; a full
/// book refuses new changes and never evicts one that is unresolved.
const MAX_RECEIPTS: usize = 128;

/// Each setting's choices, in the order the panel shows them: the most
/// private first.
const HISTORY: [HistoryVisibilityChoice; 4] = [
    HistoryVisibilityChoice::Joined,
    HistoryVisibilityChoice::Invited,
    HistoryVisibilityChoice::Shared,
    HistoryVisibilityChoice::WorldReadable,
];
const GUESTS: [GuestAccessChoice; 2] = [GuestAccessChoice::Forbidden, GuestAccessChoice::CanJoin];
const DIRECTORY: [bool; 2] = [false, true];

/// One setting's value: the history, guest access or listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    History(HistoryVisibilityChoice),
    Guests(GuestAccessChoice),
    Directory(bool),
}
impl Change {
    /// Its place in the panel: 0 history, 1 guests, 2 directory.
    fn section(self) -> usize {
        match self {
            Self::History(_) => 0,
            Self::Guests(_) => 1,
            Self::Directory(_) => 2,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::History(_) => "Who can read the history",
            Self::Guests(_) => "Guest access",
            Self::Directory(_) => "Room directory",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::History(choice) => history_label(choice),
            Self::Guests(choice) => guest_label(choice),
            Self::Directory(listed) => directory_label(listed),
        }
    }
    /// How open the room is with it: a higher rank lets more people read or
    /// join it, or find it.
    fn rank(self) -> u8 {
        use HistoryVisibilityChoice as History;
        match self {
            Self::History(History::Joined) => 0,
            Self::History(History::Invited) => 1,
            Self::History(History::Shared) => 2,
            Self::History(History::WorldReadable) => 3,
            Self::Guests(choice) => u8::from(choice == GuestAccessChoice::CanJoin),
            Self::Directory(listed) => u8::from(listed),
        }
    }
}
fn history_label(choice: HistoryVisibilityChoice) -> &'static str {
    use HistoryVisibilityChoice as Choice;
    match choice {
        Choice::Joined => "Members, from when they joined",
        Choice::Invited => "Members, from when they were invited",
        Choice::Shared => "Members, including messages from before they joined",
        Choice::WorldReadable => "Anyone, even without joining",
    }
}
fn guest_label(choice: GuestAccessChoice) -> &'static str {
    match choice {
        GuestAccessChoice::Forbidden => "Guests may not join",
        GuestAccessChoice::CanJoin => "Guests may join",
    }
}
fn directory_label(listed: bool) -> &'static str {
    if listed {
        "Listed in your homeserver's room directory"
    } else {
        "Not listed"
    }
}

/// Each setting's value, `None` when it is unknown or unrecognized.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Values {
    history: Option<HistoryVisibilityChoice>,
    guests: Option<GuestAccessChoice>,
    directory: Option<bool>,
}
impl Values {
    fn get(&self, section: usize) -> Option<Change> {
        match section {
            0 => self.history.map(Change::History),
            1 => self.guests.map(Change::Guests),
            2 => self.directory.map(Change::Directory),
            _ => None,
        }
    }
    fn set(&mut self, change: Change) {
        match change {
            Change::History(choice) => self.history = Some(choice),
            Change::Guests(choice) => self.guests = Some(choice),
            Change::Directory(listed) => self.directory = Some(listed),
        }
    }
}

/// What one read shows. The snapshot itself stays the baseline each change
/// of a save is checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    history: RoomAccessSetting<HistoryVisibilityChoice>,
    guests: RoomAccessSetting<GuestAccessChoice>,
    directory: DirectoryListing,
    permissions: RoomAccessPermissions,
    /// Per section: a known value the account may change.
    editable: [bool; 3],
}
impl Shown {
    fn of(snapshot: &RoomAccessSnapshot) -> Self {
        Self {
            history: snapshot.history,
            guests: snapshot.guests,
            directory: snapshot.directory,
            permissions: snapshot.permissions,
            editable: [
                snapshot.history_editable(),
                snapshot.guests_editable(),
                snapshot.directory_editable(),
            ],
        }
    }
    /// No setting with an unresolved earlier change may be changed now.
    fn paused(mut self, paused: &[Option<Receipt>; 3]) -> Self {
        for (editable, receipt) in self.editable.iter_mut().zip(paused) {
            *editable &= receipt.is_none();
        }
        self
    }
    fn values(&self) -> Values {
        Values {
            history: self.history.value(),
            guests: self.guests.value(),
            directory: match self.directory {
                DirectoryListing::Listed => Some(true),
                DirectoryListing::NotListed => Some(false),
                DirectoryListing::Unsupported | DirectoryListing::Unknown => None,
            },
        }
    }
}

/// One change of a save: the value read, and the one chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Step {
    before: Change,
    after: Change,
}
impl Step {
    fn opens(self) -> bool {
        self.after.rank() > self.before.rank()
    }
}

/// The chosen values that differ from a changeable read value, in the order
/// a save sends them: every change that closes the room up first, unlisting
/// first of all, then every change that opens it, a listing last. A save
/// that stops part way has opened nothing before every closing change went
/// through, and lists the room only once everything else was saved.
fn plan(shown: &Shown, draft: Values) -> Vec<Step> {
    let current = shown.values();
    let mut steps: Vec<Step> = (0..3)
        .filter(|&section| shown.editable[section])
        .filter_map(|section| {
            let before = current.get(section)?;
            let after = draft.get(section)?;
            (before != after).then_some(Step { before, after })
        })
        .collect();
    steps.sort_by_key(|step| {
        let section = step.after.section();
        let order = if step.opens() { section } else { 2 - section };
        (step.opens(), order)
    });
    steps
}

/// `draft` with choice `index` of setting `section`, when that setting may
/// be changed. Anything else changes nothing.
fn choose(shown: &Shown, draft: &mut Values, section: i32, index: i32) -> bool {
    let (Ok(section), Ok(index)) = (usize::try_from(section), usize::try_from(index)) else {
        return false;
    };
    if !shown.editable.get(section).copied().unwrap_or(false) {
        return false;
    }
    let chosen = match section {
        0 => HISTORY.get(index).copied().map(Change::History),
        1 => GUESTS.get(index).copied().map(Change::Guests),
        _ => DIRECTORY.get(index).copied().map(Change::Directory),
    };
    let Some(chosen) = chosen else {
        return false;
    };
    draft.set(chosen);
    true
}

/// A value's place among its setting's choices, -1 for none.
fn index_of(change: Option<Change>) -> i32 {
    let index = match change {
        None => None,
        Some(Change::History(choice)) => HISTORY.iter().position(|v| *v == choice),
        Some(Change::Guests(choice)) => GUESTS.iter().position(|v| *v == choice),
        Some(Change::Directory(listed)) => DIRECTORY.iter().position(|v| *v == listed),
    };
    index.map_or(-1, |index| index as i32)
}

/// What a read says about one setting, and why it cannot be changed.
fn status(section: usize, shown: Option<&Shown>, loading: bool, stale: bool) -> String {
    let Some(shown) = shown else {
        let text = if loading {
            "Reading the room's setting…"
        } else {
            "Not read. Refresh to try again."
        };
        return text.to_owned();
    };
    let lead = if stale { "Last read: " } else { "Current: " };
    let mut text = match section {
        0 => setting_text(shown.history, history_label, lead),
        1 => setting_text(shown.guests, guest_label, lead),
        _ => match shown.directory {
            DirectoryListing::Listed => format!("{lead}{}.", directory_label(true)),
            DirectoryListing::NotListed => format!("{lead}{}.", directory_label(false)),
            DirectoryListing::Unsupported => {
                "Your homeserver does not offer a room directory.".to_owned()
            }
            DirectoryListing::Unknown => {
                "Your homeserver did not say whether this room is listed. Refresh to ask again."
                    .to_owned()
            }
        },
    };
    let (permitted, denied) = match section {
        0 => (shown.permissions.history, DENIED),
        1 => (shown.permissions.guests, DENIED),
        _ => (shown.permissions.directory, DIRECTORY_DENIED),
    };
    if shown.values().get(section).is_some() && !permitted {
        text.push_str(denied);
    }
    text
}
/// Why a setting with an unresolved earlier change cannot change now.
fn paused_note(receipt: Receipt) -> &'static str {
    if receipt.waiting { WAITING } else { UNSETTLED }
}
fn setting_text<T: Copy>(
    setting: RoomAccessSetting<T>,
    label: fn(T) -> &'static str,
    lead: &str,
) -> String {
    match setting {
        RoomAccessSetting::Set(value) => format!("{lead}{}.", label(value)),
        // The protocol default: the room has no such setting of its own.
        RoomAccessSetting::Default(value) => {
            format!("{lead}{} (not set; the default).", label(value))
        }
        RoomAccessSetting::Unrecognized => UNRECOGNIZED.to_owned(),
    }
}

/// What a review shows before anything is sent. A change that opens the
/// room up is marked as a warning.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    title: &'static str,
    before: &'static str,
    after: &'static str,
    note: String,
    /// 0 plain, 1 saved, 2 a warning or an uncertain outcome, 3 not changed.
    tone: i32,
}
fn review_line(step: Step, anyone_can_join: Option<bool>) -> Line {
    use HistoryVisibilityChoice as History;
    let note = match step.after {
        Change::History(History::WorldReadable) => {
            "Anyone, including people who never join, can read the messages sent from now on."
        }
        Change::History(_) if step.opens() => {
            "Members who join later can read more of the messages sent from now on."
        }
        Change::History(_) => {
            "Applies to messages sent from now on. What people could already read stays readable."
        }
        Change::Guests(GuestAccessChoice::CanJoin) if anyone_can_join == Some(true) => {
            "The join rule last read lets anyone in, so guests can join if their homeserver \
             allows guests."
        }
        Change::Guests(GuestAccessChoice::CanJoin) => {
            "Guests still need what the join rule requires, such as an invitation, and a \
             homeserver that offers guest accounts."
        }
        Change::Guests(GuestAccessChoice::Forbidden) => "New guests cannot join.",
        Change::Directory(true) => {
            "Anyone who browses your homeserver's room directory can find this room's name, \
             topic, avatar, address and member count. The join rule still decides who may join."
        }
        Change::Directory(false) => {
            "The room leaves your homeserver's room directory. Copies made elsewhere are not \
             removed."
        }
    };
    Line {
        title: step.after.title(),
        before: step.before.label(),
        after: step.after.label(),
        note: note.to_owned(),
        tone: if step.opens() { 2 } else { 0 },
    }
}
/// What the review asks you to confirm before a change that makes the room
/// readable by anyone or findable in the directory; empty when none does.
fn acknowledgement(steps: &[Step]) -> &'static str {
    let world = Change::History(HistoryVisibilityChoice::WorldReadable);
    let listing = Change::Directory(true);
    let readable = steps.iter().any(|step| step.after == world);
    let listed = steps.iter().any(|step| step.after == listing);
    match (readable, listed) {
        (false, false) => "",
        (true, false) => {
            "I understand that anyone can read the messages sent from now on, without joining."
        }
        (false, true) => {
            "I understand that anyone who browses my homeserver's room directory can find this \
             room's name, topic, avatar, address and member count."
        }
        (true, true) => {
            "I understand that anyone can read the messages sent from now on, without joining, \
             and that anyone who browses my homeserver's room directory can find this room's \
             name, topic, avatar, address and member count."
        }
    }
}

/// What one change of a save did, as far as is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Accepted, and the read-back shows it (with every change before it).
    Saved,
    /// Accepted, but a read-back does not show it: the rest is not sent.
    Unconfirmed,
    Failed(RoomSettingsError),
    /// An earlier change did not succeed, so this one was never sent.
    NotSent,
    /// Not sent: an earlier change of this setting has no known result.
    Paused,
    /// Not sent: too many earlier changes have no known result.
    TooMany,
}

/// A step's outcome from its accepted change's read-back `values`. Every
/// change saved before it must still read back as chosen; one that does
/// not becomes unconfirmed too, so nothing is claimed that the read-back
/// disproves.
fn read_back(steps: &[Step], outcomes: &mut [Outcome], step: Step, values: Values) -> Outcome {
    let shows = |change: Change| values.get(change.section()) == Some(change);
    for (earlier, outcome) in steps.iter().zip(outcomes.iter_mut()) {
        if *outcome == Outcome::Saved && !shows(earlier.after) {
            *outcome = Outcome::Unconfirmed;
        }
    }
    if shows(step.after) {
        Outcome::Saved
    } else {
        Outcome::Unconfirmed
    }
}

/// One account's change of one room's setting (0 history, 1 guests, 2
/// directory).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ReceiptKey {
    account: String,
    room: String,
    section: usize,
}
impl ReceiptKey {
    fn new(account: &str, room: &str, section: usize) -> Self {
        Self {
            account: account.to_owned(),
            room: room.to_owned(),
            section,
        }
    }
}

/// An access change handed (or about to be handed) to a homeserver whose
/// result is not known yet: identifiers and the requested value only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    operation: u64,
    requested: Change,
    /// Its own answer may still come; false once it ended without a
    /// definite one.
    waiting: bool,
}

#[derive(Default)]
struct ReceiptBook {
    operations: u64,
    open: HashMap<ReceiptKey, Receipt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opened {
    Open(u64),
    /// An earlier change of the same setting is unresolved.
    Pending,
    Full,
}

/// Access changes whose result is not known yet, per account, room and
/// setting, kept for the whole process, outside any form or panel, so
/// hiding, reopening, refreshing or switching accounts neither erases one
/// nor makes its setting changeable again. A receipt holds no client or
/// store.
/// - It is opened before its change is handed over.
/// - The change's own definite answer (applied, accepted but not read
///   back, or refused) removes it, and only the receipt that operation
///   opened: an answer for a removed or a newer receipt changes nothing,
///   and nothing reinserts one.
/// - A change that ended without a definite answer stays unresolved for as
///   long as Morpheus runs. No read settles it, whenever it started and
///   whatever it shows: another client or an earlier state can show the
///   requested value while the unanswered request is still on its way, and
///   it could then undo a later opposite change. A change still waiting is
///   likewise settled only by its own answer.
/// - Only removing its account forgets it (`forget_account`).
/// - Unresolved receipts are never evicted; a full book refuses new
///   changes instead. Operation numbers only count up.
#[derive(Default)]
pub(super) struct Receipts(Mutex<ReceiptBook>);

impl Receipts {
    fn book(&self) -> MutexGuard<'_, ReceiptBook> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
    fn open(&self, key: &ReceiptKey, requested: Change) -> Opened {
        let mut book = self.book();
        if book.open.contains_key(key) {
            return Opened::Pending;
        }
        if book.open.len() >= MAX_RECEIPTS {
            return Opened::Full;
        }
        let Some(operation) = book.operations.checked_add(1) else {
            return Opened::Full;
        };
        book.operations = operation;
        let receipt = Receipt {
            operation,
            requested,
            waiting: true,
        };
        book.open.insert(key.clone(), receipt);
        Opened::Open(operation)
    }
    /// The change's own end: a definite answer removes its receipt; none
    /// leaves it unresolved, no longer waiting.
    fn answer(&self, key: &ReceiptKey, operation: u64, definite: bool) {
        let mut book = self.book();
        let Some(receipt) = book.open.get_mut(key) else {
            return;
        };
        if receipt.operation != operation {
            return;
        }
        if definite {
            book.open.remove(key);
        } else {
            receipt.waiting = false;
        }
    }
    /// The account was removed: its receipts go. Operation numbers keep
    /// counting, so a late answer of a forgotten change finds nothing and
    /// never matches a newer one.
    fn forget_account(&self, account: &str) {
        self.book().open.retain(|key, _| key.account != account);
    }
    /// The unresolved receipts of one room, per setting.
    fn paused(&self, account: &str, room: &str) -> [Option<Receipt>; 3] {
        let mut paused = [None; 3];
        for (key, receipt) in &self.book().open {
            if key.account == account && key.room == room && key.section < 3 {
                paused[key.section] = Some(*receipt);
            }
        }
        paused
    }
}

/// The process's one receipt book. Forms and panels come and go and
/// accounts switch; only an account's removal forgets its receipts.
static RECEIPTS: LazyLock<Receipts> = LazyLock::new(Receipts::default);

fn receipts() -> &'static Receipts {
    &RECEIPTS
}

/// An account was removed from this app (signed out or removed): its
/// receipts go. Called where its drafts are forgotten, once the removal
/// succeeded; a switch, a lock or a closed form keeps them.
pub(super) fn forget_account(account: &str) {
    receipts().forget_account(account);
}

/// Answers a receipt however its change's task ends: dropped before its
/// answer, the change stays unresolved, never removed.
struct ReceiptGuard<'a> {
    receipts: &'a Receipts,
    key: ReceiptKey,
    operation: u64,
    answered: bool,
}
impl<'a> ReceiptGuard<'a> {
    fn new(receipts: &'a Receipts, key: ReceiptKey, operation: u64) -> Self {
        Self {
            receipts,
            key,
            operation,
            answered: false,
        }
    }
    fn answer(mut self, definite: bool) {
        self.answered = true;
        self.receipts.answer(&self.key, self.operation, definite);
    }
}
impl Drop for ReceiptGuard<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.receipts.answer(&self.key, self.operation, false);
        }
    }
}
fn result_line(step: Step, outcome: Outcome) -> Line {
    use RoomSettingsError as Error;
    let tone = match outcome {
        Outcome::Saved => 1,
        Outcome::NotSent | Outcome::Paused | Outcome::TooMany => 0,
        Outcome::Unconfirmed | Outcome::Failed(Error::Unknown | Error::SavedNotRead) => 2,
        Outcome::Failed(_) => 3,
    };
    Line {
        note: outcome_note(step, outcome),
        tone,
        ..review_line(step, None)
    }
}
/// Only a definite answer says nothing changed; an unanswered change may
/// have applied and is never sent again.
fn outcome_note(step: Step, outcome: Outcome) -> String {
    use RoomSettingsError as Error;
    let note = match outcome {
        Outcome::Saved => "Saved.",
        // A later change may already have been sent when its read-back
        // disproved this one, so this never says nothing followed it.
        Outcome::Unconfirmed => {
            "Accepted by your homeserver, but the settings read back do not show it. The save \
             stopped when a read-back did not confirm every change, and the remaining changes \
             were not sent; refresh to see what the room has."
        }
        Outcome::NotSent => "Not sent: the save stopped at an earlier change.",
        Outcome::Paused => {
            "Not sent: an earlier change of this setting has no known result. Morpheus sends no \
             other change of it until that change answers, or until Morpheus restarts if it \
             never does."
        }
        Outcome::TooMany => {
            "Not sent: too many earlier changes have no known result. Morpheus sends no more \
             access changes until those changes answer, or until Morpheus restarts if they \
             never do."
        }
        Outcome::Failed(Error::Unknown) => {
            "No answer came back, so it may or may not have changed. It was not sent again; \
             refresh to see what the room has."
        }
        Outcome::Failed(Error::SavedNotRead) => {
            "Saved, but the settings could not be read back. Refresh to see them."
        }
        Outcome::Failed(Error::PermissionDenied) if step.after.section() == 2 => {
            "Not changed: your permissions in this room, or your homeserver, do not allow \
             this listing change."
        }
        Outcome::Failed(Error::PermissionDenied) => {
            "Not changed: your permissions in this room do not allow it."
        }
        Outcome::Failed(Error::BaselineChanged) => {
            "Not changed: it changed since it was read. Refresh before trying again."
        }
        Outcome::Failed(Error::SecurityBusy) => {
            "Not sent: finish the account or security step in progress first."
        }
        Outcome::Failed(error) => return format!("Not changed. {}", settings_error(error)),
    };
    note.to_owned()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Choose,
    Review,
    Saved,
}

/// The panel's state for one Room settings form.
#[derive(Default)]
pub(super) struct Access {
    open: bool,
    /// Kept across closing and reopening, so a late answer to an earlier
    /// read cannot pass for a newer one.
    progress: Progress,
    snapshot: Option<RoomAccessSnapshot>,
    draft: Values,
    mode: Mode,
    lines: Vec<Line>,
    acknowledgement: &'static str,
    acknowledged: bool,
    error: &'static str,
    notice: &'static str,
    /// Per setting, this room's unresolved earlier change (`Receipts`), as
    /// last looked up.
    paused: [Option<Receipt>; 3],
    /// The read in progress, withdrawn when the panel closes.
    reading: Option<tokio::task::AbortHandle>,
}
impl Access {
    /// What the last read shows, with every paused setting read-only.
    fn shown(&self) -> Option<Shown> {
        let shown = Shown::of(self.snapshot.as_ref()?);
        Some(shown.paused(&self.paused))
    }
    /// Withdraw a read in progress: it is stopped, and its answer can no
    /// longer fill this panel or a later one.
    fn withdraw_read(&mut self) {
        if let Some(reading) = self.reading.take() {
            reading.abort();
        }
        if self.progress.busy && self.mode != Mode::Review {
            self.progress.busy = false;
            self.progress.generation = self.progress.generation.saturating_add(1);
        }
    }
    fn choosing(&self) -> bool {
        let progress = &self.progress;
        self.open && self.mode == Mode::Choose && !progress.busy && !progress.refresh_required
    }
    /// Everything but the progress, which keeps its generation.
    fn reset(&mut self, open: bool) {
        let progress = std::mem::take(&mut self.progress);
        *self = Self {
            open,
            progress: Progress {
                refresh_required: false,
                ..progress
            },
            ..Self::default()
        };
    }
}

pub(super) fn register(ui: &AppWindow, owner: &Arc<RoomSettingsUi>) {
    let state = ui.global::<RoomAccessState>();
    state.set_history_options(options(HISTORY.map(history_label)));
    state.set_guest_options(options(GUESTS.map(guest_label)));
    state.set_directory_options(options(DIRECTORY.map(directory_label)));
    let this = owner.clone();
    let weak = ui.as_weak();
    state.on_open_panel(move || {
        if let Some(ui) = weak.upgrade() {
            this.open_access(&ui);
        }
    });
    let this = owner.clone();
    let weak = ui.as_weak();
    state.on_close_panel(move || {
        if let Some(ui) = weak.upgrade() {
            this.close_access(&ui);
        }
    });
    let this = owner.clone();
    let weak = ui.as_weak();
    state.on_action(move |action| {
        if let Some(ui) = weak.upgrade() {
            this.access_action(&ui, action);
        }
    });
    let this = owner.clone();
    let weak = ui.as_weak();
    state.on_choose(move |section, index| {
        if let Some(ui) = weak.upgrade() {
            this.access_choose(&ui, section, index);
        }
    });
    let this = owner.clone();
    let weak = ui.as_weak();
    state.on_acknowledge(move |checked| {
        if let Some(ui) = weak.upgrade() {
            this.access_acknowledge(&ui, checked);
        }
    });
}
fn options<const N: usize>(labels: [&'static str; N]) -> ModelRc<SharedString> {
    let labels: Vec<SharedString> = labels.into_iter().map(SharedString::from).collect();
    ModelRc::new(VecModel::from(labels))
}

/// Closes the panel and forgets what it showed; Room settings calls this
/// whenever its form closes.
pub(super) fn clear(ui: &AppWindow) {
    present(ui, &Access::default());
}

fn present(ui: &AppWindow, access: &Access) {
    let state = ui.global::<RoomAccessState>();
    let shown = access.shown();
    let busy = access.progress.busy;
    let stale = access.progress.refresh_required;
    let choosing = access.mode == Mode::Choose && !busy && !stale;
    let current = shown.map(|shown| shown.values()).unwrap_or_default();
    let editable = |section: usize| choosing && shown.is_some_and(|s| s.editable[section]);
    let read = |section: usize| {
        let mut text = status(section, shown.as_ref(), busy, stale);
        if let Some(receipt) = access.paused[section] {
            text.push_str(paused_note(receipt));
        }
        text
    };
    state.set_open(access.open);
    state.set_busy(busy);
    state.set_loaded(shown.is_some());
    state.set_refresh_required(stale);
    state.set_mode(match access.mode {
        Mode::Choose => 0,
        Mode::Review => 1,
        Mode::Saved => 2,
    });
    state.set_history_index(index_of(access.draft.get(0)));
    state.set_history_current(index_of(current.get(0)));
    state.set_history_editable(editable(0));
    state.set_history_status(read(0).into());
    state.set_guest_index(index_of(access.draft.get(1)));
    state.set_guest_current(index_of(current.get(1)));
    state.set_guest_editable(editable(1));
    state.set_guest_status(read(1).into());
    state.set_directory_index(index_of(access.draft.get(2)));
    state.set_directory_current(index_of(current.get(2)));
    state.set_directory_editable(editable(2));
    state.set_directory_status(read(2).into());
    let changed = shown.is_some_and(|shown| !plan(&shown, access.draft).is_empty());
    state.set_changed(access.mode == Mode::Choose && changed);
    let lines: Vec<RoomAccessLine> = access
        .lines
        .iter()
        .map(|line| RoomAccessLine {
            title: line.title.into(),
            before: line.before.into(),
            after: line.after.into(),
            note: line.note.as_str().into(),
            tone: line.tone,
        })
        .collect();
    state.set_lines(ModelRc::new(VecModel::from(lines)));
    state.set_acknowledgement(access.acknowledgement.into());
    state.set_acknowledged(access.acknowledged);
    state.set_error(access.error.into());
    state.set_notice(access.notice.into());
}

/// Whether the join rule Room settings last read lets anyone join, for the
/// guest-access review only.
fn anyone_can_join(form: &Form) -> Option<bool> {
    let state = form.content.lock().expect("Room settings state poisoned");
    let public = match &state.snapshot.as_ref()?.join_rule {
        CurrentJoinRule::Editable(rule) => rule.kind() == JoinRuleKind::Public,
        // A rule Morpheus does not edit may still let anyone in.
        CurrentJoinRule::ReadOnly(_) => return None,
    };
    Some(public)
}

async fn apply(
    session: &RoomSettingsSession,
    reviewed: &RoomAccessSnapshot,
    change: Change,
) -> Result<RoomAccessSnapshot, RoomSettingsError> {
    match change {
        Change::History(choice) => session.set_history_visibility(reviewed, choice).await,
        Change::Guests(choice) => session.set_guest_access(reviewed, choice).await,
        Change::Directory(listed) => session.set_directory_listing(reviewed, listed).await,
    }
}

impl RoomSettingsUi {
    fn open_access(self: &Arc<Self>, ui: &AppWindow) {
        // Room addresses, open over the same form, keep it until they close
        // (the same mutual admission as Room settings' own open).
        if crate::room_addresses::is_open(ui) {
            return;
        }
        let Some(form) = self.current_for(ui) else {
            return;
        };
        {
            let state = form.content.lock().expect("Room settings state poisoned");
            // Room settings' own work and confirmations come first.
            if state.session.is_none() || state.progress.busy || state.confirmation.is_some() {
                return;
            }
        }
        {
            let mut access = form.access.lock().expect(POISONED);
            if access.open {
                return;
            }
            if access.progress.busy {
                // Work for this panel is still running (closing withdraws a
                // read, so this is rare); its answer fills the panel.
                access.open = true;
                drop(access);
                self.render_access(ui, &form);
                return;
            }
            access.reset(true);
        }
        self.load_access(ui, form);
    }
    fn close_access(&self, ui: &AppWindow) {
        let Some(form) = self.current_for(ui) else {
            clear(ui);
            return;
        };
        let saving = {
            let mut access = form.access.lock().expect(POISONED);
            let saving = access.progress.busy && access.mode == Mode::Review;
            if !saving {
                // A read in progress is withdrawn before the panel hides.
                access.withdraw_read();
                access.reset(false);
            }
            saving
        };
        if saving {
            // Hiding a pending save would hide what it did: stop waiting and
            // close Room settings instead, as its own Close does.
            self.close(ui);
            return;
        }
        clear(ui);
    }
    fn access_action(self: &Arc<Self>, ui: &AppWindow, action: i32) {
        let Some(form) = self.current_for(ui) else {
            return;
        };
        match action {
            0 => self.load_access(ui, form),
            1 => self.review_access(ui, &form),
            2 => self.access_return(ui, &form, Mode::Review),
            3 => self.save_access(ui, form),
            4 => self.access_return(ui, &form, Mode::Saved),
            _ => {}
        }
    }
    /// Back from a review, or done with a save's results: the choices again.
    fn access_return(&self, ui: &AppWindow, form: &Form, from: Mode) {
        let mut access = form.access.lock().expect(POISONED);
        if !access.open || access.progress.busy || access.mode != from {
            return;
        }
        access.mode = Mode::Choose;
        access.lines.clear();
        access.acknowledgement = "";
        access.acknowledged = false;
        drop(access);
        self.render_access(ui, form);
    }
    fn access_choose(&self, ui: &AppWindow, section: i32, index: i32) {
        let Some(form) = self.current_for(ui) else {
            return;
        };
        let paused = self.paused_for(&form);
        let mut access = form.access.lock().expect(POISONED);
        if !access.choosing() {
            return;
        }
        access.paused = paused;
        let Some(shown) = access.shown() else {
            return;
        };
        if choose(&shown, &mut access.draft, section, index) {
            drop(access);
            self.render_access(ui, &form);
        }
    }
    fn access_acknowledge(&self, ui: &AppWindow, checked: bool) {
        let Some(form) = self.current_for(ui) else {
            return;
        };
        let mut access = form.access.lock().expect(POISONED);
        if !access.open || access.mode != Mode::Review || access.progress.busy {
            return;
        }
        access.acknowledged = checked && !access.acknowledgement.is_empty();
        drop(access);
        self.render_access(ui, &form);
    }
    fn review_access(&self, ui: &AppWindow, form: &Form) {
        let anyone = anyone_can_join(form);
        let paused = self.paused_for(form);
        let mut access = form.access.lock().expect(POISONED);
        if !access.choosing() {
            return;
        }
        access.paused = paused;
        let Some(shown) = access.shown() else {
            return;
        };
        let steps = plan(&shown, access.draft);
        if steps.is_empty() {
            return;
        }
        let lines = steps.iter().map(|step| review_line(*step, anyone));
        access.lines = lines.collect();
        access.acknowledgement = acknowledgement(&steps);
        access.acknowledged = false;
        access.mode = Mode::Review;
        access.error = "";
        access.notice = "";
        drop(access);
        self.render_access(ui, form);
    }
    /// A fresh read, which replaces every choice and any review. Refused
    /// while anything is pending.
    fn load_access(self: &Arc<Self>, ui: &AppWindow, form: Arc<Form>) {
        if !self.matches_ui(&form, ui) {
            return;
        }
        let session = form
            .content
            .lock()
            .expect("Room settings state poisoned")
            .session
            .clone();
        let generation = {
            let mut access = form.access.lock().expect(POISONED);
            if !access.open {
                return;
            }
            let Some(generation) = access.progress.begin(true) else {
                return;
            };
            access.mode = Mode::Choose;
            access.lines.clear();
            access.acknowledgement = "";
            access.acknowledged = false;
            access.error = "";
            access.notice = "";
            generation
        };
        self.render_access(ui, &form);
        let this = self.clone();
        let watched = form.clone();
        let reading = self.runtime.spawn(async move {
            let result = tokio::select! { biased;
                _ = form.cancelled() => return,
                result = async {
                    if !this.active(&form) { return Err(RoomSettingsError::Cancelled); }
                    session.ok_or(RoomSettingsError::Unavailable)?.load_access().await
                } => result,
            };
            if !this.active(&form) {
                return;
            }
            // A read settles no earlier change, whatever it shows.
            let mut access = form.access.lock().expect(POISONED);
            if !access.progress.finish(generation, false) {
                return;
            }
            access.reading = None;
            match result {
                Ok(snapshot) => {
                    access.draft = Shown::of(&snapshot).values();
                    access.snapshot = Some(snapshot);
                    access.progress.refresh_required = false;
                    access.notice = LOADED;
                }
                Err(error) => {
                    // Nothing stale stays on screen as if it were current.
                    access.snapshot = None;
                    access.draft = Values::default();
                    access.progress.refresh_required = true;
                    access.error = settings_error(error);
                }
            }
            drop(access);
            this.publish_access(form, generation);
        });
        let mut access = watched.access.lock().expect(POISONED);
        if access.progress.busy && access.progress.generation == generation {
            access.reading = Some(reading.abort_handle());
        }
    }
    /// Sends the reviewed changes in `plan` order, each against the
    /// reviewed read, and stops at the first that does not succeed or whose
    /// read-back does not show it. Each change opens a receipt before it is
    /// handed over; its own answer settles it, whether or not the form is
    /// still open.
    fn save_access(self: &Arc<Self>, ui: &AppWindow, form: Arc<Form>) {
        if !self.matches_ui(&form, ui) {
            return;
        }
        let session = form
            .content
            .lock()
            .expect("Room settings state poisoned")
            .session
            .clone();
        let Some(session) = session else {
            return;
        };
        let paused = self.paused_for(&form);
        let (generation, reviewed, steps) = {
            let mut access = form.access.lock().expect(POISONED);
            let confirmed = access.acknowledgement.is_empty() || access.acknowledged;
            let Some(reviewed) = access.snapshot.clone() else {
                return;
            };
            access.paused = paused;
            let steps = plan(&Shown::of(&reviewed).paused(&paused), access.draft);
            if !access.open || access.mode != Mode::Review || !confirmed || steps.is_empty() {
                return;
            }
            let Some(generation) = access.progress.begin(false) else {
                return;
            };
            access.error = "";
            access.notice = "";
            (generation, reviewed, steps)
        };
        self.render_access(ui, &form);
        let this = self.clone();
        self.runtime.spawn(async move {
            let account = form.selection.account_id().to_owned();
            let room = form.room.clone();
            let receipts = receipts();
            let mut outcomes: Vec<Outcome> = Vec::with_capacity(steps.len());
            let mut latest = None;
            for step in &steps {
                let stopped = outcomes.iter().any(|outcome| *outcome != Outcome::Saved);
                // A closed form sends nothing more. A change already handed
                // over is still waited for: its own answer, not the closing,
                // settles its receipt.
                if stopped || !this.active(&form) {
                    outcomes.push(Outcome::NotSent);
                    continue;
                }
                let key = ReceiptKey::new(&account, &room, step.after.section());
                let operation = match receipts.open(&key, step.after) {
                    Opened::Open(operation) => operation,
                    Opened::Pending => {
                        outcomes.push(Outcome::Paused);
                        continue;
                    }
                    Opened::Full => {
                        outcomes.push(Outcome::TooMany);
                        continue;
                    }
                };
                let receipt = ReceiptGuard::new(receipts, key, operation);
                let result = apply(&session, &reviewed, step.after).await;
                // Only no answer at all leaves the change unresolved.
                receipt.answer(!matches!(result, Err(RoomSettingsError::Unknown)));
                let outcome = match result {
                    Ok(snapshot) => {
                        let values = Shown::of(&snapshot).values();
                        latest = Some(snapshot);
                        read_back(&steps, &mut outcomes, *step, values)
                    }
                    Err(error) => Outcome::Failed(error),
                };
                outcomes.push(outcome);
            }
            // Whatever panel shows this room now reads its receipts again.
            this.render_receipts(&account, &room);
            if !this.active(&form) {
                return;
            }
            let failed = outcomes.iter().any(|outcome| *outcome != Outcome::Saved);
            let mut access = form.access.lock().expect(POISONED);
            if !access.progress.finish(generation, failed) {
                return;
            }
            if let Some(snapshot) = latest {
                access.snapshot = Some(snapshot);
            }
            let shown = access.snapshot.as_ref().map(Shown::of);
            access.draft = shown.map(|shown| shown.values()).unwrap_or_default();
            access.lines.clear();
            for (step, outcome) in steps.iter().zip(outcomes) {
                access.lines.push(result_line(*step, outcome));
            }
            access.mode = Mode::Saved;
            access.acknowledgement = "";
            access.acknowledged = false;
            access.notice = if failed { REFRESH_FIRST } else { SAVED };
            drop(access);
            this.publish_access(form, generation);
        });
    }
    fn publish_access(self: &Arc<Self>, form: Arc<Form>, generation: u64) {
        let this = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let current = form.access.lock().expect(POISONED).progress.generation == generation;
            if current && this.matches_ui(&form, &ui) {
                this.render_access(&ui, &form);
            }
        });
    }
    fn render_access(&self, ui: &AppWindow, form: &Form) {
        let paused = self.paused_for(form);
        let mut access = form.access.lock().expect(POISONED);
        access.paused = paused;
        present(ui, &access);
    }
    /// This form's room's unresolved earlier changes, per setting.
    fn paused_for(&self, form: &Form) -> [Option<Receipt>; 3] {
        let account = form.selection.account_id();
        receipts().paused(account, &form.room)
    }
    /// Show `room`'s receipts again in the panel open for it, if any: one
    /// may have been answered after the form that sent it closed.
    fn render_receipts(self: &Arc<Self>, account: &str, room: &str) {
        let this = self.clone();
        let (account, room) = (account.to_owned(), room.to_owned());
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let Some(form) = this.current_for(&ui) else {
                return;
            };
            let shown = form.access.lock().expect(POISONED).open;
            if shown && form.selection.account_id() == account && form.room == room {
                this.render_access(&ui, &form);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GuestAccessChoice as Guests;
    use HistoryVisibilityChoice as History;
    use RoomAccessSetting as Setting;

    fn shown(editable: [bool; 3]) -> Shown {
        Shown {
            history: Setting::Set(History::Shared),
            guests: Setting::Default(Guests::Forbidden),
            directory: DirectoryListing::NotListed,
            permissions: RoomAccessPermissions {
                history: editable[0],
                guests: editable[1],
                directory: editable[2],
            },
            editable,
        }
    }

    #[test]
    fn a_save_closes_the_room_up_first_and_lists_it_last() {
        let read = shown([true; 3]);
        let mut draft = read.values();
        draft.set(Change::History(History::WorldReadable));
        draft.set(Change::Guests(Guests::CanJoin));
        draft.set(Change::Directory(true));
        let order: Vec<_> = plan(&read, draft).iter().map(|step| step.after).collect();
        let opening = [
            Change::History(History::WorldReadable),
            Change::Guests(Guests::CanJoin),
            Change::Directory(true),
        ];
        assert_eq!(order, opening);
        // Mixed: what closes the room up goes first, unlisting first of all.
        let read = Shown {
            guests: Setting::Set(Guests::CanJoin),
            directory: DirectoryListing::Listed,
            ..shown([true; 3])
        };
        let mut draft = read.values();
        draft.set(Change::History(History::WorldReadable));
        draft.set(Change::Guests(Guests::Forbidden));
        draft.set(Change::Directory(false));
        let order: Vec<_> = plan(&read, draft).iter().map(|step| step.after).collect();
        let mixed = [
            Change::Directory(false),
            Change::Guests(Guests::Forbidden),
            Change::History(History::WorldReadable),
        ];
        assert_eq!(order, mixed);
        // Unchanged or read-only settings are not part of a save.
        assert!(plan(&read, read.values()).is_empty());
        let read_only = Shown {
            editable: [false, true, false],
            ..read
        };
        let steps = plan(&read_only, draft);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].after, Change::Guests(Guests::Forbidden));
        assert!(!steps[0].opens());
    }

    #[test]
    fn only_a_listed_choice_of_a_changeable_setting_is_chosen() {
        let read = shown([true, true, false]);
        let mut draft = read.values();
        assert!(choose(&read, &mut draft, 0, 0));
        assert_eq!(draft.history, Some(History::Joined));
        assert!(choose(&read, &mut draft, 1, 1));
        assert_eq!(draft.guests, Some(Guests::CanJoin));
        let before = draft;
        // Read-only, out of range, negative or unknown: nothing changes.
        for (section, index) in [(2, 1), (0, 4), (1, -1), (-1, 0), (3, 0)] {
            let chosen = choose(&read, &mut draft, section, index);
            assert!(!chosen, "{section} {index}");
            assert_eq!(draft, before);
        }
        assert_eq!(index_of(draft.get(0)), 0);
        assert_eq!(index_of(draft.get(2)), 0);
        assert_eq!(index_of(None), -1);
    }

    #[test]
    fn opening_the_room_up_needs_an_explicit_acknowledgement() {
        let step = |before, after| Step { before, after };
        let readable = step(
            Change::History(History::Shared),
            Change::History(History::WorldReadable),
        );
        let listed = step(Change::Directory(false), Change::Directory(true));
        let guests = step(
            Change::Guests(Guests::Forbidden),
            Change::Guests(Guests::CanJoin),
        );
        let unlisted = step(Change::Directory(true), Change::Directory(false));
        assert_eq!(acknowledgement(&[guests, unlisted]), "");
        assert!(acknowledgement(&[readable]).contains("without joining"));
        let discoverable = acknowledgement(&[listed]);
        assert!(discoverable.contains("name, topic, avatar"));
        assert!(!discoverable.contains("without joining"));
        let both = acknowledgement(&[readable, guests, listed]);
        assert!(both.contains("without joining") && both.contains("name, topic"));
        // Each opening change is marked in the review, with what it means.
        let review = review_line(listed, Some(false));
        assert_eq!(review.tone, 2);
        assert!(review.note.contains("join rule still decides"));
        let review = review_line(guests, Some(false));
        assert!(review.note.contains("what the join rule requires"));
        let review = review_line(guests, Some(true));
        assert!(review.note.contains("lets anyone in"));
        let closing = review_line(unlisted, None);
        assert_eq!(closing.tone, 0);
        assert!(closing.note.contains("not removed"));
        let narrowed = step(
            Change::History(History::Shared),
            Change::History(History::Joined),
        );
        assert!(review_line(narrowed, None).note.contains("stays readable"));
    }

    #[test]
    fn save_results_say_only_what_is_known() {
        use RoomSettingsError as Error;
        let listed = Step {
            before: Change::Directory(false),
            after: Change::Directory(true),
        };
        let unknown = result_line(listed, Outcome::Failed(Error::Unknown));
        assert!(unknown.note.contains("may or may not have changed"));
        assert!(unknown.note.contains("not sent again"));
        assert!(!unknown.note.contains("Not changed"));
        assert_eq!(unknown.tone, 2);
        let read_back = result_line(listed, Outcome::Failed(Error::SavedNotRead));
        assert!(read_back.note.starts_with("Saved"));
        let saved = result_line(listed, Outcome::Saved);
        assert_eq!((saved.note.as_str(), saved.tone), ("Saved.", 1));
        assert_eq!(saved.before, "Not listed");
        let denied = result_line(listed, Outcome::Failed(Error::PermissionDenied));
        assert!(denied.note.contains("or your homeserver"));
        for error in [Error::Refused, Error::RateLimited, Error::SignInExpired] {
            let line = result_line(listed, Outcome::Failed(error));
            assert!(line.note.starts_with("Not changed"), "{error:?}");
            assert_eq!(line.tone, 3);
        }
        let stale = result_line(listed, Outcome::Failed(Error::BaselineChanged));
        let note = stale.note.as_str();
        assert!(note.starts_with("Not changed: it changed since it was read"));
        let busy = result_line(listed, Outcome::Failed(Error::SecurityBusy));
        assert!(busy.note.starts_with("Not sent"));
        let skipped = result_line(listed, Outcome::NotSent);
        assert!(skipped.note.starts_with("Not sent"));
    }

    #[test]
    fn statuses_show_what_was_read_and_why_it_cannot_change() {
        let read = Shown {
            permissions: RoomAccessPermissions {
                history: true,
                guests: false,
                directory: false,
            },
            ..shown([true, false, false])
        };
        let history = status(0, Some(&read), false, false);
        let expected = "Current: Members, including messages from before they joined.";
        assert_eq!(history, expected);
        let guests = status(1, Some(&read), false, false);
        assert!(guests.contains("(not set; the default)") && guests.contains(DENIED));
        let directory = status(2, Some(&read), false, false);
        assert!(directory.starts_with("Current: Not listed.") && directory.contains("addresses"));
        let stale = status(0, Some(&read), false, true);
        assert!(stale.starts_with("Last read: "));
        let unknown = Shown {
            history: Setting::Unrecognized,
            directory: DirectoryListing::Unknown,
            ..read
        };
        assert_eq!(status(0, Some(&unknown), false, false), UNRECOGNIZED);
        let directory = status(2, Some(&unknown), false, false);
        assert!(directory.contains("did not say") && !directory.contains("addresses"));
        let missing = Shown {
            directory: DirectoryListing::Unsupported,
            ..read
        };
        let unsupported = status(2, Some(&missing), false, false);
        assert!(unsupported.contains("does not offer"));
        assert!(status(0, None, true, false).starts_with("Reading"));
        assert!(status(0, None, false, true).contains("Refresh"));
    }

    /// A change's receipt outlives every panel and form, and only its own
    /// definite answer settles it: answers of other operations, a dropped
    /// task and a full book change nothing, and nothing evicts it.
    #[test]
    fn only_a_change_s_own_definite_answer_settles_its_receipt() {
        let receipts = Receipts::default();
        let room = "!room:example.org";
        let joined = Change::History(History::Joined);
        let key = ReceiptKey::new("a", room, 0);
        let Opened::Open(first) = receipts.open(&key, joined) else {
            panic!("a first change opens its receipt");
        };
        // One change of a setting at a time.
        assert_eq!(receipts.open(&key, joined), Opened::Pending);
        let paused = receipts.paused("a", room);
        assert!(paused[0].is_some_and(|receipt| receipt.waiting));
        assert!(paused[1].is_none() && paused[2].is_none());
        assert_eq!(receipts.paused("b", room), [None; 3]);
        assert_eq!(receipts.paused("a", "!other:example.org"), [None; 3]);
        // Another operation's answer changes nothing.
        receipts.answer(&key, first + 1, true);
        assert!(receipts.paused("a", room)[0].is_some_and(|receipt| receipt.waiting));
        // Its own definite answer settles it.
        receipts.answer(&key, first, true);
        assert_eq!(receipts.paused("a", room), [None; 3]);
        // Its late answer touches nothing, not even a newer change.
        let Opened::Open(second) = receipts.open(&key, Change::History(History::Invited)) else {
            panic!("settled: the setting may change again");
        };
        assert!(second > first);
        receipts.answer(&key, first, true);
        assert!(receipts.paused("a", room)[0].is_some());
        receipts.answer(&key, second, true);
        assert_eq!(receipts.paused("a", room), [None; 3]);
        // A task that ends before its answer leaves the change unresolved.
        let Opened::Open(third) = receipts.open(&key, joined) else {
            panic!("free again");
        };
        drop(ReceiptGuard::new(&receipts, key.clone(), third));
        assert!(receipts.paused("a", room)[0].is_some_and(|r| !r.waiting));
        // A full book refuses new changes and evicts nothing.
        for n in 1..MAX_RECEIPTS {
            let room = format!("!r{n}:example.org");
            let other = ReceiptKey::new("a", &room, 1);
            assert!(
                matches!(receipts.open(&other, joined), Opened::Open(_)),
                "{n}"
            );
        }
        let one_more = ReceiptKey::new("a", "!last:example.org", 2);
        assert_eq!(receipts.open(&one_more, joined), Opened::Full);
        assert!(receipts.paused("a", room)[0].is_some(), "never evicted");
    }

    /// RAC44-01: an unanswered change keeps its setting paused for as long
    /// as Morpheus runs. The lifecycle: a change is handed over and gets no
    /// answer while reads of the room overlap it (one begun before the
    /// change opened, one before its end); those reads, a stale generation,
    /// a superseded read, a fresh read, close and reopen, and an account
    /// switch there and back all leave it unresolved; a read showing the
    /// requested value leaves the setting paused, and the opposite choice is
    /// refused without a second request. A read's completion needs a matrix
    /// snapshot this crate cannot build, so the source guard at the end pins
    /// every production path that can change the book.
    #[test]
    fn a_read_never_settles_an_unanswered_change_and_no_second_change_is_sent() {
        let receipts = Receipts::default();
        let room = "!room:example.org";
        let joined = Change::History(History::Joined);
        let key = ReceiptKey::new("a", room, 0);
        // The save's panel and the panel of a later form of the same room.
        let mut saving = Access {
            mode: Mode::Review,
            ..Access::default()
        };
        let mut reading = Access::default();
        // A read of the room begun before the change opened.
        let mut older = Access::default();
        let before = older.progress.begin(true).unwrap();
        let save = saving.progress.begin(false).unwrap();
        // The change opens its receipt and is handed over.
        let Opened::Open(operation) = receipts.open(&key, joined) else {
            panic!("the change opens its receipt");
        };
        let unanswered = ReceiptGuard::new(&receipts, key.clone(), operation);
        // A read of the room starts while the change waits for its answer.
        let early = reading.progress.begin(true).unwrap();
        // The change's own bound ends it with no answer: Unknown.
        unanswered.answer(false);
        assert!(saving.progress.finish(save, true));
        let unknown = receipts.paused("a", room)[0].unwrap();
        assert!(!unknown.waiting);
        assert_eq!(paused_note(unknown), UNSETTLED);
        // Both reads end after it: a read is not the change's answer, so
        // nothing is settled.
        assert!(older.progress.finish(before, false));
        assert!(reading.progress.finish(early, false));
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // A stale generation finishes nothing and settles nothing.
        assert!(!reading.progress.finish(early, false));
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // A read withdrawn by closing, answering late: superseded, it
        // finishes and settles nothing.
        let superseded = reading.progress.begin(true).unwrap();
        reading.withdraw_read();
        assert!(!reading.progress.finish(superseded, false));
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // A fresh read started after the end.
        let fresh = reading.progress.begin(true).unwrap();
        assert!(reading.progress.finish(fresh, false));
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // Close and reopen; switch to another account and back.
        reading.reset(false);
        reading.reset(true);
        assert_eq!(receipts.paused("b", room), [None; 3]);
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // A read showing the requested value, then the opposite choice:
        // not choosable, not planned (although it would be without the
        // pause), and refused by the book, so no second request is sent.
        let base = Shown {
            history: Setting::Set(History::Joined),
            ..shown([true; 3])
        };
        let read = base.paused(&receipts.paused("a", room));
        let mut draft = read.values();
        assert!(!choose(&read, &mut draft, 0, 2));
        let mut wanted = read.values();
        wanted.set(Change::History(History::Shared));
        assert!(plan(&read, wanted).is_empty());
        assert_eq!(plan(&base, wanted).len(), 1, "planned without the pause");
        let opposite = Change::History(History::Shared);
        assert_eq!(receipts.open(&key, opposite), Opened::Pending);
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // A late answer of a newer operation number cannot settle it.
        receipts.answer(&key, operation + 1, true);
        assert_eq!(receipts.paused("a", room)[0], Some(unknown));
        // No production path lets a read settle a receipt: the book changes
        // only in open, answer and forget_account, and outside them only the
        // save opens and answers receipts; the read path never names the
        // book.
        let source = include_str!("room_access_settings.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("reconcile"));
        for change in ["open.insert(", "open.remove(", "open.retain("] {
            assert_eq!(production.matches(change).count(), 1, "{change}");
        }
        let opens = production.matches("receipts.open(").count();
        assert_eq!(opens, 1, "the save only");
        let answer = ".answer(&self.key, self.operation";
        let answers = production.matches(answer).count();
        assert_eq!(answers, 2, "a receipt's own guard only");
        let read_path = production
            .split("fn load_access")
            .nth(1)
            .and_then(|rest| rest.split("fn save_access").next())
            .unwrap();
        assert!(!read_path.contains("receipts"));
        // Nor anywhere else a read's completion reaches (publish_access,
        // render_access, paused_for, present): only the guard and the save
        // answer a receipt, only the book's own methods lock it, and it is
        // named only to define it, forget an account, save and look up.
        let ends = production.matches(".answer(").count();
        assert_eq!(ends, 3, "the guard's two and the save's one");
        let locks = production.matches(".book()").count();
        assert_eq!(locks, 4, "open, answer, forget_account, paused");
        let named = production.matches("receipts()").count();
        assert_eq!(named, 4, "defined, forgotten, saved, looked up");
        assert_eq!(production.matches("&RECEIPTS").count(), 1);
        let lookup = production
            .split("fn paused_for")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .unwrap();
        assert!(lookup.contains("receipts().paused(") && !lookup.contains("answer"));
    }

    /// RAC44-03: the book has no switch, lock or close hook, so receipts
    /// outlive them; removing an account forgets exactly its receipts, a
    /// late completion cannot bring one back or touch a newer change, and a
    /// full book still refuses without evicting.
    #[test]
    fn only_removing_an_account_forgets_its_receipts() {
        let receipts = Receipts::default();
        let room = "!room:example.org";
        let joined = Change::History(History::Joined);
        let mine = ReceiptKey::new("a", room, 0);
        let theirs = ReceiptKey::new("b", room, 0);
        let Opened::Open(gone) = receipts.open(&mine, joined) else {
            panic!("a's change");
        };
        let late = ReceiptGuard::new(&receipts, mine.clone(), gone);
        let Opened::Open(kept) = receipts.open(&theirs, joined) else {
            panic!("b's change");
        };
        receipts.answer(&theirs, kept, false);
        // Reading either account's receipts, as a switch there and back
        // does, forgets nothing.
        assert!(receipts.paused("a", room)[0].is_some());
        assert!(receipts.paused("b", room)[0].is_some());
        // a is removed: only its receipts go.
        receipts.forget_account("a");
        assert_eq!(receipts.paused("a", room), [None; 3]);
        let left = receipts.paused("b", room)[0];
        assert!(left.is_some_and(|receipt| receipt.operation == kept));
        // Its change's late completion brings nothing back.
        late.answer(false);
        assert_eq!(receipts.paused("a", room), [None; 3]);
        receipts.answer(&mine, gone, true);
        assert_eq!(receipts.paused("a", room), [None; 3]);
        // A newer change of the same setting after a reused account: a
        // higher operation number, untouched by the forgotten one's answer.
        let Opened::Open(newer) = receipts.open(&mine, joined) else {
            panic!("free after removal");
        };
        assert!(newer > kept && kept > gone);
        receipts.answer(&mine, gone, true);
        let open = receipts.paused("a", room)[0];
        assert!(open.is_some_and(|receipt| receipt.operation == newer));
        // Saturation still refuses without evicting; removal frees only the
        // removed account's slots.
        let mut n = 0;
        while receipts.book().open.len() < MAX_RECEIPTS {
            n += 1;
            let account = if n % 2 == 0 { "a" } else { "b" };
            let room = format!("!r{n}:example.org");
            let key = ReceiptKey::new(account, &room, 1);
            assert!(matches!(receipts.open(&key, joined), Opened::Open(_)));
        }
        let third = ReceiptKey::new("c", room, 2);
        assert_eq!(receipts.open(&third, joined), Opened::Full);
        let held_by_b = held(&receipts, "b");
        receipts.forget_account("a");
        assert!(matches!(receipts.open(&third, joined), Opened::Open(_)));
        assert_eq!(held(&receipts, "b"), held_by_b, "nothing of b is evicted");
    }

    /// RAC44-03 on the process-wide book production uses: the removal
    /// boundary forgets exactly the removed account's receipts. Only a
    /// complete removal (`if result.complete`) and a successful logout (its
    /// `Ok` arm) call it, beside room permissions' receipts (root's R1); the
    /// draft cleanup a partial removal also runs does not, and no switch,
    /// lock or close path does.
    #[test]
    fn the_production_book_is_forgotten_only_at_account_removal() {
        let room = "!rac03:example.org";
        let joined = Change::History(History::Joined);
        let gone = ReceiptKey::new("rac03-removed", room, 0);
        let kept = ReceiptKey::new("rac03-kept", room, 0);
        let Opened::Open(operation) = receipts().open(&kept, joined) else {
            panic!("the kept account's change");
        };
        receipts().answer(&kept, operation, false);
        assert!(matches!(receipts().open(&gone, joined), Opened::Open(_)));
        super::super::forget_access_account("rac03-removed");
        assert_eq!(receipts().paused("rac03-removed", room), [None; 3]);
        let left = receipts().paused("rac03-kept", room)[0];
        assert!(left.is_some_and(|receipt| receipt.operation == operation));
        receipts().forget_account("rac03-kept");
        // The production callers are the two removal boundaries.
        let live = include_str!("../live.rs");
        assert!(
            !live.contains("forget_access_account"),
            "not the draft cleanup"
        );
        let main = include_str!("../main.rs");
        let access = include_str!("../account_access.rs");
        let parent = include_str!("../room_settings.rs");
        let defined = parent.matches("forget_access_account").count();
        assert_eq!(defined, 1, "defined once");
        // Each removal path calls it once, after its removal succeeded (the
        // logout's Ok arm, the complete removal's block), next to room
        // permissions' cleanup, with no other arm or function between.
        let removal = |source: &str, call: &str, after: &str| -> String {
            assert_eq!(source.matches("forget_access_account(").count(), 1);
            let (before, _) = source.split_once(call).unwrap();
            let (_, arm) = before.rsplit_once(after).unwrap();
            assert!(!arm.contains("=>") && !arm.contains("fn "), "{after}");
            assert!(arm.contains("room_permissions::forget_account("), "{after}");
            arm.to_owned()
        };
        removal(
            main,
            "room_settings::forget_access_account(&account_id)",
            "Ok(result) => {",
        );
        let complete = removal(
            access,
            "crate::room_settings::forget_access_account(&id)",
            "if result.complete {",
        );
        // Still inside the complete removal's block: a partial one keeps them.
        assert!(!complete.contains('}'), "inside `if result.complete`");
        // Here: the method, the module's forget_account and its one call.
        let own = include_str!("room_access_settings.rs");
        let production = own.split("#[cfg(test)]").next().unwrap();
        assert_eq!(production.matches("forget_account(").count(), 3);
    }

    fn held(receipts: &Receipts, account: &str) -> usize {
        let book = receipts.book();
        book.open.keys().filter(|k| k.account == account).count()
    }

    /// A paused setting cannot be chosen or saved, and says why.
    #[test]
    fn a_paused_setting_is_read_only_and_says_why() {
        let read = shown([true; 3]);
        let receipt = Receipt {
            operation: 1,
            requested: Change::Guests(Guests::CanJoin),
            waiting: true,
        };
        let paused = read.paused(&[None, Some(receipt), None]);
        assert_eq!(paused.editable, [true, false, true]);
        let mut draft = paused.values();
        assert!(!choose(&paused, &mut draft, 1, 1));
        let mut wanted = read.values();
        wanted.set(Change::Guests(Guests::CanJoin));
        wanted.set(Change::History(History::Joined));
        let steps = plan(&paused, wanted);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].after, Change::History(History::Joined));
        assert_eq!(paused_note(receipt), WAITING);
        assert!(WAITING.contains("Nothing more is sent"));
        assert!(UNSETTLED.contains("may still apply") && UNSETTLED.contains("restarts"));
    }

    /// Only a read-back that shows a change, and every change saved before
    /// it, counts as saved; anything else stops the rest of the save.
    #[test]
    fn a_save_continues_only_while_its_read_backs_show_every_change() {
        let closing = Step {
            before: Change::Directory(true),
            after: Change::Directory(false),
        };
        let narrowing = Step {
            before: Change::History(History::Shared),
            after: Change::History(History::Joined),
        };
        let steps = [closing, narrowing];
        let both = Values {
            history: Some(History::Joined),
            guests: Some(Guests::Forbidden),
            directory: Some(false),
        };
        let mut outcomes = vec![Outcome::Saved];
        assert_eq!(
            read_back(&steps, &mut outcomes, narrowing, both),
            Outcome::Saved
        );
        assert_eq!(outcomes, [Outcome::Saved]);
        // The read-back shows the old history: not saved.
        let old = Values {
            history: Some(History::Shared),
            ..both
        };
        assert_eq!(
            read_back(&steps, &mut outcomes, narrowing, old),
            Outcome::Unconfirmed
        );
        // It disproves the earlier closing change: that one is not claimed.
        let relisted = Values {
            directory: Some(true),
            ..both
        };
        let mut outcomes = vec![Outcome::Saved];
        let outcome = read_back(&steps, &mut outcomes, narrowing, relisted);
        assert_eq!(outcome, Outcome::Saved);
        assert_eq!(outcomes, [Outcome::Unconfirmed]);
        // The later narrowing was sent (and saved), so the earlier change's
        // line must not say that nothing after it was sent.
        let earlier = result_line(closing, outcomes[0]);
        assert!(!earlier.note.contains("Nothing"), "{}", earlier.note);
        assert!(
            earlier
                .note
                .contains("stopped when a read-back did not confirm every change")
        );
        // An unknown listing confirms nothing.
        let unknown = Values {
            directory: None,
            ..both
        };
        let mut outcomes = Vec::new();
        assert_eq!(
            read_back(&steps, &mut outcomes, closing, unknown),
            Outcome::Unconfirmed
        );
        let line = result_line(closing, Outcome::Unconfirmed);
        assert_eq!(line.tone, 2);
        assert!(line.note.starts_with("Accepted"));
        assert!(line.note.contains("the remaining changes were not sent"));
        assert!(!line.note.contains("Saved"));
        for outcome in [Outcome::Paused, Outcome::TooMany] {
            let line = result_line(closing, outcome);
            assert!(line.note.starts_with("Not sent"), "{outcome:?}");
            assert_eq!(line.tone, 0);
            // A read frees nothing, so no note sends anyone to refresh.
            assert!(!line.note.contains("Refresh"), "{outcome:?}");
            assert!(line.note.contains("restarts"), "{outcome:?}");
        }
    }

    /// Closing the panel withdraws a read in progress before it hides; a
    /// save in progress is not withdrawn here (Stop and close ends it).
    #[test]
    fn closing_withdraws_a_read_so_its_answer_fills_nothing() {
        let mut access = Access::default();
        let reading = access.progress.begin(true).unwrap();
        access.withdraw_read();
        assert!(!access.progress.busy);
        assert!(
            !access.progress.finish(reading, false),
            "a late answer fills nothing"
        );
        assert!(
            access.progress.begin(true).is_some(),
            "a new read may start"
        );
        let mut access = Access {
            mode: Mode::Review,
            ..Access::default()
        };
        let saving = access.progress.begin(false).unwrap();
        access.withdraw_read();
        assert!(access.progress.busy);
        assert!(access.progress.finish(saving, false));
    }

    #[test]
    fn reopening_keeps_the_generation_so_a_late_answer_cannot_pass_for_a_new_one() {
        let mut access = Access::default();
        let first = access.progress.begin(true).unwrap();
        assert!(access.progress.finish(first, true));
        access.reset(true);
        assert!(access.open && !access.progress.refresh_required);
        let second = access.progress.begin(true).unwrap();
        assert_ne!(first, second);
        assert!(!access.progress.finish(first, false));
        assert!(access.progress.finish(second, false));
    }
}
