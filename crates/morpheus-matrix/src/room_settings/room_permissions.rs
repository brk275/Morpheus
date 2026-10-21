//! Room permissions (`m.room.power_levels`) for the native editor: each
//! member's level and what each action needs, read fresh from the homeserver
//! and changed by one bounded write.
//!
//! A child of `room_settings`: it works on a settings session (its exact
//! selection, its cancellation, and the app's admission asked right before
//! the hand-off) and uses that module's bounded reads (`state`,
//! `state_and_raw` with `read_config`), its single bounded write
//! (`send_state` with `write_config`: no retry, no authentication replay)
//! and its lossless field parser (`content_fields`). Nothing is forked.
//!
//! Reading: the room's create event, this account's membership and the
//! power levels as their exact content text, one bounded GET each. Levels
//! follow the pinned ruma rules for the room version, privileged creators
//! included. An unknown room version, a room without a power-levels event,
//! or content Morpheus cannot edit without loss is shown read-only. At most
//! [`MAX_PERMISSION_USERS`] users and [`MAX_PERMISSION_EVENTS`] event
//! entries are listed; the rest are counted.
//!
//! Saving: one edit of at most [`MAX_PERMISSION_CHANGES`] changes is checked
//! against a fresh read immediately before the write: the stored text must
//! still be byte for byte the reviewed one, and every change must be one
//! Matrix lets this account make now. A level above your own, a member at or
//! above your level, a permission that needs more than your level and a
//! privileged creator are refused; lowering your own level needs an explicit
//! confirmation. Only the changed entries are rewritten: every other field,
//! entry, order and number keeps its exact text. The write is sent once and
//! never retried; after the hand-off the outcome is only what is known.
//! Matrix has no conditional write, so a change made elsewhere between the
//! fresh read and the write can still be overwritten. The levels are then
//! read back and each change is reported as shown or not.

use std::collections::{BTreeMap, HashMap, HashSet};

use matrix_sdk::ruma::{Int, UserId, events::room::power_levels::UserPowerLevel};

use super::*;

/// Users with their own level listed, at most; the rest are counted.
pub const MAX_PERMISSION_USERS: usize = 500;
/// Event entries listed besides the common ones, at most.
pub const MAX_PERMISSION_EVENTS: usize = 100;
/// Changes one save carries, at most.
pub const MAX_PERMISSION_CHANGES: usize = 50;
const MAX_EVENT_TYPE_BYTES: usize = 255;
const MAX_USER_ID_BYTES: usize = 255;
const MAX_NAME_CHARS: usize = 80;
// Local display names only; a slow store leaves user IDs.
const NAMES_TIMEOUT: Duration = Duration::from_secs(3);

/// State event types the Matrix specification defines beyond the common
/// ones: an explicit level removed falls back to `state_default` for them.
const KNOWN_STATE_EVENTS: [&str; 10] = [
    "m.room.create",
    "m.room.member",
    "m.room.third_party_invite",
    "m.room.guest_access",
    "m.room.aliases",
    "m.space.child",
    "m.space.parent",
    "m.call.member",
    "m.room.related_groups",
    "m.room.retention",
];

/// Message (non-state) event types the specification defines beyond the
/// common ones: they fall back to `events_default`.
const KNOWN_MESSAGE_EVENTS: [&str; 8] = [
    "m.room.message",
    "m.room.encrypted",
    "m.sticker",
    "m.room.redaction",
    "m.call.invite",
    "m.call.answer",
    "m.call.hangup",
    "m.poll.start",
];

/// The event types every editor shows, state events unless noted: what
/// most rooms change.
const COMMON_EVENTS: [(&str, bool); 12] = [
    ("m.room.name", true),
    ("m.room.topic", true),
    ("m.room.avatar", true),
    ("m.room.canonical_alias", true),
    ("m.room.join_rules", true),
    ("m.room.history_visibility", true),
    ("m.room.power_levels", true),
    ("m.room.pinned_events", true),
    ("m.room.encryption", true),
    ("m.room.server_acl", true),
    ("m.room.tombstone", true),
    ("m.reaction", false),
];

/// One permission a room sets.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PermissionKey {
    /// The level of everyone without one of their own.
    UsersDefault,
    /// What sending a message (any non-state event) needs by default.
    EventsDefault,
    /// What changing room state needs by default.
    StateDefault,
    Invite,
    Kick,
    Ban,
    /// Removing other members' messages.
    Redact,
    /// Notifying the whole room (`@room`).
    NotifyRoom,
    /// One `events` entry, by event type.
    Event(String),
}

impl PermissionKey {
    /// The top-level content field, or the map and entry it lives in.
    fn field(&self) -> Field<'_> {
        match self {
            Self::UsersDefault => Field::Top("users_default"),
            Self::EventsDefault => Field::Top("events_default"),
            Self::StateDefault => Field::Top("state_default"),
            Self::Invite => Field::Top("invite"),
            Self::Kick => Field::Top("kick"),
            Self::Ban => Field::Top("ban"),
            Self::Redact => Field::Top("redact"),
            Self::NotifyRoom => Field::Nested("notifications", "room"),
            Self::Event(kind) => Field::Nested("events", kind),
        }
    }
}

enum Field<'a> {
    Top(&'static str),
    Nested(&'static str, &'a str),
}

/// A user's level: a number, or a privileged creator's unlimited power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionLevel {
    Level(i64),
    Creator,
}

impl PermissionLevel {
    fn from_user(level: UserPowerLevel) -> Self {
        match level {
            UserPowerLevel::Int(level) => Self::Level(level.into()),
            _ => Self::Creator,
        }
    }

    /// Whether this level is at least `level`.
    pub fn reaches(self, level: i64) -> bool {
        match self {
            Self::Creator => true,
            Self::Level(own) => own >= level,
        }
    }

    /// Whether this level is above `level`.
    pub fn above(self, level: i64) -> bool {
        match self {
            Self::Creator => true,
            Self::Level(own) => own > level,
        }
    }
}

/// What kind of event an `events` entry is for, which decides the default
/// that applies without the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventClass {
    /// Not an event entry: a top-level permission or `@room`.
    NotEvent,
    /// A known state event: `state_default` applies.
    State,
    /// A known message event: `events_default` applies.
    Message,
    /// A custom type: `events_default` applies when it is sent as a message
    /// and `state_default` when it is sent as state, so its default is
    /// conditional.
    Unknown,
}

/// The class of one event type.
pub fn event_class(kind: &str) -> EventClass {
    if let Some((_, state)) = COMMON_EVENTS.iter().find(|(common, _)| *common == kind) {
        return if *state {
            EventClass::State
        } else {
            EventClass::Message
        };
    }
    if KNOWN_STATE_EVENTS.contains(&kind) {
        EventClass::State
    } else if KNOWN_MESSAGE_EVENTS.contains(&kind) {
        EventClass::Message
    } else {
        EventClass::Unknown
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionThreshold {
    pub key: PermissionKey,
    /// The level the action needs now: the room's own value or the default
    /// that applies (for an unset `Unknown` entry, the message default).
    pub level: i64,
    /// The room sets it itself; otherwise a default applies.
    pub explicit: bool,
    /// For an event entry, which default applies without it.
    pub class: EventClass,
    /// This account may change it now under the Matrix rules.
    pub editable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionUser {
    pub user_id: String,
    /// The room display name when cached, else the user ID.
    pub display_name: String,
    pub level: PermissionLevel,
    /// This account may change this user's level now.
    pub editable: bool,
    pub own: bool,
}

/// Why nothing can be edited here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionsReadOnly {
    /// The room has no power-levels event; Morpheus does not create one,
    /// since the first event changes several defaults at once.
    NoPowerLevels,
    /// The room version's rules are unknown to this build.
    UnknownVersion,
    /// The content is not a plain object, names something twice, or holds
    /// `users`, `events` or `notifications` as something else.
    Unsupported,
}

/// The room's permissions as read, bounded, for the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomPermissionsSnapshot {
    pub room_id: String,
    pub room_version: String,
    pub own_user: String,
    pub own_level: PermissionLevel,
    /// This account may change permissions at all now.
    pub can_change: bool,
    /// The level changing permissions needs.
    pub change_level: i64,
    pub users_default: i64,
    /// This account has a level of its own (not the default), from the full
    /// stored levels.
    pub own_explicit: bool,
    /// Every user with a level of their own, from the full stored levels,
    /// not only the listed ones: the facts a typed user, a staged removal
    /// and a review rely on. Bounded by the size of a state event.
    pub exact_users: BTreeMap<String, i64>,
    /// Full parsed levels and an unambiguous stored power-level event were
    /// available. Only then can absence in this snapshot prove a saved
    /// removal; an unsupported or missing event supplies no such proof.
    pub has_complete_state: bool,
    /// The top-level permissions, `@room`, the common event types, then the
    /// room's other event entries.
    pub thresholds: Vec<PermissionThreshold>,
    /// Privileged creators, then users with their own level, highest first.
    pub users: Vec<PermissionUser>,
    pub omitted_users: usize,
    pub omitted_events: usize,
    pub read_only: Option<PermissionsReadOnly>,
}

/// The exact stored content a save must still find. Opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionsBaseline(Option<String>);

/// A read: what the editor shows and the baseline it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomPermissionsView {
    pub snapshot: RoomPermissionsSnapshot,
    pub baseline: PermissionsBaseline,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PermissionTarget {
    Threshold(PermissionKey),
    User(String),
}

/// One change: a new level, or `None` to remove the entry so the default
/// applies again (users and event entries only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionChange {
    pub target: PermissionTarget,
    pub level: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionsEdit {
    pub changes: Vec<PermissionChange>,
    /// The user confirmed lowering their own level, knowing they cannot
    /// raise it again themselves.
    pub own_demotion_confirmed: bool,
}

/// What one change does, as reviewed: the effective level before and after
/// and whether the room sets each itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewedChange {
    pub target: PermissionTarget,
    pub before: PermissionLevel,
    pub before_explicit: bool,
    /// The level after the whole edit, with the edit's own defaults.
    pub after: i64,
    pub after_explicit: bool,
    /// A removed custom event entry: `after` applies to it as a message
    /// event, this to it as a state event.
    pub after_if_state: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionsReview {
    pub changes: Vec<ReviewedChange>,
    /// Your own level goes down: (before, after).
    pub own_demotion: Option<(PermissionLevel, i64)>,
    /// After the edit no other listed user could change permissions.
    pub no_other_manager: bool,
}

/// Why an edit cannot be saved; nothing was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionProblem {
    NoChanges,
    TooManyChanges,
    Duplicate,
    InvalidUser,
    InvalidEventType,
    OutOfRange,
    /// A top-level permission can be changed but not removed.
    NotRemovable,
    ReadOnly(PermissionsReadOnly),
    /// Your level does not allow changing permissions.
    CannotChange,
    /// A privileged creator's power cannot be changed.
    CreatorFixed,
    /// The member's level is not below yours.
    NotBelowYou,
    /// The new level is above yours.
    AboveYourLevel,
    /// The permission now needs more than your level.
    NeedsMoreThanYou,
    /// Lowering your own level was not confirmed.
    OwnDemotionUnconfirmed,
    /// The edited content would exceed what Matrix accepts.
    TooLarge,
}

impl PermissionProblem {
    /// A fixed sentence; nothing was sent.
    pub fn text(self) -> &'static str {
        match self {
            Self::NoChanges => "Nothing was changed.",
            Self::TooManyChanges => "Save at most 50 changes at a time.",
            Self::Duplicate => "The same permission or user is changed twice.",
            Self::InvalidUser => "Enter a full Matrix user ID, like @name:example.org.",
            Self::InvalidEventType => "Enter a valid event type of at most 255 bytes.",
            Self::OutOfRange => "Use a whole number from -9007199254740991 to 9007199254740991.",
            Self::NotRemovable => "This permission can be changed but not removed.",
            Self::ReadOnly(PermissionsReadOnly::NoPowerLevels) => {
                "This room has no permissions event. Morpheus does not create one."
            }
            Self::ReadOnly(PermissionsReadOnly::UnknownVersion) => {
                "Morpheus does not know this room version's rules, so it does not change them."
            }
            Self::ReadOnly(PermissionsReadOnly::Unsupported) => {
                "This room's permissions use content Morpheus cannot change without loss."
            }
            Self::CannotChange => "Your level does not allow changing this room's permissions.",
            Self::CreatorFixed => "Room creators keep unlimited power; it cannot be changed.",
            Self::NotBelowYou => "You can only change the level of members below your own level.",
            Self::AboveYourLevel => "You cannot give a level above your own.",
            Self::NeedsMoreThanYou => {
                "You cannot change a permission that needs a level above your own."
            }
            Self::OwnDemotionUnconfirmed => "Confirm lowering your own level first.",
            Self::TooLarge => "The permissions would become too large for Matrix to accept.",
        }
    }
}

/// A load or save that did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionsError {
    /// Reading, scope or write outcome, as for every settings write: after
    /// the hand-off only `Unknown`, `SavedNotRead` or a fixed refusal.
    Settings(RoomSettingsError),
    /// The edit itself was refused; nothing was sent.
    Problem(PermissionProblem),
}

impl From<RoomSettingsError> for PermissionsError {
    fn from(error: RoomSettingsError) -> Self {
        Self::Settings(error)
    }
}

impl From<PermissionProblem> for PermissionsError {
    fn from(problem: PermissionProblem) -> Self {
        Self::Problem(problem)
    }
}

/// A save that reached the homeserver and was read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionsSaved {
    /// The permissions read after the write.
    pub view: RoomPermissionsView,
    /// Each change, and whether the read-back shows it.
    pub shown: Vec<(PermissionTarget, bool)>,
}

impl RoomSettingsSession {
    /// The room's permissions, read fresh under the session's scope.
    pub async fn load_permissions(
        &self,
    ) -> std::result::Result<RoomPermissionsView, PermissionsError> {
        let view = self
            .run(READ_TIMEOUT, async {
                let fresh = read_permissions(&self.room, &self.own_user).await?;
                self.check()?;
                Ok(fresh.view)
            })
            .await?;
        Ok(view)
    }

    /// One save of `edit`, reviewed against `reviewed`. Refusals send
    /// nothing; once the write is handed over, a stop or failure is only
    /// what may have happened, and nothing is resent.
    pub async fn save_permissions(
        &self,
        reviewed: &RoomPermissionsView,
        edit: &PermissionsEdit,
    ) -> std::result::Result<PermissionsSaved, PermissionsError> {
        shape(edit)?;
        let dispatch = Dispatch::default();
        let after = self
            .run_write(&dispatch, async {
                let fresh = read_permissions(&self.room, &self.own_user).await?;
                // Anything changed since the review, in any byte: nothing is sent.
                require_baseline(&reviewed.baseline.0, &fresh.view.baseline.0)?;
                let content = match check_fresh(&fresh, edit) {
                    Ok(content) => content,
                    Err(problem) => return Ok(Err(problem)),
                };
                self.send_state(&dispatch, StateEventType::RoomPowerLevels, content)
                    .await?;
                self.check()?;
                let after = read_permissions(&self.room, &self.own_user).await?;
                // The read-back is checked against the full stored levels.
                let Fresh { view, levels, raw } = after;
                Ok(Ok((view, ReadBack { levels, raw })))
            })
            .await??;
        let (after, fresh) = after;
        let shown = edit
            .changes
            .iter()
            .map(|change| (change.target.clone(), shows(&fresh, change)))
            .collect();
        Ok(PermissionsSaved { view: after, shown })
    }
}

/// A fresh read: the view, the computed levels and the exact content text.
struct Fresh {
    view: RoomPermissionsView,
    levels: Option<RoomPowerLevels>,
    raw: Option<String>,
}

async fn read_permissions(room: &Room, own: &OwnedUserId) -> Result<Fresh> {
    ensure_joined(room)?;
    let create = state(room, StateEventType::RoomCreate, "")
        .await?
        .ok_or(RoomSettingsError::InvalidState)?;
    let member = state(room, StateEventType::RoomMember, own.as_str())
        .await?
        .ok_or(RoomSettingsError::NotJoined)?;
    let member: RoomMemberEventContent = content(&member)?;
    if member.membership != MembershipState::Join {
        return Err(RoomSettingsError::NotJoined);
    }
    let (power, raw) = state_and_raw(room, StateEventType::RoomPowerLevels).await?;
    ensure_joined(room)?;
    let mut fresh = assemble_permissions(room.room_id(), own, &create, power.as_ref(), raw)?;
    fill_names(room, &mut fresh.view.snapshot.users).await;
    Ok(fresh)
}

/// The room's levels under its version's rules, with its creators; `None`
/// for a room version these rules do not cover, which grants nothing. The
/// same computation as `assemble`; one copy should remain at integration.
fn room_levels(
    create: &StateEnvelope,
    creation: &RoomCreateEventContent,
    power: Option<&StateEnvelope>,
) -> Result<Option<RoomPowerLevels>> {
    let Some(rules) = creation.room_version.rules() else {
        return Ok(None);
    };
    let mut creators = if rules.authorization.use_room_create_sender {
        vec![create.sender.clone()]
    } else {
        #[allow(deprecated)]
        vec![
            creation
                .creator
                .clone()
                .ok_or(RoomSettingsError::InvalidState)?,
        ]
    };
    if rules.authorization.additional_room_creators {
        creators.extend(creation.additional_creators.clone());
    }
    if creators.len() > 64 {
        return Err(RoomSettingsError::InvalidState);
    }
    let power = match power {
        Some(event) => {
            RoomPowerLevelsSource::Original(content::<RoomPowerLevelsEventContent>(event)?)
        }
        None => RoomPowerLevelsSource::None,
    };
    Ok(Some(RoomPowerLevels::new(
        power,
        &rules.authorization,
        creators,
    )))
}

/// The view of a fresh read. Pure: no request.
fn assemble_permissions(
    room_id: &RoomId,
    own: &OwnedUserId,
    create: &StateEnvelope,
    power: Option<&StateEnvelope>,
    raw: Option<String>,
) -> Result<Fresh> {
    let creation: RoomCreateEventContent = content(create)?;
    let levels = room_levels(create, &creation, power)?;
    let read_only = match (&levels, raw.as_deref()) {
        (None, _) => Some(PermissionsReadOnly::UnknownVersion),
        (_, None) => Some(PermissionsReadOnly::NoPowerLevels),
        (_, Some(text)) if !editable_structure(text) => Some(PermissionsReadOnly::Unsupported),
        _ => None,
    };
    let room_version = creation.room_version.as_str().to_owned();
    let snapshot = match &levels {
        Some(levels) => {
            let raw = raw.as_deref();
            snapshot_of(room_id, own, room_version, levels, raw, read_only)
        }
        None => RoomPermissionsSnapshot {
            room_id: room_id.to_string(),
            room_version,
            own_user: own.to_string(),
            own_level: PermissionLevel::Level(0),
            can_change: false,
            change_level: 0,
            users_default: 0,
            own_explicit: false,
            exact_users: BTreeMap::new(),
            has_complete_state: false,
            thresholds: Vec::new(),
            users: Vec::new(),
            omitted_users: 0,
            omitted_events: 0,
            read_only,
        },
    };
    let view = RoomPermissionsView {
        snapshot,
        baseline: PermissionsBaseline(raw.clone()),
    };
    Ok(Fresh { view, levels, raw })
}

/// Whether the content can be edited without loss: an object with unique
/// names whose `users`, `events` and `notifications`, where present, are
/// objects with unique names too.
fn editable_structure(text: &str) -> bool {
    let Ok(fields) = content_fields(text) else {
        return false;
    };
    fields.iter().all(|(name, value)| {
        !matches!(name.as_str(), "users" | "events" | "notifications")
            || content_fields(value.get()).is_ok()
    })
}

fn complete_power_state(levels: Option<&RoomPowerLevels>, raw: Option<&str>) -> bool {
    levels.is_some() && raw.is_some_and(editable_structure)
}

fn top_level_names(text: &str) -> HashSet<String> {
    content_fields(text)
        .map(|fields| fields.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

fn int(value: Int) -> i64 {
    value.into()
}

fn snapshot_of(
    room_id: &RoomId,
    own: &OwnedUserId,
    room_version: String,
    levels: &RoomPowerLevels,
    raw: Option<&str>,
    read_only: Option<PermissionsReadOnly>,
) -> RoomPermissionsSnapshot {
    let present = raw.map(top_level_names).unwrap_or_default();
    let own_level = PermissionLevel::from_user(levels.for_user(own));
    let events: BTreeMap<String, i64> = levels
        .events
        .iter()
        .map(|(kind, level)| (kind.to_string(), int(*level)))
        .collect();
    let change_level = events
        .get("m.room.power_levels")
        .copied()
        .unwrap_or(int(levels.state_default));
    let can_change =
        read_only.is_none() && levels.user_can_send_state(own, StateEventType::RoomPowerLevels);
    // A permission may change when its current value is not above you.
    let editable =
        |explicit: Option<i64>| can_change && explicit.is_none_or(|level| own_level.reaches(level));
    let top = [
        (
            PermissionKey::UsersDefault,
            levels.users_default,
            "users_default",
        ),
        (
            PermissionKey::EventsDefault,
            levels.events_default,
            "events_default",
        ),
        (
            PermissionKey::StateDefault,
            levels.state_default,
            "state_default",
        ),
        (PermissionKey::Invite, levels.invite, "invite"),
        (PermissionKey::Kick, levels.kick, "kick"),
        (PermissionKey::Ban, levels.ban, "ban"),
        (PermissionKey::Redact, levels.redact, "redact"),
    ];
    let mut thresholds = Vec::new();
    for (key, level, name) in top {
        let explicit = present.contains(name);
        thresholds.push(PermissionThreshold {
            key,
            level: int(level),
            explicit,
            class: EventClass::NotEvent,
            editable: editable(explicit.then_some(int(level))),
        });
    }
    let notify = int(levels.notifications.room);
    // Without its own entry the default of 50 applies.
    let notify_explicit = raw
        .and_then(|text| nested_names(text, "notifications"))
        .is_some_and(|names| names.contains("room"));
    thresholds.push(PermissionThreshold {
        key: PermissionKey::NotifyRoom,
        level: notify,
        explicit: notify_explicit,
        class: EventClass::NotEvent,
        editable: editable(notify_explicit.then_some(notify)),
    });
    let mut listed = HashSet::new();
    for (kind, state) in COMMON_EVENTS {
        let explicit = events.get(kind).copied();
        let default = if state {
            levels.state_default
        } else {
            levels.events_default
        };
        thresholds.push(PermissionThreshold {
            key: PermissionKey::Event(kind.to_owned()),
            level: explicit.unwrap_or(int(default)),
            explicit: explicit.is_some(),
            class: if state {
                EventClass::State
            } else {
                EventClass::Message
            },
            editable: editable(explicit),
        });
        listed.insert(kind);
    }
    let mut omitted_events = 0;
    for (kind, level) in &events {
        if listed.contains(kind.as_str()) {
            continue;
        }
        // A type Morpheus could not write back exactly is counted, not listed.
        if thresholds.len() >= COMMON_EVENTS.len() + 8 + MAX_PERMISSION_EVENTS
            || !valid_event_type(kind)
        {
            omitted_events += 1;
            continue;
        }
        thresholds.push(PermissionThreshold {
            key: PermissionKey::Event(kind.clone()),
            level: *level,
            explicit: true,
            class: event_class(kind),
            editable: editable(Some(*level)),
        });
    }
    let mut users = Vec::new();
    let mut creators: Vec<String> = levels
        .rules
        .privileged_creators
        .iter()
        .flatten()
        .map(ToString::to_string)
        .collect();
    creators.sort();
    for creator in creators {
        users.push(PermissionUser {
            own: creator == own.as_str(),
            display_name: creator.clone(),
            user_id: creator,
            level: PermissionLevel::Creator,
            editable: false,
        });
    }
    let mut explicit: Vec<(&OwnedUserId, i64)> = levels
        .users
        .iter()
        .map(|(user, level)| (user, int(*level)))
        .collect();
    // Highest first, then by ID, so the list is stable between reads.
    explicit.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut omitted_users = 0;
    for (user, level) in explicit {
        if users.len() >= MAX_PERMISSION_USERS {
            omitted_users += 1;
            continue;
        }
        users.push(PermissionUser {
            user_id: user.to_string(),
            display_name: user.to_string(),
            level: PermissionLevel::Level(level),
            editable: read_only.is_none() && levels.user_can_change_user_power_level(own, user),
            own: user == own,
        });
    }
    let exact_users = levels
        .users
        .iter()
        .map(|(user, level)| (user.to_string(), int(*level)))
        .collect();
    RoomPermissionsSnapshot {
        room_id: room_id.to_string(),
        room_version,
        own_user: own.to_string(),
        own_level,
        can_change,
        change_level,
        users_default: int(levels.users_default),
        own_explicit: levels.users.contains_key(own),
        exact_users,
        has_complete_state: complete_power_state(Some(levels), raw),
        thresholds,
        users,
        omitted_users,
        omitted_events,
        read_only,
    }
}

/// An event type Morpheus lists and writes: bounded, printable, no spaces.
fn valid_event_type(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= MAX_EVENT_TYPE_BYTES
        && !kind.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// The names inside one nested object of the content, if it is one.
fn nested_names(text: &str, map: &str) -> Option<HashSet<String>> {
    let fields = content_fields(text).ok()?;
    let (_, value) = fields.into_iter().find(|(name, _)| name == map)?;
    let inner = content_fields(value.get()).ok()?;
    Some(inner.into_iter().map(|(name, _)| name).collect())
}

/// What the Matrix rules ask of one edit, from either a read the user
/// reviewed or a fresh read before the write.
struct Authority {
    own: String,
    own_level: PermissionLevel,
    can_change: bool,
    users_default: i64,
    read_only: Option<PermissionsReadOnly>,
    /// Users with their own level, and privileged creators.
    users: HashMap<String, i64>,
    creators: HashSet<String>,
    /// The values the room sets itself.
    explicit: HashMap<PermissionKey, i64>,
    /// The levels that apply now, set or default.
    effective: HashMap<PermissionKey, i64>,
    events_default: i64,
    state_default: i64,
    /// The level changing permissions needs now.
    change_level: i64,
}

impl Authority {
    fn of_snapshot(snapshot: &RoomPermissionsSnapshot) -> Self {
        // Every user with a level of their own, not only the listed ones.
        let users: HashMap<String, i64> = snapshot
            .exact_users
            .iter()
            .map(|(user, level)| (user.clone(), *level))
            .collect();
        let creators: HashSet<String> = snapshot
            .users
            .iter()
            .filter(|user| user.level == PermissionLevel::Creator)
            .map(|user| user.user_id.clone())
            .collect();
        let mut explicit = HashMap::new();
        let mut effective = HashMap::new();
        for threshold in &snapshot.thresholds {
            if threshold.explicit {
                explicit.insert(threshold.key.clone(), threshold.level);
            }
            effective.insert(threshold.key.clone(), threshold.level);
        }
        let level = |key: PermissionKey| effective.get(&key).copied().unwrap_or(0);
        let events_default = level(PermissionKey::EventsDefault);
        let state_default = level(PermissionKey::StateDefault);
        Self {
            own: snapshot.own_user.clone(),
            own_level: snapshot.own_level,
            can_change: snapshot.can_change,
            users_default: snapshot.users_default,
            read_only: snapshot.read_only,
            events_default,
            state_default,
            change_level: snapshot.change_level,
            users,
            creators,
            explicit,
            effective,
        }
    }

    fn of_levels(fresh: &Fresh) -> Option<Self> {
        let levels = fresh.levels.as_ref()?;
        let snapshot = &fresh.view.snapshot;
        let mut authority = Self::of_snapshot(snapshot);
        // Every user and event entry, not only the listed ones.
        authority.users = levels
            .users
            .iter()
            .map(|(user, level)| (user.to_string(), int(*level)))
            .collect();
        for (kind, level) in &levels.events {
            let key = PermissionKey::Event(kind.to_string());
            authority.explicit.insert(key.clone(), int(*level));
            authority.effective.insert(key, int(*level));
        }
        Some(authority)
    }

    /// The effective level of a user now.
    fn user_level(&self, user: &str) -> PermissionLevel {
        if self.creators.contains(user) {
            return PermissionLevel::Creator;
        }
        PermissionLevel::Level(self.users.get(user).copied().unwrap_or(self.users_default))
    }

    /// The defaults after the whole edit: a staged value, else the current.
    fn proposed(&self, edit: &PermissionsEdit) -> Proposed {
        let staged = |key: PermissionKey| {
            let target = PermissionTarget::Threshold(key);
            edit.changes
                .iter()
                .find(|change| change.target == target)
                .and_then(|change| change.level)
        };
        Proposed {
            users_default: staged(PermissionKey::UsersDefault).unwrap_or(self.users_default),
            events_default: staged(PermissionKey::EventsDefault).unwrap_or(self.events_default),
            state_default: staged(PermissionKey::StateDefault).unwrap_or(self.state_default),
        }
    }

    /// One change under the Matrix rules for power-level events, authorized
    /// against the current (fresh) state: its effect after the whole edit,
    /// with the edit's own defaults, or why it is refused.
    fn check(
        &self,
        change: &PermissionChange,
        proposed: &Proposed,
    ) -> std::result::Result<ReviewedChange, PermissionProblem> {
        if let Some(reason) = self.read_only {
            return Err(PermissionProblem::ReadOnly(reason));
        }
        if let Some(level) = change.level {
            in_range(level)?;
        }
        match &change.target {
            PermissionTarget::User(user) => {
                if self.creators.contains(user) {
                    return Err(PermissionProblem::CreatorFixed);
                }
                if !self.can_change {
                    return Err(PermissionProblem::CannotChange);
                }
                let old = self.users.get(user).copied();
                // Another member's own level must be below yours.
                if *user != self.own && old.is_some_and(|old| !self.own_level.above(old)) {
                    return Err(PermissionProblem::NotBelowYou);
                }
                if change.level.is_some_and(|new| !self.own_level.reaches(new)) {
                    return Err(PermissionProblem::AboveYourLevel);
                }
                Ok(ReviewedChange {
                    target: change.target.clone(),
                    before: self.user_level(user),
                    before_explicit: old.is_some(),
                    after: change.level.unwrap_or(proposed.users_default),
                    after_explicit: change.level.is_some(),
                    after_if_state: None,
                })
            }
            PermissionTarget::Threshold(key) => {
                if !self.can_change {
                    return Err(PermissionProblem::CannotChange);
                }
                let old = self.explicit.get(key).copied();
                // The default without the entry, from the edit's own
                // defaults; a custom event type's is conditional.
                let mut if_state = None;
                let default = match key {
                    PermissionKey::Event(kind) => match event_class(kind) {
                        EventClass::State => proposed.state_default,
                        EventClass::Unknown => {
                            if_state = Some(proposed.state_default);
                            proposed.events_default
                        }
                        _ => proposed.events_default,
                    },
                    PermissionKey::NotifyRoom => 50,
                    _ if change.level.is_none() => return Err(PermissionProblem::NotRemovable),
                    _ => 0,
                };
                if old.is_some_and(|old| !self.own_level.reaches(old))
                    || change.level.is_some_and(|new| !self.own_level.reaches(new))
                {
                    return Err(PermissionProblem::NeedsMoreThanYou);
                }
                let before = self.effective.get(key).copied().or(old).unwrap_or(default);
                Ok(ReviewedChange {
                    target: change.target.clone(),
                    before: PermissionLevel::Level(before),
                    before_explicit: old.is_some(),
                    after: change.level.unwrap_or(default),
                    after_explicit: change.level.is_some(),
                    after_if_state: if_state.filter(|_| change.level.is_none()),
                })
            }
        }
    }

    /// Your effective level after the whole edit: your own entry as changed,
    /// or removed (then the edit's `users_default`), or kept; without an
    /// entry of your own, the edit's `users_default`.
    fn own_after(&self, changes: &[ReviewedChange], proposed: &Proposed) -> PermissionLevel {
        if self.creators.contains(&self.own) {
            return PermissionLevel::Creator;
        }
        let own = PermissionTarget::User(self.own.clone());
        if let Some(change) = changes.iter().find(|change| change.target == own) {
            return PermissionLevel::Level(change.after);
        }
        match self.users.get(&self.own) {
            Some(level) => PermissionLevel::Level(*level),
            None => PermissionLevel::Level(proposed.users_default),
        }
    }

    fn review(
        &self,
        edit: &PermissionsEdit,
    ) -> std::result::Result<PermissionsReview, PermissionProblem> {
        let proposed = self.proposed(edit);
        let mut changes = Vec::with_capacity(edit.changes.len());
        for change in &edit.changes {
            changes.push(self.check(change, &proposed)?);
        }
        // Your whole effective level before and after, whatever lowers it:
        // your own entry, its removal, or a new default you inherit.
        let own_before = self.user_level(&self.own);
        let own_demotion = match (own_before, self.own_after(&changes, &proposed)) {
            (PermissionLevel::Level(before), PermissionLevel::Level(after)) if after < before => {
                Some((own_before, after))
            }
            _ => None,
        };
        if own_demotion.is_some() && !edit.own_demotion_confirmed {
            return Err(PermissionProblem::OwnDemotionUnconfirmed);
        }
        // What changing permissions needs after the edit.
        let power_levels = PermissionKey::Event("m.room.power_levels".to_owned());
        let staged_power = PermissionTarget::Threshold(power_levels.clone());
        let change_level = match changes.iter().find(|change| change.target == staged_power) {
            Some(change) => change.after,
            None => match self.explicit.get(&power_levels) {
                Some(level) => *level,
                None => proposed.state_default,
            },
        };
        let after = |user: &str| {
            let target = PermissionTarget::User(user.to_owned());
            match changes.iter().find(|change| change.target == target) {
                Some(change) => PermissionLevel::Level(change.after),
                None if self.creators.contains(user) => PermissionLevel::Creator,
                None => PermissionLevel::Level(
                    self.users
                        .get(user)
                        .copied()
                        .unwrap_or(proposed.users_default),
                ),
            }
        };
        let others = self.creators.iter().chain(self.users.keys());
        // Members without a level of their own get the edit's default.
        let everyone_can = proposed.users_default >= change_level;
        let no_other_manager = own_demotion.is_some()
            && !everyone_can
            && !others
                .filter(|user| **user != self.own)
                .any(|user| after(user.as_str()).reaches(change_level));
        Ok(PermissionsReview {
            changes,
            own_demotion,
            no_other_manager,
        })
    }
}

/// The defaults after an edit.
struct Proposed {
    users_default: i64,
    events_default: i64,
    state_default: i64,
}

/// The local checks of an edit's shape: something to save, within bounds,
/// each target once and well formed. Nothing is read or sent.
fn shape(edit: &PermissionsEdit) -> std::result::Result<(), PermissionProblem> {
    if edit.changes.is_empty() {
        return Err(PermissionProblem::NoChanges);
    }
    if edit.changes.len() > MAX_PERMISSION_CHANGES {
        return Err(PermissionProblem::TooManyChanges);
    }
    let mut seen = HashSet::new();
    for change in &edit.changes {
        if !seen.insert(&change.target) {
            return Err(PermissionProblem::Duplicate);
        }
        match &change.target {
            PermissionTarget::User(user) => {
                if user.len() > MAX_USER_ID_BYTES || UserId::parse(user).is_err() {
                    return Err(PermissionProblem::InvalidUser);
                }
            }
            PermissionTarget::Threshold(PermissionKey::Event(kind)) => {
                if !valid_event_type(kind) {
                    return Err(PermissionProblem::InvalidEventType);
                }
            }
            PermissionTarget::Threshold(_) => {}
        }
        if let Some(level) = change.level {
            in_range(level)?;
        }
    }
    Ok(())
}

fn in_range(level: i64) -> std::result::Result<(), PermissionProblem> {
    Int::new(level)
        .map(|_| ())
        .ok_or(PermissionProblem::OutOfRange)
}

/// The edit as reviewed against `snapshot`: each change's effect, your own
/// demotion, and whether anyone else could still change permissions. The
/// save checks the same rules again against a fresh read, with every user
/// and entry, not only the listed ones.
pub fn review_permissions(
    snapshot: &RoomPermissionsSnapshot,
    edit: &PermissionsEdit,
) -> std::result::Result<PermissionsReview, PermissionProblem> {
    shape(edit)?;
    Authority::of_snapshot(snapshot).review(edit)
}

/// The edit checked against a fresh read, with every user and entry, and
/// the content to write.
fn check_fresh(
    fresh: &Fresh,
    edit: &PermissionsEdit,
) -> std::result::Result<Box<RawValue>, PermissionProblem> {
    let unknown = PermissionProblem::ReadOnly(PermissionsReadOnly::UnknownVersion);
    let authority = Authority::of_levels(fresh).ok_or(unknown)?;
    authority.review(edit)?;
    let none = PermissionProblem::ReadOnly(PermissionsReadOnly::NoPowerLevels);
    let stored = fresh.raw.as_deref().ok_or(none)?;
    edited_power_levels(stored, &edit.changes)
}

/// The full stored levels of a read-back: every user and entry, and the
/// exact content, not the display-capped lists.
struct ReadBack {
    levels: Option<RoomPowerLevels>,
    raw: Option<String>,
}

/// Whether the full read-back shows one change: the exact entry with the
/// exact level, or (for a removal) the entry really absent. Without the
/// full levels nothing is claimed.
fn shows(fresh: &ReadBack, change: &PermissionChange) -> bool {
    if !complete_power_state(fresh.levels.as_ref(), fresh.raw.as_deref()) {
        return false;
    }
    let (Some(levels), Some(raw)) = (fresh.levels.as_ref(), fresh.raw.as_deref()) else {
        return false;
    };
    match &change.target {
        PermissionTarget::User(user) => {
            let stored = levels
                .users
                .iter()
                .find(|(id, _)| id.as_str() == user.as_str())
                .map(|(_, level)| int(*level));
            stored == change.level
        }
        PermissionTarget::Threshold(key) => match key.field() {
            Field::Top(name) => {
                let value = match key {
                    PermissionKey::UsersDefault => levels.users_default,
                    PermissionKey::EventsDefault => levels.events_default,
                    PermissionKey::StateDefault => levels.state_default,
                    PermissionKey::Invite => levels.invite,
                    PermissionKey::Kick => levels.kick,
                    PermissionKey::Ban => levels.ban,
                    PermissionKey::Redact => levels.redact,
                    _ => return false,
                };
                // Top-level permissions are set, never removed.
                change.level == Some(int(value)) && top_level_names(raw).contains(name)
            }
            Field::Nested("notifications", _) => {
                let present =
                    nested_names(raw, "notifications").is_some_and(|names| names.contains("room"));
                match change.level {
                    Some(level) => present && int(levels.notifications.room) == level,
                    None => !present,
                }
            }
            Field::Nested(_, kind) => {
                let stored = levels
                    .events
                    .iter()
                    .find(|(event, _)| event.to_string() == kind)
                    .map(|(_, level)| int(*level));
                stored == change.level
            }
        },
    }
}

/// `stored` with each change applied: a top-level permission set in place
/// (or appended), and a `users`, `events` or `notifications` entry set,
/// appended or removed inside its object. Every other field and entry, the
/// order and every number keep their exact text. Content that is not an
/// object, names something twice or holds one of these maps as something
/// else is refused, never rewritten.
fn edited_power_levels(
    stored: &str,
    changes: &[PermissionChange],
) -> std::result::Result<Box<RawValue>, PermissionProblem> {
    let unsupported = PermissionProblem::ReadOnly(PermissionsReadOnly::Unsupported);
    let mut top = content_fields(stored).map_err(|_| unsupported)?;
    for change in changes {
        let (map, entry) = match &change.target {
            PermissionTarget::User(user) => ("users", user.as_str()),
            PermissionTarget::Threshold(key) => match key.field() {
                Field::Top(name) => {
                    let level = change.level.ok_or(PermissionProblem::NotRemovable)?;
                    set_field(&mut top, name, Some(level_text(level)?));
                    continue;
                }
                Field::Nested(map, entry) => (map, entry),
            },
        };
        let slot = top.iter().position(|(name, _)| name == map);
        let mut inner = match slot {
            Some(index) => content_fields(top[index].1.get()).map_err(|_| unsupported)?,
            None => Vec::new(),
        };
        let value = change.level.map(level_text).transpose()?;
        set_field(&mut inner, entry, value);
        let object = RawValue::from_string(object_text(&inner)?).map_err(|_| unsupported)?;
        match slot {
            Some(index) => top[index].1 = object,
            None if !inner.is_empty() => top.push((map.to_owned(), object)),
            None => {}
        }
    }
    let text = object_text(&top)?;
    if text.len() > MAX_STATE_BYTES {
        return Err(PermissionProblem::TooLarge);
    }
    RawValue::from_string(text).map_err(|_| unsupported)
}

/// Set `name` in place (or append it), or remove it with `None`.
fn set_field(fields: &mut Vec<(String, Box<RawValue>)>, name: &str, value: Option<Box<RawValue>>) {
    match (
        fields.iter().position(|(existing, _)| existing == name),
        value,
    ) {
        (Some(index), Some(value)) => fields[index].1 = value,
        (None, Some(value)) => fields.push((name.to_owned(), value)),
        (Some(index), None) => {
            fields.remove(index);
        }
        (None, None) => {}
    }
}

fn level_text(level: i64) -> std::result::Result<Box<RawValue>, PermissionProblem> {
    in_range(level)?;
    RawValue::from_string(level.to_string()).map_err(|_| PermissionProblem::OutOfRange)
}

fn object_text(
    fields: &[(String, Box<RawValue>)],
) -> std::result::Result<String, PermissionProblem> {
    let mut text = String::from("{");
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        let name = serde_json::to_string(name)
            .map_err(|_| PermissionProblem::ReadOnly(PermissionsReadOnly::Unsupported))?;
        text.push_str(&name);
        text.push(':');
        text.push_str(value.get());
    }
    text.push('}');
    Ok(text)
}

/// Display names from this device's member store only, within a short
/// budget; a user without one keeps the user ID.
async fn fill_names(room: &Room, users: &mut [PermissionUser]) {
    let deadline = time::Instant::now() + NAMES_TIMEOUT;
    for user in users.iter_mut() {
        let Ok(id) = UserId::parse(&user.user_id) else {
            continue;
        };
        let member = time::timeout_at(deadline, room.get_member_no_sync(&id)).await;
        let Ok(Ok(Some(member))) = member else {
            continue;
        };
        if let Some(name) = member.display_name() {
            let name = crate::pinned_messages::plain_text(name, MAX_NAME_CHARS);
            if !name.is_empty() {
                user.display_name = name;
            }
        }
    }
}

#[cfg(test)]
mod tests;
