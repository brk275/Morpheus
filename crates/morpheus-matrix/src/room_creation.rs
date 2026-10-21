//! Creating a room or a Space for the selected account from one validated
//! plan. The plan's preview lists exactly the settings that are sent: who
//! may join, directory listing, address, encryption, history, guests and
//! placement. Nothing the homeserver would otherwise choose silently is left
//! to a preset.
//!
//! The create request is sent with no SDK retries (retry_limit 0), and
//! Morpheus never sends it again by itself. A reply that never arrives is an
//! unknown outcome: the caller must say the room may exist. A definite
//! refusal from the homeserver (4xx) created nothing. retry_limit 0 alone
//! does NOT make it a single attempt on the wire; see below.
//!
//! The pinned client refreshes an expired access token (handle_refresh_tokens)
//! and then sends the refused request once more, whatever its RequestConfig.
//! Such a refusal (401 M_UNKNOWN_TOKEN) comes before the homeserver acts, so
//! at most one room results. But the second send could go out after the
//! account was left while the refresh waited, so the send races the
//! selection's revocation: a revoked send is dropped there, and reported as
//! an unknown outcome because it may already have been sent.
//!
//! The two mutations (the create request and the Space's m.space.child write)
//! also opt out of that refresh and replay with
//! `RequestConfig::disable_auth_refresh()` (`mutation_config`; the vendored
//! SDK's "Opt-in authentication refresh and replay suppression" patch),
//! together with zero HTTP retries and the revocation race, so each is one
//! attempt on the wire. A refused token then returns the original 401, which
//! is reported as unconfirmed and needs an explicit new attempt. Reads keep
//! the client's refresh policy.
//!
//! Creating, placing in a Space, waiting for sync and opening are separate
//! steps: a created room is confirmed, with its ID, before anything else
//! happens to it, and nothing later turns it into an unknown outcome.
//!
//! A room made inside a Space names the Space as its parent from its first
//! event. The Space lists it only through an `m.space.child` write, after a
//! fresh power-level check. When the room exists but could not be listed,
//! that is reported as its own outcome with the room, and listing it again
//! is an explicit, idempotent state write.
//!
//! Encryption is the default for rooms. A Space holds no messages and is
//! never encrypted, and nothing here ever turns encryption off.

use std::time::Duration;

use matrix_sdk::{
    Client, HttpError, RoomState,
    config::RequestConfig,
    ruma::{
        OwnedRoomId, OwnedServerName, OwnedUserId, RoomAliasId, RoomId, RoomVersionId, ServerName,
        api::{
            client::{
                discovery::get_capabilities,
                room::{Visibility, create_room},
                state::send_state_event,
            },
            error::ErrorKind,
        },
        events::{
            InitialStateEvent, StateEventType,
            room::{
                encryption::RoomEncryptionEventContent,
                guest_access::{GuestAccess, RoomGuestAccessEventContent},
                history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
                join_rules::{AllowRule, JoinRule, RoomJoinRulesEventContent},
            },
            space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
        },
        room::RoomType,
        serde::Raw,
    },
};

use crate::{AccountManager, AccountSelection, pinned_messages::plain_text, room_settings};

pub const MAX_CREATION_NAME_BYTES: usize = 255;
pub const MAX_CREATION_TOPIC_BYTES: usize = 4096;
pub const MAX_ALIAS_LOCALPART_BYTES: usize = 64;
const MAX_LABEL_CHARS: usize = 120;
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(15);
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);
const LINK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a created room may take to arrive through sync before the
/// result is returned anyway.
const SYNC_WAIT: Duration = Duration::from_secs(15);
const MAX_REPLY_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationKind {
    Room,
    Space,
}

/// Who may join without an invitation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationAccess {
    /// Only people who are invited.
    InviteOnly,
    /// Members of the parent Space, without an invitation.
    SpaceMembers,
    /// Anyone who has the address or a link.
    Public,
}

/// What the person chose, before validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreationChoices {
    pub kind: CreationKind,
    pub name: String,
    pub topic: String,
    /// An address such as `team`, `#team` or `#team:own.server`; empty for
    /// none.
    pub alias: String,
    pub access: CreationAccess,
    /// List it in the homeserver's public room directory; public only.
    pub listed: bool,
    /// Encrypt its messages from the first one; rooms only.
    pub encrypted: bool,
    /// Create it inside this joined Space instead of Home.
    pub parent: Option<String>,
    /// The Space suggests it to its members.
    pub suggested: bool,
}

impl CreationChoices {
    /// The safe starting point: invite only, not listed, and encrypted for
    /// a room. A Space is never encrypted.
    pub fn new(kind: CreationKind, parent: Option<String>) -> Self {
        Self {
            kind,
            name: String::new(),
            topic: String::new(),
            alias: String::new(),
            access: CreationAccess::InviteOnly,
            listed: false,
            encrypted: kind == CreationKind::Room,
            parent,
            suggested: false,
        }
    }
}

/// What this account and homeserver allow, read before anything is offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreationContext {
    pub user_id: String,
    pub server_name: String,
    /// The room version the homeserver will use, when it said.
    pub room_version: Option<String>,
    /// New rooms may admit a Space's members without an invitation.
    pub space_members_supported: bool,
    pub parent: Option<ParentContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentContext {
    pub space_id: String,
    /// Bounded plain text.
    pub name: String,
    /// The account may list rooms in the Space now (fresh power levels).
    pub can_link: bool,
}

/// One line of the settings preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewLine {
    pub label: &'static str,
    pub value: String,
}

/// A validated plan: exactly what `create_room_for` sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreationPlan {
    kind: CreationKind,
    name: String,
    topic: Option<String>,
    alias_localpart: Option<String>,
    access: CreationAccess,
    listed: bool,
    encrypted: bool,
    parent: Option<ParentContext>,
    suggested: bool,
    user_id: String,
    server_name: String,
    room_version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationError {
    NameMissing,
    NameTooLong,
    NameInvalid,
    TopicTooLong,
    TopicInvalid,
    AliasInvalid,
    AliasServer,
    AccessUnavailable,
    ListedNeedsPublic,
    EncryptedSpace,
    PlacementNotAllowed,
    ContextChanged,
    AccountChanged,
    AliasTaken,
    RateLimited,
    Forbidden,
    Unsupported,
    Rejected,
    /// The request may have reached the homeserver; nothing is known.
    OutcomeUnknown,
    Unavailable,
    /// The caller's own admission refused the hand-off (in the app: a
    /// security or account step began since the click, or runs now).
    /// Nothing was sent.
    SecurityBusy,
}

impl CreationError {
    /// A fixed sentence; the homeserver's reason is never shown.
    pub fn text(self) -> &'static str {
        match self {
            Self::NameMissing => "Enter a name.",
            Self::NameTooLong => "Use a name of at most 255 bytes.",
            Self::NameInvalid => "Remove line breaks and control characters from the name.",
            Self::TopicTooLong => "Use a topic of at most 4096 bytes.",
            Self::TopicInvalid => {
                "Remove control characters other than line breaks from the topic."
            }
            Self::AliasInvalid => {
                "Use an address of 1 to 64 lowercase letters, digits, dots, dashes, underscores or equals signs."
            }
            Self::AliasServer => "An address can only be created on your own homeserver.",
            Self::AccessUnavailable => {
                "Space members can join without an invitation only in a room created in a Space, on a homeserver that supports it."
            }
            Self::ListedNeedsPublic => {
                "Only a room anyone can join can be listed in the public directory."
            }
            Self::EncryptedSpace => "Spaces hold no messages, so they are not encrypted.",
            Self::PlacementNotAllowed => {
                "You can no longer add rooms to this Space. Nothing was created; create it in Home instead."
            }
            Self::ContextChanged => {
                "The account or Space changed. Nothing was created; review the settings again."
            }
            Self::AccountChanged => "The account changed before anything was sent.",
            Self::AliasTaken => {
                "That address is already in use. Nothing was created; choose another address."
            }
            Self::RateLimited => {
                "Your homeserver is limiting requests. Nothing was created; try again shortly."
            }
            Self::Forbidden => "Your homeserver does not allow this. Nothing was created.",
            Self::Unsupported => {
                "Your homeserver does not support these settings. Nothing was created."
            }
            Self::Rejected => "Your homeserver refused these settings. Nothing was created.",
            Self::OutcomeUnknown => {
                "No reply arrived, so it may or may not have been created. Check your room list before creating it again."
            }
            Self::Unavailable => "Creating is unavailable right now. Nothing was sent.",
            Self::SecurityBusy => {
                "Finish the account or security step in progress, then try again. Nothing was created."
            }
        }
    }
    /// Whether the homeserver may have created something.
    pub fn may_exist(self) -> bool {
        self == Self::OutcomeUnknown
    }
}

/// Why a created room is not listed in its Space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkFailure {
    AccountChanged,
    NotJoined,
    NotAllowed,
    RateLimited,
    Rejected,
    /// No reply arrived; listing it again is safe.
    OutcomeUnknown,
    Unavailable,
    /// The caller's own admission refused the hand-off (in the app: a
    /// security or account step began since the click). Nothing was sent;
    /// listing it again is safe.
    SecurityBusy,
}

impl LinkFailure {
    pub fn text(self) -> &'static str {
        match self {
            Self::AccountChanged => "The account changed before it was added to the Space.",
            Self::NotJoined => "You are no longer in the Space.",
            Self::NotAllowed => "You are no longer allowed to add rooms to this Space.",
            Self::RateLimited => {
                "Your homeserver is limiting requests; try adding it again shortly."
            }
            Self::Rejected => "The Space's homeserver refused to list it.",
            Self::OutcomeUnknown => {
                "Its placement in the Space is unconfirmed: no reply arrived. Adding it again is safe."
            }
            Self::Unavailable => "The Space could not be updated right now.",
            Self::SecurityBusy => {
                "A security step was in progress, so it was not added; add it again once it ends."
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Home,
    Linked {
        space_id: String,
    },
    /// The room exists and names the Space as its parent, but the Space
    /// does not list it.
    NotLinked {
        space_id: String,
        failure: LinkFailure,
    },
}

/// A room the homeserver confirmed it created, with what placing it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedRoom {
    pub room_id: String,
    pub kind: CreationKind,
    pub name: String,
    /// The Space to list it in, already named as its parent.
    pub parent: Option<String>,
    pub suggested: bool,
}

/// Await `send` unless `selection` is revoked; `None` then, and the send is
/// dropped wherever it was. That includes a token refresh the base SDK is
/// waiting on before it would send the request again (client/futures.rs,
/// `RetryRequest::Yes`), so nothing goes out for an account that is no
/// longer selected. Revocation is checked FIRST on every poll: when it and
/// the send's next step are both ready, the send is not resumed, so no
/// request is started after the account was left. A send that returned
/// before the revocation keeps its result.
pub(crate) async fn race_revocation<F: std::future::Future>(
    selection: &AccountSelection,
    send: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = selection.revoked() => None,
        output = send => Some(output),
    }
}

impl CreationPlan {
    /// Validate `choices` against what `context` allows.
    pub fn new(
        choices: &CreationChoices,
        context: &CreationContext,
    ) -> Result<Self, CreationError> {
        let name = choices.name.trim();
        if name.is_empty() {
            return Err(CreationError::NameMissing);
        }
        if name.len() > MAX_CREATION_NAME_BYTES {
            return Err(CreationError::NameTooLong);
        }
        if name.chars().any(char::is_control) {
            return Err(CreationError::NameInvalid);
        }
        let topic = choices.topic.trim();
        if topic.len() > MAX_CREATION_TOPIC_BYTES {
            return Err(CreationError::TopicTooLong);
        }
        if topic.chars().any(|ch| ch.is_control() && ch != '\n') {
            return Err(CreationError::TopicInvalid);
        }
        let server =
            ServerName::parse(&context.server_name).map_err(|_| CreationError::Unavailable)?;
        let alias_localpart = alias_localpart(&choices.alias, &server)?;
        let parent = match &choices.parent {
            None => None,
            Some(space_id) => {
                let parent = context
                    .parent
                    .as_ref()
                    .filter(|parent| &parent.space_id == space_id)
                    .ok_or(CreationError::ContextChanged)?;
                if !parent.can_link {
                    return Err(CreationError::PlacementNotAllowed);
                }
                RoomId::parse(space_id).map_err(|_| CreationError::ContextChanged)?;
                Some(parent.clone())
            }
        };
        if choices.access == CreationAccess::SpaceMembers
            && (parent.is_none() || !context.space_members_supported)
        {
            return Err(CreationError::AccessUnavailable);
        }
        if choices.listed && choices.access != CreationAccess::Public {
            return Err(CreationError::ListedNeedsPublic);
        }
        if choices.kind == CreationKind::Space && choices.encrypted {
            return Err(CreationError::EncryptedSpace);
        }
        Ok(Self {
            kind: choices.kind,
            name: name.to_owned(),
            topic: (!topic.is_empty()).then(|| topic.to_owned()),
            alias_localpart,
            access: choices.access,
            listed: choices.listed,
            encrypted: choices.encrypted,
            suggested: choices.suggested && parent.is_some(),
            parent,
            user_id: context.user_id.clone(),
            server_name: server.to_string(),
            room_version: context.room_version.clone(),
        })
    }

    pub fn kind(&self) -> CreationKind {
        self.kind
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn parent_id(&self) -> Option<&str> {
        self.parent.as_ref().map(|parent| parent.space_id.as_str())
    }
    /// The full address, such as `#team:example.org`.
    pub fn alias(&self) -> Option<String> {
        let localpart = self.alias_localpart.as_ref()?;
        Some(format!("#{localpart}:{}", self.server_name))
    }

    /// The settings this plan applies, one per line, as plain text.
    pub fn preview(&self) -> Vec<PreviewLine> {
        let line = |label, value: String| PreviewLine { label, value };
        let noun = match self.kind {
            CreationKind::Room => "room",
            CreationKind::Space => "Space",
        };
        let mut lines = vec![
            line(
                "Creates",
                format!(
                    "A {noun} named “{}”",
                    plain_text(&self.name, MAX_LABEL_CHARS)
                ),
            ),
            line(
                "On",
                format!(
                    "{}, as {}",
                    self.server_name,
                    plain_text(&self.user_id, MAX_LABEL_CHARS)
                ),
            ),
            line(
                "Who can join",
                match self.access {
                    CreationAccess::InviteOnly => "Only people you invite".to_owned(),
                    CreationAccess::SpaceMembers => format!(
                        "Members of “{}”, and people you invite",
                        self.parent
                            .as_ref()
                            .map_or("", |parent| parent.name.as_str())
                    ),
                    CreationAccess::Public => "Anyone with the address or a link".to_owned(),
                },
            ),
            line(
                "Directory",
                if self.listed {
                    format!("Listed in {}'s public room directory", self.server_name)
                } else {
                    "Not listed".to_owned()
                },
            ),
            line("Address", self.alias().unwrap_or_else(|| "None".to_owned())),
        ];
        lines.push(line(
            "Messages",
            match (self.kind, self.encrypted) {
                (CreationKind::Space, _) => "A Space holds no messages, so nothing is encrypted",
                (CreationKind::Room, true) => {
                    "End-to-end encrypted from the first message; this cannot be turned off later"
                }
                (CreationKind::Room, false) => {
                    "Not encrypted by Morpheus; your homeserver may still turn encryption on"
                }
            }
            .to_owned(),
        ));
        lines.push(line(
            "History",
            "Members can read earlier messages they can decrypt".to_owned(),
        ));
        lines.push(line("Guests", "Not allowed".to_owned()));
        lines.push(line(
            "Placement",
            match &self.parent {
                None => "Home, not in a Space".to_owned(),
                Some(parent) => format!(
                    "Inside “{}”{}",
                    parent.name,
                    if self.suggested {
                        ", suggested to its members"
                    } else {
                        ""
                    }
                ),
            },
        ));
        lines.push(line(
            "Room version",
            match &self.room_version {
                Some(version) => format!("{version}, your homeserver's default"),
                None => "Your homeserver's default".to_owned(),
            },
        ));
        if let Some(topic) = &self.topic {
            lines.push(line("Topic", plain_text(topic, MAX_LABEL_CHARS * 2)));
        }
        lines
    }

    /// The create request, with every applied setting stated explicitly.
    pub(crate) fn request(&self) -> Result<create_room::v3::Request, CreationError> {
        let invalid = |_| CreationError::Unavailable;
        let server = ServerName::parse(&self.server_name).map_err(invalid)?;
        let mut request = create_room::v3::Request::new();
        request.name = Some(self.name.clone());
        request.topic = self.topic.clone();
        request.room_alias_name = self.alias_localpart.clone();
        request.visibility = if self.listed {
            Visibility::Public
        } else {
            Visibility::Private
        };
        request.preset = Some(match self.access {
            CreationAccess::Public => create_room::v3::RoomPreset::PublicChat,
            _ => create_room::v3::RoomPreset::PrivateChat,
        });
        if self.kind == CreationKind::Space {
            let mut creation = create_room::v3::CreationContent::new();
            creation.room_type = Some(RoomType::Space);
            let creation = Raw::new(&creation).map_err(|_| CreationError::Unavailable)?;
            request.creation_content = Some(creation);
        }
        let mut state = Vec::new();
        let join_rule = match self.access {
            CreationAccess::InviteOnly => RoomJoinRulesEventContent::new(JoinRule::Invite),
            CreationAccess::Public => RoomJoinRulesEventContent::new(JoinRule::Public),
            CreationAccess::SpaceMembers => {
                let parent = self
                    .parent
                    .as_ref()
                    .ok_or(CreationError::AccessUnavailable)?;
                let space = RoomId::parse(&parent.space_id).map_err(invalid)?;
                RoomJoinRulesEventContent::restricted(vec![AllowRule::room_membership(space)])
            }
        };
        state.push(InitialStateEvent::with_empty_state_key(join_rule).to_raw_any());
        let history = RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared);
        state.push(InitialStateEvent::with_empty_state_key(history).to_raw_any());
        let guests = RoomGuestAccessEventContent::new(GuestAccess::Forbidden);
        state.push(InitialStateEvent::with_empty_state_key(guests).to_raw_any());
        // Installed with the room, before anyone can join or send.
        if self.kind == CreationKind::Room && self.encrypted {
            let encryption = RoomEncryptionEventContent::with_recommended_defaults();
            state.push(InitialStateEvent::with_empty_state_key(encryption).to_raw_any());
        }
        if let Some(parent) = &self.parent {
            let space = RoomId::parse(&parent.space_id).map_err(invalid)?;
            let mut content = SpaceParentEventContent::new(vec![server.clone()]);
            content.canonical = true;
            state.push(InitialStateEvent::new(space, content).to_raw_any());
        }
        request.initial_state = state;
        Ok(request)
    }
}

/// The localpart of an address on `server`, or `None` for no address.
fn alias_localpart(input: &str, server: &ServerName) -> Result<Option<String>, CreationError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let input = input.strip_prefix('#').unwrap_or(input);
    let localpart = match input.split_once(':') {
        Some((localpart, host)) if host == server.as_str() => localpart,
        Some(_) => return Err(CreationError::AliasServer),
        None => input,
    };
    let valid = !localpart.is_empty()
        && localpart.len() <= MAX_ALIAS_LOCALPART_BYTES
        && localpart.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._=-".contains(&byte)
        });
    if !valid {
        return Err(CreationError::AliasInvalid);
    }
    RoomAliasId::parse(format!("#{localpart}:{server}"))
        .map_err(|_| CreationError::AliasInvalid)?;
    Ok(Some(localpart.to_owned()))
}

/// A mutation is one attempt on the wire: no HTTP retries and no automatic
/// token refresh and replay (`disable_auth_refresh`), within `reply_config`'s
/// bounds.
fn mutation_config(timeout: Duration) -> RequestConfig {
    reply_config(timeout).disable_auth_refresh()
}

fn reply_config(timeout: Duration) -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(timeout)
        .max_response_bytes(MAX_REPLY_BYTES)
}

/// A definite 4xx refusal created nothing; anything else may have.
pub(crate) fn create_failure(error: &HttpError) -> CreationError {
    let Some(api) = error.as_client_api_error() else {
        return CreationError::OutcomeUnknown;
    };
    if !api.status_code.is_client_error() {
        return CreationError::OutcomeUnknown;
    }
    match api.error_kind() {
        // Without the refresh and replay, an expired token returns this 401
        // as is. It is treated as unconfirmed, needing an explicit new try.
        Some(ErrorKind::UnknownToken { .. }) => CreationError::OutcomeUnknown,
        Some(ErrorKind::RoomInUse) => CreationError::AliasTaken,
        Some(ErrorKind::LimitExceeded(_)) => CreationError::RateLimited,
        Some(ErrorKind::Forbidden) => CreationError::Forbidden,
        Some(ErrorKind::UnsupportedRoomVersion) => CreationError::Unsupported,
        _ => CreationError::Rejected,
    }
}

pub(crate) fn link_failure(error: &HttpError) -> LinkFailure {
    let Some(api) = error.as_client_api_error() else {
        return LinkFailure::OutcomeUnknown;
    };
    if !api.status_code.is_client_error() {
        return LinkFailure::OutcomeUnknown;
    }
    match api.error_kind() {
        Some(ErrorKind::UnknownToken { .. }) => LinkFailure::OutcomeUnknown,
        Some(ErrorKind::LimitExceeded(_)) => LinkFailure::RateLimited,
        Some(ErrorKind::Forbidden) => LinkFailure::NotAllowed,
        _ => LinkFailure::Rejected,
    }
}

impl AccountManager {
    /// What the selected account may create, and whether it may place a
    /// room in `parent`. Reads only: the homeserver's capabilities and the
    /// Space's create and power-level state, each without SDK retries.
    pub async fn creation_context_for(
        &self,
        selection: &AccountSelection,
        parent: Option<&str>,
    ) -> Result<CreationContext, CreationError> {
        let client = self.creation_client(selection).await?;
        let own = client
            .user_id()
            .ok_or(CreationError::Unavailable)?
            .to_owned();
        let request = get_capabilities::v3::Request::new();
        let capabilities = selection
            .guard(async {
                client
                    .send(request)
                    .with_request_config(reply_config(CONTEXT_TIMEOUT))
                    .await
            })
            .await
            .ok_or(CreationError::AccountChanged)?;
        // Without capabilities, the default version is unknown and only the
        // choices every room version supports are offered.
        let version = capabilities
            .ok()
            .map(|response| response.capabilities.room_versions.default);
        let parent = match parent {
            None => None,
            Some(space_id) => Some(
                self.parent_context(selection, &client, &own, space_id)
                    .await?,
            ),
        };
        if !self.owns_selection(selection) {
            return Err(CreationError::AccountChanged);
        }
        Ok(CreationContext {
            user_id: own.to_string(),
            server_name: own.server_name().to_string(),
            space_members_supported: version
                .as_ref()
                .is_some_and(room_settings::supports_restricted),
            room_version: version.as_ref().map(RoomVersionId::to_string),
            parent,
        })
    }

    /// Create what `plan` describes, with one create request (no HTTP retries
    /// and no token-refresh replay, raced against revocation), and
    /// return the room the homeserver confirmed. Nothing else happens here:
    /// listing it in its Space (`place_created_room_for`), waiting for it to
    /// sync and opening it come after, and cannot change this result.
    /// `admit` is the caller's own check (the app's security gate at the
    /// click), asked with the exact selection right before the hand-off, as
    /// `join_room_for` does: refused, nothing is sent.
    pub async fn create_room_for(
        &self,
        selection: &AccountSelection,
        plan: &CreationPlan,
        admit: impl Fn() -> bool,
    ) -> Result<CreatedRoom, CreationError> {
        let client = self.creation_client(selection).await?;
        let own = client
            .user_id()
            .ok_or(CreationError::Unavailable)?
            .to_owned();
        if own.as_str() != plan.user_id {
            return Err(CreationError::ContextChanged);
        }
        if let Some(space_id) = plan.parent_id() {
            let space = RoomId::parse(space_id).map_err(|_| CreationError::ContextChanged)?;
            let allowed = selection
                .guard(may_link(&client, &own, &space))
                .await
                .ok_or(CreationError::AccountChanged)?;
            match allowed {
                Ok(true) => {}
                Ok(false) => return Err(CreationError::PlacementNotAllowed),
                Err(_) => return Err(CreationError::Unavailable),
            }
        }
        let request = plan.request()?;
        if !self.owns_selection(selection) {
            return Err(CreationError::AccountChanged);
        }
        if !admit() {
            return Err(CreationError::SecurityBusy);
        }
        // Admitted: from here a cancellation may follow a send, so it reads
        // as an unknown outcome, never as nothing sent.
        let sending = async {
            client
                .send(request)
                .with_request_config(mutation_config(CREATE_TIMEOUT))
                .await
        };
        let response = race_revocation(selection, sending)
            .await
            .ok_or(CreationError::OutcomeUnknown)?
            .map_err(|error| create_failure(&error))?;
        Ok(CreatedRoom {
            room_id: response.room_id.to_string(),
            kind: plan.kind,
            name: plan.name.clone(),
            parent: plan.parent_id().map(str::to_owned),
            suggested: plan.suggested,
        })
    }

    /// List a created room in the Space it names as its parent, with one
    /// write (no HTTP retries and no token-refresh replay), after
    /// a fresh power-level check, only while `admit` holds at the hand-off
    /// (see `link_created_room_for`). A room without one stays in Home.
    pub async fn place_created_room_for(
        &self,
        selection: &AccountSelection,
        created: &CreatedRoom,
        admit: impl Fn() -> bool,
    ) -> Placement {
        let Some(space_id) = &created.parent else {
            return Placement::Home;
        };
        let (room_id, suggested) = (&created.room_id, created.suggested);
        let linked = self
            .link_created_room_for(selection, space_id, room_id, suggested, admit)
            .await;
        match linked {
            Ok(()) => Placement::Linked {
                space_id: space_id.clone(),
            },
            Err(failure) => Placement::NotLinked {
                space_id: space_id.clone(),
                failure,
            },
        }
    }

    /// Wait, at most `SYNC_WAIT`, for a created room to arrive through the
    /// account's sync. Sends nothing; `false` once the selection ended.
    pub async fn await_created_room_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> bool {
        let Ok(room_id) = RoomId::parse(room_id) else {
            return false;
        };
        let Ok(client) = self.creation_client(selection).await else {
            return false;
        };
        tokio::time::timeout(
            SYNC_WAIT,
            selection.guard(client.await_room_remote_echo(&room_id)),
        )
        .await
        .is_ok_and(|room| room.is_some())
    }

    /// List a created room in its Space again, after the first attempt
    /// failed. The write is idempotent; it is still sent only on request.
    /// `admit` is the caller's own check, asked with the exact selection
    /// right before the write: refused, nothing is sent.
    pub async fn link_created_room_for(
        &self,
        selection: &AccountSelection,
        space_id: &str,
        room_id: &str,
        suggested: bool,
        admit: impl Fn() -> bool,
    ) -> Result<(), LinkFailure> {
        let client = self
            .creation_client(selection)
            .await
            .map_err(|_| LinkFailure::AccountChanged)?;
        let own = client.user_id().ok_or(LinkFailure::Unavailable)?.to_owned();
        let space = RoomId::parse(space_id).map_err(|_| LinkFailure::Rejected)?;
        let child = RoomId::parse(room_id).map_err(|_| LinkFailure::Rejected)?;
        let server = own.server_name().to_owned();
        if !self.owns_selection(selection) {
            return Err(LinkFailure::AccountChanged);
        }
        client
            .get_room(&space)
            .filter(|room| room.state() == RoomState::Joined)
            .ok_or(LinkFailure::NotJoined)?;
        // The fresh permission read, outside the write's race: a revocation
        // during it sends nothing.
        let allowed = selection
            .guard(may_link(&client, &own, &space))
            .await
            .ok_or(LinkFailure::AccountChanged)?;
        match allowed {
            Ok(true) => {}
            Ok(false) => return Err(LinkFailure::NotAllowed),
            Err(room_settings::RoomSettingsError::NotJoined) => return Err(LinkFailure::NotJoined),
            Err(_) => return Err(LinkFailure::Unavailable),
        }
        // The PUT handoff: the exact selection and the caller's admission
        // again, right after the read.
        if !self.owns_selection(selection) {
            return Err(LinkFailure::AccountChanged);
        }
        if !admit() {
            return Err(LinkFailure::SecurityBusy);
        }
        // Dropped on revocation, like the create; it may have been sent.
        race_revocation(
            selection,
            put_child(&client, &space, &child, &server, suggested),
        )
        .await
        .unwrap_or(Err(LinkFailure::OutcomeUnknown))
    }

    async fn creation_client(&self, selection: &AccountSelection) -> Result<Client, CreationError> {
        self.hierarchy_client_for(selection)
            .await
            .map_err(|error| match error {
                crate::MatrixCoreError::SelectedAccountChanged => CreationError::AccountChanged,
                _ => CreationError::Unavailable,
            })
    }

    async fn parent_context(
        &self,
        selection: &AccountSelection,
        client: &Client,
        own: &OwnedUserId,
        space_id: &str,
    ) -> Result<ParentContext, CreationError> {
        let space = RoomId::parse(space_id).map_err(|_| CreationError::ContextChanged)?;
        let room = client
            .get_room(&space)
            .filter(|room| room.state() == RoomState::Joined && room.is_space())
            .ok_or(CreationError::ContextChanged)?;
        let name = parent_label(room.name().as_deref(), space.as_str());
        let can_link = selection
            .guard(may_link(client, own, &space))
            .await
            .ok_or(CreationError::AccountChanged)?
            .unwrap_or(false);
        Ok(ParentContext {
            space_id: space.to_string(),
            name,
            can_link,
        })
    }
}

/// The Space's name for display: its own name as bounded plain text, or,
/// when it has none (absent, empty, or only control, formatting or
/// whitespace characters), its exact room ID put through the same bounded
/// plain-text policy. An accepted room ID may hold control and bidi
/// characters and be of any length (compat-arbitrary-length-ids), so it is
/// never shown raw. Display only: ParentContext::space_id, and every
/// lookup, permission check, request and action, keeps the exact ID.
pub(crate) fn parent_label(name: Option<&str>, space_id: &str) -> String {
    name.map(|name| plain_text(name, MAX_LABEL_CHARS))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| plain_text(space_id, MAX_LABEL_CHARS))
}

/// Whether `own` may list rooms in the joined Space, from fresh state.
async fn may_link(
    client: &Client,
    own: &OwnedUserId,
    space: &RoomId,
) -> Result<bool, room_settings::RoomSettingsError> {
    let room = client
        .get_room(space)
        .filter(|room| room.is_space())
        .ok_or(room_settings::RoomSettingsError::NotJoined)?;
    let check = room_settings::can_send_state(&room, own, StateEventType::SpaceChild);
    tokio::time::timeout(CONTEXT_TIMEOUT, check)
        .await
        .unwrap_or(Err(room_settings::RoomSettingsError::TimedOut))
}

/// Write the Space's `m.space.child` for `child` as one attempt
/// (`mutation_config`). The permission was checked just before, outside the
/// race (`link_created_room_for`).
async fn put_child(
    client: &Client,
    space: &RoomId,
    child: &RoomId,
    server: &OwnedServerName,
    suggested: bool,
) -> Result<(), LinkFailure> {
    let mut content = SpaceChildEventContent::new(vec![server.clone()]);
    content.suggested = suggested;
    let child: OwnedRoomId = child.to_owned();
    let request = send_state_event::v3::Request::new(space.to_owned(), &child, &content)
        .map_err(|_| LinkFailure::Unavailable)?;
    client
        .send(request)
        .with_request_config(mutation_config(LINK_TIMEOUT))
        .await
        .map(|_| ())
        .map_err(|error| link_failure(&error))
}

#[cfg(test)]
mod tests;
