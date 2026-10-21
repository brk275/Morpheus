//! Room and Space invitations: the Home cards and the invitations center.
//!
//! The center lists the visible account's invitations as the SDK holds them
//! from sync (`AccountManager::invitations_selected`; no request is sent),
//! searchable, with only the summaries their invite state supplied. Both the
//! cards and the center answer through `LiveUi::respond_to_invite`, the one
//! fenced answer path; nothing joins or declines without a deliberate choice.
//!
//! Fences:
//! - Every listing and answer captures the account's `AccountSelection` on
//!   the UI thread; a result is used only while that exact selection is
//!   current and the account is still the one on screen, so a switch from A
//!   to B and back to A drops every older result.
//! - Each listed invitation has a generation, kept while the same selection
//!   lists it again and new for every other listing. A choice echoes it; a
//!   row from an older listing or selection answers nothing.
//! - One answer per invitation until its effect shows here. Each admitted
//!   answer holds a guard for its account and room (never the room alone)
//!   that records its exact admission. The guard blocks every other answer
//!   for that account's room, from any path and under any later selection,
//!   while the answer is out, once the homeserver confirmed it but sync has
//!   not brought it, and once it got no answer and may still apply. Only a
//!   read of that account's own store showing the membership the answer
//!   asked for retires it, or an answer that was not applied at all (nothing
//!   sent, or refused). A listing's absence never does (a listing is
//!   bounded, and another account's never names the room), nor does a
//!   timer. At most `MAX_ANSWER_GUARDS` are held; past that a new answer is
//!   refused rather than an unresolved guard dropped.
//! - After an answer the list is read again, so it shows the real state.
//! - An answer is handed over only while what it was chosen under still
//!   holds (`Admission`, asked by the core right before the hand-off): the
//!   session is live, no security step is in progress or began since, and a
//!   center answer's view has not closed since. Otherwise nothing is sent,
//!   its guard is retired and the notice says why. The account transition
//!   it waits for first is bounded (`LiveUi::respond_to_invite`); past it,
//!   nothing is sent either. A Home card's accept opens the room only from
//!   the navigation it was chosen in.
//! - An accepted direct-message invitation's `m.direct` listing is part of
//!   that join: closing the view does not stop it, but the end of the
//!   session or a security step does. Its outcome is shown with the
//!   answer's: not listed (and why), or possibly listed with no answer.
//!   Nothing lists it again by itself, also after reopening the center.
//! - A listing whose write got no answer is kept apart from the screen and
//!   from the answer's guard (`DirectReceipt`, by account, room and the
//!   answer's own number), whatever the view or the account on screen did
//!   meanwhile, and the center names it again whenever it opens for that
//!   account. No membership sync and no account-data read counts as its
//!   answer, and nothing writes it again. These receipts count toward
//!   `MAX_ANSWER_GUARDS` and are never dropped.
//! - A late answer's notice never replaces a newer answer's: once an
//!   answer's finish set the notice, an older answer's finish only updates
//!   its own row and receipts.
//! - An account's complete local removal, or a successful logout of the
//!   selected account, forgets everything kept here for it, under the same
//!   mutex (`forget_account`): its guards, its possibly applied listings,
//!   its row errors, the rows shown for it and every outcome on screen
//!   written for it. Each outcome on screen, the center's notice with the
//!   room it opens and the Home cards' error, names the account it was
//!   written for, whether or not the center was ever open, and is
//!   renumbered with every new outcome (`Owner`); the queued clear of a
//!   forgotten account's outcome acts only while that write is still the
//!   one shown, so an outcome written since, another account's included,
//!   stays. Nothing is sent or deleted elsewhere. An answer of that account
//!   finishing afterwards finds no guard of its own and records and shows
//!   nothing: everything a finish records needs its own admitted guard, the
//!   answer's exact number, at that moment. A switch (A to B and back to A
//!   included), a partial removal or a drafts cleanup forgets nothing.
//!
//! Errors use fixed wording. A bulk decline is bounded, needs a separate
//! confirmation and answers each invitation on its own, rechecking its row
//! and the selection before each request. Inactive accounts are never read.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::{
    AccountSelection, AnswerFence, DirectMark, DirectWriteError, InvitationAnswer,
    InvitationOutcome, InvitationSummary, Invitations, MatrixCoreError,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;

use crate::{
    AppWindow, InvitationItem, InvitationsState,
    live::{LiveUi, OwnedOpen},
    notifications::DesktopNotifications,
};

/// Home shows this many invitations as cards; the center lists them all.
pub(crate) const HOME_CARDS: usize = 3;
/// Most invitations one confirmed bulk decline answers, each on its own.
pub(crate) const MAX_BULK_DECLINE: usize = 25;
/// A bulk decline's confirmation expires after this long unanswered.
const BULK_CONFIRM_TTL: Duration = Duration::from_secs(60);
/// Most unresolved answers held at once, across accounts. Past it, a new
/// answer is refused rather than an unresolved one forgotten.
const MAX_ANSWER_GUARDS: usize = 64;
const MAX_QUERY_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 80;
const MAX_DETAIL_CHARS: usize = 160;
const MAX_TOPIC_CHARS: usize = 160;

const READ_FAILED: &str = "Invitations could not be read. Refresh to try again.";
const SIGN_IN: &str = "Sign in to answer invitations.";
const GONE: &str = "No longer an invitation here; nothing was sent.";
const ACCOUNT_CHANGED: &str = "The account changed before the answer was sent; nothing was sent.";
const NOT_JOINED: &str = "Joining was refused or not sent; nothing changed.";
const NOT_DECLINED: &str = "Declining was refused or not sent; nothing changed.";
const JOIN_UNCONFIRMED: &str =
    "No answer came back; the join may still reach the homeserver, so it is not sent again.";
const DECLINE_UNCONFIRMED: &str =
    "No answer came back; the decline may still reach the homeserver, so it is not sent again.";
const JOINED_UNSYNCED: &str =
    "Joined; sync has not brought the room here yet. Choose Check and open.";
const DECLINED_UNSYNCED: &str =
    "Declined; sync has not brought that here yet, so it stays marked answered.";
const WAITING_FOR_SYNC: &str = "Answered; waiting for sync…";
const UNCONFIRMED: &str =
    "Not confirmed; it may still reach the homeserver, so it is not sent again.";
const ALREADY_ANSWERED: &str =
    "This invitation already has an answer waiting to show here; nothing more was sent.";
const SATURATED: &str = "Too many answers here are still unconfirmed, so this one was not sent.";
const NOT_HERE_YET: &str =
    "Joined, but sync has not brought the room here yet. Choose Check and open again later.";
const HOME_JOINED_UNSYNCED: &str = "Joined; sync has not brought the room here yet.";
const HOME_DECLINED_UNSYNCED: &str = "Declined; sync has not brought that here yet.";
const BULK_EXPIRED: &str =
    "That confirmation expired; nothing was declined. Choose Decline all shown again.";
const BUSY: &str = "Another account step was still running, so nothing was sent. Try again.";
const VIEW_CLOSED: &str =
    "The invitations view closed before the answer was sent; nothing was sent.";
const SECURITY_STEP: &str = "A security step began before the answer was sent; nothing was sent.";
const SESSION_ENDED: &str = "The session ended before the answer was sent; nothing was sent.";
const DIRECT_NOT_TRIED: &str = "Not marked as a direct message.";
const DIRECT_NO_INVITER: &str = "Not marked as a direct message: no inviter was named.";
const DIRECT_NOT_SENT: &str = "Not marked as a direct message; nothing was written.";
const DIRECT_UNREADABLE: &str = "Not marked as a direct message: the list could not be read.";
const DIRECT_CHANGED: &str = "Not marked as a direct message: the list changed elsewhere.";
const DIRECT_REFUSED: &str = "Marking as a direct message was refused; nothing changed.";
const DIRECT_UNKNOWN: &str = "Marking as a direct message may have applied; it is not sent again.";
const DIRECT_STILL_UNKNOWN: &str =
    "direct-message markings may have applied; they are not sent again.";

/// Why an answer was withdrawn before it was handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Withdrawal {
    /// The session ended: locked, signed out or detached.
    Ended,
    /// A security step is in progress, or began since the answer was chosen.
    Security,
    /// The center view it was chosen in closed.
    Closed,
}

/// Why an answer may not go now, from what holds now: the session is live,
/// the security gate is where it was when the answer was chosen, and the
/// view it was chosen in is still open.
fn withdrawal(live: bool, security_unchanged: bool, view_open: bool) -> Option<Withdrawal> {
    if !live {
        Some(Withdrawal::Ended)
    } else if !security_unchanged {
        Some(Withdrawal::Security)
    } else if !view_open {
        Some(Withdrawal::Closed)
    } else {
        None
    }
}

/// Whether the `m.direct` listing after a confirmed join may still go: it is
/// part of that join, so a closed view does not stop it, but no account
/// data is written once the session ended or a security step began.
fn listing_allowed(reason: Option<Withdrawal>) -> bool {
    !matches!(reason, Some(Withdrawal::Ended | Withdrawal::Security))
}

fn withdrawn_text(reason: Withdrawal) -> &'static str {
    match reason {
        Withdrawal::Ended => SESSION_ENDED,
        Withdrawal::Security => SECURITY_STEP,
        Withdrawal::Closed => VIEW_CLOSED,
    }
}

/// What one answer was chosen under, captured where it was chosen; the core
/// asks it right before each request is handed over (see the module notes).
struct Admission {
    live: Arc<LiveUi>,
    /// The security gate's generation when the answer was chosen.
    ringing: u64,
    /// The center view it was chosen in and that view's number then; `None`
    /// for a Home card, which stays on screen while its account does.
    view: Option<(Arc<AtomicU64>, u64)>,
    /// Why `may_answer` said no, once it did.
    withdrawn: OnceLock<Withdrawal>,
}

impl Admission {
    fn new(live: &Arc<LiveUi>, ringing: u64, view: Option<(Arc<AtomicU64>, u64)>) -> Self {
        Self {
            live: live.clone(),
            ringing,
            view,
            withdrawn: OnceLock::new(),
        }
    }

    /// Why nothing more may be sent for this answer now, if anything.
    fn now(&self) -> Option<Withdrawal> {
        let view_open = self
            .view
            .as_ref()
            .is_none_or(|(view, at)| view.load(Ordering::Acquire) == *at);
        let live = self.live.is_live.load(Ordering::Acquire);
        let unchanged = self.live.security_gate.ringing_allowed(self.ringing);
        withdrawal(live, unchanged, view_open)
    }

    /// Why the core was told not to send the answer, if it was.
    fn withdrawn(&self) -> Option<Withdrawal> {
        self.withdrawn.get().copied()
    }
}

impl AnswerFence for Admission {
    fn may_answer(&self) -> bool {
        let Some(reason) = self.now() else {
            return true;
        };
        let _ = self.withdrawn.set(reason);
        false
    }

    fn may_list(&self) -> bool {
        listing_allowed(self.now())
    }
}

/// What an accepted direct-message invitation's `m.direct` listing adds to
/// its notice: nothing once it is listed, else why it is not known to be.
fn direct_note(result: &Result<InvitationAnswer, MatrixCoreError>) -> Option<&'static str> {
    let answer = result.as_ref().ok()?;
    match answer.direct {
        DirectMark::NotDirect | DirectMark::Listed(_) => None,
        DirectMark::NotTried if answer.outcome == InvitationOutcome::Unconfirmed => {
            Some(DIRECT_NOT_TRIED)
        }
        DirectMark::NotTried => Some(DIRECT_NO_INVITER),
        DirectMark::Failed(error) => Some(match error {
            DirectWriteError::NotSent => DIRECT_NOT_SENT,
            DirectWriteError::Unreadable => DIRECT_UNREADABLE,
            DirectWriteError::BaselineChanged => DIRECT_CHANGED,
            DirectWriteError::Refused => DIRECT_REFUSED,
            DirectWriteError::Unknown => DIRECT_UNKNOWN,
        }),
    }
}

/// `text`, then `note` when there is one.
fn with_note(text: &str, note: Option<&str>) -> String {
    match note {
        Some(note) => format!("{text} {note}"),
        None => text.to_owned(),
    }
}

/// Whether a bulk decline asked for at `asked` may still be confirmed `now`.
fn confirmation_live(asked: Option<Instant>, now: Instant) -> bool {
    asked.is_some_and(|asked| now.saturating_duration_since(asked) < BULK_CONFIRM_TTL)
}

/// A confirmed join the notice can open, through the Space route for a
/// Space as Home's accept does. An unsettled one opens only once this
/// account's store shows it joined.
#[derive(Clone, Debug, PartialEq, Eq)]
struct JoinedNotice {
    room_id: String,
    is_space: bool,
    settled: bool,
}

/// The joined room the notice for `result` offers to open, if any. The Space
/// kind is the one the core read when the answer was admitted.
fn joined_notice(
    room_id: &str,
    accept: bool,
    result: &Result<InvitationAnswer, MatrixCoreError>,
) -> Option<JoinedNotice> {
    let answer = result.as_ref().ok()?;
    if !accept || answer.outcome == InvitationOutcome::Unconfirmed {
        return None;
    }
    Some(JoinedNotice {
        room_id: room_id.to_owned(),
        is_space: answer.is_space,
        settled: answer.outcome == InvitationOutcome::Settled,
    })
}

/// The highlight after the list changed: the same invitation while it is
/// still listed, else the one now in its place, else none.
fn kept_index(shown: &[String], index: i32, next: &[String]) -> i32 {
    let (Ok(at), Some(last)) = (usize::try_from(index), next.len().checked_sub(1)) else {
        return -1;
    };
    let found = shown
        .get(at)
        .and_then(|chosen| next.iter().position(|room_id| room_id == chosen));
    i32::try_from(found.unwrap_or(at.min(last))).unwrap_or(-1)
}

/// What one answer did, in fixed wording, and whether it is flagged: not
/// applied, or possibly applied with no confirmation. A confirmed answer
/// whose new membership sync has not brought yet says so. `Err` means
/// nothing was applied (see `AccountManager::respond_invitation_selected`).
fn outcome(
    accept: bool,
    result: &Result<InvitationAnswer, MatrixCoreError>,
) -> (&'static str, bool) {
    match result.as_ref().map(|answer| answer.outcome) {
        Ok(InvitationOutcome::Settled) if accept => ("Joined.", false),
        Ok(InvitationOutcome::Settled) => ("Declined.", false),
        Ok(InvitationOutcome::Confirmed) if accept => (JOINED_UNSYNCED, false),
        Ok(InvitationOutcome::Confirmed) => (DECLINED_UNSYNCED, false),
        Ok(InvitationOutcome::Unconfirmed) if accept => (JOIN_UNCONFIRMED, true),
        Ok(InvitationOutcome::Unconfirmed) => (DECLINE_UNCONFIRMED, true),
        Err(MatrixCoreError::RoomNotFound(_)) => (GONE, true),
        Err(MatrixCoreError::SelectedAccountChanged) => (ACCOUNT_CHANGED, true),
        Err(MatrixCoreError::AccountAccessBusy) => (BUSY, true),
        Err(_) if accept => (NOT_JOINED, true),
        Err(_) => (NOT_DECLINED, true),
    }
}

/// How one answer ended, as its worker saw it.
struct Finished {
    accept: bool,
    result: Result<InvitationAnswer, MatrixCoreError>,
    /// Why its own admission withdrew it before the hand-off, if it did.
    withdrawn: Option<Withdrawal>,
}

impl Finished {
    /// `outcome`, except that an answer its admission withdrew says why
    /// nothing was sent.
    fn text(&self) -> (&'static str, bool) {
        match (self.withdrawn, &self.result) {
            (Some(reason), Err(_)) => (withdrawn_text(reason), true),
            _ => outcome(self.accept, &self.result),
        }
    }
}

/// Whether a bulk decline stops after this answer: its admission withdrew
/// it, the account changed, or the account transition stayed busy.
fn ends_bulk(finished: &Finished) -> bool {
    finished.withdrawn.is_some()
        || matches!(
            finished.result,
            Err(MatrixCoreError::SelectedAccountChanged | MatrixCoreError::AccountAccessBusy)
        )
}

/// What remains of an answer's guard once the answer finished: `None`
/// retires it, because its effect shows here or nothing was applied.
fn remaining(result: &Result<InvitationAnswer, MatrixCoreError>) -> Option<Stage> {
    match result.as_ref().map(|answer| answer.outcome) {
        Ok(InvitationOutcome::Confirmed) => Some(Stage::Confirmed),
        Ok(InvitationOutcome::Unconfirmed) => Some(Stage::Unconfirmed),
        Ok(InvitationOutcome::Settled) | Err(_) => None,
    }
}

/// One listed invitation, its texts already sanitized.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    room_id: String,
    generation: i32,
    name: String,
    detail: String,
    topic: String,
    kind: i32,
    encryption: i32,
    /// Lowercase name, alias and inviter, for the search.
    folded: String,
}

fn plain(text: &str, max_chars: usize) -> String {
    crate::directory_plain_text(text, max_chars)
}

impl Row {
    fn new(summary: &InvitationSummary, generation: i32) -> Self {
        let alias = summary
            .canonical_alias
            .as_deref()
            .map(|alias| plain(alias, MAX_NAME_CHARS))
            .unwrap_or_default();
        let name = plain(&summary.name, MAX_NAME_CHARS);
        // Only supplied names: the SDK's name, else the alias, else a label.
        let name = if !name.is_empty() {
            name
        } else if !alias.is_empty() {
            alias.clone()
        } else {
            "Unnamed room".to_owned()
        };
        let kind = if summary.is_space {
            1
        } else if summary.is_direct {
            2
        } else {
            0
        };
        let inviter_id = summary
            .inviter_id
            .as_deref()
            .map(|id| plain(id, MAX_NAME_CHARS))
            .unwrap_or_default();
        let inviter_name = summary
            .inviter_name
            .as_deref()
            .map(|name| plain(name, MAX_NAME_CHARS))
            .unwrap_or_default();
        let mut detail = vec![
            match kind {
                1 => "Space",
                2 => "Direct message",
                _ => "Room",
            }
            .to_owned(),
        ];
        // A display name is shown only beside the Matrix ID it belongs to.
        let ignored = if summary.inviter_ignored {
            ", whom you ignore"
        } else {
            ""
        };
        match (inviter_name.is_empty(), inviter_id.is_empty()) {
            (false, false) => {
                detail.push(format!("Invited by {inviter_name} ({inviter_id}){ignored}"));
            }
            (true, false) => detail.push(format!("Invited by {inviter_id}{ignored}")),
            _ => {}
        }
        if !alias.is_empty() && alias != name {
            detail.push(alias.clone());
        }
        let detail = plain(&detail.join(" · "), MAX_DETAIL_CHARS);
        Self {
            folded: [
                name.as_str(),
                alias.as_str(),
                inviter_name.as_str(),
                inviter_id.as_str(),
            ]
            .join(" ")
            .to_lowercase(),
            room_id: summary.room_id.clone(),
            generation,
            name,
            detail,
            topic: summary
                .topic
                .as_deref()
                .map(|topic| plain(topic, MAX_TOPIC_CHARS))
                .unwrap_or_default(),
            kind,
            encryption: match summary.encrypted {
                Some(true) => 1,
                Some(false) => 2,
                None => 0,
            },
        }
    }
}

/// The next listing's rows. An invitation the same selection listed before
/// keeps its generation; every other one gets a new one, so no choice made
/// on an older listing or account selection can match it.
fn relist(
    previous: &[Row],
    same_selection: bool,
    summaries: &[InvitationSummary],
    next_generation: &mut i32,
) -> Vec<Row> {
    summaries
        .iter()
        .map(|summary| {
            let kept = same_selection
                .then(|| previous.iter().find(|row| row.room_id == summary.room_id))
                .flatten()
                .map(|row| row.generation);
            let generation = kept.unwrap_or_else(|| {
                *next_generation = next_generation.checked_add(1).unwrap_or(1);
                *next_generation
            });
            Row::new(summary, generation)
        })
        .collect()
}

/// An accepted direct-message invitation whose `m.direct` write got no
/// answer: it may have applied, and it is never sent again. Kept under its
/// account and room, apart from the guard and the screen (see the module
/// notes).
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectReceipt {
    /// The answer whose listing it was: a receipt of a newer answer is never
    /// replaced by an older one's finish.
    answer: u64,
    /// The row's name when that answer was admitted, else empty.
    name: String,
}

/// What the center says on opening about the account's listings that may
/// have applied: one by its name (or room), more by their number.
fn direct_reminder(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("{one}: {DIRECT_UNKNOWN}")),
        many => Some(format!("{} {DIRECT_STILL_UNKNOWN}", many.len())),
    }
}

/// Where an answer stands until its account's store shows its effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Sent and not finished.
    Sending,
    /// The homeserver confirmed it; sync has not brought it here yet.
    Confirmed,
    /// Handed to the homeserver with no answer back: it may still apply.
    Unconfirmed,
}

/// One admitted answer whose effect its account's store does not show yet
/// (see the module notes), kept under its account and room.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Guard {
    /// The answer's own number, from its exact admission: only its own
    /// finish, or a store read made for it, touches the guard.
    answer: u64,
    accept: bool,
    stage: Stage,
    /// Its row's name when it was admitted (empty for a Home card), so a
    /// refresh that removed the row does not change what its notice says.
    name: String,
}

/// Why an answer was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// Not the row on screen, or a bulk decline is running.
    Quiet,
    /// An answer of this account for this room is unresolved.
    Unresolved,
    /// `MAX_ANSWER_GUARDS` answers are unresolved.
    Saturated,
}

/// The status a guarded row shows.
fn guard_status(guard: &Guard) -> &'static str {
    match guard.stage {
        Stage::Sending if guard.accept => "Joining…",
        Stage::Sending => "Declining…",
        Stage::Confirmed => WAITING_FOR_SYNC,
        Stage::Unconfirmed => UNCONFIRMED,
    }
}

/// The notice after a bulk decline, in fixed wording.
fn bulk_summary(declined: usize, unconfirmed: usize, refused: usize, full: bool) -> String {
    let mut summary = format!("Declined {declined}.");
    if unconfirmed > 0 {
        summary.push_str(&format!(" {unconfirmed} not confirmed and not sent again."));
    }
    if refused > 0 {
        summary.push_str(&format!(" {refused} not applied."));
    }
    if unconfirmed + refused > 0 {
        summary.push_str(" See their rows.");
    }
    if full {
        summary.push(' ');
        summary.push_str(SATURATED);
    }
    summary
}

fn key(account_id: &str, room_id: &str) -> (String, String) {
    (account_id.to_owned(), room_id.to_owned())
}

/// The account one outcome on screen was written for, and that write's
/// number: the center's notice (with the room it opens, `State::joined`)
/// and the Home cards' error each have one. Every new outcome written to
/// either, on the UI thread, renumbers it under the state mutex; rewording
/// the outcome already shown (`NOT_HERE_YET`) keeps its owner and number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Owner {
    account_id: Option<String>,
    stamp: u64,
}

impl Owner {
    /// It now shows an outcome of `account_id` (`None`: of no account).
    fn write(&mut self, account_id: Option<&str>) -> u64 {
        self.account_id = account_id.map(str::to_owned);
        self.stamp = self.stamp.wrapping_add(1);
        self.stamp
    }

    /// `account_id` was forgotten: when this outcome is its own, it is no
    /// one's any more, and the write number the screen keeps showing it
    /// under until something else is written there.
    fn forget(&mut self, account_id: &str) -> Option<u64> {
        let own = self.account_id.as_deref() == Some(account_id);
        own.then(|| self.write(None))
    }
}

/// What `State::forget_account` took from the outcomes on screen: for each
/// that was the account's, the write number under which the screen may
/// still show it (see `clear_forgotten`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Forgotten {
    notice: Option<u64>,
    home: Option<u64>,
}

/// One finished answer, as its worker saw it, for `State::conclude`.
struct Concluding<'a> {
    account_id: &'a str,
    room_id: &'a str,
    answer: u64,
    /// What remains of its guard (`remaining`).
    next: Option<Stage>,
    direct: DirectMark,
    /// The row error to keep, when nothing was applied.
    failed: Option<&'static str>,
    /// The answer's account is the one on screen.
    shown: bool,
    joined: Option<JoinedNotice>,
}

/// What `State::conclude` did: the answer's own guard, when it still was,
/// and whether the notice is this answer's to set.
struct Concluded {
    guard: Option<Guard>,
    noticed: bool,
}

#[derive(Default)]
struct State {
    /// Bumped by every refresh and close; a listing is used only while its
    /// number is current.
    operation: u64,
    next_generation: i32,
    next_answer: u64,
    selection: Option<AccountSelection>,
    rows: Vec<Row>,
    total: usize,
    loaded: bool,
    /// Lowercase search text.
    query: String,
    /// Unresolved answers by account and room. They outlive a refresh, a
    /// close and an account switch; see the module notes.
    guards: HashMap<(String, String), Guard>,
    /// Listings that may have applied, by account and room. They outlive
    /// the guard, a refresh, a close and an account switch.
    direct: HashMap<(String, String), DirectReceipt>,
    /// The answer whose finish last set the notice: an older answer's late
    /// finish never replaces it.
    noticed: u64,
    /// The last failed answer's row error, by account and room (a Home
    /// card's too, which only that account's listing shows).
    errors: HashMap<(String, String), &'static str>,
    /// The joined room the notice's Open action opens; it is part of the
    /// notice, so `notice` owns it.
    joined: Option<JoinedNotice>,
    /// Whose outcome the center's notice shows, and the Home cards' error.
    notice: Owner,
    home: Owner,
    /// A bulk decline waiting for its confirmation: exactly these rows, and
    /// when it was asked for; it expires after `BULK_CONFIRM_TTL`.
    bulk: Option<Vec<(String, i32)>>,
    bulk_asked: Option<Instant>,
    /// Bumped by every confirmed bulk decline, close and account change; a
    /// running bulk decline sends nothing more once it changed.
    bulk_run: u64,
    bulk_running: bool,
}

impl State {
    /// Whether this listing belongs to `selection`, the account on screen.
    fn lists(&self, selection: &AccountSelection) -> bool {
        let Some(own) = &self.selection else {
            return false;
        };
        self.loaded
            && own.is_current()
            && selection.is_current()
            && own.account_id() == selection.account_id()
    }

    fn listed(&self, room_id: &str, generation: i32) -> bool {
        self.rows
            .iter()
            .any(|row| row.room_id == room_id && row.generation == generation)
    }

    fn matching(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|row| self.query.is_empty() || row.folded.contains(&self.query))
    }

    fn guard(&self, account_id: &str, room_id: &str) -> Option<&Guard> {
        self.guards.get(&key(account_id, room_id))
    }

    /// Whether an answer of `account_id` is out now.
    fn answering(&self, account_id: &str) -> bool {
        self.guards
            .iter()
            .any(|((account, _), guard)| account == account_id && guard.stage == Stage::Sending)
    }

    /// Admit one answer and return its number. A center row (`generation`)
    /// must be listed with that generation while the listing belongs to the
    /// selection on screen (`listing_current`). No single answer runs during
    /// a bulk decline, none while the account's room has an unresolved
    /// guard, and none past `MAX_ANSWER_GUARDS` unresolved ones.
    fn admit(
        &mut self,
        room_id: &str,
        generation: Option<i32>,
        listing_current: bool,
        account_id: &str,
        accept: bool,
        from_bulk: bool,
    ) -> Result<u64, Refusal> {
        if let Some(generation) = generation
            && !(listing_current && self.listed(room_id, generation))
        {
            return Err(Refusal::Quiet);
        }
        if self.bulk_running && !from_bulk {
            return Err(Refusal::Quiet);
        }
        if self.guard(account_id, room_id).is_some() {
            return Err(Refusal::Unresolved);
        }
        // Unresolved listings count too: none is ever dropped.
        if self.guards.len() + self.direct.len() >= MAX_ANSWER_GUARDS {
            return Err(Refusal::Saturated);
        }
        let name = self
            .rows
            .iter()
            .find(|row| listing_current && row.room_id == room_id)
            .map(|row| row.name.clone())
            .unwrap_or_default();
        self.next_answer = self.next_answer.wrapping_add(1);
        let answer = self.next_answer;
        let guard = Guard {
            answer,
            accept,
            stage: Stage::Sending,
            name,
        };
        self.guards.insert(key(account_id, room_id), guard);
        self.errors.remove(&key(account_id, room_id));
        Ok(answer)
    }

    /// Answer `answer` of `account_id` for `room_id` finished: its own guard,
    /// never a newer one, moves on to `next`, or is retired with `None`.
    /// Returns that guard, if it was still there.
    fn finish(
        &mut self,
        account_id: &str,
        room_id: &str,
        answer: u64,
        next: Option<Stage>,
    ) -> Option<Guard> {
        let key = key(account_id, room_id);
        if self.guards.get(&key).map(|guard| guard.answer) != Some(answer) {
            return None;
        }
        match next {
            Some(stage) => {
                let guard = self.guards.get_mut(&key)?;
                guard.stage = stage;
                Some(guard.clone())
            }
            None => self.guards.remove(&key),
        }
    }

    /// Keep answer `answer`'s listing of `room_id` when its write got no
    /// answer (`mark`), under `account_id`, whatever is on screen. A receipt
    /// of a newer answer is never replaced; nothing else removes one.
    fn record_direct(
        &mut self,
        account_id: &str,
        room_id: &str,
        answer: u64,
        name: &str,
        mark: DirectMark,
    ) {
        if mark != DirectMark::Failed(DirectWriteError::Unknown) {
            return;
        }
        let key = key(account_id, room_id);
        if self
            .direct
            .get(&key)
            .is_some_and(|kept| kept.answer > answer)
        {
            return;
        }
        let name = name.to_owned();
        self.direct.insert(key, DirectReceipt { answer, name });
    }

    /// Forget every record of `account_id`: its guards (so none of its
    /// admitted answers owns anything any more), its possibly applied
    /// listings, its row errors, the outcomes on screen written for it (the
    /// notice with its room, the Home cards' error) whether or not the
    /// center was ever open, and, when the shown listing is that account's,
    /// its rows and a waiting bulk confirmation. Nothing is kept to mark it:
    /// a later finish of its answers finds no guard. Returns what the screen
    /// may still show of it, for `clear_forgotten`.
    fn forget_account(&mut self, account_id: &str) -> Forgotten {
        self.guards.retain(|(account, _), _| account != account_id);
        self.direct.retain(|(account, _), _| account != account_id);
        self.errors.retain(|(account, _), _| account != account_id);
        let notice = self.notice.forget(account_id);
        if notice.is_some() {
            self.joined = None;
        }
        let home = self.home.forget(account_id);
        let shown = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.account_id() == account_id);
        if shown {
            self.operation = self.operation.wrapping_add(1);
            self.bulk_run = self.bulk_run.wrapping_add(1);
            self.selection = None;
            self.rows.clear();
            self.total = 0;
            self.loaded = false;
            self.bulk = None;
        }
        Forgotten { notice, home }
    }

    /// The center closed: its listing, search, confirmation and notice go.
    /// Every guard and possibly applied listing stays, and so does the Home
    /// cards' error.
    fn close(&mut self) {
        self.operation = self.operation.wrapping_add(1);
        self.bulk_run = self.bulk_run.wrapping_add(1);
        self.selection = None;
        self.rows.clear();
        self.errors.clear();
        self.total = 0;
        self.loaded = false;
        self.query.clear();
        self.bulk = None;
        self.joined = None;
        self.notice.write(None);
    }

    /// Record one finished answer, all under this mutex: its own guard moves
    /// on or is retired (`finish`), and only when it was still that answer's
    /// guard, is anything else recorded: a possibly applied listing, a row
    /// error and, while `shown` and no newer answer's finish set it, the
    /// notice (`takes_notice`) with its room, now owned by `account_id`. An
    /// answer whose guard is gone (its account was forgotten) or is another
    /// answer's records nothing.
    fn conclude(&mut self, finished: Concluding<'_>) -> Concluded {
        let Concluding {
            account_id,
            room_id,
            answer,
            next,
            direct,
            failed,
            shown,
            joined,
        } = finished;
        let Some(guard) = self.finish(account_id, room_id, answer, next) else {
            return Concluded {
                guard: None,
                noticed: false,
            };
        };
        self.record_direct(account_id, room_id, answer, &guard.name, direct);
        if shown && let Some(text) = failed {
            self.errors.insert(key(account_id, room_id), text);
        }
        let noticed = shown && self.takes_notice(answer);
        if noticed {
            self.joined = joined;
            self.notice.write(Some(account_id));
        }
        Concluded {
            guard: Some(guard),
            noticed,
        }
    }

    /// Whether answer `answer`'s finish may set the notice: no newer answer's
    /// finish set it. If so, it is now this answer's.
    fn takes_notice(&mut self, answer: u64) -> bool {
        if answer < self.noticed {
            return false;
        }
        self.noticed = answer;
        true
    }

    /// The listings of `account_id` that may have applied, by name, or by
    /// room when it had none.
    fn direct_unknown(&self, account_id: &str) -> Vec<String> {
        let mut names = self
            .direct
            .iter()
            .filter(|((account, _), _)| account == account_id)
            .map(|((_, room_id), receipt)| match receipt.name.as_str() {
                "" => plain(room_id, MAX_NAME_CHARS),
                name => name.to_owned(),
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// The answers of `account_id` that only a read of its store can retire
    /// now, as (room, accepted, answer number).
    fn unsettled(&self, account_id: &str) -> Vec<(String, bool, u64)> {
        self.guards
            .iter()
            .filter(|((account, _), guard)| account == account_id && guard.stage != Stage::Sending)
            .map(|((_, room_id), guard)| (room_id.clone(), guard.accept, guard.answer))
            .collect()
    }

    /// Retire the guards of `account_id` that a read of its store, made for
    /// `checked`, shows settled. A guard admitted again since is kept.
    fn settle(&mut self, account_id: &str, checked: &[(String, bool, u64)], settled: &[bool]) {
        for ((room_id, _, answer), settled) in checked.iter().zip(settled) {
            let key = key(account_id, room_id);
            let own = self.guards.get(&key).map(|guard| guard.answer);
            if *settled && own == Some(*answer) {
                self.guards.remove(&key);
            }
        }
    }

    /// Take one listing of the selection on screen. Guards are left alone:
    /// a room missing from a bounded listing proves nothing.
    fn take_listing(&mut self, listing: &Invitations) {
        let same = self.loaded;
        let mut next = self.next_generation;
        self.rows = relist(&self.rows, same, &listing.invitations, &mut next);
        self.next_generation = next;
        self.total = listing.total;
        self.loaded = true;
        // Errors of invitations no longer listed go with them.
        let listed = self
            .rows
            .iter()
            .map(|row| row.room_id.clone())
            .collect::<Vec<_>>();
        self.errors
            .retain(|(_, room_id), _| listed.contains(room_id));
        // A confirmation names exact rows; keep only those still listed.
        if let Some(bulk) = self.bulk.take() {
            let kept = bulk
                .into_iter()
                .filter(|(room_id, generation)| self.listed(room_id, *generation))
                .collect::<Vec<_>>();
            self.bulk = (!kept.is_empty()).then_some(kept);
        }
    }
}

pub(crate) struct InvitationsCenter {
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    state: Mutex<State>,
    /// The center's open view: bumped when it closes, so an answer chosen in
    /// it and not handed over yet is withdrawn.
    view: Arc<AtomicU64>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

/// The registered center, for an account's complete removal
/// (`forget_account`); set once by `register`.
static CENTER: OnceLock<std::sync::Weak<InvitationsCenter>> = OnceLock::new();

/// Forget everything the center keeps for `account_id` (see the module
/// notes). Metadata only. Callers: account_access's removal, only when its
/// result is `complete`, and main.rs's logout of the selected account, only
/// when it succeeded; never an account switch, a partial removal or a
/// drafts cleanup.
pub(crate) fn forget_account(account_id: &str) {
    if let Some(center) = CENTER.get().and_then(std::sync::Weak::upgrade) {
        center.forget(account_id);
    }
}

pub(crate) fn register(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    runtime: Handle,
) -> Arc<InvitationsCenter> {
    let center = Arc::new(InvitationsCenter {
        live,
        notifications,
        ui: ui.as_weak(),
        runtime,
        state: Mutex::new(State::default()),
        view: Arc::default(),
    });
    let _ = CENTER.set(Arc::downgrade(&center));
    let global = ui.global::<InvitationsState>();
    let refresh = center.clone();
    global.on_refresh(move || refresh.refresh());
    let closed = center.clone();
    global.on_closed(move || closed.closed());
    let search = center.clone();
    global.on_search(move |query| search.search(&query));
    let answer = center.clone();
    global.on_answer(move |room_id, generation, accept| {
        answer.answer(&room_id, Some(generation), accept, false);
    });
    let ask = center.clone();
    global.on_ask_bulk_decline(move || ask.ask_bulk_decline());
    let confirm = center.clone();
    global.on_confirm_bulk_decline(move || confirm.confirm_bulk_decline());
    let cancel = center.clone();
    global.on_cancel_bulk_decline(move || cancel.cancel_bulk_decline());
    let open = center.clone();
    global.on_open_room(move |room_id| open.open_room(&room_id));
    // The Home cards: the card on screen, for the account on screen; an
    // accepted invitation opens, as before.
    let card = center.clone();
    ui.on_respond_invite(move |room_id, accept| card.answer(&room_id, None, accept, true));
    center
}

impl InvitationsCenter {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// The live account on screen and its current selection.
    fn shown_selection(&self, ui: &AppWindow) -> Option<AccountSelection> {
        let account_id = ui.get_active_account_local_id().to_string();
        if !ui.get_live_session()
            || !self.live.is_live.load(Ordering::Acquire)
            || account_id.is_empty()
        {
            return None;
        }
        self.live
            .manager
            .account_selection(&account_id)
            .filter(AccountSelection::is_current)
    }

    /// Whether a listing for `operation` under `selection` may be shown.
    fn owns(&self, ui: &AppWindow, operation: u64, selection: &AccountSelection) -> bool {
        self.state().operation == operation
            && selection.is_current()
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == selection.account_id()
            && ui.global::<InvitationsState>().get_open()
    }

    /// Read the visible account's invitations, only while the center is
    /// open, and which of its unresolved answers its store shows settled.
    /// Rows of another account or selection are cleared at once; guards are
    /// not (see the module notes).
    fn refresh(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<InvitationsState>();
        if !global.get_open() {
            return;
        }
        let selection = self.shown_selection(&ui);
        let (operation, checks) = {
            let mut state = self.state();
            state.operation = state.operation.wrapping_add(1);
            let same = match (&state.selection, &selection) {
                (Some(own), Some(new)) => own.is_current() && own.account_id() == new.account_id(),
                _ => false,
            };
            if !same {
                state.rows.clear();
                state.errors.clear();
                state.total = 0;
                state.loaded = false;
                state.bulk = None;
                state.bulk_run = state.bulk_run.wrapping_add(1);
                state.joined = None;
                // A listing of this account that may have applied is named
                // again, whatever was shown before.
                let names = selection
                    .as_ref()
                    .map(|selection| state.direct_unknown(selection.account_id()))
                    .unwrap_or_default();
                let reminder = direct_reminder(&names);
                // A reminder is that account's; no reminder is no one's.
                let owner = selection.as_ref().filter(|_| reminder.is_some());
                state.notice.write(owner.map(AccountSelection::account_id));
                global.set_notice(reminder.unwrap_or_default().into());
                global.set_notice_room_id("".into());
                global.set_notice_room_ready(false);
            }
            state.selection = selection.clone();
            let checks = selection
                .as_ref()
                .map(|selection| state.unsettled(selection.account_id()))
                .unwrap_or_default();
            (state.operation, checks)
        };
        global.set_account_id(
            selection
                .as_ref()
                .map_or("", AccountSelection::account_id)
                .into(),
        );
        global.set_error("".into());
        let Some(selection) = selection else {
            global.set_loading(false);
            self.publish(&ui);
            return;
        };
        global.set_loading(true);
        self.publish(&ui);
        let center = self.clone();
        self.runtime.spawn(async move {
            let manager = &center.live.manager;
            let listing = manager.invitations_selected(&selection).await;
            // Only this account's own store retires an unresolved answer.
            let answers = checks
                .iter()
                .map(|(room_id, accept, _)| (room_id.clone(), *accept))
                .collect::<Vec<_>>();
            let settled = if answers.is_empty() {
                Ok(Vec::new())
            } else {
                manager
                    .invitation_answers_settled_selected(&selection, &answers)
                    .await
            };
            let weak = center.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                center.apply_listing(&ui, operation, &selection, listing, &checks, settled);
            });
        });
    }

    /// Show one listing, and retire the guards its store read shows settled,
    /// only while that exact listing and selection are still the ones on
    /// screen.
    fn apply_listing(
        &self,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        listing: Result<Invitations, MatrixCoreError>,
        checks: &[(String, bool, u64)],
        settled: Result<Vec<bool>, MatrixCoreError>,
    ) {
        if !self.owns(ui, operation, selection) {
            return;
        }
        let global = ui.global::<InvitationsState>();
        global.set_loading(false);
        {
            let mut state = self.state();
            if let Ok(settled) = &settled {
                state.settle(selection.account_id(), checks, settled);
            }
            if let Ok(listing) = &listing {
                state.take_listing(listing);
            }
        }
        if listing.is_err() {
            global.set_error(READ_FAILED.into());
        }
        self.publish(ui);
    }

    fn search(&self, query: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        {
            let mut state = self.state();
            state.query = query
                .chars()
                .take(MAX_QUERY_CHARS)
                .collect::<String>()
                .trim()
                .to_lowercase();
            // A confirmation was for the rows shown before.
            state.bulk = None;
        }
        self.publish(&ui);
    }

    /// One deliberate answer. A center row passes its generation and must
    /// still be listed for the selection on screen; a Home card is the card
    /// on screen for that account now.
    fn answer(self: &Arc<Self>, room_id: &str, generation: Option<i32>, accept: bool, open: bool) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(selection) = self.shown_selection(&ui) else {
            self.state().home.write(None);
            ui.set_invite_error(SIGN_IN.into());
            return;
        };
        // Off Home, or while another surface owns the window, nothing is
        // answered and nothing is withdrawn: refused, never deferred.
        if ui.get_page() != 0 || ui.get_invitations_entry_blocked() {
            return;
        }
        // What the answer is chosen under: a center row's view, the security
        // gate and, for a Home card's accept, where the person is now.
        let open_view = self.view.load(Ordering::Acquire);
        let view = generation.map(|_| (self.view.clone(), open_view));
        let ringing = self.live.security_gate.ringing_generation();
        let admission = Admission::new(&self.live, ringing, view);
        if let Some(reason) = admission.now() {
            self.say(&ui, selection.account_id(), withdrawn_text(reason));
            return;
        }
        // A Home card's accept opens under the navigation now and the same
        // security generation as its admission, asked again at the open's
        // commit and snapshot. A decline opens nothing.
        let opens = (open && accept).then(|| self.live.open_request(ringing));
        let admitted = {
            let mut state = self.state();
            let listing_current = state.lists(&selection);
            let account_id = selection.account_id();
            state.admit(
                room_id,
                generation,
                listing_current,
                account_id,
                accept,
                false,
            )
        };
        let answer = match admitted {
            Ok(answer) => answer,
            Err(Refusal::Quiet) => return,
            Err(Refusal::Unresolved) => {
                self.say(&ui, selection.account_id(), ALREADY_ANSWERED);
                return;
            }
            Err(Refusal::Saturated) => {
                self.say(&ui, selection.account_id(), SATURATED);
                return;
            }
        };
        // Admitted, an accept that opens takes the conversation over: a call
        // bar click still waiting, a held unread jump, a shown link's read,
        // join and follow-up, an older owned open and identity details'
        // focus-return intent end now, synchronously, before this click's
        // open is owned and anything is set. Visible owners refused above.
        if opens.is_some() {
            crate::withdraw_call_bar(&ui);
            crate::matrix_links::clear(&ui);
            crate::identity_warnings::withdraw(&ui);
        }
        // Owned from this click by its exact account: every protected
        // surface entered from now on withdraws it for good.
        let opens = opens.map(|open| open.owned(&selection));
        let owned = opens.as_ref().map(|open| open.authority().clone());
        if let Some(owned) = &owned {
            crate::matrix_links::own_automatic_open(owned.clone());
        }
        self.state().home.write(None);
        ui.set_invite_error("".into());
        self.publish(&ui);
        let center = self.clone();
        let room_id = room_id.to_owned();
        self.runtime.spawn(async move {
            let weak = center.ui.clone();
            let result = center
                .live
                .respond_to_invite(&selection, &room_id, accept, &admission, opens, weak)
                .await;
            // Its open, if any, ended with the answer: nothing stays owned.
            if let Some(owned) = owned {
                owned.withdraw();
            }
            let finished = Finished {
                accept,
                result,
                withdrawn: admission.withdrawn(),
            };
            let weak = center.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                center.finish_answer(&ui, &selection, &room_id, answer, finished, true);
            });
        });
    }

    /// Say `text` where the person answered: in the center's notice while it
    /// is open, else on the Home cards, as `account_id`'s.
    fn say(&self, ui: &AppWindow, account_id: &str, text: &'static str) {
        let global = ui.global::<InvitationsState>();
        if global.get_open() {
            {
                let mut state = self.state();
                state.joined = None;
                state.notice.write(Some(account_id));
            }
            global.set_notice(text.into());
            global.set_notice_room_id("".into());
            global.set_notice_room_ready(false);
        } else {
            self.state().home.write(Some(account_id));
            ui.set_invite_error(text.into());
        }
    }

    /// Record one finished answer and read the list again. Only the answer's
    /// own guard changes; the outcome is shown only to the account it
    /// belongs to, with the name its row had when it was admitted, and with
    /// what became of a direct-message listing.
    fn finish_answer(
        self: &Arc<Self>,
        ui: &AppWindow,
        selection: &AccountSelection,
        room_id: &str,
        answer: u64,
        finished: Finished,
        requery: bool,
    ) {
        let account_id = selection.account_id();
        // A confirmed answer's banners go.
        let confirmed = finished
            .result
            .as_ref()
            .is_ok_and(|done| done.outcome != InvitationOutcome::Unconfirmed);
        if confirmed && selection.is_current() {
            self.notifications.withdraw_room(account_id, room_id);
        }
        let shown = selection.is_current()
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == account_id;
        show_finished(
            ui,
            &self.state,
            account_id,
            room_id,
            answer,
            shown,
            finished,
        );
        self.publish(ui);
        // The real state decides what is listed, whatever this view did
        // meanwhile.
        if requery && shown {
            self.refresh();
        }
    }

    /// Prepare a bulk decline of the shown invitations: at most
    /// `MAX_BULK_DECLINE`, none with an unresolved answer, named exactly.
    fn ask_bulk_decline(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(selection) = self.shown_selection(&ui) else {
            return;
        };
        {
            let mut state = self.state();
            if !state.lists(&selection) || state.bulk_running {
                return;
            }
            let account_id = selection.account_id();
            let rows = state
                .matching()
                .filter(|row| state.guard(account_id, &row.room_id).is_none())
                .take(MAX_BULK_DECLINE)
                .map(|row| (row.room_id.clone(), row.generation))
                .collect::<Vec<_>>();
            state.bulk_asked = (!rows.is_empty()).then(Instant::now);
            state.bulk = (!rows.is_empty()).then_some(rows);
        }
        self.publish(&ui);
    }

    fn cancel_bulk_decline(&self) {
        self.state().bulk = None;
        if let Some(ui) = self.ui.upgrade() {
            self.publish(&ui);
        }
    }

    /// Decline the confirmed rows one at a time. Before each request the row
    /// must still be listed with its generation, the selection current and
    /// the run not cancelled by a close or account change; a request already
    /// sent is not recalled.
    fn confirm_bulk_decline(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(selection) = self.shown_selection(&ui) else {
            return;
        };
        let (rows, run) = {
            let mut state = self.state();
            if !state.lists(&selection) || state.bulk_running {
                return;
            }
            let Some(rows) = state.bulk.take() else {
                return;
            };
            if !confirmation_live(state.bulk_asked.take(), Instant::now()) {
                state.notice.write(Some(selection.account_id()));
                drop(state);
                ui.global::<InvitationsState>()
                    .set_notice(BULK_EXPIRED.into());
                self.publish(&ui);
                return;
            }
            state.bulk_run = state.bulk_run.wrapping_add(1);
            state.bulk_running = true;
            (rows, state.bulk_run)
        };
        // Every row is withdrawn once this view closes or a security step
        // begins after this confirmation.
        let view = (self.view.clone(), self.view.load(Ordering::Acquire));
        let ringing = self.live.security_gate.ringing_generation();
        self.publish(&ui);
        let center = self.clone();
        self.runtime.spawn(async move {
            let (mut declined, mut unconfirmed, mut refused) = (0_usize, 0_usize, 0_usize);
            let mut full = false;
            for (room_id, generation) in rows {
                let answer = {
                    let mut state = center.state();
                    if state.bulk_run != run || !selection.is_current() {
                        break;
                    }
                    let listing_current = state.lists(&selection);
                    let account_id = selection.account_id();
                    let admitted = state.admit(
                        &room_id,
                        Some(generation),
                        listing_current,
                        account_id,
                        false,
                        true,
                    );
                    match admitted {
                        Ok(answer) => answer,
                        // Every unresolved guard is kept: stop here.
                        Err(Refusal::Saturated) => {
                            full = true;
                            break;
                        }
                        Err(_) => continue,
                    }
                };
                center.republish();
                let admission = Admission::new(&center.live, ringing, Some(view.clone()));
                let weak = center.ui.clone();
                let result = center
                    .live
                    .respond_to_invite(&selection, &room_id, false, &admission, None, weak)
                    .await;
                match result.as_ref().map(|done| done.outcome) {
                    Ok(InvitationOutcome::Unconfirmed) => unconfirmed += 1,
                    Ok(_) => declined += 1,
                    Err(_) => refused += 1,
                }
                let done = Finished {
                    accept: false,
                    result,
                    withdrawn: admission.withdrawn(),
                };
                let stop = ends_bulk(&done);
                let center_now = center.clone();
                let selection_now = selection.clone();
                let weak = center.ui.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    center_now.finish_answer(&ui, &selection_now, &room_id, answer, done, false);
                });
                if stop {
                    break;
                }
            }
            let weak = center.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                center.state().bulk_running = false;
                let shown = selection.is_current()
                    && ui.get_active_account_local_id().as_str() == selection.account_id();
                if shown {
                    let global = ui.global::<InvitationsState>();
                    {
                        let mut state = center.state();
                        state.joined = None;
                        state.notice.write(Some(selection.account_id()));
                    }
                    global.set_notice_room_id("".into());
                    global.set_notice_room_ready(false);
                    let summary = bulk_summary(declined, unconfirmed, refused, full);
                    global.set_notice(summary.into());
                }
                center.publish(&ui);
                if shown {
                    center.refresh();
                }
            });
        });
    }

    /// Open a room this account just joined: only while it is the account
    /// on screen. A join sync has not brought here is opened only once a read
    /// of the account's store shows it joined; otherwise the notice says so
    /// and keeps its manual Check and open. Nothing retries by itself.
    fn open_room(self: &Arc<Self>, room_id: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<InvitationsState>();
        let Some(selection) = self.shown_selection(&ui) else {
            return;
        };
        let joined = self.state().joined.clone();
        let Some(joined) = joined.filter(|joined| joined.room_id == room_id) else {
            return;
        };
        if global.get_notice_room_id().as_str() != room_id {
            return;
        }
        // What this Open is chosen under, at its click: the account on screen,
        // the security gate (no step running now or begun since) and the
        // navigation now. Its one authority is owned from here, so every
        // protected surface entered later withdraws it for good.
        let ringing = self.live.security_gate.ringing_generation();
        if !self.live.security_gate.ringing_allowed(ringing) {
            return;
        }
        // Only from the center while it shows, with no other surface over it:
        // refused before anything is withdrawn.
        if !global.get_open() || ui.get_page() != 0 || ui.get_invitations_entry_blocked() {
            return;
        }
        // Admitted, the Open takes the conversation over: a call bar click
        // still waiting, a held unread jump, a link's held follow-up, an
        // older owned open and identity details' focus-return intent end
        // now, synchronously, before this Open's own open is owned.
        crate::withdraw_call_bar(&ui);
        crate::matrix_links::clear(&ui);
        crate::identity_warnings::withdraw(&ui);
        let owned = self.live.open_request(ringing).owned(&selection);
        crate::matrix_links::own_automatic_open(owned.authority().clone());
        if joined.settled {
            self.open_joined(&ui, &selection, &joined, owned);
            return;
        }
        // The answer this check is for: a guard admitted later is not its.
        let answer = self
            .state()
            .guard(selection.account_id(), room_id)
            .map(|guard| guard.answer);
        let center = self.clone();
        self.runtime.spawn(async move {
            let checked = [(joined.room_id.clone(), true)];
            let manager = &center.live.manager;
            let settled = manager
                .invitation_answers_settled_selected(&selection, &checked)
                .await;
            let here = settled.is_ok_and(|settled| settled.first() == Some(&true));
            let weak = center.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let global = ui.global::<InvitationsState>();
                // Still the same notice, for the same account on screen.
                let same = selection.is_current()
                    && ui.get_active_account_local_id().as_str() == selection.account_id()
                    && global.get_notice_room_id().as_str() == joined.room_id;
                if !same {
                    owned.authority().withdraw();
                    return;
                }
                if !here {
                    owned.authority().withdraw();
                    global.set_notice(NOT_HERE_YET.into());
                    return;
                }
                // Its store shows the join: the answer it checked is settled.
                if let Some(answer) = answer {
                    let checked = [(joined.room_id.clone(), true, answer)];
                    center
                        .state()
                        .settle(selection.account_id(), &checked, &[true]);
                }
                center.open_joined(&ui, &selection, &joined, owned);
            });
        });
    }

    /// Close the center and open `joined` under `owned`, the Open's own
    /// authority from its click, for its exact `selection`: through the same
    /// owned reveal as a Home card's accept (LiveUi::open_answered), a room
    /// in the current Space when it lists it, else the Space that does, else
    /// Home, and a Space as its owned Space reveal. Nothing is chosen again
    /// from what is on screen later.
    fn open_joined(
        &self,
        ui: &AppWindow,
        selection: &AccountSelection,
        joined: &JoinedNotice,
        owned: OwnedOpen,
    ) {
        // Already ended (a newer Open, or a protected surface entered since
        // the click): the center stays as it is and nothing opens.
        if owned.authority().withdrawn() {
            return;
        }
        ui.global::<InvitationsState>().set_open(false);
        self.closed();
        let (live, selection) = (self.live.clone(), selection.clone());
        let (room_id, is_space) = (joined.room_id.clone(), joined.is_space);
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let authority = owned.authority().clone();
            live.open_answered(&selection, &room_id, is_space, Some(owned), weak)
                .await;
            // Done: nothing it could still open stays owned.
            authority.withdraw();
        });
    }

    /// The center closed: clear its rows. Answers already handed over keep
    /// going, and every guard stays, so nothing is answered twice; one not
    /// handed over yet is withdrawn, and a bulk decline sends nothing more.
    fn closed(&self) {
        // An answer chosen in this view and not handed over yet is withdrawn.
        self.view.fetch_add(1, Ordering::AcqRel);
        self.state().close();
        if let Some(ui) = self.ui.upgrade() {
            show_closed(&ui, &self.state);
        }
    }

    /// `forget_account` for this center: the state first, under its mutex,
    /// then, on the UI thread, what the screen still shows of that account
    /// (`clear_forgotten`).
    fn forget(self: &Arc<Self>, account_id: &str) {
        let forgotten = self.state().forget_account(account_id);
        let center = self.clone();
        let account_id = account_id.to_owned();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            clear_forgotten(&ui, &center.state, &account_id, forgotten);
        });
    }

    /// `publish` from a worker task.
    fn republish(self: &Arc<Self>) {
        let center = self.clone();
        let weak = self.ui.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| center.publish(&ui));
    }

    /// Show the rows matching the search (`publish_rows`).
    fn publish(&self, ui: &AppWindow) {
        publish_rows(ui, &self.state);
    }
}

/// One finished answer on the UI thread (`InvitationsCenter::finish_answer`),
/// all under `state`'s mutex: it is concluded (`State::conclude`), and only
/// when the notice became its own does it write the center's notice and,
/// while the center is closed, the Home cards' error, each now owned by its
/// account. A forget of that account cannot come between, and after one,
/// its finish records and shows nothing.
fn show_finished(
    ui: &AppWindow,
    state: &Mutex<State>,
    account_id: &str,
    room_id: &str,
    answer: u64,
    shown: bool,
    finished: Finished,
) {
    let (text, flagged) = finished.text();
    let note = direct_note(&finished.result);
    let Finished { accept, result, .. } = finished;
    let joined = joined_notice(room_id, accept, &result);
    let direct = result
        .as_ref()
        .map_or(DirectMark::NotDirect, |done| done.direct);
    let global = ui.global::<InvitationsState>();
    // A Home card's answer: only one this device already shows, and listed
    // when it is a direct message, needs no word there.
    let unsynced = result
        .as_ref()
        .is_ok_and(|done| done.outcome == InvitationOutcome::Confirmed);
    let home = if global.get_open() {
        None
    } else if unsynced && accept {
        Some(HOME_JOINED_UNSYNCED)
    } else if unsynced {
        Some(HOME_DECLINED_UNSYNCED)
    } else if flagged || note.is_some() {
        Some(text)
    } else {
        None
    };
    // Everything recorded needs this answer's own guard, now: one of a
    // forgotten account, or another answer's, records nothing. A listing
    // that may have applied is kept whatever is on screen; an answer that
    // was not applied leaves its error on its row; a newer answer's notice
    // is never replaced by this one's.
    let mut state = lock(state);
    let Concluded { guard, noticed } = state.conclude(Concluding {
        account_id,
        room_id,
        answer,
        next: remaining(&result),
        direct,
        failed: result.is_err().then_some(text),
        shown,
        joined: joined.clone(),
    });
    if !noticed {
        return;
    }
    let told = with_note(text, note);
    let notice = match guard.filter(|guard| !guard.name.is_empty()) {
        Some(guard) => format!("{}: {told}", guard.name),
        None => told,
    };
    global.set_notice(notice.into());
    let open = joined.as_ref().map_or("", |joined| joined.room_id.as_str());
    global.set_notice_room_id(open.into());
    global.set_notice_room_ready(joined.is_some_and(|joined| joined.settled));
    if let Some(home) = home {
        state.home.write(Some(account_id));
        ui.set_invite_error(with_note(home, note).into());
    }
}

/// The UI side of `forget_account`, queued after its state part: the
/// center's notice (with its room) and the Home cards' error are cleared
/// only while they still show the write that account owned when it was
/// forgotten, so an outcome written since, another account's included,
/// stays; the listing's account and read error only while they are that
/// account's. Then the rows are shown again.
fn clear_forgotten(ui: &AppWindow, state: &Mutex<State>, account_id: &str, forgotten: Forgotten) {
    let global = ui.global::<InvitationsState>();
    {
        let owners = lock(state);
        if forgotten.notice == Some(owners.notice.stamp) {
            global.set_notice("".into());
            global.set_notice_room_id("".into());
            global.set_notice_room_ready(false);
        }
        if forgotten.home == Some(owners.home.stamp) {
            ui.set_invite_error("".into());
        }
    }
    if global.get_account_id().as_str() == account_id {
        global.set_account_id("".into());
        global.set_error("".into());
    }
    publish_rows(ui, state);
}

/// The UI side of the center's close, after `State::close`: its listing,
/// search and notice go from the screen; the Home cards' error stays.
fn show_closed(ui: &AppWindow, state: &Mutex<State>) {
    let global = ui.global::<InvitationsState>();
    global.set_account_id("".into());
    global.set_loading(false);
    global.set_error("".into());
    global.set_notice("".into());
    global.set_notice_room_id("".into());
    global.set_notice_room_ready(false);
    global.set_query("".into());
    global.set_index(-1);
    publish_rows(ui, state);
}

/// Show the rows matching the search, each with its own guard or last
/// error for the account on screen, and the counts. Home cards pause while
/// the account on screen has an answer out.
fn publish_rows(ui: &AppWindow, state: &Mutex<State>) {
    let global = ui.global::<InvitationsState>();
    let account_id = ui.get_active_account_local_id().to_string();
    let state = lock(state);
    let items = state
        .matching()
        .map(|row| {
            let guard = state.guard(&account_id, &row.room_id);
            let error = state.errors.get(&key(&account_id, &row.room_id)).copied();
            InvitationItem {
                room_id: row.room_id.as_str().into(),
                generation: row.generation,
                name: row.name.as_str().into(),
                detail: row.detail.as_str().into(),
                topic: row.topic.as_str().into(),
                kind: row.kind,
                encryption: row.encryption,
                busy: guard.is_some(),
                status: match (guard, error) {
                    (Some(guard), _) => guard_status(guard).into(),
                    (None, Some(error)) => error.into(),
                    (None, None) => SharedString::new(),
                },
                failed: match guard {
                    Some(guard) => guard.stage == Stage::Unconfirmed,
                    None => error.is_some(),
                },
            }
        })
        .collect::<Vec<_>>();
    let count = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
    let model = global.get_items();
    let shown = model
        .iter()
        .map(|item| item.room_id.to_string())
        .collect::<Vec<_>>();
    let next = items
        .iter()
        .map(|item| item.room_id.to_string())
        .collect::<Vec<_>>();
    let index = kept_index(&shown, global.get_index(), &next);
    global.set_matching(count(items.len()));
    if shown == next {
        // The same invitations: rows update in place, keeping keyboard
        // focus on them.
        for (row, item) in items.into_iter().enumerate() {
            if model.row_data(row).as_ref() != Some(&item) {
                model.set_row_data(row, item);
            }
        }
    } else {
        global.set_items(ModelRc::new(VecModel::from(items)));
        // Rows were rebuilt; a focused row is gone and the search field
        // takes focus, with the highlight kept on the same invitation.
        global.set_focus_request(global.get_focus_request().wrapping_add(1));
    }
    global.set_index(index);
    global.set_total(count(state.total));
    global.set_read(count(state.rows.len()));
    global.set_bulk_count(count(state.bulk.as_ref().map_or(0, Vec::len)));
    global.set_bulk_running(state.bulk_running);
    global.set_answering(state.answering(&account_id));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn summary(room: &str, name: &str) -> InvitationSummary {
        InvitationSummary {
            room_id: room.into(),
            name: name.into(),
            topic: None,
            canonical_alias: None,
            is_space: false,
            is_direct: false,
            inviter_id: None,
            inviter_name: None,
            inviter_ignored: false,
            encrypted: None,
        }
    }

    #[test]
    fn rows_show_only_what_the_invite_state_supplied() {
        let bare = Row::new(&summary("!a:example.org", ""), 1);
        assert_eq!(bare.name, "Unnamed room");
        assert_eq!(bare.detail, "Room");
        assert!(bare.topic.is_empty() && bare.encryption == 0);
        let mut full = summary("!b:example.org", "Team\u{202e} chat");
        full.is_space = true;
        full.canonical_alias = Some("#team:example.org".into());
        full.inviter_id = Some("@alice:example.org".into());
        full.inviter_name = Some("Alice".into());
        full.topic = Some("Plans".into());
        full.encrypted = Some(true);
        let row = Row::new(&full, 2);
        assert!(!row.name.contains('\u{202e}'));
        assert_eq!(row.kind, 1);
        assert_eq!(row.encryption, 1);
        // A display name only beside its Matrix ID.
        assert!(
            row.detail.contains("Invited by Alice (@alice:example.org)"),
            "{}",
            row.detail
        );
        assert!(row.detail.contains("#team:example.org"));
        assert!(row.folded.contains("alice") && row.folded.contains("#team"));
        let mut id_only = summary("!c:example.org", "Ops");
        id_only.inviter_id = Some("@bob:example.org".into());
        assert!(
            Row::new(&id_only, 3)
                .detail
                .ends_with("Invited by @bob:example.org")
        );
    }

    #[test]
    fn an_ignored_inviter_is_named_as_such_and_stays_answerable() {
        let mut ignored = summary("!d:example.org", "Promo");
        ignored.inviter_id = Some("@spam:example.org".into());
        ignored.inviter_ignored = true;
        let row = Row::new(&ignored, 1);
        assert!(
            row.detail
                .contains("Invited by @spam:example.org, whom you ignore")
        );
    }

    #[test]
    fn the_highlight_follows_its_invitation_across_a_refresh() {
        let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let shown = ids(&["!a:x", "!b:x", "!c:x"]);
        // The same invitation, wherever it moved.
        assert_eq!(kept_index(&shown, 1, &ids(&["!z:x", "!a:x", "!b:x"])), 2);
        // Gone: the one now in its place, or the last one.
        assert_eq!(kept_index(&shown, 1, &ids(&["!a:x", "!c:x"])), 1);
        assert_eq!(kept_index(&shown, 2, &ids(&["!a:x"])), 0);
        // Nothing chosen, or nothing left: no highlight.
        assert_eq!(kept_index(&shown, -1, &ids(&["!a:x"])), -1);
        assert_eq!(kept_index(&shown, 0, &[]), -1);
    }

    fn answered(
        outcome: InvitationOutcome,
        is_space: bool,
    ) -> Result<InvitationAnswer, MatrixCoreError> {
        Ok(InvitationAnswer {
            outcome,
            is_space,
            direct: DirectMark::NotDirect,
        })
    }

    /// Admit an accept of `!a` for `account`: from its current row with a
    /// generation, else from a Home card.
    fn accept(state: &mut State, account: &str, generation: Option<i32>) -> Result<u64, Refusal> {
        state.admit("!a", generation, generation.is_some(), account, true, false)
    }

    #[test]
    fn a_confirmed_join_opens_with_the_kind_read_when_it_was_admitted() {
        // The core read the Space kind from the invite state when it admitted
        // the answer, so no row needs to be listed any more.
        let settled = answered(InvitationOutcome::Settled, true);
        let space = joined_notice("!s:x", true, &settled).unwrap();
        assert!(space.is_space && space.settled && space.room_id == "!s:x");
        // Confirmed but not here yet: a manual Check and open, no promise.
        let confirmed = answered(InvitationOutcome::Confirmed, false);
        let room = joined_notice("!r:x", true, &confirmed).unwrap();
        assert!(!room.is_space && !room.settled);
        assert!(!JOINED_UNSYNCED.contains("opens once"));
        assert!(JOINED_UNSYNCED.contains("Check and open"));
        // Nothing to open after a decline, an unconfirmed join, or a refusal.
        assert_eq!(joined_notice("!r:x", false, &settled), None);
        let unconfirmed = answered(InvitationOutcome::Unconfirmed, true);
        assert_eq!(joined_notice("!r:x", true, &unconfirmed), None);
        let refused = Err(MatrixCoreError::Sdk("refused".into()));
        assert_eq!(joined_notice("!r:x", true, &refused), None);
    }

    #[test]
    fn a_bulk_confirmation_expires() {
        let asked = Instant::now();
        assert!(confirmation_live(Some(asked), asked));
        assert!(confirmation_live(Some(asked), asked + BULK_CONFIRM_TTL / 2));
        assert!(!confirmation_live(Some(asked), asked + BULK_CONFIRM_TTL));
        assert!(!confirmation_live(None, asked));
        assert!(BULK_EXPIRED.contains("nothing was declined"));
    }

    #[test]
    fn generations_survive_a_relist_only_under_the_same_selection() {
        let mut next = 0;
        let a = summary("!a:x", "A");
        let first = relist(&[], false, &[a.clone(), summary("!b:x", "B")], &mut next);
        assert_eq!((first[0].generation, first[1].generation), (1, 2));
        // The same selection lists A again: it keeps its generation; a new
        // invitation gets a new one.
        let again = relist(&first, true, &[a.clone(), summary("!c:x", "C")], &mut next);
        assert_eq!((again[0].generation, again[1].generation), (1, 3));
        // After A to B and back to A, nothing from the older listing matches.
        let later = relist(&again, false, &[a], &mut next);
        assert_eq!(later[0].generation, 4);
        assert!(!later.iter().any(|row| row.generation == 1));
    }

    #[test]
    fn an_invitation_is_answered_once_and_only_from_its_current_row() {
        let mut state = State {
            rows: relist(&[], false, &[summary("!a", "A")], &mut 0),
            ..Default::default()
        };
        let (quiet, busy) = (Err(Refusal::Quiet), Err(Refusal::Unresolved));
        // A row of an older listing, or of a listing for another selection
        // (A to B and back to A), answers nothing.
        let older = state.admit("!a", Some(2), true, "work", true, false);
        let stale = state.admit("!a", Some(1), false, "work", true, false);
        let unlisted = state.admit("!b", Some(1), true, "work", true, false);
        assert!(older == quiet && stale == quiet && unlisted == quiet);
        let first = accept(&mut state, "work", Some(1)).unwrap();
        // Its row's name was taken when it was admitted.
        assert_eq!(state.guard("work", "!a").unwrap().name, "A");
        // While it is out, no second click, Home card or bulk row sends again.
        assert_eq!(accept(&mut state, "work", Some(1)), busy);
        assert_eq!(accept(&mut state, "work", None), busy);
        let bulk = state.admit("!a", Some(1), true, "work", false, true);
        assert_eq!(bulk, busy);
        assert!(state.answering("work") && !state.answering("home"));
        // An older answer finishing never touches a newer one's guard.
        assert_eq!(state.finish("work", "!a", first + 1, None), None);
        assert!(state.answering("work"));
        assert!(state.finish("work", "!a", first, None).is_some());
        assert!(!state.answering("work"));
        // A running bulk decline blocks single answers but not its own rows.
        state.bulk_running = true;
        assert_eq!(accept(&mut state, "work", Some(1)), quiet);
        let own = state.admit("!a", Some(1), true, "work", false, true);
        assert!(own.is_ok());
    }

    #[test]
    fn a_guard_holds_until_its_own_account_store_shows_the_answer() {
        let mut state = State {
            rows: relist(&[], false, &[summary("!a", "A")], &mut 0),
            ..Default::default()
        };
        // Kept per account and room: another account's answer for the same
        // room is its own.
        let work = accept(&mut state, "work", Some(1)).unwrap();
        let home = accept(&mut state, "home", None).unwrap();
        // Confirmed but not synced yet, or sent with no answer back: kept.
        let confirmed = remaining(&answered(InvitationOutcome::Confirmed, false));
        let lost = remaining(&answered(InvitationOutcome::Unconfirmed, false));
        assert_eq!(confirmed, Some(Stage::Confirmed));
        assert_eq!(lost, Some(Stage::Unconfirmed));
        assert!(state.finish("work", "!a", work, confirmed).is_some());
        assert!(state.finish("home", "!a", home, lost).is_some());
        let busy = Err(Refusal::Unresolved);
        assert_eq!(accept(&mut state, "work", Some(1)), busy);
        assert_eq!(accept(&mut state, "home", None), busy);
        // A listing without the room (bounded, or another account's) proves
        // nothing.
        state.take_listing(&Invitations::default());
        assert!(state.guard("work", "!a").is_some());
        assert!(state.guard("home", "!a").is_some());
        // Only a read of that account's own store retires a guard, and only
        // for the exact answer it was made for.
        let checked = state.unsettled("work");
        assert_eq!(checked, [("!a".to_owned(), true, work)]);
        state.settle("work", &checked, &[false]);
        assert!(state.guard("work", "!a").is_some());
        state.settle("work", &[("!a".into(), true, work + 9)], &[true]);
        assert!(state.guard("work", "!a").is_some());
        state.settle("work", &checked, &[true]);
        assert!(state.guard("work", "!a").is_none());
        assert!(state.guard("home", "!a").is_some());
        // Settled here, or not applied at all: nothing is kept.
        let settled = answered(InvitationOutcome::Settled, false);
        assert_eq!(remaining(&settled), None);
        let gone = Err(MatrixCoreError::RoomNotFound("!a".into()));
        assert_eq!(remaining(&gone), None);
    }

    /// A listing whose write got no answer is kept apart from the guard and
    /// the screen: past the join settling, a close, another account on
    /// screen, a membership read and a listing; only a newer answer's own
    /// receipt replaces it, and none is ever dropped.
    #[test]
    fn a_possibly_applied_direct_listing_outlives_the_view_and_the_account() {
        let mut state = State {
            rows: relist(&[], false, &[summary("!a", "Dana")], &mut 0),
            ..Default::default()
        };
        let first = accept(&mut state, "work", Some(1)).unwrap();
        // The join settled; its listing's write was held, then the account on
        // screen changed before it ended: unknown.
        let unknown = DirectMark::Failed(DirectWriteError::Unknown);
        let guard = state.finish("work", "!a", first, None).unwrap();
        state.record_direct("work", "!a", first, &guard.name, unknown);
        assert!(state.guard("work", "!a").is_none());
        // Nothing on screen, no listing and no membership read settles it.
        state.rows.clear();
        state.selection = None;
        state.joined = None;
        state.take_listing(&Invitations::default());
        state.settle("work", &[("!a".into(), true, first)], &[true]);
        assert_eq!(state.direct_unknown("work"), ["Dana"]);
        assert!(state.direct_unknown("home").is_empty());
        // The center names it again when it opens for that account.
        let reminder = direct_reminder(&state.direct_unknown("work")).unwrap();
        assert_eq!(reminder, format!("Dana: {DIRECT_UNKNOWN}"));
        assert!(reminder.contains("not sent again"));
        assert_eq!(direct_reminder(&[]), None);
        // A definite outcome neither adds nor removes a receipt.
        state.record_direct("work", "!a", first + 1, "Other", DirectMark::NotTried);
        let not_sent = DirectMark::Failed(DirectWriteError::NotSent);
        state.record_direct("work", "!b", first + 1, "Bea", not_sent);
        assert_eq!(state.direct_unknown("work"), ["Dana"]);
        // A newer answer's receipt replaces it; an older finish never
        // replaces a newer one.
        state.record_direct("work", "!a", first + 5, "Dana again", unknown);
        state.record_direct("work", "!a", first, "Dana", unknown);
        assert_eq!(state.direct_unknown("work"), ["Dana again"]);
        // Without a name it is named by its room.
        state.record_direct("work", "!c:x", first + 6, "", unknown);
        assert_eq!(state.direct_unknown("work"), ["!c:x", "Dana again"]);
        let many = direct_reminder(&state.direct_unknown("work")).unwrap();
        assert!(many.starts_with("2 ") && many.contains("not sent again"));
        // A newer answer's notice is never replaced by an older one's late
        // finish; an older one finishing first still sets its own.
        let older = state.admit("!x", None, false, "work", true, false).unwrap();
        let newer = state.admit("!y", None, false, "work", true, false).unwrap();
        assert!(state.takes_notice(older));
        assert!(state.takes_notice(newer));
        assert!(!state.takes_notice(older));
        assert!(state.takes_notice(newer));
        // Unresolved listings count toward the bound: never dropped, new
        // answers refused instead.
        let mut full = State::default();
        full.record_direct("work", "!kept", 1, "", unknown);
        for at in 1..MAX_ANSWER_GUARDS {
            let room = format!("!{at}");
            assert!(full.admit(&room, None, false, "work", true, false).is_ok());
        }
        let refused = full.admit("!last", None, false, "work", true, false);
        assert_eq!(refused, Err(Refusal::Saturated));
        assert_eq!(full.direct_unknown("work"), ["!kept"]);
    }

    /// An account's complete removal forgets its guards, listings and
    /// shown records; its admitted answer finishing after that records
    /// nothing (no listing, row error, notice or room), nor does an answer
    /// finishing with another answer's number; another account's records
    /// stay, and its own finish still records.
    #[test]
    fn a_forgotten_account_keeps_nothing_and_its_late_finish_records_nothing() {
        let mut state = State {
            rows: relist(&[], false, &[summary("!a", "Dana")], &mut 0),
            ..Default::default()
        };
        let unknown = DirectMark::Failed(DirectWriteError::Unknown);
        /// Answer `answer` of `account` for `!a` finished: settled, its
        /// listing unknown, shown, with `failed` as its row error.
        fn finish(
            answer: u64,
            account: &'static str,
            failed: Option<&'static str>,
        ) -> Concluding<'static> {
            Concluding {
                account_id: account,
                room_id: "!a",
                answer,
                next: None,
                direct: DirectMark::Failed(DirectWriteError::Unknown),
                failed,
                shown: true,
                joined: Some(JoinedNotice {
                    room_id: "!a".into(),
                    is_space: false,
                    settled: true,
                }),
            }
        }
        // Work's answer is out, an earlier listing of work may have applied,
        // and home has its own answer out for the same room.
        let out = accept(&mut state, "work", Some(1)).unwrap();
        state.record_direct("work", "!old", out, "Old", unknown);
        let home = accept(&mut state, "home", None).unwrap();
        state.forget_account("work");
        assert!(state.guard("work", "!a").is_none());
        assert!(state.direct_unknown("work").is_empty());
        assert!(state.guard("home", "!a").is_some(), "another account stays");
        // Work's answer finishes after the removal: nothing comes back.
        let late = state.conclude(finish(out, "work", Some(NOT_JOINED)));
        assert!(late.guard.is_none() && !late.noticed);
        assert!(state.direct_unknown("work").is_empty());
        assert!(state.errors.is_empty() && state.joined.is_none());
        assert!(state.guard("work", "!a").is_none());
        // Home's guard, finished with another answer's number: nothing.
        let wrong = state.conclude(finish(home + 1, "home", None));
        assert!(wrong.guard.is_none() && !wrong.noticed);
        assert!(state.direct_unknown("home").is_empty());
        assert!(state.guard("home", "!a").is_some());
        // Its own number: recorded (a Home card's has no row name, so by its
        // room), and the notice is its to set.
        let own = state.conclude(finish(home, "home", None));
        assert!(own.guard.is_some() && own.noticed);
        assert_eq!(state.direct_unknown("home"), ["!a"]);
        assert!(state.joined.is_some());
        // Forgetting another account leaves home's receipts alone, and a
        // second forget of work finds nothing to forget.
        state.forget_account("work");
        assert_eq!(state.direct_unknown("home"), ["!a"]);
    }

    /// INVDA02-HOME on the real window, through the very functions the
    /// finish, forget and close callbacks run (`show_finished`, then
    /// `forget_account` and its queued `clear_forgotten`, `State::close` and
    /// `show_closed`, `publish_rows`), the center never opened: a Home
    /// card's refused answer and its join are owned by their account; a
    /// complete forget clears exactly that account's notice, room, Home
    /// error, row errors and listing receipt, and its join finishing after
    /// it, before the queued clear runs, records and writes nothing; another
    /// account's newer outcome, shown before the removed account's queued
    /// clear runs, stays; a close and another account's forget keep every
    /// guard and receipt, and a row error and the notice's room stay their
    /// own account's, shown only to it. Run from the UI suite's invitations
    /// step; it leaves the center closed and empty with no notice or Home
    /// error, and puts back the search, highlight and account on screen.
    pub(crate) fn home_outcomes_belong_to_their_account(ui: &AppWindow) {
        let global = ui.global::<InvitationsState>();
        assert!(!global.get_open(), "the center is closed");
        assert!(global.get_notice().is_empty() && ui.get_invite_error().is_empty());
        let (active, query, index) = (
            ui.get_active_account_local_id(),
            global.get_query(),
            global.get_index(),
        );
        let rows = [
            summary("!a", "Alpha"),
            summary("!b", "Beta"),
            summary("!c", "Gamma"),
        ];
        let state = Mutex::new(State {
            rows: relist(&[], false, &rows, &mut 0),
            ..Default::default()
        });
        // A Home card's answer of `account` for `room`, admitted.
        let home = |account: &str, room: &str| {
            let admitted = lock(&state).admit(room, None, false, account, true, false);
            admitted.unwrap()
        };
        let finished = |result| Finished {
            accept: true,
            result,
            withdrawn: None,
        };
        let refused = || finished(Err(MatrixCoreError::Sdk("refused".into())));
        // A join sync has not brought yet, whose listing got no answer.
        let unsynced = || {
            finished(Ok(InvitationAnswer {
                outcome: InvitationOutcome::Confirmed,
                is_space: false,
                direct: DirectMark::Failed(DirectWriteError::Unknown),
            }))
        };
        let status = |row: usize| global.get_items().row_data(row).unwrap().status;

        // a answers two Home cards: one is refused, then one joins.
        let failed = home("a", "!b");
        show_finished(ui, &state, "a", "!b", failed, true, refused());
        assert_eq!(ui.get_invite_error(), NOT_JOINED);
        let join = home("a", "!a");
        show_finished(ui, &state, "a", "!a", join, true, unsynced());
        let notice = format!("{JOINED_UNSYNCED} {DIRECT_UNKNOWN}");
        assert_eq!(global.get_notice(), notice.as_str());
        assert_eq!(global.get_notice_room_id(), "!a");
        let home_error = format!("{HOME_JOINED_UNSYNCED} {DIRECT_UNKNOWN}");
        assert_eq!(ui.get_invite_error(), home_error.as_str());
        {
            let kept = lock(&state);
            assert_eq!(kept.notice.account_id.as_deref(), Some("a"));
            assert_eq!(kept.home.account_id.as_deref(), Some("a"));
            assert!(kept.joined.is_some());
            assert_eq!(kept.errors.get(&key("a", "!b")), Some(&NOT_JOINED));
            assert_eq!(kept.direct_unknown("a"), ["!a"]);
        }
        ui.set_active_account_local_id("a".into());
        publish_rows(ui, &state);
        assert_eq!(status(0), WAITING_FOR_SYNC);
        assert_eq!(status(1), NOT_JOINED);
        // b's Home answers are out meanwhile.
        let theirs = home("b", "!a");
        let refusal = home("b", "!c");

        // a is removed completely: its records go at once, under the mutex.
        let forgotten = lock(&state).forget_account("a");
        assert!(forgotten.notice.is_some() && forgotten.home.is_some());
        {
            let kept = lock(&state);
            assert!(kept.guard("a", "!a").is_none() && kept.direct_unknown("a").is_empty());
            assert!(!kept.errors.contains_key(&key("a", "!b")));
            assert!(kept.joined.is_none());
            assert!(kept.notice.account_id.is_none() && kept.home.account_id.is_none());
            assert!(kept.guard("b", "!a").is_some(), "b's stays");
        }
        // Its join finishes before the queued clear runs: no guard of its
        // own, so it records and writes nothing.
        let shown = (global.get_notice(), ui.get_invite_error());
        let late = finished(answered(InvitationOutcome::Settled, false));
        show_finished(ui, &state, "a", "!a", join, true, late);
        assert_eq!((global.get_notice(), ui.get_invite_error()), shown);
        {
            let kept = lock(&state);
            assert!(kept.joined.is_none() && kept.notice.account_id.is_none());
            assert!(kept.direct_unknown("a").is_empty());
        }
        // The queued clear: everything a's writes left goes, rows included.
        clear_forgotten(ui, &state, "a", forgotten);
        assert_eq!(global.get_notice(), "");
        assert_eq!(global.get_notice_room_id(), "");
        assert!(!global.get_notice_room_ready());
        assert_eq!(ui.get_invite_error(), "");
        assert!(status(0).is_empty() && status(1).is_empty());

        // c's join shows and c is removed; before c's queued clear runs,
        // b's newer join shows. The clear leaves b's outcome alone.
        let gone = home("c", "!b");
        show_finished(ui, &state, "c", "!b", gone, true, unsynced());
        let forgotten = lock(&state).forget_account("c");
        let newer = home("b", "!b");
        show_finished(ui, &state, "b", "!b", newer, true, unsynced());
        clear_forgotten(ui, &state, "c", forgotten);
        assert_eq!(global.get_notice(), notice.as_str());
        assert_eq!(global.get_notice_room_id(), "!b");
        assert_eq!(ui.get_invite_error(), home_error.as_str());
        {
            let kept = lock(&state);
            assert_eq!(kept.notice.account_id.as_deref(), Some("b"));
            assert_eq!(kept.home.account_id.as_deref(), Some("b"));
            assert!(kept.direct_unknown("c").is_empty());
        }
        // b's older answer gets no answer back: unresolved, kept, and it
        // does not replace b's newer notice.
        let lost = finished(answered(InvitationOutcome::Unconfirmed, false));
        show_finished(ui, &state, "b", "!a", theirs, true, lost);
        assert_eq!(global.get_notice_room_id(), "!b");
        // b's older answer for !c is refused: only b's row error, while b's
        // newer join keeps the notice and its room.
        show_finished(ui, &state, "b", "!c", refusal, true, refused());
        assert_eq!(global.get_notice_room_id(), "!b");
        // Another account's forget keeps b's row error, notice and room;
        // the error shows on b's rows only, never on a's.
        let stranger = lock(&state).forget_account("x");
        assert!(stranger.notice.is_none() && stranger.home.is_none());
        {
            let kept = lock(&state);
            assert!(kept.joined.is_some());
            assert_eq!(kept.notice.account_id.as_deref(), Some("b"));
            assert_eq!(kept.errors.get(&key("b", "!c")), Some(&NOT_JOINED));
        }
        publish_rows(ui, &state);
        assert!(status(2).is_empty());
        ui.set_active_account_local_id("b".into());
        publish_rows(ui, &state);
        assert_eq!(status(2), NOT_JOINED);
        ui.set_active_account_local_id("a".into());

        // The close path: the notice is no one's and leaves the screen;
        // the Home cards' error, every guard and the receipt stay, and so
        // does all of it when another account is forgotten.
        lock(&state).close();
        show_closed(ui, &state);
        assert_eq!(global.get_notice(), "");
        assert_eq!(ui.get_invite_error(), home_error.as_str());
        let other = lock(&state).forget_account("x");
        assert!(other.notice.is_none() && other.home.is_none());
        {
            let kept = lock(&state);
            let stage = kept.guard("b", "!a").map(|guard| guard.stage);
            assert_eq!(stage, Some(Stage::Unconfirmed));
            assert!(kept.guard("b", "!b").is_some());
            assert_eq!(kept.direct_unknown("b"), ["!b"]);
            assert_eq!(kept.home.account_id.as_deref(), Some("b"));
        }
        // b is removed completely too: its Home error goes with it.
        let forgotten = lock(&state).forget_account("b");
        assert_eq!(forgotten.notice, None, "closed: no one's");
        clear_forgotten(ui, &state, "b", forgotten);
        assert_eq!(ui.get_invite_error(), "");
        {
            let kept = lock(&state);
            assert!(kept.guards.is_empty() && kept.direct.is_empty() && kept.errors.is_empty());
        }

        // Only a complete removal and a successful logout forget, each right
        // after room_permissions' own forget at that boundary; a partial
        // removal or an account switch never reaches it.
        for (source, after, beside) in [
            (
                include_str!("account_access.rs"),
                "if result.complete {",
                "crate::room_permissions::forget_account(&id);",
            ),
            (
                include_str!("main.rs"),
                "Ok(result) => {",
                "room_permissions::forget_account(&account_id);",
            ),
        ] {
            assert_eq!(source.matches("invitations::forget_account(").count(), 1);
            let (before, _) = source.split_once("invitations::forget_account(").unwrap();
            let (_, arm) = before.rsplit_once(after).unwrap();
            // Still in that arm: no other match arm began since.
            assert!(!arm.contains("=>"), "{after}");
            // Next to room_permissions' forget: only comments between them.
            let (_, gap) = arm.rsplit_once(beside).unwrap();
            assert!(!gap.contains(';') && !gap.contains('}'), "{beside}");
        }

        // The center empty; the search, highlight and account as they were.
        publish_rows(ui, &Mutex::new(State::default()));
        global.set_query(query);
        global.set_index(index);
        ui.set_active_account_local_id(active);
    }

    /// INV-OPEN-02: the center's Open and Check and open take the Open's
    /// own click (account, security, navigation) and the owned reveal a Home
    /// card's accept uses (LiveUi::open_answered); nothing opens through the
    /// generic room or Space callbacks, which would choose again from what is
    /// on screen. Both clicks own their open where every protected entry
    /// withdraws it (matrix_links::own_automatic_open).
    #[test]
    fn the_centers_open_goes_through_the_owned_reveal_only() {
        let source = include_str!("invitations.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("invoke_open_room"));
        assert!(!production.contains("invoke_select_space"));
        assert_eq!(production.matches(".owned(&selection)").count(), 2);
        assert_eq!(production.matches("own_automatic_open(").count(), 2);
        assert_eq!(production.matches(".open_answered(").count(), 1);
    }

    /// A Home answer and the center's manual Open are entries like the
    /// window's others: a visible owner refuses each before anything is
    /// withdrawn; admitted, the work an open displaces (a waiting call bar
    /// click, a held unread jump and a link's follow-up through
    /// matrix_links::clear, identity details' focus intent) ends before the
    /// click's open is owned, and the owned open is registered before any
    /// setter or spawn. Source order only: the UI suite registers no center.
    #[test]
    fn answers_and_opens_refuse_first_and_retire_before_owning() {
        let source = include_str!("invitations.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let order = |body: &str, steps: &[&str]| {
            let at = steps
                .iter()
                .map(|step| body.find(step).unwrap_or_else(|| panic!("{step}")))
                .collect::<Vec<_>>();
            assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{steps:?}");
        };
        let (_, answer) = production.split_once("    fn answer(").unwrap();
        let answer = answer.split("\n    fn ").next().unwrap();
        order(
            answer,
            &[
                "ui.get_invitations_entry_blocked()",
                "crate::withdraw_call_bar(&ui);",
                "crate::matrix_links::clear(&ui);",
                "crate::identity_warnings::withdraw(&ui);",
                "crate::matrix_links::own_automatic_open(",
                "ui.set_invite_error(\"\".into());",
                "self.runtime.spawn(",
            ],
        );
        let (_, open) = production.split_once("    fn open_room(").unwrap();
        let open = open.split("\n    fn ").next().unwrap();
        order(
            open,
            &[
                "ui.get_invitations_entry_blocked()",
                "crate::withdraw_call_bar(&ui);",
                "crate::matrix_links::clear(&ui);",
                "crate::identity_warnings::withdraw(&ui);",
                "crate::matrix_links::own_automatic_open(",
                "self.open_joined(",
                "self.runtime.spawn(",
            ],
        );
    }

    #[test]
    fn past_the_bound_new_answers_are_refused_and_no_guard_is_dropped() {
        let mut state = State::default();
        for at in 0..MAX_ANSWER_GUARDS {
            let room = format!("!{at}");
            assert!(state.admit(&room, None, false, "work", true, false).is_ok());
        }
        let full = state.admit("!last", None, false, "work", true, false);
        assert_eq!(full, Err(Refusal::Saturated));
        assert_eq!(state.guards.len(), MAX_ANSWER_GUARDS);
        assert!(state.guard("work", "!0").is_some());
        // A guard its own result retired makes room again.
        let first = state.guard("work", "!0").unwrap().answer;
        assert!(state.finish("work", "!0", first, None).is_some());
        assert!(
            state
                .admit("!last", None, false, "work", true, false)
                .is_ok()
        );
        assert!(SATURATED.contains("was not sent"));
    }

    #[test]
    fn outcomes_never_claim_more_than_the_homeserver_confirmed() {
        let settled = answered(InvitationOutcome::Settled, false);
        assert_eq!(outcome(true, &settled), ("Joined.", false));
        assert_eq!(outcome(false, &settled), ("Declined.", false));
        // Confirmed, but sync has not brought the new membership yet.
        let confirmed = answered(InvitationOutcome::Confirmed, false);
        assert_eq!(outcome(true, &confirmed), (JOINED_UNSYNCED, false));
        assert_eq!(outcome(false, &confirmed), (DECLINED_UNSYNCED, false));
        // Sent with no answer back: it may still apply, and says so.
        let lost = answered(InvitationOutcome::Unconfirmed, false);
        assert_eq!(outcome(true, &lost), (JOIN_UNCONFIRMED, true));
        assert_eq!(outcome(false, &lost), (DECLINE_UNCONFIRMED, true));
        // Not applied at all: nothing was sent, or it was refused.
        let gone = Err(MatrixCoreError::RoomNotFound("!a:x".into()));
        assert_eq!(outcome(true, &gone), (GONE, true));
        let switched = Err(MatrixCoreError::SelectedAccountChanged);
        assert_eq!(outcome(false, &switched), (ACCOUNT_CHANGED, true));
        let refused = Err(MatrixCoreError::Sdk("M_FORBIDDEN secret".into()));
        assert_eq!(outcome(true, &refused), (NOT_JOINED, true));
        assert_eq!(outcome(false, &refused), (NOT_DECLINED, true));
        // The account transition stayed busy past its bound: nothing sent.
        let busy = Err(MatrixCoreError::AccountAccessBusy);
        assert_eq!(outcome(true, &busy), (BUSY, true));
        for text in [
            GONE,
            ACCOUNT_CHANGED,
            NOT_JOINED,
            NOT_DECLINED,
            JOIN_UNCONFIRMED,
            DECLINE_UNCONFIRMED,
            JOINED_UNSYNCED,
            DECLINED_UNSYNCED,
            UNCONFIRMED,
            NOT_HERE_YET,
            READ_FAILED,
            SIGN_IN,
            BUSY,
            VIEW_CLOSED,
            SECURITY_STEP,
            SESSION_ENDED,
            DIRECT_NOT_TRIED,
            DIRECT_NO_INVITER,
            DIRECT_NOT_SENT,
            DIRECT_UNREADABLE,
            DIRECT_CHANGED,
            DIRECT_REFUSED,
            DIRECT_UNKNOWN,
        ] {
            assert!(!text.contains("M_"), "{text}");
            assert!(!text.to_lowercase().contains("undone"), "{text}");
            // Nothing promises a retry that does not happen by itself.
            assert!(!text.contains("opens once"), "{text}");
            assert!(!text.contains("goes from this list"), "{text}");
        }
    }

    #[test]
    fn a_bulk_decline_reports_each_way_it_ended() {
        assert_eq!(bulk_summary(3, 0, 0, false), "Declined 3.");
        assert_eq!(
            bulk_summary(1, 2, 3, false),
            "Declined 1. 2 not confirmed and not sent again. 3 not applied. See their rows."
        );
        assert!(bulk_summary(0, 0, 0, true).ends_with(SATURATED));
    }

    #[test]
    fn an_answer_goes_only_while_what_it_was_chosen_under_holds() {
        // Live, the security gate where it was, the view still open: it goes.
        assert_eq!(withdrawal(true, true, true), None);
        // Otherwise nothing is sent, and the first reason in this order says
        // why.
        assert_eq!(withdrawal(false, false, false), Some(Withdrawal::Ended));
        assert_eq!(withdrawal(true, false, false), Some(Withdrawal::Security));
        assert_eq!(withdrawal(true, true, false), Some(Withdrawal::Closed));
        // The listing after a confirmed join is part of that join: a closed
        // view does not stop it; the end of the session or a security step
        // does.
        assert!(listing_allowed(None));
        assert!(listing_allowed(Some(Withdrawal::Closed)));
        assert!(!listing_allowed(Some(Withdrawal::Security)));
        assert!(!listing_allowed(Some(Withdrawal::Ended)));
        // A withdrawn answer says why and that nothing was sent, and ends a
        // bulk decline.
        for reason in [Withdrawal::Ended, Withdrawal::Security, Withdrawal::Closed] {
            let withdrawn = Finished {
                accept: true,
                result: Err(MatrixCoreError::Sdk("withdrawn".into())),
                withdrawn: Some(reason),
            };
            let (text, flagged) = withdrawn.text();
            assert!(flagged && text.ends_with("nothing was sent."), "{text}");
            assert_eq!(text, withdrawn_text(reason));
            assert!(ends_bulk(&withdrawn));
        }
        // Not withdrawn: its own outcome stands; only an account change or a
        // busy transition ends a bulk decline.
        let busy = Finished {
            accept: false,
            result: Err(MatrixCoreError::AccountAccessBusy),
            withdrawn: None,
        };
        assert_eq!(busy.text(), (BUSY, true));
        assert!(ends_bulk(&busy));
        let refused = Finished {
            accept: false,
            result: Err(MatrixCoreError::Sdk("refused".into())),
            withdrawn: None,
        };
        assert_eq!(refused.text(), (NOT_DECLINED, true));
        assert!(!ends_bulk(&refused));
    }

    #[test]
    fn a_direct_listing_is_reported_only_as_far_as_it_is_known() {
        use morpheus_matrix::DirectListing;
        let joined = |outcome, direct| -> Result<InvitationAnswer, MatrixCoreError> {
            Ok(InvitationAnswer {
                outcome,
                is_space: false,
                direct,
            })
        };
        let settled = InvitationOutcome::Settled;
        // Listed now or already, or nothing to list: no word.
        for direct in [
            DirectMark::NotDirect,
            DirectMark::Listed(DirectListing::Listed),
            DirectMark::Listed(DirectListing::AlreadyListed),
        ] {
            assert_eq!(direct_note(&joined(settled, direct)), None);
        }
        // An unconfirmed join is never listed; a join whose invitation named
        // no inviter cannot be.
        let lost = joined(InvitationOutcome::Unconfirmed, DirectMark::NotTried);
        assert_eq!(direct_note(&lost), Some(DIRECT_NOT_TRIED));
        let unnamed = joined(settled, DirectMark::NotTried);
        assert_eq!(direct_note(&unnamed), Some(DIRECT_NO_INVITER));
        // Each failure says what it means. Only a write that got no answer
        // may have applied, and it is never sent again.
        let failed = |error| direct_note(&joined(settled, DirectMark::Failed(error))).unwrap();
        assert_eq!(failed(DirectWriteError::Unknown), DIRECT_UNKNOWN);
        assert!(DIRECT_UNKNOWN.contains("may have applied"));
        assert!(DIRECT_UNKNOWN.contains("not sent again"));
        assert_eq!(failed(DirectWriteError::BaselineChanged), DIRECT_CHANGED);
        assert_eq!(failed(DirectWriteError::Unreadable), DIRECT_UNREADABLE);
        assert_eq!(failed(DirectWriteError::Refused), DIRECT_REFUSED);
        assert_eq!(failed(DirectWriteError::NotSent), DIRECT_NOT_SENT);
        for note in [
            DIRECT_CHANGED,
            DIRECT_UNREADABLE,
            DIRECT_REFUSED,
            DIRECT_NOT_SENT,
        ] {
            assert!(!note.contains("may have applied"), "{note}");
        }
        // An answer that was not applied has no listing to report.
        let refused = Err(MatrixCoreError::Sdk("refused".into()));
        assert_eq!(direct_note(&refused), None);
        // The note follows the outcome's own words.
        let told = with_note("Joined.", Some(DIRECT_NOT_SENT));
        assert_eq!(told, format!("Joined. {DIRECT_NOT_SENT}"));
        assert_eq!(with_note("Joined.", None), "Joined.");
    }
}
