//! Which existing rooms one joined Space lists: add a room the account has
//! joined to THIS Space, or remove one from it.
//!
//! A Space lists a room through its own `m.space.child` state event, keyed
//! by the room's ID, and nothing else is touched:
//! - the room keeps its members, the account stays in it, nothing is
//!   deleted;
//! - other Spaces' links and the room's own `m.space.parent` events are
//!   left as they are, so a room may stay in several Spaces.
//!
//! Every change is reviewed first (`review_space_link_for`), against fresh,
//! bounded reads:
//! - the account's right to change the Space's children, from its create
//!   and power-level state;
//! - the current `m.space.child` content for that room, kept as the
//!   baseline.
//!
//! The write (`apply_space_link_for`):
//! - keeps every field of that content it does not change, as raw JSON
//!   byte for byte (numbers beyond u64 and decimals included): Add sets
//!   `via`; Remove drops only `via`, which is how Matrix ends a link;
//! - is refused when the baseline changed meanwhile;
//! - is sent as one attempt on the wire (no HTTP retries and no automatic
//!   token refresh and replay), raced against the exact selection's
//!   revocation;
//! - is read back.
//! A dispatched write without a confirmed reply is an unknown outcome,
//! never "unchanged". A revocation that wins before the write's first step
//! sent nothing, and says so.
//!
//! The no-replay opt-in, `RequestConfig::disable_auth_refresh()`, is root's
//! SDK change d4c5d08. It is not in this branch's base (c7d0e12), so this
//! module compiles only once combined with it.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    config::RequestConfig,
    deserialized_responses::RawAnySyncOrStrippedState,
    ruma::{
        OwnedServerName, OwnedUserId, RoomId, ServerName,
        api::{client::state::send_state_event, error::ErrorKind},
        events::{
            AnyStateEventContent, StateEventType,
            room::join_rules::{JoinRule, RoomJoinRulesEventContent},
        },
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

use crate::{
    AccountManager, AccountSelection, RoomSettingsCancellation, SpaceLevelPage,
    pinned_messages::plain_text,
    room_settings::{self, RoomSettingsError},
};

/// Joined rooms one view offers at most.
pub const MAX_SPACE_CANDIDATES: usize = 500;
/// Links of the Space read from the account's state at most.
pub const MAX_SPACE_LINKS: usize = 500;
/// Route servers one new link names at most.
pub const MAX_LINK_VIA: usize = 3;
const MAX_NAME_CHARS: usize = 120;
/// Hierarchy pages a cycle check reads below a Space being added.
const MAX_CYCLE_PAGES: usize = 3;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const READ_BOUND: Duration = Duration::from_secs(30);
const MAX_REPLY_BYTES: u64 = 64 * 1024;
const MAX_LINK_CONTENT_BYTES: usize = 64 * 1024;
const MAX_CACHED_EVENT_BYTES: usize = 64 * 1024;
/// Nesting compared at most when a link is read back.
const MAX_JSON_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceManageError {
    AccountChanged,
    Cancelled,
    SecurityBusy,
    NotJoined,
    NotSpace,
    InvalidRoom,
    NotAllowed,
    AlreadyLinked,
    NotLinked,
    WouldCycle,
    BaselineChanged,
    /// The Space's or the room's access changed since the review.
    ConditionsChanged,
    /// Who may join the Space or the room could not be read.
    AccessUnreadable,
    InvalidState,
    RateLimited,
    Rejected,
    /// The write was dispatched and its effect is not known.
    OutcomeUnknown,
    Unavailable,
}

impl SpaceManageError {
    /// A fixed sentence; the homeserver's reason is never shown.
    pub fn text(self) -> &'static str {
        match self {
            Self::AccountChanged => "The account changed. Nothing was changed.",
            Self::Cancelled => "This Space change was cancelled before anything was sent.",
            Self::SecurityBusy => {
                "The view or security context changed. Nothing was sent; review the Space change again."
            }
            Self::NotJoined => "You are not in that room or Space.",
            Self::NotSpace => "This is not a Space.",
            Self::InvalidRoom => "That room cannot be listed in this Space.",
            Self::NotAllowed => "You are not allowed to change which rooms this Space lists.",
            Self::AlreadyLinked => "This Space already lists that room.",
            Self::NotLinked => "This Space does not list that room.",
            Self::WouldCycle => {
                "That Space already contains this Space, so it cannot be listed inside it."
            }
            Self::BaselineChanged => {
                "This Space's link to the room changed since you reviewed it. Nothing was changed; review it again."
            }
            Self::ConditionsChanged => {
                "Who can join the Space or the room changed since you reviewed it. Nothing was changed; review it again."
            }
            Self::AccessUnreadable => {
                "Morpheus could not read who can join the Space or the room, so it cannot say what listing it would show. Nothing was changed."
            }
            Self::InvalidState => "This Space's link uses data Morpheus cannot change safely.",
            Self::RateLimited => {
                "Your homeserver is limiting requests. Nothing was changed; try again shortly."
            }
            Self::Rejected => "Your homeserver refused the change. Nothing was changed.",
            Self::OutcomeUnknown => {
                "No reply arrived, so the change may or may not have been made. Morpheus is reading this link again."
            }
            Self::Unavailable => "The Space could not be read right now.",
        }
    }
    /// Whether the Space may have changed.
    pub fn may_have_changed(self) -> bool {
        self == Self::OutcomeUnknown
    }
}

type Result<T> = std::result::Result<T, SpaceManageError>;

/// One joined room the account could add or remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceRoomCandidate {
    pub room_id: String,
    /// Bounded plain text.
    pub name: String,
    pub is_space: bool,
    /// THIS Space lists it, as the account's synced state shows; `None` when
    /// that state could not be read in full (see `links_complete`).
    pub linked: Option<bool>,
    /// Anyone may join it.
    pub public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceRoomsView {
    pub space_id: String,
    pub space_name: String,
    pub space_public: bool,
    /// The account may change the Space's children now (fresh power levels).
    pub can_manage: bool,
    /// Joined rooms by name, without the Space itself.
    pub candidates: Vec<SpaceRoomCandidate>,
    /// More joined rooms exist than `candidates` holds.
    pub truncated: bool,
    /// Every link of the Space was read from synced state. Otherwise a room
    /// not seen as linked may still be listed, and its row says so.
    pub links_complete: bool,
    /// Rooms the Space lists that this account has not joined (counted
    /// against every joined room, not only the shown ones).
    pub other_children: usize,
}

/// What a review is asked to prepare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkIntent {
    Add,
    Remove,
    /// Decide from the link's fresh state: remove it when it is listed, add
    /// it otherwise. For a room whose synced link state is unknown.
    Check,
}

/// Who may join the Space and the room, read fresh when a change is
/// reviewed, and again before it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Conditions {
    space_public: bool,
    room_public: bool,
}

/// Something the person is told before confirming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkWarning {
    /// A public Space will list a room that is not public.
    PublicSpaceListsPrivateRoom,
    /// Morpheus could not read far enough to rule out a loop.
    MayContainThisSpace,
}

/// A reviewed Add or Remove: what is written, and what it was checked
/// against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceLinkChange {
    pub space_id: String,
    pub room_id: String,
    pub add: bool,
    pub warnings: Vec<LinkWarning>,
    /// The link's content when reviewed, as the server sent it (`None`: no
    /// event).
    baseline: Option<String>,
    /// The raw JSON to write.
    content: String,
    /// The access the review explained; an Add is refused if it changed.
    conditions: Option<Conditions>,
}

/// A write that was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceLinkResult {
    /// A fresh read shows exactly what was written.
    pub confirmed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Yes,
    No,
    Unknown,
}

impl AccountManager {
    /// The joined rooms the account could add to or remove from the joined
    /// Space `space_id`, with what the Space lists now. Reads only.
    pub async fn space_rooms_for(
        &self,
        selection: &AccountSelection,
        space_id: &str,
    ) -> Result<SpaceRoomsView> {
        let (client, own, space) = self.managed_space(selection, space_id).await?;
        let can_manage = self.may_manage(selection, &space, &own).await?;
        let links = linked_children(&space).await;
        let joined_rooms = client
            .joined_rooms()
            .into_iter()
            .filter(|room| room.room_id() != space.room_id())
            .collect::<Vec<_>>();
        // Counted against every joined room, before the shown list is cut.
        let joined = joined_rooms
            .iter()
            .map(|room| room.room_id().to_string())
            .collect::<HashSet<_>>();
        let other_children = links
            .linked
            .iter()
            .filter(|id| !joined.contains(*id))
            .count();
        let mut rooms = joined_rooms
            .iter()
            .map(|room| SpaceRoomCandidate {
                linked: links.state(room.room_id().as_str()),
                public: matches!(room.join_rule(), Some(JoinRule::Public)),
                is_space: room.is_space(),
                name: room_name(room),
                room_id: room.room_id().to_string(),
            })
            .collect::<Vec<_>>();
        rooms.sort_by_cached_key(|room| (room.name.to_lowercase(), room.room_id.clone()));
        let truncated = rooms.len() > MAX_SPACE_CANDIDATES;
        rooms.truncate(MAX_SPACE_CANDIDATES);
        if !self.owns_selection(selection) {
            return Err(SpaceManageError::AccountChanged);
        }
        Ok(SpaceRoomsView {
            space_id: space.room_id().to_string(),
            space_name: room_name(&space),
            space_public: matches!(space.join_rule(), Some(JoinRule::Public)),
            can_manage,
            candidates: rooms,
            truncated,
            links_complete: links.complete && links.unreadable.is_empty(),
            other_children,
        })
    }

    /// Review adding or removing `room_id` in the joined Space `space_id`
    /// (`intent`). Reads only, all fresh:
    /// - the account's right and the link's current content (the baseline);
    /// - for an Add, who may join the Space and the room;
    /// - for a Space being added, whether it already contains this one.
    pub async fn review_space_link_for(
        &self,
        selection: &AccountSelection,
        space_id: &str,
        room_id: &str,
        intent: LinkIntent,
    ) -> Result<SpaceLinkChange> {
        let child = RoomId::parse(room_id).map_err(|_| SpaceManageError::InvalidRoom)?;
        let (client, own, space) = self.managed_space(selection, space_id).await?;
        if child.as_str() == space.room_id().as_str() {
            return Err(SpaceManageError::InvalidRoom);
        }
        if !self.may_manage(selection, &space, &own).await? {
            return Err(SpaceManageError::NotAllowed);
        }
        let baseline = self.child_content(selection, &space, &child).await?;
        let linked = match baseline.as_deref() {
            Some(content) => link_state(content)?,
            None => false,
        };
        let add = match intent {
            LinkIntent::Add => true,
            LinkIntent::Remove => false,
            LinkIntent::Check => !linked,
        };
        let mut warnings = Vec::new();
        let mut conditions = None;
        let content = if add {
            if linked {
                return Err(SpaceManageError::AlreadyLinked);
            }
            let room = client
                .get_room(&child)
                .filter(|room| room.state() == RoomState::Joined)
                .ok_or(SpaceManageError::NotJoined)?;
            if room.is_space() {
                match self
                    .reaches(selection, room_id, space.room_id().as_str())
                    .await
                {
                    Reach::Yes => return Err(SpaceManageError::WouldCycle),
                    Reach::Unknown => warnings.push(LinkWarning::MayContainThisSpace),
                    Reach::No => {}
                }
            }
            // Fresh, not the synced copies: the review explains these.
            let fresh = self.conditions(selection, &space, &room).await?;
            if fresh.space_public && !fresh.room_public {
                warnings.push(LinkWarning::PublicSpaceListsPrivateRoom);
            }
            conditions = Some(fresh);
            added_content(baseline.as_deref(), &via(&own, &child))?
        } else {
            if !linked {
                return Err(SpaceManageError::NotLinked);
            }
            removed_content(baseline.as_deref())?
        };
        if !self.owns_selection(selection) {
            return Err(SpaceManageError::AccountChanged);
        }
        Ok(SpaceLinkChange {
            space_id: space.room_id().to_string(),
            room_id: child.to_string(),
            add,
            warnings,
            baseline,
            content,
            conditions,
        })
    }

    /// Whether the Space lists `room_id` now, from a fresh read of that one
    /// link. For reconciling after a write whose reply never came.
    pub async fn space_link_state_for(
        &self,
        selection: &AccountSelection,
        space_id: &str,
        room_id: &str,
    ) -> Result<bool> {
        let child = RoomId::parse(room_id).map_err(|_| SpaceManageError::InvalidRoom)?;
        let (_, _, space) = self.managed_space(selection, space_id).await?;
        let content = self.child_content(selection, &space, &child).await?;
        if !self.owns_selection(selection) {
            return Err(SpaceManageError::AccountChanged);
        }
        match content.as_deref() {
            Some(content) => link_state(content),
            None => Ok(false),
        }
    }

    /// Apply a reviewed change: one write of the Space's `m.space.child`
    /// for that room only, after the right and the baseline are read again.
    pub async fn apply_space_link_for(
        &self,
        selection: &AccountSelection,
        change: &SpaceLinkChange,
    ) -> Result<SpaceLinkResult> {
        // Compatibility API for non-UI callers: it has account ownership,
        // but no application's view/security authority. The actual panel
        // must use apply_space_link_guarded with its captured admission.
        self.apply_space_link_guarded(selection, change, &RoomSettingsCancellation::new(), &|| {
            true
        })
        .await
    }

    /// Apply only while the caller's captured view/security authority and
    /// cancellation still admit this exact selection. Every preflight wait,
    /// including the manager lock, is finite and cancellation-first.
    pub async fn apply_space_link_guarded(
        &self,
        selection: &AccountSelection,
        change: &SpaceLinkChange,
        cancellation: &RoomSettingsCancellation,
        admission: &(dyn Fn() -> bool + Sync),
    ) -> Result<SpaceLinkResult> {
        let child = RoomId::parse(&change.room_id).map_err(|_| SpaceManageError::InvalidRoom)?;
        let (client, own, space) = guarded(
            self,
            selection,
            cancellation,
            admission,
            READ_BOUND,
            self.managed_space(selection, &change.space_id),
        )
        .await?;
        if !guarded(
            self,
            selection,
            cancellation,
            admission,
            READ_BOUND,
            self.may_manage(selection, &space, &own),
        )
        .await?
        {
            return Err(SpaceManageError::NotAllowed);
        }
        // The exact bytes the review read: any change, even one a lossy
        // comparison would miss, means review it again.
        if guarded(
            self,
            selection,
            cancellation,
            admission,
            READ_BOUND,
            self.child_content(selection, &space, &child),
        )
        .await?
            != change.baseline
        {
            return Err(SpaceManageError::BaselineChanged);
        }
        if change.add {
            // With the access the review explained.
            let room = client
                .get_room(&child)
                .filter(|room| room.state() == RoomState::Joined)
                .ok_or(SpaceManageError::NotJoined)?;
            if Some(
                guarded(
                    self,
                    selection,
                    cancellation,
                    admission,
                    READ_BOUND,
                    self.conditions(selection, &space, &room),
                )
                .await?,
            ) != change.conditions
            {
                return Err(SpaceManageError::ConditionsChanged);
            }
        }
        if change.content.len() > MAX_LINK_CONTENT_BYTES {
            return Err(SpaceManageError::InvalidState);
        }
        let body = RawValue::from_string(change.content.clone())
            .map_err(|_| SpaceManageError::InvalidState)?;
        let request = send_state_event::v3::Request::new_raw(
            space.room_id().to_owned(),
            StateEventType::SpaceChild,
            child.to_string(),
            Raw::<AnyStateEventContent>::from_json(body),
        );
        // Write admission, after every read and with no await before the
        // send: the exact selection, the Space joined and, for an Add, the
        // room still joined.
        if !self.owns_selection(selection) {
            return Err(SpaceManageError::AccountChanged);
        }
        if !admits_write(space.state(), change.add, joined_state(&client, &child)) {
            return Err(SpaceManageError::NotJoined);
        }
        // The dispatch helper checks the captured authority once more on
        // the request's first poll. No manager mutex crosses this await.
        dispatch(self, selection, cancellation, admission, async {
            client
                .send(request)
                .with_request_config(write_config())
                .await
                .map_err(|error| write_failure(&error))
        })
        .await?;
        // A known success stays known even if its presentation has closed.
        let after = guarded(
            self,
            selection,
            cancellation,
            admission,
            READ_BOUND,
            self.child_content(selection, &space, &child),
        )
        .await
        .ok();
        Ok(SpaceLinkResult {
            confirmed: after
                .flatten()
                .is_some_and(|after| same_content(&after, &change.content)),
        })
    }

    async fn managed_space(
        &self,
        selection: &AccountSelection,
        space_id: &str,
    ) -> Result<(Client, OwnedUserId, Room)> {
        let space_id = RoomId::parse(space_id).map_err(|_| SpaceManageError::NotSpace)?;
        let client = tokio::time::timeout(READ_BOUND, self.hierarchy_client_for(selection))
            .await
            .map_err(|_| SpaceManageError::Unavailable)?
            .map_err(|error| match error {
                crate::MatrixCoreError::SelectedAccountChanged => SpaceManageError::AccountChanged,
                _ => SpaceManageError::Unavailable,
            })?;
        let own = client
            .user_id()
            .ok_or(SpaceManageError::Unavailable)?
            .to_owned();
        let space = client
            .get_room(&space_id)
            .filter(|room| room.state() == RoomState::Joined)
            .ok_or(SpaceManageError::NotJoined)?;
        if !space.is_space() {
            return Err(SpaceManageError::NotSpace);
        }
        Ok((client, own, space))
    }

    async fn may_manage(
        &self,
        selection: &AccountSelection,
        space: &Room,
        own: &OwnedUserId,
    ) -> Result<bool> {
        let check = room_settings::can_send_state(space, own, StateEventType::SpaceChild);
        selection
            .guard(check)
            .await
            .ok_or(SpaceManageError::AccountChanged)?
            .map_err(read_failure)
    }

    /// Who may join the Space and the room, from fresh join-rule state.
    async fn conditions(
        &self,
        selection: &AccountSelection,
        space: &Room,
        room: &Room,
    ) -> Result<Conditions> {
        Ok(Conditions {
            space_public: self.public_now(selection, space).await?,
            room_public: self.public_now(selection, room).await?,
        })
    }

    async fn public_now(&self, selection: &AccountSelection, room: &Room) -> Result<bool> {
        let read = room_settings::state_content(room, StateEventType::RoomJoinRules, "");
        let content = selection
            .guard(read)
            .await
            .ok_or(SpaceManageError::AccountChanged)?
            .map_err(read_failure)?;
        public_rule(content.as_ref())
    }

    /// The link's content as the server sent it, byte for byte.
    async fn child_content(
        &self,
        selection: &AccountSelection,
        space: &Room,
        child: &RoomId,
    ) -> Result<Option<String>> {
        let kind = StateEventType::SpaceChild;
        let read = room_settings::state_content_raw(space, kind, child.as_str());
        let content = selection
            .guard(read)
            .await
            .ok_or(SpaceManageError::AccountChanged)?
            .map_err(read_failure)?;
        Ok(content.map(|content| content.get().to_owned()))
    }

    /// Whether `target` is listed in the hierarchy below `from`, read one
    /// bounded level at a time (`space_level_for`), at most
    /// `MAX_CYCLE_PAGES` pages and two levels deep.
    async fn reaches(&self, selection: &AccountSelection, from: &str, target: &str) -> Reach {
        let mut token: Option<String> = None;
        let mut scan = ReachScan::default();
        for page_index in 0..MAX_CYCLE_PAGES {
            let Ok(page) = self
                .space_level_for(selection, from, token.as_deref())
                .await
            else {
                return Reach::Unknown;
            };
            if let Some(reach) = scan.page(page_index == 0, &page, from, target) {
                return reach;
            }
            token = page.next_batch;
        }
        Reach::Unknown
    }
}

/// The cycle check's pages so far. A page that lists the target decides
/// Yes whatever else it lacks; anything a page leaves unread (no entry for
/// the Space itself on the first page, a truncated page, links past an
/// entry's cut, a nested Space a level deeper) is kept, so that a clean
/// last page still ends Unknown, never No.
#[derive(Default)]
pub(crate) struct ReachScan {
    unread: bool,
}

impl ReachScan {
    /// Take one page; `Some` once the pages decide.
    pub(crate) fn page(
        &mut self,
        first: bool,
        page: &SpaceLevelPage,
        from: &str,
        target: &str,
    ) -> Option<Reach> {
        let (listed, unseen) = scan_page(page, from, target);
        if listed {
            return Some(Reach::Yes);
        }
        self.unread |= unseen || page.truncated || (first && page.space.is_none());
        if page.next_batch.is_some() {
            return None;
        }
        Some(if self.unread {
            Reach::Unknown
        } else {
            Reach::No
        })
    }
}

/// Whether `target` is listed on one level page below `from`, and whether
/// the page leaves links unread: a truncated link list (the 501st link may
/// be the target), or a nested Space whose own children are a level deeper.
pub(crate) fn scan_page(page: &SpaceLevelPage, from: &str, target: &str) -> (bool, bool) {
    let mut listed = false;
    let mut unseen = false;
    for entry in page.space.iter().chain(page.entries.iter()) {
        listed |= entry.node.room_id == target
            || entry
                .node
                .children
                .iter()
                .any(|link| link.room_id == target);
        unseen |= entry.links_truncated
            || (entry.node.room_id != from
                && entry.node.is_space
                && !entry.node.children.is_empty());
    }
    (listed, unseen)
}

/// Check the manager's exact token and the UI's captured authority. This
/// is an SDK handoff check, not a distributed transaction or wire lock.
fn scope_check(
    manager: &AccountManager,
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    admission: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(SpaceManageError::Cancelled);
    }
    if !manager.owns_selection(selection) {
        return Err(SpaceManageError::AccountChanged);
    }
    if !admission() {
        return Err(SpaceManageError::SecurityBusy);
    }
    Ok(())
}

/// A cancellable finite wait. Cancellation wins even when work is ready.
async fn guarded<T>(
    manager: &AccountManager,
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    admission: &(dyn Fn() -> bool + Sync),
    bound: Duration,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    scope_check(manager, selection, cancellation, admission)?;
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(SpaceManageError::Cancelled),
        result = selection.guard(tokio::time::timeout(bound, work)) => match result {
            None => Err(SpaceManageError::AccountChanged),
            Some(Err(_)) => Err(SpaceManageError::Unavailable),
            Some(Ok(result)) => result,
        },
    }
}

/// All preflights are done. The first poll asks the captured authority and
/// marks handoff before polling the SDK request. Once started, dropping a
/// request can only leave Unknown; no automatic retry follows.
async fn dispatch<T>(
    manager: &AccountManager,
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    admission: &(dyn Fn() -> bool + Sync),
    send: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let started = AtomicBool::new(false);
    let sending = async {
        scope_check(manager, selection, cancellation, admission)?;
        started.store(true, Ordering::Release);
        send.await
    };
    match guarded(
        manager,
        selection,
        cancellation,
        admission,
        WRITE_TIMEOUT,
        sending,
    )
    .await
    {
        Err(error)
            if started.load(Ordering::Acquire)
                && matches!(
                    error,
                    SpaceManageError::Cancelled
                        | SpaceManageError::AccountChanged
                        | SpaceManageError::SecurityBusy
                        | SpaceManageError::Unavailable
                ) =>
        {
            Err(SpaceManageError::OutcomeUnknown)
        }
        result => result,
    }
}

/// Await `send` unless `selection` is revoked; `None` then, and the send is
/// dropped wherever it was. Revocation is checked FIRST on every poll: when
/// it and the send's next step are both ready, the send is not resumed, so
/// no request starts after the account was left. A send that returned before
/// the revocation keeps its result.
#[cfg(test)]
async fn unless_revoked<F: std::future::Future>(
    selection: &AccountSelection,
    send: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = selection.revoked() => None,
        output = send => Some(output),
    }
}

/// A write dropped by revocation: before its first step nothing was sent;
/// after it, the request may have left.
#[cfg(test)]
pub(crate) fn dropped(started: bool) -> SpaceManageError {
    if started {
        SpaceManageError::OutcomeUnknown
    } else {
        SpaceManageError::AccountChanged
    }
}

/// One attempt on the wire: no HTTP retries, no automatic token refresh and
/// replay (root's d4c5d08), a timeout and a small reply.
fn write_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(WRITE_TIMEOUT)
        .max_response_bytes(MAX_REPLY_BYTES)
        .disable_auth_refresh()
}

fn read_failure(error: RoomSettingsError) -> SpaceManageError {
    match error {
        RoomSettingsError::AccountChanged | RoomSettingsError::Cancelled => {
            SpaceManageError::AccountChanged
        }
        RoomSettingsError::NotJoined => SpaceManageError::NotJoined,
        RoomSettingsError::RateLimited => SpaceManageError::RateLimited,
        RoomSettingsError::InvalidState => SpaceManageError::InvalidState,
        _ => SpaceManageError::Unavailable,
    }
}

/// A definite 4xx refusal changed nothing; anything else may have.
pub(crate) fn write_failure(error: &HttpError) -> SpaceManageError {
    let Some(api) = error.as_client_api_error() else {
        return SpaceManageError::OutcomeUnknown;
    };
    if !api.status_code.is_client_error() {
        return SpaceManageError::OutcomeUnknown;
    }
    match api.error_kind() {
        // Without the replay, an expired token returns its 401 as is.
        Some(ErrorKind::UnknownToken { .. }) => SpaceManageError::OutcomeUnknown,
        Some(ErrorKind::Forbidden) => SpaceManageError::NotAllowed,
        Some(ErrorKind::LimitExceeded(_)) => SpaceManageError::RateLimited,
        _ => SpaceManageError::Rejected,
    }
}

/// Whether a join-rules content says anyone may join. No event is not
/// public (the default join rule is invite); an event whose `join_rule` is
/// missing or not a string is unreadable, never read as not public, which
/// would leave out the public-Space disclosure.
pub(crate) fn public_rule(content: Option<&Value>) -> Result<bool> {
    let Some(content) = content else {
        return Ok(false);
    };
    let content: RoomJoinRulesEventContent =
        serde_json::from_value(content.clone()).map_err(|_| SpaceManageError::AccessUnreadable)?;
    match content.join_rule {
        JoinRule::Public => Ok(true),
        JoinRule::Invite
        | JoinRule::Knock
        | JoinRule::Private
        | JoinRule::Restricted(_)
        | JoinRule::KnockRestricted(_) => Ok(false),
        _ => Err(SpaceManageError::AccessUnreadable),
    }
}

/// Whether the write may start: the Space joined and, for an Add, the room
/// joined (`None`: not known to the client).
pub(crate) fn admits_write(space: RoomState, add: bool, room: Option<RoomState>) -> bool {
    space == RoomState::Joined && (!add || room == Some(RoomState::Joined))
}

fn joined_state(client: &Client, room: &RoomId) -> Option<RoomState> {
    client.get_room(room).map(|room| room.state())
}

/// Top-level fields of a JSON object, each kept as raw JSON. A normal map
/// keeps only the last of duplicate names; this refuses them instead, so
/// no field is dropped on write (as pinned_messages does).
struct UniqueFields(BTreeMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = UniqueFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate link content field"));
                    }
                }
                Ok(UniqueFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

fn object_fields(content: &str) -> Result<BTreeMap<String, Box<RawValue>>> {
    if content.len() > MAX_LINK_CONTENT_BYTES {
        return Err(SpaceManageError::InvalidState);
    }
    serde_json::from_str::<UniqueFields>(content)
        .map(|fields| fields.0)
        .map_err(|_| SpaceManageError::InvalidState)
}

/// Whether link content lists the room: a non-empty `via` list. Content
/// that is not an object with unique fields is unreadable, not "absent".
pub(crate) fn link_state(content: &str) -> Result<bool> {
    let fields = object_fields(content)?;
    let Some(via) = fields.get("via") else {
        return Ok(false);
    };
    let via: Vec<OwnedServerName> =
        serde_json::from_str(via.get()).map_err(|_| SpaceManageError::InvalidState)?;
    Ok(!via.is_empty())
}

/// The content that lists the room: the baseline's fields, each byte for
/// byte, with `via` set.
pub(crate) fn added_content(baseline: Option<&str>, via: &[OwnedServerName]) -> Result<String> {
    let mut fields = match baseline {
        None => BTreeMap::new(),
        Some(content) => object_fields(content)?,
    };
    if via.is_empty() {
        return Err(SpaceManageError::InvalidRoom);
    }
    let via = via.iter().map(|server| server.as_str()).collect::<Vec<_>>();
    let via = serde_json::value::to_raw_value(&via).map_err(|_| SpaceManageError::InvalidState)?;
    fields.insert("via".to_owned(), via);
    encode(&fields)
}

/// The content that ends the link: the baseline's fields without `via`,
/// every other one byte for byte.
pub(crate) fn removed_content(baseline: Option<&str>) -> Result<String> {
    let Some(content) = baseline else {
        return Err(SpaceManageError::InvalidState);
    };
    let mut fields = object_fields(content)?;
    fields.remove("via");
    encode(&fields)
}

fn encode(fields: &BTreeMap<String, Box<RawValue>>) -> Result<String> {
    let encoded = serde_json::value::to_raw_value(fields)
        .map(|raw| raw.get().to_owned())
        .map_err(|_| SpaceManageError::InvalidState)?;
    if encoded.len() > MAX_LINK_CONTENT_BYTES {
        return Err(SpaceManageError::InvalidState);
    }
    Ok(encoded)
}

/// Whether a read-back shows exactly what was written: the same JSON, with
/// object fields in any order and any whitespace, and every number compared
/// as written (no rounding, so `1.10` is not `1.1`).
pub(crate) fn same_content(read: &str, written: &str) -> bool {
    if read.len() > MAX_LINK_CONTENT_BYTES || written.len() > MAX_LINK_CONTENT_BYTES {
        return false;
    }
    let (Ok(read), Ok(written)) = (
        serde_json::from_str::<Box<RawValue>>(read),
        serde_json::from_str::<Box<RawValue>>(written),
    ) else {
        return false;
    };
    match (canonical(&read, 0), canonical(&written, 0)) {
        (Some(read), Some(written)) => read == written,
        _ => false,
    }
}

/// JSON in a form that compares without losing any digit.
#[derive(Debug, PartialEq, Eq)]
enum Canonical {
    Null,
    Bool(bool),
    Number(String),
    Text(String),
    List(Vec<Canonical>),
    Object(BTreeMap<String, Canonical>),
}

fn canonical(raw: &RawValue, depth: usize) -> Option<Canonical> {
    if depth > MAX_JSON_DEPTH {
        return None;
    }
    let text = raw.get().trim();
    match text.as_bytes().first().copied()? {
        b'{' => {
            let fields = serde_json::from_str::<UniqueFields>(text).ok()?.0;
            let fields = fields
                .iter()
                .map(|(key, value)| Some((key.clone(), canonical(value, depth + 1)?)))
                .collect::<Option<BTreeMap<_, _>>>()?;
            Some(Canonical::Object(fields))
        }
        b'[' => {
            let items = serde_json::from_str::<Vec<Box<RawValue>>>(text).ok()?;
            let items = items
                .iter()
                .map(|item| canonical(item, depth + 1))
                .collect::<Option<Vec<_>>>()?;
            Some(Canonical::List(items))
        }
        b'"' => serde_json::from_str::<String>(text)
            .ok()
            .map(Canonical::Text),
        b't' | b'f' => serde_json::from_str::<bool>(text).ok().map(Canonical::Bool),
        b'n' => (text == "null").then_some(Canonical::Null),
        _ => Some(Canonical::Number(text.to_owned())),
    }
}

/// The route servers of a new link: the account's homeserver, which is in
/// the room, and the server of the room's ID when it names another one.
pub(crate) fn via(own: &OwnedUserId, room: &RoomId) -> Vec<OwnedServerName> {
    let mut via = vec![own.server_name().to_owned()];
    let named = room
        .as_str()
        .split_once(':')
        .and_then(|(_, server)| ServerName::parse(server).ok());
    if let Some(server) = named
        && !via.contains(&server)
    {
        via.push(server);
    }
    via.truncate(MAX_LINK_VIA);
    via
}

/// What the account's synced state says THIS Space lists.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Links {
    /// Rooms it lists.
    pub(crate) linked: HashSet<String>,
    /// Rooms whose link event could not be read (malformed content).
    pub(crate) unreadable: HashSet<String>,
    /// Every link event was read: the store answered, none was past
    /// MAX_SPACE_LINKS, and every one named a room.
    pub(crate) complete: bool,
}

impl Links {
    /// A room's link state: listed, absent, or `None` when not known.
    pub(crate) fn state(&self, room_id: &str) -> Option<bool> {
        if self.linked.contains(room_id) {
            Some(true)
        } else if self.unreadable.contains(room_id) || !self.complete {
            None
        } else {
            Some(false)
        }
    }

    /// Take one link event: its state key and content, each `None` when it
    /// could not be read. Nothing unread is taken as absent.
    pub(crate) fn take(&mut self, key: Option<String>, content: Option<Box<RawValue>>) {
        let Some(room) = key.as_deref().and_then(|key| RoomId::parse(key).ok()) else {
            // It may name any room: no row is known to be absent.
            self.complete = false;
            return;
        };
        match content.map(|content| link_state(content.get())) {
            Some(Ok(true)) => {
                self.linked.insert(room.to_string());
            }
            Some(Ok(false)) => {}
            Some(Err(_)) | None => {
                self.unreadable.insert(room.to_string());
            }
        }
    }
}

/// The rooms THIS Space lists, from the account's synced state (no
/// request). Incomplete when the store could not be read or holds more
/// links than are read; a link event that could not be read is unknown,
/// never absent.
async fn linked_children(space: &Room) -> Links {
    let Ok(events) = space.get_state_events(StateEventType::SpaceChild).await else {
        return Links::default();
    };
    let mut links = Links {
        complete: events.len() <= MAX_SPACE_LINKS,
        ..Links::default()
    };
    for event in events.iter().take(MAX_SPACE_LINKS) {
        take_cached_event(&mut links, event);
    }
    links
}

fn take_cached_event(links: &mut Links, event: &RawAnySyncOrStrippedState) {
    let bytes = match event {
        RawAnySyncOrStrippedState::Sync(raw) => raw.json().get().len(),
        RawAnySyncOrStrippedState::Stripped(raw) => raw.json().get().len(),
    };
    if bytes > MAX_CACHED_EVENT_BYTES {
        // No key or content parsing/allocation before this bound. Its
        // unknown key could name any otherwise absent candidate.
        links.complete = false;
        return;
    }
    let (key, content) = match event {
        RawAnySyncOrStrippedState::Sync(raw) => (
            raw.get_field::<String>("state_key").ok().flatten(),
            raw.get_field::<Box<RawValue>>("content").ok().flatten(),
        ),
        RawAnySyncOrStrippedState::Stripped(raw) => (
            raw.get_field::<String>("state_key").ok().flatten(),
            raw.get_field::<Box<RawValue>>("content").ok().flatten(),
        ),
    };
    links.take(key, content);
}

fn room_name(room: &Room) -> String {
    let name = room
        .cached_display_name()
        .map(|name| name.to_string())
        .or_else(|| room.name())
        .unwrap_or_default();
    let name = plain_text(&name, MAX_NAME_CHARS);
    if name.is_empty() {
        room.room_id().to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests;
