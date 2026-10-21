//! A joined room's addresses for one exact selected account:
//! - the aliases the account's own homeserver keeps for the room
//!   (`GET /rooms/{roomId}/aliases`, local aliases only);
//! - the room's published addresses, `m.room.canonical_alias`: one main
//!   address and alternatives, which may name any server.
//!
//! A room alias is a Matrix ID (`#localpart:server`), never a URL. Three
//! things change separately, each one request with its own outcome, never
//! retried, never rolled back, never promised together:
//! - adding an alias on the account's own homeserver
//!   (`PUT /directory/room/{alias}`);
//! - removing one there (`DELETE /directory/room/{alias}`), only after a
//!   fresh lookup shows it still points to this room. That lookup is not a
//!   compare-and-swap: the alias could change between it and the delete;
//! - the published addresses, edited from the exact stored text (unknown
//!   fields kept byte for byte), refused when the stored content is not the
//!   reviewed one or its schema is not one Morpheus can edit.
//! Aliases on another server are never created or deleted here; they can
//! only be published or unpublished.
//!
//! The parent module's contracts are shared, not copied: its bounded reads
//! (`state_and_raw`, `state`, `read_config`), its write config and refusal
//! categories, its lossless content editor, its form cancellation and the
//! selection guard. Every wait races the form's cancellation, then the
//! selection. The final checks (form open, exact selection current, room
//! joined, the app's admission) run after the last await, right before the
//! hand-off; before it, a stop sends nothing. After it, a stop, a timeout
//! or an answer other than a 4xx is `Unknown`. A fresh read shows observed
//! current state, not proof that an unanswered request finished. The app
//! retains that uncertainty independently of the form.

use std::sync::{Arc, RwLock, atomic::Ordering};

use matrix_sdk::{
    Room,
    ruma::{
        OwnedRoomAliasId, OwnedServerName, OwnedUserId, RoomAliasId,
        api::{
            client::{
                alias::{create_alias, delete_alias, get_alias},
                room::aliases,
            },
            error::ErrorKind,
        },
        events::{
            StateEventType,
            room::{
                canonical_alias::RoomCanonicalAliasEventContent,
                create::RoomCreateEventContent,
                member::{MembershipState, RoomMemberEventContent},
                power_levels::{
                    RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
                },
            },
        },
    },
};
use serde_json::value::RawValue;
use tokio::sync::Mutex;

use super::{
    APPLIED, Admission, Dispatch, READ_TIMEOUT, REFUSED, RoomSettingsCancellation,
    RoomSettingsError, SENT, StateEnvelope, WRITE_TIMEOUT, content, content_fields, edited_content,
    ensure_joined, ensure_scope, guarded, missing, parse_raw_state, parse_state, read_config,
    refusal, request_failure, write_config,
};
use crate::{AccountManager, AccountSelection, account_room};

#[cfg(test)]
mod tests;

/// Addresses shown at most; a longer list says it was cut.
pub const MAX_ROOM_ADDRESSES: usize = 100;
/// Alternative published addresses Morpheus writes at most.
pub const MAX_ALTERNATIVE_ADDRESSES: usize = 20;
/// Characters of a new address's local part at most (a Matrix ID is at most
/// 255 bytes in all).
pub const MAX_ALIAS_LOCALPART_CHARS: usize = 64;

/// Why a room-address action did not complete, as a fixed sentence: the
/// homeserver's own text is never kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomAddressError {
    #[error("This address form is closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Join this room before changing its addresses")]
    NotJoined,
    #[error("Room addresses are unavailable; refresh before retrying")]
    Unavailable,
    #[error("The homeserver is limiting requests; wait and refresh before retrying")]
    RateLimited,
    #[error("The operation timed out; refresh before retrying")]
    TimedOut,
    #[error("The published addresses use data Morpheus cannot edit safely")]
    Unsupported,
    #[error("Your current room permissions do not allow this change")]
    PermissionDenied,
    #[error("The published addresses changed on the server; refresh before saving")]
    BaselineChanged,
    #[error("Enter a local part of letters, digits and . _ = - / + (at most 64)")]
    InvalidAlias,
    #[error("Only addresses on your own homeserver can be added or removed here")]
    NotLocal,
    #[error("That address is already in use; nothing was changed")]
    AliasTaken,
    #[error("That address no longer points to this room; nothing was changed")]
    MappingChanged,
    #[error("That address does not exist any more; nothing was changed")]
    NotFound,
    #[error("Finish the account or security step in progress first; nothing was sent")]
    SecurityBusy,
    #[error("The homeserver refused this change; nothing was changed")]
    Refused,
    #[error("This sign-in needs renewing; the homeserver did not apply the change")]
    SignInExpired,
    #[error("No answer came back; the change may or may not have been made. Refresh to see")]
    Unknown,
    #[error("The change was made, but reading the addresses back failed; refresh to see them")]
    SavedNotRead,
}

impl From<RoomSettingsError> for RoomAddressError {
    fn from(error: RoomSettingsError) -> Self {
        match error {
            RoomSettingsError::Cancelled => Self::Cancelled,
            RoomSettingsError::AccountChanged => Self::AccountChanged,
            RoomSettingsError::NotJoined => Self::NotJoined,
            RoomSettingsError::RateLimited => Self::RateLimited,
            RoomSettingsError::TimedOut => Self::TimedOut,
            RoomSettingsError::InvalidState => Self::Unsupported,
            RoomSettingsError::PermissionDenied => Self::PermissionDenied,
            RoomSettingsError::BaselineChanged => Self::BaselineChanged,
            RoomSettingsError::SecurityBusy => Self::SecurityBusy,
            RoomSettingsError::Refused => Self::Refused,
            RoomSettingsError::SignInExpired => Self::SignInExpired,
            RoomSettingsError::Unknown => Self::Unknown,
            RoomSettingsError::SavedNotRead => Self::SavedNotRead,
            _ => Self::Unavailable,
        }
    }
}

impl RoomAddressError {
    /// Whether an outcome needs retained reconciliation, not an automatic retry.
    pub fn may_have_changed(self) -> bool {
        matches!(self, Self::Unknown | Self::SavedNotRead)
    }
}

type Result<T> = std::result::Result<T, RoomAddressError>;

/// One known address of the room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomAddress {
    /// The alias, `#localpart:server` (parsed).
    pub alias: String,
    pub server: String,
    /// On the account's own homeserver: the only ones added or removed here.
    pub local: bool,
    /// The account's homeserver lists it for this room.
    pub listed: bool,
    /// The room's main published address.
    pub main: bool,
    /// One of its alternative published addresses.
    pub alternative: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomAddressesSnapshot {
    /// The main address first, then alternatives, then the rest, by alias.
    pub addresses: Vec<RoomAddress>,
    /// More addresses than MAX_ROOM_ADDRESSES were known.
    pub truncated: bool,
    /// The homeserver answered for its own list; otherwise only published
    /// addresses are known.
    pub local_list_known: bool,
    /// The account's own homeserver, where new addresses are added.
    pub own_server: String,
    /// The account may change the published addresses (fresh power levels).
    pub can_publish: bool,
    /// The stored published addresses use a schema Morpheus can edit.
    pub publish_editable: bool,
}

/// One edit of the published addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishEdit {
    SetMain(String),
    ClearMain,
    AddAlternative(String),
    RemoveAlternative(String),
}

/// The published addresses as stored: the main one and alternatives, and
/// whether every stored field of theirs is one Morpheus understands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Published {
    pub(crate) main: Option<String>,
    pub(crate) alternatives: Vec<String>,
    pub(crate) supported: bool,
}

#[derive(Clone)]
struct Loaded {
    snapshot: RoomAddressesSnapshot,
    /// The stored canonical content's exact text (`None`: no event).
    canonical_raw: Option<String>,
}

pub struct RoomAddressesSession {
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
    room: Room,
    own_user: OwnedUserId,
    own_server: OwnedServerName,
    loaded: RwLock<Loaded>,
    operation: Mutex<()>,
    admission: Admission,
}

impl AccountManager {
    /// Open the address form of a joined room for the exact `selection`.
    /// Admission is mandatory before the first read and every later handoff;
    /// app callers capture their view/navigation/security lifetime at the click.
    pub async fn open_room_addresses_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: RoomSettingsCancellation,
        admission: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<RoomAddressesSession> {
        ensure_scope(selection, &cancellation)?;
        if !admission() {
            return Err(RoomAddressError::SecurityBusy);
        }
        let opened = guarded(selection, &cancellation, READ_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self
                .account_selection(selection.account_id())
                .ok_or(RoomSettingsError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(RoomSettingsError::AccountChanged);
            }
            ensure_scope(selection, &cancellation)?;
            if !admission() {
                return Err(RoomSettingsError::SecurityBusy);
            }
            let room = account_room(&state, selection.account_id(), room_id)
                .map_err(|_| RoomSettingsError::Unavailable)?;
            ensure_joined(&room)?;
            let own_user = room
                .client()
                .user_id()
                .ok_or(RoomSettingsError::Unavailable)?
                .to_owned();
            Ok((room, own_user))
        })
        .await;
        let (room, own_user) = opened?;
        let own_server = own_user.server_name().to_owned();
        let check = || {
            ensure_scope(selection, &cancellation)?;
            ensure_joined(&room)?;
            if !admission() {
                return Err(RoomAddressError::SecurityBusy);
            }
            Ok(())
        };
        let read = guarded(selection, &cancellation, READ_TIMEOUT, async {
            Ok(read_addresses(&room, &own_user, &check).await)
        })
        .await?;
        let loaded = read?;
        ensure_scope(selection, &cancellation)?;
        Ok(RoomAddressesSession {
            selection: selection.clone(),
            cancellation,
            room,
            own_user,
            own_server,
            loaded: RwLock::new(loaded),
            operation: Mutex::new(()),
            admission,
        })
    }
}

impl RoomAddressesSession {
    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }

    pub fn room_id(&self) -> &str {
        self.room.room_id().as_str()
    }

    pub fn snapshot(&self) -> RoomAddressesSnapshot {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .clone()
    }

    fn check(&self) -> Result<()> {
        ensure_scope(&self.selection, &self.cancellation)?;
        ensure_joined(&self.room)?;
        if !(self.admission)() {
            return Err(RoomAddressError::SecurityBusy);
        }
        Ok(())
    }

    /// The checks right before a request is handed over, after the last
    /// await: the form is open, the exact selection is current, the room is
    /// joined on this device and the app admits a change.
    fn admit(&self) -> Result<()> {
        self.check()
    }

    /// `work` under the form's one operation at a time, its cancellation,
    /// the selection and `limit`. Once a request was handed over, a stop or
    /// a failure reads as what it may have done.
    async fn run<T>(
        &self,
        limit: std::time::Duration,
        dispatch: &Dispatch,
        work: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let ran = guarded(&self.selection, &self.cancellation, limit, async {
            let _operation = self.operation.lock().await;
            Ok(async {
                self.check()?;
                work.await
            }
            .await)
        })
        .await;
        let result = match ran {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        };
        result.map_err(|error| outcome(dispatch.state.load(Ordering::Acquire), error))
    }

    /// Read the room's addresses again.
    pub async fn refresh(&self) -> Result<RoomAddressesSnapshot> {
        let dispatch = Dispatch::default();
        self.run(READ_TIMEOUT, &dispatch, async {
            let fresh = read_addresses(&self.room, &self.own_user, &|| self.check()).await?;
            self.publish(fresh)
        })
        .await
    }

    fn publish(&self, fresh: Loaded) -> Result<RoomAddressesSnapshot> {
        self.check()?;
        let snapshot = fresh.snapshot.clone();
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = fresh;
        Ok(snapshot)
    }

    /// The read after a change: its failure says the change was made.
    async fn read_back(&self) -> Result<RoomAddressesSnapshot> {
        self.check()?;
        match read_addresses(&self.room, &self.own_user, &|| self.check()).await {
            Ok(fresh) => self.publish(fresh),
            Err(_) => Err(RoomAddressError::SavedNotRead),
        }
    }

    /// Add `#localpart:<own server>` for this room on the account's
    /// homeserver: one request. It does not publish it.
    pub async fn add_address(&self, localpart: &str) -> Result<RoomAddressesSnapshot> {
        let alias = local_alias(localpart, &self.own_server)?;
        let dispatch = Dispatch::default();
        self.run(WRITE_TIMEOUT, &dispatch, async {
            fresh_member(&self.room, &self.own_user, &|| self.check()).await?;
            let request = create_alias::v3::Request::new(alias, self.room.room_id().to_owned());
            let client = self.room.client();
            self.admit()?;
            dispatch.state.store(SENT, Ordering::Release);
            let sent = client
                .send(request)
                .with_request_config(write_config())
                .await;
            settle(&dispatch, sent.map(|_| ()), Change::Add)?;
            self.read_back().await
        })
        .await
    }

    /// Remove one of this room's addresses from the account's homeserver:
    /// only an address on that server, only after a fresh lookup shows it
    /// still points to this room (not a compare-and-swap), one request. A
    /// published address stays published until that is changed separately.
    pub async fn remove_address(&self, alias: &str) -> Result<RoomAddressesSnapshot> {
        let alias = parse_alias(alias)?;
        if alias.server_name() != &*self.own_server {
            return Err(RoomAddressError::NotLocal);
        }
        let dispatch = Dispatch::default();
        self.run(WRITE_TIMEOUT, &dispatch, async {
            fresh_member(&self.room, &self.own_user, &|| self.check()).await?;
            self.points_here(&alias).await?;
            let request = delete_alias::v3::Request::new(alias);
            let client = self.room.client();
            self.admit()?;
            dispatch.state.store(SENT, Ordering::Release);
            let sent = client
                .send(request)
                .with_request_config(write_config())
                .await;
            settle(&dispatch, sent.map(|_| ()), Change::Remove)?;
            self.read_back().await
        })
        .await
    }

    /// Edit the published addresses: from the exact stored text, which must
    /// still be the reviewed one, with every field Morpheus does not edit
    /// kept byte for byte. A newly published address must point to this
    /// room now. One state request.
    pub async fn edit_published(&self, edit: PublishEdit) -> Result<RoomAddressesSnapshot> {
        let dispatch = Dispatch::default();
        self.run(WRITE_TIMEOUT, &dispatch, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .canonical_raw
                .clone();
            fresh_member(&self.room, &self.own_user, &|| self.check()).await?;
            let (_, stored) =
                address_state_and_raw(&self.room, StateEventType::RoomCanonicalAlias, "", &|| {
                    self.check()
                })
                .await?;
            if stored != baseline {
                return Err(RoomAddressError::BaselineChanged);
            }
            let published = published(stored.as_deref());
            if !published.supported {
                return Err(RoomAddressError::Unsupported);
            }
            if !may_publish(&self.room, &self.own_user, &|| self.check()).await? {
                return Err(RoomAddressError::PermissionDenied);
            }
            let (main, alternatives, added) = edited(&published, &edit)?;
            if let Some(added) = &added {
                self.points_here(&parse_alias(added)?).await?;
            }
            let mut typed = RoomCanonicalAliasEventContent::new();
            typed.alias = main.as_deref().map(parse_alias).transpose()?;
            typed.alt_aliases = alternatives
                .iter()
                .map(|alias| parse_alias(alias))
                .collect::<Result<Vec<_>>>()?;
            let content = edited_content(stored.as_deref(), &["alias", "alt_aliases"], typed)?;
            self.send_published(&dispatch, content).await?;
            self.read_back().await
        })
        .await
    }

    /// Whether `alias` points to this room now (a fresh lookup through the
    /// account's homeserver).
    async fn points_here(&self, alias: &RoomAliasId) -> Result<()> {
        self.check()?;
        let request = get_alias::v3::Request::new(alias.to_owned());
        let answer = self
            .room
            .client()
            .send(request)
            .with_request_config(address_read_config())
            .await;
        self.check()?;
        match answer {
            Ok(found) if found.room_id.as_str() == self.room.room_id().as_str() => Ok(()),
            Ok(_) => Err(RoomAddressError::MappingChanged),
            Err(error) if missing(&error) => Err(RoomAddressError::NotFound),
            Err(error) => Err(read_failure(error)),
        }
    }

    async fn send_published(&self, dispatch: &Dispatch, content: Box<RawValue>) -> Result<()> {
        let room_id = self.room.room_id().to_owned();
        let content = matrix_sdk::ruma::serde::Raw::from_json(content);
        let kind = StateEventType::RoomCanonicalAlias;
        let request = matrix_sdk::ruma::api::client::state::send_state_event::v3::Request::new_raw(
            room_id,
            kind,
            String::new(),
            content,
        );
        let client = self.room.client();
        self.admit()?;
        dispatch.state.store(SENT, Ordering::Release);
        let sent = client
            .send(request)
            .with_request_config(write_config())
            .await;
        settle(dispatch, sent.map(|_| ()), Change::Publish)
    }
}

/// Which change an answer belongs to, for its 4xx categories.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    Add,
    Remove,
    Publish,
}

/// Record one request's answer: success applied it, a 4xx refused it (a
/// fixed category), anything else may have applied it.
fn settle(
    dispatch: &Dispatch,
    sent: std::result::Result<(), matrix_sdk::HttpError>,
    change: Change,
) -> Result<()> {
    match sent {
        Ok(()) => {
            dispatch.state.store(APPLIED, Ordering::Release);
            Ok(())
        }
        Err(error) => match answer_refusal(&error, change) {
            Some(refused) => {
                dispatch.state.store(REFUSED, Ordering::Release);
                Err(refused)
            }
            None => Err(RoomAddressError::Unknown),
        },
    }
}

/// A definite 4xx as this change's fixed category; `None` when it may have
/// applied.
fn answer_refusal(error: &matrix_sdk::HttpError, change: Change) -> Option<RoomAddressError> {
    let answer = error.as_client_api_error()?;
    let status = answer.status_code.as_u16();
    let base = refusal(Some(answer))?;
    Some(match (change, status) {
        // An alias that exists already, for this or another room.
        (Change::Add, 409) => RoomAddressError::AliasTaken,
        (Change::Remove, 404) => RoomAddressError::NotFound,
        // A published address that does not point to this room.
        (Change::Publish, 400) if answer.error_kind() == Some(&ErrorKind::BadAlias) => {
            RoomAddressError::MappingChanged
        }
        _ => base.into(),
    })
}

/// What a failure after the hand-off reads as: the dispatch state wins over
/// the error, so a stop is never "nothing was sent" once a request left.
pub(crate) fn outcome(state: u8, error: RoomAddressError) -> RoomAddressError {
    match state {
        SENT => RoomAddressError::Unknown,
        APPLIED if error != RoomAddressError::SavedNotRead => RoomAddressError::SavedNotRead,
        _ => error,
    }
}

/// The address a typed local part makes on the account's own server, as the
/// review shows it and add_address creates it.
pub fn new_local_address(
    localpart: &str,
    own_server: &str,
) -> std::result::Result<String, RoomAddressError> {
    let server =
        OwnedServerName::try_from(own_server).map_err(|_| RoomAddressError::InvalidAlias)?;
    local_alias(localpart, &server).map(|alias| alias.to_string())
}

/// `#localpart:server` from a typed local part on the account's server.
pub(crate) fn local_alias(localpart: &str, server: &OwnedServerName) -> Result<OwnedRoomAliasId> {
    let localpart = localpart.trim().trim_start_matches('#');
    let valid = !localpart.is_empty()
        && localpart.chars().count() <= MAX_ALIAS_LOCALPART_CHARS
        && localpart
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "._=-/+".contains(ch));
    if !valid {
        return Err(RoomAddressError::InvalidAlias);
    }
    RoomAliasId::parse(format!("#{}:{server}", localpart.to_ascii_lowercase()))
        .map_err(|_| RoomAddressError::InvalidAlias)
}

fn parse_alias(alias: &str) -> Result<OwnedRoomAliasId> {
    RoomAliasId::parse(alias).map_err(|_| RoomAddressError::InvalidAlias)
}

/// The published addresses in the stored text, read field by field from the
/// exact text. Anything else in `alias` or `alt_aliases` than an alias (or
/// none, an empty or null main) leaves them read-only (`supported` false).
pub(crate) fn published(stored: Option<&str>) -> Published {
    let Some(text) = stored else {
        return Published {
            supported: true,
            ..Published::default()
        };
    };
    let Ok(fields) = content_fields(text) else {
        return Published::default();
    };
    let mut parsed = Published {
        supported: true,
        ..Published::default()
    };
    for (name, value) in &fields {
        match name.as_str() {
            "alias" => match serde_json::from_str::<Option<String>>(value.get()) {
                Ok(None) => {}
                Ok(Some(alias)) if alias.is_empty() => {}
                Ok(Some(alias)) if RoomAliasId::parse(&alias).is_ok() => {
                    parsed.main = Some(alias);
                }
                _ => parsed.supported = false,
            },
            "alt_aliases" => match serde_json::from_str::<Vec<String>>(value.get()) {
                Ok(list) if list.iter().all(|alias| RoomAliasId::parse(alias).is_ok()) => {
                    parsed.alternatives = list;
                }
                _ => parsed.supported = false,
            },
            _ => {}
        }
    }
    parsed
}

/// The published addresses after `edit`, and the address it newly
/// publishes (which must point here).
pub(crate) fn edited(
    published: &Published,
    edit: &PublishEdit,
) -> Result<(Option<String>, Vec<String>, Option<String>)> {
    let mut main = published.main.clone();
    let mut alternatives = published.alternatives.clone();
    let mut added = None;
    match edit {
        PublishEdit::SetMain(alias) => {
            let alias = parse_alias(alias)?.to_string();
            // Promotion also rechecks a possibly reassigned alternative.
            added = Some(alias.clone());
            // The previous main stays published, as an alternative.
            if let Some(previous) = main.take().filter(|previous| *previous != alias)
                && !alternatives.contains(&previous)
            {
                alternatives.push(previous);
            }
            alternatives.retain(|other| *other != alias);
            main = Some(alias);
        }
        PublishEdit::ClearMain => main = None,
        PublishEdit::AddAlternative(alias) => {
            let alias = parse_alias(alias)?.to_string();
            if main.as_deref() != Some(alias.as_str()) && !alternatives.contains(&alias) {
                added = Some(alias.clone());
                alternatives.push(alias);
            }
        }
        PublishEdit::RemoveAlternative(alias) => alternatives.retain(|other| other != alias),
    }
    // A list already longer stays editable as long as it does not grow.
    if alternatives.len() > MAX_ALTERNATIVE_ADDRESSES
        && alternatives.len() > published.alternatives.len()
    {
        return Err(RoomAddressError::Unsupported);
    }
    Ok((main, alternatives, added))
}

/// The account is still a joined member (fresh member state).
type Check<'a> = &'a (dyn Fn() -> Result<()> + Send + Sync);

// Feature-local opt-out: the parent helper intentionally keeps its other
// callers unchanged. A 401 must not start an uncapped token refresh/replay.
fn address_read_config() -> matrix_sdk::config::RequestConfig {
    read_config().disable_auth_refresh()
}

fn read_failure(error: matrix_sdk::HttpError) -> RoomAddressError {
    if error
        .as_client_api_error()
        .is_some_and(|answer| matches!(answer.error_kind(), Some(ErrorKind::UnknownToken(_))))
    {
        RoomAddressError::SignInExpired
    } else {
        request_failure(error).into()
    }
}

// Reuse the one reviewed state/raw parsers; only the request config and
// operation admission belong to this feature. No new envelope/parser.
async fn address_state_and_raw(
    room: &Room,
    kind: StateEventType,
    key: &str,
    check: Check<'_>,
) -> Result<(Option<StateEnvelope>, Option<String>)> {
    check()?;
    let mut request =
        matrix_sdk::ruma::api::client::state::get_state_event_for_key::v3::Request::new(
            room.room_id().to_owned(),
            kind.clone(),
            key.into(),
        );
    request.format =
        matrix_sdk::ruma::api::client::state::get_state_event_for_key::v3::StateEventFormat::Event;
    let response = room
        .client()
        .send(request)
        .with_request_config(address_read_config())
        .await;
    check()?;
    match response {
        Ok(answer) => {
            let raw = answer.event_or_content.get();
            let event = parse_state(raw, room.room_id(), &kind, key)?;
            let content = parse_raw_state(raw, room.room_id(), &kind, key)?;
            Ok((Some(event), Some(content.get().to_owned())))
        }
        Err(error) if missing(&error) => Ok((None, None)),
        Err(error) => Err(read_failure(error)),
    }
}

async fn fresh_member(room: &Room, own: &OwnedUserId, check: Check<'_>) -> Result<()> {
    ensure_joined(room)?;
    let member = address_state_and_raw(room, StateEventType::RoomMember, own.as_str(), check)
        .await?
        .0
        .ok_or(RoomAddressError::NotJoined)?;
    let member: RoomMemberEventContent = content(&member)?;
    if member.membership != MembershipState::Join {
        return Err(RoomAddressError::NotJoined);
    }
    Ok(())
}

/// Whether the account may send `m.room.canonical_alias` now, from fresh
/// create and power-level state, with the same creator rules as the room
/// settings' permissions. An unknown room version grants nothing.
async fn may_publish(room: &Room, own: &OwnedUserId, check: Check<'_>) -> Result<bool> {
    let create = address_state_and_raw(room, StateEventType::RoomCreate, "", check)
        .await?
        .0
        .ok_or(RoomAddressError::Unsupported)?;
    let power = address_state_and_raw(room, StateEventType::RoomPowerLevels, "", check)
        .await?
        .0;
    let creation: RoomCreateEventContent = content(&create)?;
    let Some(rules) = creation.room_version.rules() else {
        return Ok(false);
    };
    let mut creators = if rules.authorization.use_room_create_sender {
        vec![create.sender.clone()]
    } else {
        #[allow(deprecated)]
        vec![
            creation
                .creator
                .clone()
                .ok_or(RoomAddressError::Unsupported)?,
        ]
    };
    if rules.authorization.additional_room_creators {
        creators.extend(creation.additional_creators.clone());
    }
    if creators.len() > 64 {
        return Err(RoomAddressError::Unsupported);
    }
    let power = match power {
        Some(event) => {
            RoomPowerLevelsSource::Original(content::<RoomPowerLevelsEventContent>(&event)?)
        }
        None => RoomPowerLevelsSource::None,
    };
    let levels = RoomPowerLevels::new(power, &rules.authorization, creators);
    Ok(levels.user_can_send_state(own, StateEventType::RoomCanonicalAlias))
}

/// A read the homeserver answered with a 4xx.
fn refused_read(error: &matrix_sdk::HttpError) -> bool {
    error
        .as_client_api_error()
        .is_some_and(|answer| answer.status_code.is_client_error())
}

/// Every address of the room, all fresh: the homeserver's own list, the
/// published addresses and the permission to change them.
async fn read_addresses(room: &Room, own: &OwnedUserId, check: Check<'_>) -> Result<Loaded> {
    fresh_member(room, own, check).await?;
    check()?;
    let own_server = own.server_name().to_owned();
    let listed = room
        .client()
        .send(aliases::v3::Request::new(room.room_id().to_owned()))
        .with_request_config(address_read_config())
        .await;
    check()?;
    let (listed, local_list_known) = match listed {
        Ok(answer) => (
            answer
                .aliases
                .into_iter()
                .map(|alias| alias.to_string())
                .collect::<Vec<_>>(),
            true,
        ),
        // A refusal (not allowed to list, or unsupported): only the
        // published addresses are known, and the snapshot says so.
        Err(error)
            if refused_read(&error)
                && !matches!(
                    error.client_api_error_kind(),
                    Some(ErrorKind::UnknownToken(_))
                ) =>
        {
            (Vec::new(), false)
        }
        Err(error) => return Err(read_failure(error)),
    };
    let (_, canonical_raw) =
        address_state_and_raw(room, StateEventType::RoomCanonicalAlias, "", check).await?;
    let published = published(canonical_raw.as_deref());
    let can_publish = may_publish(room, own, check).await?;
    check()?;
    ensure_joined(room)?;
    let (addresses, truncated) = merged(&listed, &published, own_server.as_str());
    Ok(Loaded {
        snapshot: RoomAddressesSnapshot {
            addresses,
            truncated,
            local_list_known,
            own_server: own_server.to_string(),
            can_publish,
            publish_editable: published.supported,
        },
        canonical_raw,
    })
}

/// The homeserver's list and the published addresses as one list: each
/// alias once, the main first, then alternatives, then the rest by alias,
/// at most MAX_ROOM_ADDRESSES. Whatever does not parse as an alias is left
/// out.
pub(crate) fn merged(
    listed: &[String],
    published: &Published,
    own_server: &str,
) -> (Vec<RoomAddress>, bool) {
    let mut addresses: Vec<RoomAddress> = Vec::new();
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let mut add = |alias: &str, listed: bool, main: bool, alternative: bool| {
        let Ok(parsed) = RoomAliasId::parse(alias) else {
            return;
        };
        let alias = parsed.to_string();
        match seen.get(&alias) {
            Some(&index) => {
                let known = &mut addresses[index];
                known.listed |= listed;
                known.main |= main;
                known.alternative |= alternative;
            }
            None => {
                let server = parsed.server_name().to_string();
                seen.insert(alias.clone(), addresses.len());
                addresses.push(RoomAddress {
                    local: server == own_server,
                    server,
                    alias,
                    listed,
                    main,
                    alternative,
                });
            }
        }
    };
    if let Some(main) = &published.main {
        add(main, false, true, false);
    }
    for alias in &published.alternatives {
        add(alias, false, false, true);
    }
    for alias in listed {
        add(alias, true, false, false);
    }
    addresses.sort_by(|a, b| {
        (!a.main, !a.alternative, a.alias.to_lowercase()).cmp(&(
            !b.main,
            !b.alternative,
            b.alias.to_lowercase(),
        ))
    });
    let truncated = addresses.len() > MAX_ROOM_ADDRESSES;
    addresses.truncate(MAX_ROOM_ADDRESSES);
    (addresses, truncated)
}
