//! Members of one room whose cryptographic identity changed, for one exact
//! account selection. Read-only.
//!
//! Everything comes from the SDK's opt-in stored-members feed,
//! `Room::subscribe_to_identity_status_changes_no_sync` (Morpheus patch, see
//! vendor/matrix-sdk/MORPHEUS_PATCH.md), on the account's own client. Nothing
//! here changes trust: no identity is pinned, verified, withdrawn or
//! acknowledged, nothing is dismissed, and room-key sharing is untouched.
//!
//! Not asking the homeserver is a property of that SDK entry, not of the
//! checks below. Its subscription awaits the crypto identity stream (a read
//! lock on the client's OlmMachine and the crypto store's broadcast), registers
//! a room-member event handler, and reads the joined and invited members
//! stored on this device and their identities from the local crypto store.
//! Each later update reads one stored member (`get_member_no_sync`) or one
//! stored identity. None of these awaits can request `/members`, a profile or
//! keys, or change a sync flag, whatever the member list's state is at the
//! time. A stored member counts only while joined or invited, so a member who
//! left or was banned is not warned about again until they rejoin. A local read
//! that fails is never an empty answer: the entry refuses to start, or the
//! feed ends, and the room is reported as not checked.
//!
//! `Room::are_members_synced` decides only what may truthfully be claimed:
//! with an incomplete local member list the feed could miss members. The feed
//! is started only while the list is complete, that is checked again before
//! every poll of the feed and after every await before a report, and the feed
//! is dropped (reported as [`IdentityWarningUpdate::NotChecked`]) as soon as
//! the list is not, for example after a gappy sync. Cancellation and selection
//! revocation are checked before every poll and after every await. Member
//! names come from the local store only.
//!
//! The SDK's first item is the room's current state; later items change it. A
//! member in `PinViolation` or `VerificationViolation` is warned about, and
//! `Pinned` or `Verified` (also sent when a warned member leaves) clears them.
//! An identity that is merely new or unverified is never reported as changed.
//!
//! Display names are untrusted server data: bounded and cleaned here. User
//! IDs are untrusted too, and are kept exact so they can route a member
//! lookup; they are never a trust claim. ruma accepts a user ID whose
//! localpart holds any character but `:` and NUL (its backwards-compatible
//! check: line breaks, tabs, direction and other formatting characters, any
//! Unicode), and the SDK's `compat-arbitrary-length-ids` removes the 255-byte
//! limit, so an accepted ID has no length bound. The exact ID is used only to
//! follow members, as the shown-ID whitelist and to route View member; it is
//! never shown or read aloud. What is shown is [`display_user_id`]: bounded,
//! with every character outside printable ASCII escaped visibly. Following is
//! bounded by entry count ([`MAX_TRACKED_IDENTITY_WARNINGS`]), not by bytes.

use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

use futures_util::{FutureExt, Stream, StreamExt, future::poll_fn};
use matrix_sdk::{
    Room, RoomState,
    ruma::{OwnedUserId, UserId},
};
use matrix_sdk_crypto::{IdentityState, IdentityStatusChange};
use tokio::{sync::Notify, time};

use crate::{AccountManager, AccountSelection, account_room};

/// Members shown with their details; the rest are counted.
pub const MAX_IDENTITY_WARNINGS: usize = 64;
/// Members followed at once. Past this the count is a lower bound
/// ([`IdentityWarnings::at_least`]) until the feed starts again. This bounds
/// entries, not bytes: each keeps its exact user ID, which has no length bound
/// while the SDK's ID compatibility is enabled.
pub const MAX_TRACKED_IDENTITY_WARNINGS: usize = 1024;
/// Members whose local display names are read; the rest show their ID.
pub const MAX_IDENTITY_WARNING_NAMES: usize = 8;
/// Longest display name kept, in characters.
pub const MAX_IDENTITY_NAME_CHARS: usize = 128;
/// Longest user ID shown or read aloud ([`display_user_id`]), in characters,
/// escapes and the truncation mark included. Every character is ASCII but
/// that one `…` (3 bytes), so it is also at most this many plus 2 bytes.
pub const MAX_IDENTITY_ID_DISPLAY_CHARS: usize = 128;
/// A shortened shown ID keeps this many characters before its `…`.
const ID_HEAD_CHARS: usize = (MAX_IDENTITY_ID_DISPLAY_CHARS - 1) / 2;
/// And this many after it, where the server name ends.
const ID_TAIL_CHARS: usize = MAX_IDENTITY_ID_DISPLAY_CHARS - 1 - ID_HEAD_CHARS;
/// Input examined for one label, in bytes.
const MAX_LABEL_INPUT_BYTES: usize = 1024;
const NAME_LIMIT: Duration = Duration::from_millis(400);
const SUBSCRIBE_LIMIT: Duration = Duration::from_secs(15);

/// Why a member is warned about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityWarningKind {
    /// Their identity changed after this account verified them
    /// (`VerificationViolation`). Listed first.
    VerifiedChanged,
    /// Their identity changed since this account first saw it, and the change
    /// was not acknowledged (`PinViolation`).
    Changed,
}

/// One warned member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityWarning {
    /// The exact Matrix user ID the SDK reported, byte for byte. Routes a
    /// member lookup; not a trust claim. Never shown or read aloud: an
    /// accepted ID may hold line-break, control or direction characters and
    /// has no length bound ([`Self::display_id`] is what is shown).
    pub user_id: String,
    /// Their display name in this device's store, cleaned, when read.
    pub name: Option<String>,
    pub kind: IdentityWarningKind,
}

impl IdentityWarning {
    /// [`Self::user_id`] as it may be shown or read aloud
    /// ([`display_user_id`]); never for routing or lookup.
    pub fn display_id(&self) -> String {
        display_user_id(&self.user_id)
    }
}

/// The warned members of one room as last reported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityWarnings {
    /// Verified-identity changes first, then by user ID; at most
    /// [`MAX_IDENTITY_WARNINGS`].
    pub members: Vec<IdentityWarning>,
    /// Warned members followed but not in `members`.
    pub omitted: usize,
    /// More members may be warned about than [`Self::count`]: more than
    /// [`MAX_TRACKED_IDENTITY_WARNINGS`] were reported, so the count is a
    /// lower bound until the feed starts again. It stays set when every
    /// followed member clears (count 0): those not followed may still be
    /// warned about.
    pub at_least: bool,
}

impl IdentityWarnings {
    /// Warned members known; a lower bound when [`Self::at_least`].
    pub fn count(&self) -> usize {
        self.members.len() + self.omitted
    }

    /// Nobody is warned about, as far as the feed knows: no followed member
    /// and no warning that could not be followed.
    pub fn is_empty(&self) -> bool {
        self.count() == 0 && !self.at_least
    }
}

/// One report of the feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityWarningUpdate {
    /// The room's member list is not complete in this device's store, or the
    /// SDK feed could not start, so no identity was checked. Nothing is
    /// claimed either way, and earlier warnings no longer hold.
    NotChecked,
    /// Checked against the SDK's current state.
    Checked(IdentityWarnings),
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum IdentityWarningError {
    #[error("the selected account changed")]
    AccountChanged,
    #[error("identity warnings were closed")]
    Cancelled,
    #[error("the room is not available")]
    Unavailable,
}

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Stops a watch, and any wait inside it, from every clone.
#[derive(Clone, Default)]
pub struct IdentityWarningCancellation(Arc<CancellationState>);

impl IdentityWarningCancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    /// Resolves once any clone is cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

type ChangeStream = Pin<Box<dyn Stream<Item = Vec<IdentityStatusChange>> + Send>>;
type InfoStream = Pin<Box<dyn Stream<Item = ()> + Send>>;

/// The SDK feed for one room, run only while the member list is complete in
/// the local store. Dropping it drops the SDK feed and its event handler.
pub struct IdentityWarningWatch {
    room: Room,
    selection: AccountSelection,
    cancellation: IdentityWarningCancellation,
    /// Wakes on every change of the room's local state (a later sync).
    info: InfoStream,
    changes: Option<ChangeStream>,
    set: WarningSet,
    /// `NotChecked` was the last report, so it is not repeated.
    reported_unchecked: bool,
    /// The SDK refused to start (or ended at once): retry only after the
    /// room changes.
    refused: bool,
}

/// What one wait of a running feed produced.
enum Step {
    Stop,
    Info(bool),
    Unsynced,
    Changes(Option<Vec<IdentityStatusChange>>),
}

/// What starting the SDK feed produced.
enum Started {
    /// The room's current state, or `None` once the watch ended.
    Report(Option<IdentityWarningUpdate>),
    Retry,
    Stop,
}

impl IdentityWarningWatch {
    /// The next report. `None` once the selection is revoked, the watch is
    /// cancelled, or the room's state is gone. A report is returned only
    /// while the selection is still current and the watch not cancelled.
    pub async fn next(&mut self) -> Option<IdentityWarningUpdate> {
        loop {
            if !self.in_scope() {
                return None;
            }
            if self.changes.is_none() {
                if !self.room.are_members_synced() || self.refused {
                    if !self.reported_unchecked {
                        self.reported_unchecked = true;
                        return Some(IdentityWarningUpdate::NotChecked);
                    }
                    // Wait for the room's state to change (a later sync).
                    if !self.wait_for_room().await {
                        return None;
                    }
                    self.refused = false;
                    continue;
                }
                match self.start().await {
                    Started::Report(update) => return update,
                    Started::Retry => continue,
                    Started::Stop => return None,
                }
            }
            let step = {
                let room = &self.room;
                let changes = self.changes.as_mut().expect("a running feed");
                let selection = &self.selection;
                let cancellation = &self.cancellation;
                let info = &mut self.info;
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Step::Stop,
                    _ = selection.revoked() => Step::Stop,
                    more = info.next() => Step::Info(more.is_some()),
                    // Checked before every poll: no change is processed
                    // against an incomplete member list, so nothing is
                    // claimed from one. (The stored-members feed never
                    // requests the list either way.)
                    batch = poll_fn(|cx| {
                        if !room.are_members_synced() {
                            return Poll::Ready(None);
                        }
                        changes.poll_next_unpin(cx).map(Some)
                    }) => match batch {
                        None => Step::Unsynced,
                        Some(batch) => Step::Changes(batch),
                    },
                }
            };
            match step {
                Step::Stop | Step::Info(false) => return None,
                Step::Info(true) => {
                    if !self.room.are_members_synced() {
                        let update = self.drop_feed();
                        return self.publish(update);
                    }
                }
                Step::Unsynced => {
                    let update = self.drop_feed();
                    return self.publish(update);
                }
                // The SDK feed ended (its client went away, or a stored
                // member or identity could not be read for a change): not
                // checked, never a silent all-clear.
                Step::Changes(None) => {
                    self.refused = true;
                    let update = self.drop_feed();
                    return self.publish(update);
                }
                Step::Changes(Some(batch)) => {
                    self.set.apply(batch);
                    return self.report().await;
                }
            }
        }
    }

    /// The followed warnings, with names read from the local store. `None`
    /// once the watch is cancelled or its selection revoked, also during
    /// those reads; `NotChecked` (and the feed dropped) when the member list
    /// became incomplete meanwhile, so `Checked` is never claimed for it.
    async fn report(&mut self) -> Option<IdentityWarningUpdate> {
        let warnings = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return None,
            _ = self.selection.revoked() => return None,
            warnings = self.set.warnings(&self.room) => warnings,
        };
        if !self.room.are_members_synced() {
            let update = self.drop_feed();
            return self.publish(update);
        }
        self.publish(IdentityWarningUpdate::Checked(warnings))
    }

    fn in_scope(&self) -> bool {
        !self.cancellation.is_cancelled() && self.selection.is_current()
    }

    fn publish(&self, update: IdentityWarningUpdate) -> Option<IdentityWarningUpdate> {
        self.in_scope().then_some(update)
    }

    /// Stop the SDK feed and forget everything it reported.
    fn drop_feed(&mut self) -> IdentityWarningUpdate {
        self.changes = None;
        self.set = WarningSet::default();
        self.reported_unchecked = true;
        IdentityWarningUpdate::NotChecked
    }

    /// `false` once the watch left its scope or the room's state is gone.
    async fn wait_for_room(&mut self) -> bool {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => false,
            _ = self.selection.revoked() => false,
            more = self.info.next() => more.is_some(),
        }
    }

    /// Start the SDK feed and report the room's current state.
    async fn start(&mut self) -> Started {
        // The stored-members entry: its awaits read this device's stores only.
        let subscribed = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return Started::Stop,
            _ = self.selection.revoked() => return Started::Stop,
            subscribed = time::timeout(
                SUBSCRIBE_LIMIT,
                self.room.subscribe_to_identity_status_changes_no_sync(),
            ) => subscribed,
        };
        if !self.in_scope() {
            return Started::Stop;
        }
        let Ok(Ok(stream)) = subscribed else {
            // The SDK could not start the feed (for example no encryption
            // state yet, or the stored members or their identities could not
            // be read, which is not the same as nobody being warned about):
            // nothing was checked; retry after the room changes.
            self.refused = true;
            return Started::Retry;
        };
        if !self.room.are_members_synced() {
            // Incomplete again while subscribing: dropped before any poll.
            return Started::Retry;
        }
        let mut changes: ChangeStream = Box::pin(stream);
        self.set = WarningSet::default();
        // The first poll yields the current state at once when anyone is
        // warned about; pending means nobody is (the SDK yields a first
        // state only when it is not empty).
        match changes.next().now_or_never() {
            Some(Some(batch)) => self.set.apply(batch),
            Some(None) => {
                self.refused = true;
                return Started::Retry;
            }
            None => {}
        }
        self.changes = Some(changes);
        self.reported_unchecked = false;
        Started::Report(self.report().await)
    }
}

impl AccountManager {
    /// Watch the identity warnings of one joined room for exactly this
    /// selection. Resolving the room reads only local state; the watch starts
    /// the SDK feed only while the member list is complete locally.
    pub async fn identity_warnings_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &IdentityWarningCancellation,
    ) -> std::result::Result<IdentityWarningWatch, IdentityWarningError> {
        let scope = || {
            if cancellation.is_cancelled() {
                return Err(IdentityWarningError::Cancelled);
            }
            if !selection.is_current() {
                return Err(IdentityWarningError::AccountChanged);
            }
            Ok(())
        };
        scope()?;
        let room = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(IdentityWarningError::Cancelled),
            _ = selection.revoked() => return Err(IdentityWarningError::AccountChanged),
            room = async {
                let state = self.state.lock().await;
                let current = self
                    .account_selection(selection.account_id())
                    .ok_or(IdentityWarningError::AccountChanged)?;
                if !selection.same_epoch(&current) {
                    return Err(IdentityWarningError::AccountChanged);
                }
                scope()?;
                account_room(&state, selection.account_id(), room_id)
                    .map_err(|_| IdentityWarningError::Unavailable)
            } => room?,
        };
        scope()?;
        if room.state() != RoomState::Joined {
            return Err(IdentityWarningError::Unavailable);
        }
        let info: InfoStream = Box::pin(room.subscribe_info().map(|_| ()));
        Ok(IdentityWarningWatch {
            room,
            selection: selection.clone(),
            cancellation: cancellation.clone(),
            info,
            changes: None,
            set: WarningSet::default(),
            reported_unchecked: false,
            refused: false,
        })
    }
}

/// The warned members followed from one SDK feed.
#[derive(Debug, Default)]
struct WarningSet {
    tracked: BTreeMap<OwnedUserId, IdentityWarningKind>,
    /// A warning arrived while `tracked` was full.
    untracked: bool,
}

impl WarningSet {
    fn apply(&mut self, changes: Vec<IdentityStatusChange>) {
        for change in changes {
            match warning_kind(&change.changed_to) {
                Some(kind) => {
                    if self.tracked.len() < MAX_TRACKED_IDENTITY_WARNINGS
                        || self.tracked.contains_key(&change.user_id)
                    {
                        self.tracked.insert(change.user_id, kind);
                    } else {
                        self.untracked = true;
                    }
                }
                None => {
                    self.tracked.remove(&change.user_id);
                }
            }
        }
    }

    /// The members to show, in order, and how many more are followed.
    fn shown(&self) -> (Vec<(&OwnedUserId, IdentityWarningKind)>, usize) {
        let mut members: Vec<_> = self
            .tracked
            .iter()
            .map(|(user, kind)| (user, *kind))
            .collect();
        // Stable, so user ID order is kept within each kind.
        members.sort_by_key(|(_, kind)| *kind);
        let omitted = members.len().saturating_sub(MAX_IDENTITY_WARNINGS);
        members.truncate(MAX_IDENTITY_WARNINGS);
        (members, omitted)
    }

    /// The report without names, and the shown members' IDs in its order.
    /// `at_least` stays set while a warning could not be followed, also once
    /// every followed member cleared.
    fn unnamed(&self) -> (Vec<&OwnedUserId>, IdentityWarnings) {
        let (shown, omitted) = self.shown();
        let users = shown.iter().map(|(user, _)| *user).collect();
        let members = shown
            .into_iter()
            .map(|(user, kind)| IdentityWarning {
                user_id: user.to_string(),
                name: None,
                kind,
            })
            .collect();
        let warnings = IdentityWarnings {
            members,
            omitted,
            at_least: self.untracked,
        };
        (users, warnings)
    }

    async fn warnings(&self, room: &Room) -> IdentityWarnings {
        let (users, mut warnings) = self.unnamed();
        for (user, member) in users
            .into_iter()
            .zip(&mut warnings.members)
            .take(MAX_IDENTITY_WARNING_NAMES)
        {
            member.name = local_name(room, user).await;
        }
        warnings
    }
}

/// Only the two violation states warn. `Pinned` and `Verified` clear,
/// including the `Pinned` the SDK sends when a warned member leaves.
fn warning_kind(state: &IdentityState) -> Option<IdentityWarningKind> {
    match state {
        IdentityState::PinViolation => Some(IdentityWarningKind::Changed),
        IdentityState::VerificationViolation => Some(IdentityWarningKind::VerifiedChanged),
        IdentityState::Pinned | IdentityState::Verified => None,
    }
}

/// Trimmed, bounded and free of control or direction-override characters;
/// `None` when nothing printable remains.
fn clean_label(value: &str) -> Option<String> {
    let cleaned: String = value
        .char_indices()
        .take_while(|(offset, _)| *offset < MAX_LABEL_INPUT_BYTES)
        .map(|(_, character)| character)
        .filter(|&character| !hidden(character))
        .skip_while(|character| character.is_whitespace())
        .take(MAX_IDENTITY_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim_end();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

fn hidden(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

/// A user ID as it may be shown or read aloud; never for routing or lookup.
///
/// Printable ASCII (`!` to `~`) stays itself, a backslash is doubled, and
/// every other character is escaped visibly: `\n`, `\r`, `\t`, or `\u{hex}`
/// for any other control, a space, a line or paragraph separator, a direction
/// or other formatting character and any other non-ASCII character. Nothing
/// is removed or replaced by something that could be its own character, so
/// distinct IDs never show the same text; an ID in the spec's grammar, or a
/// historical one, shows unchanged except that a backslash is doubled.
///
/// At most [`MAX_IDENTITY_ID_DISPLAY_CHARS`] characters: a longer escaped ID
/// keeps at most its first 63 (`ID_HEAD_CHARS`) and last 64 (`ID_TAIL_CHARS`)
/// characters, where the server name ends, around one `…`, cutting only
/// between whole escapes. `…` marks only that cut: as a character of the ID
/// it is always escaped. Only that much of the ID is read, however long it
/// is. Two long IDs that agree in both kept ends show the same; the exact IDs
/// still differ.
pub fn display_user_id(user_id: &str) -> String {
    let mut shown = String::new();
    for character in user_id.chars() {
        shown.push_str(&escaped(character));
        if shown.len() > MAX_IDENTITY_ID_DISPLAY_CHARS {
            return shortened(user_id);
        }
    }
    shown
}

/// One character of a shown user ID (see [`display_user_id`]).
fn escaped(character: char) -> String {
    match character {
        '\\' => "\\\\".to_owned(),
        '!'..='~' => character.to_string(),
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\t' => "\\t".to_owned(),
        other => other.escape_unicode().collect(),
    }
}

/// The longest whole escapes from each end that fit around one `…`.
fn shortened(user_id: &str) -> String {
    let head = fitting(user_id.chars(), ID_HEAD_CHARS);
    let mut tail = fitting(user_id.chars().rev(), ID_TAIL_CHARS);
    tail.reverse();
    let mut shown = head.concat();
    shown.push('…');
    shown.push_str(&tail.concat());
    shown
}

/// The escapes of `characters`, in order, while together they fit in
/// `limit` characters; an escape is never split.
fn fitting(characters: impl Iterator<Item = char>, limit: usize) -> Vec<String> {
    let mut used = 0;
    characters
        .map(escaped)
        .take_while(|unit| {
            used += unit.len();
            used <= limit
        })
        .collect()
}

/// A member's display name from this device's store only (never `/members`).
async fn local_name(room: &Room, user: &UserId) -> Option<String> {
    let read = async {
        let member = room.get_member_no_sync(user).await.ok()??;
        member.display_name().and_then(clean_label)
    };
    time::timeout(NAME_LIMIT, read).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(user: &str, state: IdentityState) -> IdentityStatusChange {
        IdentityStatusChange {
            user_id: UserId::parse(user).unwrap(),
            changed_to: state,
        }
    }

    fn kinds(set: &WarningSet) -> Vec<(String, IdentityWarningKind)> {
        set.shown()
            .0
            .into_iter()
            .map(|(user, kind)| (user.to_string(), kind))
            .collect()
    }

    #[test]
    fn only_violations_warn_and_pinned_or_verified_clear_them() {
        let mut set = WarningSet::default();
        // The SDK's first state lists only members not pinned or verified.
        set.apply(vec![
            change("@pat:example.org", IdentityState::PinViolation),
            change("@sam:example.org", IdentityState::VerificationViolation),
        ]);
        assert_eq!(
            kinds(&set),
            [
                (
                    "@sam:example.org".into(),
                    IdentityWarningKind::VerifiedChanged
                ),
                ("@pat:example.org".into(), IdentityWarningKind::Changed),
            ]
        );
        // Acknowledged elsewhere, or a warned member left (sent as Pinned).
        set.apply(vec![change("@pat:example.org", IdentityState::Pinned)]);
        set.apply(vec![change("@sam:example.org", IdentityState::Verified)]);
        assert!(kinds(&set).is_empty());
        // A new or merely unverified identity is never a warning.
        set.apply(vec![change("@new:example.org", IdentityState::Pinned)]);
        assert!(kinds(&set).is_empty());
        assert!(!set.untracked);
    }

    #[test]
    fn a_member_moves_between_kinds_without_being_counted_twice() {
        let mut set = WarningSet::default();
        set.apply(vec![change(
            "@pat:example.org",
            IdentityState::PinViolation,
        )]);
        set.apply(vec![change(
            "@pat:example.org",
            IdentityState::VerificationViolation,
        )]);
        assert_eq!(
            kinds(&set),
            [(
                "@pat:example.org".into(),
                IdentityWarningKind::VerifiedChanged
            )]
        );
    }

    #[test]
    fn counts_are_exact_until_tracking_is_full_then_a_lower_bound() {
        let mut set = WarningSet::default();
        let users: Vec<String> = (0..MAX_TRACKED_IDENTITY_WARNINGS + 3)
            .map(|index| format!("@member{index:05}:example.org"))
            .collect();
        set.apply(
            users
                .iter()
                .map(|user| change(user, IdentityState::PinViolation))
                .collect(),
        );
        let (shown, omitted) = set.shown();
        assert_eq!(shown.len(), MAX_IDENTITY_WARNINGS);
        assert_eq!(
            omitted,
            MAX_TRACKED_IDENTITY_WARNINGS - MAX_IDENTITY_WARNINGS
        );
        // Three reported warnings could not be followed: never an exact count.
        assert!(set.untracked);
        // A followed member clearing still lowers what is followed.
        set.apply(vec![change(&users[0], IdentityState::Pinned)]);
        assert_eq!(set.tracked.len(), MAX_TRACKED_IDENTITY_WARNINGS - 1);
        assert!(set.untracked, "still a lower bound until the feed restarts");
    }

    #[test]
    fn clearing_every_followed_member_keeps_the_unfollowed_ones_uncertain() {
        let mut set = WarningSet::default();
        let users: Vec<String> = (0..MAX_TRACKED_IDENTITY_WARNINGS + 1)
            .map(|index| format!("@member{index:05}:example.org"))
            .collect();
        set.apply(
            users
                .iter()
                .map(|user| change(user, IdentityState::PinViolation))
                .collect(),
        );
        // Every followed member clears; the one not followed may still be in
        // violation, so the report is not empty and claims no count.
        set.apply(
            users[..MAX_TRACKED_IDENTITY_WARNINGS]
                .iter()
                .map(|user| change(user, IdentityState::Pinned))
                .collect(),
        );
        let (shown, report) = set.unnamed();
        assert!(shown.is_empty());
        assert_eq!((report.count(), report.omitted), (0, 0));
        assert!(report.at_least && !report.is_empty());
        // Its clearing alone does not make the report empty either: it was
        // never followed.
        set.apply(vec![change(
            &users[MAX_TRACKED_IDENTITY_WARNINGS],
            IdentityState::Pinned,
        )]);
        assert!(!set.unnamed().1.is_empty());
        // Only a feed that starts again (a new set) is empty.
        assert!(WarningSet::default().unnamed().1.is_empty());
    }

    #[test]
    fn the_report_lists_followed_members_in_order_with_ids() {
        let mut set = WarningSet::default();
        set.apply(vec![
            change("@pat:example.org", IdentityState::PinViolation),
            change("@sam:example.org", IdentityState::VerificationViolation),
        ]);
        let (shown, report) = set.unnamed();
        assert_eq!(
            shown.iter().map(|user| user.as_str()).collect::<Vec<_>>(),
            ["@sam:example.org", "@pat:example.org"]
        );
        assert_eq!(
            report
                .members
                .iter()
                .map(|member| (member.user_id.as_str(), member.kind, member.name.is_none()))
                .collect::<Vec<_>>(),
            [
                (
                    "@sam:example.org",
                    IdentityWarningKind::VerifiedChanged,
                    true
                ),
                ("@pat:example.org", IdentityWarningKind::Changed, true),
            ]
        );
        assert!(!report.at_least && !report.is_empty());
    }

    #[test]
    fn labels_are_bounded_and_cleaned() {
        assert_eq!(
            clean_label("  Pat\u{202e} Doe\u{200b} "),
            Some("Pat Doe".into())
        );
        assert_eq!(clean_label("\u{202e}\u{200b}"), None);
        let long = "a".repeat(MAX_IDENTITY_NAME_CHARS * 2);
        assert_eq!(
            clean_label(&long).unwrap().chars().count(),
            MAX_IDENTITY_NAME_CHARS
        );
    }

    /// IWCOMP01: ruma's backwards-compatible localpart check and the SDK's ID
    /// compatibility accept user IDs with line breaks, tabs, direction
    /// characters, any Unicode and more than 255 bytes (parsed by `change`
    /// exactly as the SDK parses them). They are followed and reported byte
    /// for byte; only what is shown is escaped and bounded.
    #[test]
    fn accepted_hostile_ids_are_kept_exact_and_only_shown_escaped() {
        let long = format!("@{}:example.org", "a".repeat(300));
        assert!(long.len() > 255);
        let hostile = [
            "@pat\nadmin:example.org",
            "@pat\tx:example.org",
            "@a\u{202e}gro.x:evil.org",
            "@\u{fc}\u{540d}:x.org",
            "@a\u{2028}b:x.org",
            long.as_str(),
        ];
        let mut set = WarningSet::default();
        set.apply(
            hostile
                .iter()
                .map(|user| change(user, IdentityState::PinViolation))
                .collect(),
        );
        let (_, report) = set.unnamed();
        let mut reported: Vec<&str> = report
            .members
            .iter()
            .map(|member| member.user_id.as_str())
            .collect();
        reported.sort_unstable();
        let mut exact = hostile.to_vec();
        exact.sort_unstable();
        assert_eq!(reported, exact, "byte for byte");
        let cases = [
            ("@pat\nadmin:example.org", r"@pat\nadmin:example.org"),
            ("@pat\tx:example.org", r"@pat\tx:example.org"),
            ("@a\u{202e}gro.x:evil.org", r"@a\u{202e}gro.x:evil.org"),
            ("@\u{fc}\u{540d}:x.org", r"@\u{fc}\u{540d}:x.org"),
            ("@a\u{2028}b:x.org", r"@a\u{2028}b:x.org"),
        ];
        for (user, shown) in cases {
            assert_eq!(display_user_id(user), shown, "{user:?}");
        }
        for member in &report.members {
            let shown = member.display_id();
            assert_eq!(shown, display_user_id(&member.user_id));
            let visible = shown.chars().all(|c| c == '…' || c.is_ascii_graphic());
            assert!(visible, "{shown:?}");
            assert!(shown.chars().count() <= MAX_IDENTITY_ID_DISPLAY_CHARS);
            assert!(shown.len() <= MAX_IDENTITY_ID_DISPLAY_CHARS + 2);
        }
    }

    /// The shown form is visible and never merges distinct IDs: nothing is
    /// removed, a backslash is doubled, `…` in an ID is escaped, and an ID
    /// in the spec's grammar shows unchanged.
    #[test]
    fn shown_ids_escape_visibly_and_stay_distinct() {
        assert_eq!(display_user_id("@pat:example.org"), "@pat:example.org");
        let cases = [
            ("@ab:x.org", "@ab:x.org"),
            ("@a\nb:x.org", r"@a\nb:x.org"),
            ("@a\\nb:x.org", r"@a\\nb:x.org"),
            ("@a\rb:x.org", r"@a\rb:x.org"),
            ("@a b:x.org", r"@a\u{20}b:x.org"),
            ("@a\u{0}b:x.org", r"@a\u{0}b:x.org"),
            ("@a\u{85}b:x.org", r"@a\u{85}b:x.org"),
            ("@a\u{200b}b:x.org", r"@a\u{200b}b:x.org"),
            ("@a\u{2066}b:x.org", r"@a\u{2066}b:x.org"),
            ("@a\u{feff}b:x.org", r"@a\u{feff}b:x.org"),
            ("@a\u{2026}:x.org", r"@a\u{2026}:x.org"),
        ];
        for (user, shown) in cases {
            assert_eq!(display_user_id(user), shown, "{user:?}");
        }
        let mut shown: Vec<String> = cases
            .iter()
            .map(|(user, _)| display_user_id(user))
            .collect();
        shown.sort_unstable();
        shown.dedup();
        assert_eq!(shown.len(), cases.len(), "distinct IDs show distinct text");
    }

    /// The shown form is bounded whatever the ID's length or escapes: exactly
    /// the limit is shown whole; longer keeps whole escapes from both ends,
    /// the server name's end included, around one visible `…`.
    #[test]
    fn shown_ids_are_bounded_with_one_visible_cut() {
        let fits = format!("@{}:x.org", "a".repeat(MAX_IDENTITY_ID_DISPLAY_CHARS - 7));
        assert_eq!(fits.len(), MAX_IDENTITY_ID_DISPLAY_CHARS);
        assert_eq!(display_user_id(&fits), fits);
        let over = format!("@{}:x.org", "a".repeat(MAX_IDENTITY_ID_DISPLAY_CHARS - 6));
        let shown = display_user_id(&over);
        assert_eq!(shown.chars().count(), MAX_IDENTITY_ID_DISPLAY_CHARS);
        assert!(shown.starts_with("@aaa") && shown.ends_with("aa:x.org"));
        assert_eq!(shown.matches('…').count(), 1);
        // Over 255 bytes, long multibyte, and long with escapes on both sides
        // of every cut.
        for user in [
            format!("@{}:example.org", "a".repeat(300)),
            format!("@{}:example.org", "\u{540d}".repeat(200)),
            format!("@{}:example.org", "\u{202e}\n".repeat(100_000)),
        ] {
            let shown = display_user_id(&user);
            assert!(shown.chars().count() <= MAX_IDENTITY_ID_DISPLAY_CHARS);
            assert!(shown.len() <= MAX_IDENTITY_ID_DISPLAY_CHARS + 2);
            assert!(shown.ends_with(":example.org"), "the server name's end");
            assert_eq!(shown.matches('…').count(), 1);
            let (head, tail) = shown.split_once('…').unwrap();
            for part in [head, tail] {
                // Whole escapes only: each `\u{` closes in the same part.
                assert_eq!(part.matches(r"\u{").count(), part.matches('}').count());
                assert!(part.chars().all(|c| c.is_ascii_graphic()), "{part:?}");
            }
        }
    }

    #[test]
    fn a_report_counts_shown_and_omitted_members() {
        let warnings = IdentityWarnings {
            members: vec![IdentityWarning {
                user_id: "@pat:example.org".into(),
                name: None,
                kind: IdentityWarningKind::Changed,
            }],
            omitted: 2,
            at_least: false,
        };
        assert_eq!(warnings.count(), 3);
    }

    #[tokio::test]
    async fn cancelling_any_clone_stops_every_wait_even_before_the_first_poll() {
        let cancellation = IdentityWarningCancellation::new();
        let watch_side = cancellation.clone();
        let waiting = tokio::spawn(async move { watch_side.cancelled().await });
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("a pending wait ends")
            .unwrap();
        // Cancelled before a wait starts: it resolves at once.
        tokio::time::timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .expect("never waits once cancelled");
        assert!(cancellation.clone().is_cancelled());
    }
}
