//! Who can read a room's history, whether guests may join, and whether the
//! account's homeserver lists the room in its room directory, on the same
//! revocable room-settings session as every other room setting.
//!
//! History visibility and guest access are room state. Each is read with a
//! bounded state read (`access_state`, through the parent's state parsers)
//! and kept as the exact stored text; a change rewrites only its own field
//! and keeps every other byte (`edited_content`), once a fresh read finds
//! that text unchanged and the account's power levels allowing it. Content
//! Morpheus would not rewrite, or a value it does not know, is read-only.
//!
//! The directory listing is not room state but the homeserver's own record:
//! the spec's public `GET /directory/list/room/{roomId}` (no access token)
//! on the bounded read config, changed with one authenticated `PUT`. Without
//! a usable answer the listing is `Unknown` or `Unsupported`, never guessed,
//! and only a definite answer can be changed. Who may change it is up to
//! the homeserver: Morpheus first asks for the room's alias permission, as
//! Synapse does, and the homeserver's refusal stays final. Only the
//! account's own homeserver directory is covered.
//!
//! Every change is one request, handed over at most once after the final
//! checks every room-settings write makes (`admit`): no retry, no
//! authentication refresh and replay, a timeout and a 64 KiB answer. Only a
//! 4xx answer means nothing changed; any other outcome after the hand-off is
//! `Unknown` and is never sent again. A change applies from then on: it does
//! not take back history people could already read, and allowing guests
//! neither creates guest accounts nor gives anyone keys to encrypted
//! messages.
//!
//! Closing the form withdraws a change only before its request is handed
//! over. Once handed over it is left to its own bounds, so its own answer
//! (applied, refused, or none) says what it did, not the closing. An account
//! change still ends the wait, as for every room setting.
//!
//! Every read here, the creation, membership and power-level reads behind
//! each permission check included, also opts out of the SDK's
//! authentication refresh and replay (`access_read_config`), so a refresh
//! cannot add retried or uncapped reads behind them.

use std::future::Future;

use matrix_sdk::config::RequestConfig;
use matrix_sdk::ruma::{
    OwnedRoomId,
    api::client::{
        directory::{get_room_visibility, set_room_visibility},
        room::Visibility,
        state::get_state_event_for_key,
    },
    events::room::{
        guest_access::{GuestAccess, RoomGuestAccessEventContent},
        history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
    },
};

use super::{
    APPLIED, AccountSelection, Dispatch, ErrorKind, HttpError, MembershipState, Ordering,
    OwnedUserId, READ_TIMEOUT, REFUSED, Result, Room, RoomMemberEventContent,
    RoomSettingsCancellation, RoomSettingsError, RoomSettingsSession, SENT, StateEnvelope,
    StateEventType, WRITE_TIMEOUT, content, content_fields, edited_content, ensure_joined,
    ensure_scope, missing, parse_raw_state, parse_state, power_levels, read_config, refusal,
    request_failure, require_baseline, write_config,
};

/// Who can read the messages sent while the setting is in force, from the
/// most private choice to the most open one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryVisibilityChoice {
    /// Members, from when they joined.
    Joined,
    /// Members, from when they were invited.
    Invited,
    /// Members, including messages from before they joined.
    Shared,
    /// Anyone, even without joining.
    WorldReadable,
}
impl HistoryVisibilityChoice {
    fn wire(self) -> HistoryVisibility {
        match self {
            Self::Joined => HistoryVisibility::Joined,
            Self::Invited => HistoryVisibility::Invited,
            Self::Shared => HistoryVisibility::Shared,
            Self::WorldReadable => HistoryVisibility::WorldReadable,
        }
    }
}

/// Whether guest accounts may join the room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestAccessChoice {
    Forbidden,
    CanJoin,
}
impl GuestAccessChoice {
    fn wire(self) -> GuestAccess {
        match self {
            Self::Forbidden => GuestAccess::Forbidden,
            Self::CanJoin => GuestAccess::CanJoin,
        }
    }
}

/// One state setting as the room has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomAccessSetting<T> {
    /// The room has no such event, or (guest access only) one without a
    /// value, as a redacted one is: the protocol default applies.
    Default(T),
    Set(T),
    /// A value Morpheus does not know, or content it would not rewrite:
    /// shown as read-only, never changed.
    Unrecognized,
}
impl<T: Copy> RoomAccessSetting<T> {
    /// The setting in force, when it is one Morpheus can change.
    pub fn value(&self) -> Option<T> {
        match self {
            Self::Default(value) | Self::Set(value) => Some(*value),
            Self::Unrecognized => None,
        }
    }
}

/// Whether the account's homeserver lists the room in its room directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryListing {
    Listed,
    NotListed,
    /// The homeserver does not offer a room directory listing.
    Unsupported,
    /// No usable answer: a failure, a limit, or a value Morpheus does not
    /// know. Refresh to ask again.
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoomAccessPermissions {
    pub history: bool,
    pub guests: bool,
    /// The room's alias permission, which Synapse requires to change a
    /// listing. Other homeservers may decide differently; they answer.
    pub directory: bool,
}

/// One fresh read. Its private baseline ties a change to exactly what was
/// read: the account, the room, each stored text and the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomAccessSnapshot {
    pub history: RoomAccessSetting<HistoryVisibilityChoice>,
    pub guests: RoomAccessSetting<GuestAccessChoice>,
    pub directory: DirectoryListing,
    pub permissions: RoomAccessPermissions,
    baseline: AccessBaseline,
}
impl RoomAccessSnapshot {
    pub fn history_editable(&self) -> bool {
        self.permissions.history && self.history.value().is_some()
    }
    pub fn guests_editable(&self) -> bool {
        self.permissions.guests && self.guests.value().is_some()
    }
    pub fn directory_editable(&self) -> bool {
        self.permissions.directory
            && matches!(
                self.directory,
                DirectoryListing::Listed | DirectoryListing::NotListed
            )
    }
}

/// Each event's content as the exact text the server sent (`None` without
/// the event), as `RawContents` keeps the other settings.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AccessBaseline {
    account: String,
    room: OwnedRoomId,
    history: Option<String>,
    guests: Option<String>,
}

impl RoomSettingsSession {
    /// A fresh read of the room's history visibility, guest access and
    /// directory listing, with the account's permission for each.
    pub async fn load_access(&self) -> Result<RoomAccessSnapshot> {
        self.run(READ_TIMEOUT, async {
            let fresh = self.read_access().await?;
            self.check()?;
            Ok(fresh)
        })
        .await
    }
    /// One history-visibility change from `reviewed`. It applies to messages
    /// sent from then on; what people could already read stays readable.
    pub async fn set_history_visibility(
        &self,
        reviewed: &RoomAccessSnapshot,
        choice: HistoryVisibilityChoice,
    ) -> Result<RoomAccessSnapshot> {
        let dispatch = Dispatch::default();
        self.run_access_write(&dispatch, async {
            let fresh = self.fresh_against(reviewed).await?;
            require_baseline(&reviewed.baseline.history, &fresh.baseline.history)?;
            let Some(current) = fresh.history.value() else {
                return Err(RoomSettingsError::ReadOnly);
            };
            if !fresh.permissions.history {
                return Err(RoomSettingsError::PermissionDenied);
            }
            // Nothing to send; the room already has it.
            if current == choice {
                return Ok(fresh);
            }
            let typed = RoomHistoryVisibilityEventContent::new(choice.wire());
            let stored = fresh.baseline.history.as_deref();
            let content = edited_content(stored, &["history_visibility"], typed)?;
            let kind = StateEventType::RoomHistoryVisibility;
            self.send_state(&dispatch, kind, content).await?;
            self.check()?;
            self.read_access().await
        })
        .await
    }
    /// One guest-access change from `reviewed`. Guests still need a join
    /// rule that admits them and a homeserver that offers guest accounts.
    pub async fn set_guest_access(
        &self,
        reviewed: &RoomAccessSnapshot,
        choice: GuestAccessChoice,
    ) -> Result<RoomAccessSnapshot> {
        let dispatch = Dispatch::default();
        self.run_access_write(&dispatch, async {
            let fresh = self.fresh_against(reviewed).await?;
            require_baseline(&reviewed.baseline.guests, &fresh.baseline.guests)?;
            let Some(current) = fresh.guests.value() else {
                return Err(RoomSettingsError::ReadOnly);
            };
            if !fresh.permissions.guests {
                return Err(RoomSettingsError::PermissionDenied);
            }
            if current == choice {
                return Ok(fresh);
            }
            let typed = RoomGuestAccessEventContent::new(choice.wire());
            let stored = fresh.baseline.guests.as_deref();
            let content = edited_content(stored, &["guest_access"], typed)?;
            let kind = StateEventType::RoomGuestAccess;
            self.send_state(&dispatch, kind, content).await?;
            self.check()?;
            self.read_access().await
        })
        .await
    }
    /// One listing change from `reviewed`, on the account's homeserver.
    /// Listing makes the room's name, topic, avatar, address and member
    /// count findable there; it never changes who may join.
    pub async fn set_directory_listing(
        &self,
        reviewed: &RoomAccessSnapshot,
        listed: bool,
    ) -> Result<RoomAccessSnapshot> {
        let dispatch = Dispatch::default();
        self.run_access_write(&dispatch, async {
            let fresh = self.fresh_against(reviewed).await?;
            let current = match fresh.directory {
                DirectoryListing::Listed => true,
                DirectoryListing::NotListed => false,
                DirectoryListing::Unsupported => return Err(RoomSettingsError::ReadOnly),
                DirectoryListing::Unknown => return Err(RoomSettingsError::Unavailable),
            };
            if fresh.directory != reviewed.directory {
                return Err(RoomSettingsError::BaselineChanged);
            }
            if !fresh.permissions.directory {
                return Err(RoomSettingsError::PermissionDenied);
            }
            if current == listed {
                return Ok(fresh);
            }
            self.send_listing(&dispatch, listed).await?;
            self.check()?;
            self.read_access().await
        })
        .await
    }
    /// An access change under the session's one operation at a time, the
    /// write timeout and the selection guard (`withdrawable_until_sent`):
    /// closing the form stops it only before its request is handed over;
    /// after that, only its own answer or bound, or an account change, ends
    /// it. A stop or failure is reported by what may have reached the
    /// homeserver, as for every write.
    async fn run_access_write<T>(
        &self,
        dispatch: &Dispatch,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let work = async {
            let _operation = self.operation.lock().await;
            self.check()?;
            work.await
        };
        withdrawable_until_sent(&self.selection, &self.cancellation, dispatch, work)
            .await
            .map_err(|error| dispatch.outcome(error))
    }
    /// A fresh read for a change to what `reviewed` showed, refused when
    /// that came from another account's or another room's read.
    async fn fresh_against(&self, reviewed: &RoomAccessSnapshot) -> Result<RoomAccessSnapshot> {
        if reviewed.baseline.account != self.selection.account_id() {
            return Err(RoomSettingsError::AccountChanged);
        }
        if reviewed.baseline.room.as_str() != self.room.room_id().as_str() {
            return Err(RoomSettingsError::BaselineChanged);
        }
        self.read_access().await
    }
    async fn read_access(&self) -> Result<RoomAccessSnapshot> {
        let room = &self.room;
        let own = &self.own_user;
        ensure_joined(room)?;
        let (create, power) = access_authority(room, own).await?;
        let history = access_state(room, StateEventType::RoomHistoryVisibility).await?;
        let guests = access_state(room, StateEventType::RoomGuestAccess).await?;
        let directory = directory_listing(room).await;
        ensure_joined(room)?;
        // The parent's one power-level rule (shared with room permissions).
        let permissions = match power_levels(&create, power.as_ref())? {
            Some(levels) => RoomAccessPermissions {
                history: levels.user_can_send_state(own, StateEventType::RoomHistoryVisibility),
                guests: levels.user_can_send_state(own, StateEventType::RoomGuestAccess),
                directory: levels.user_can_send_state(own, StateEventType::RoomCanonicalAlias),
            },
            // A room version without known rules grants nothing.
            None => RoomAccessPermissions::default(),
        };
        Ok(RoomAccessSnapshot {
            history: history_setting(history.as_deref()),
            guests: guest_setting(guests.as_deref()),
            directory,
            permissions,
            baseline: AccessBaseline {
                account: self.selection.account_id().to_owned(),
                room: room.room_id().to_owned(),
                history,
                guests,
            },
        })
    }
    /// The listing change's one request, on the write config. As in
    /// `send_state`, nothing is awaited between the final checks and the
    /// hand-off, and only a 4xx answer means nothing changed.
    async fn send_listing(&self, dispatch: &Dispatch, listed: bool) -> Result<()> {
        let visibility = if listed {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let room_id = self.room.room_id().to_owned();
        let request = set_room_visibility::v3::Request::new(room_id, visibility);
        let client = self.room.client();
        self.admit()?;
        dispatch.state.store(SENT, Ordering::Release);
        let sent = client
            .send(request)
            .with_request_config(write_config())
            .await;
        match sent {
            Ok(_) => {
                dispatch.state.store(APPLIED, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                let refused = refusal(error.as_client_api_error());
                if refused.is_some() {
                    dispatch.state.store(REFUSED, Ordering::Release);
                }
                Err(refused.unwrap_or(RoomSettingsError::Unknown))
            }
        }
    }
}

/// `work` raced against the form's `cancellation` only until `dispatch`
/// shows its request handed over, then against nothing but its own
/// WRITE_TIMEOUT and `selection` (an account change). A withdrawal that
/// comes first drops `work` with nothing sent: the hand-off is decided in
/// the same poll that stores SENT (`admit`, then the send), so the check
/// here sees either nothing handed over or the hand-off.
async fn withdrawable_until_sent<T>(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    dispatch: &Dispatch,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    ensure_scope(selection, cancellation)?;
    let handed_over = || dispatch.state.load(Ordering::Acquire) != 0;
    let waiting = selection.guard(tokio::time::timeout(WRITE_TIMEOUT, work));
    tokio::pin!(waiting);
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled(), if !handed_over() => {
                if !handed_over() {
                    return Err(RoomSettingsError::Cancelled);
                }
                // Handed over meanwhile: wait for its own answer.
            }
            result = &mut waiting => {
                return match result {
                    Some(Ok(result)) => result,
                    Some(Err(_)) => Err(RoomSettingsError::TimedOut),
                    None => Err(RoomSettingsError::AccountChanged),
                };
            }
        }
    }
}

/// The parent's bounded read config, plus no authentication refresh and
/// replay (root's SDK opt-out, as `write_config` uses), for the reads this
/// module adds. The shared `read_config` is left as it is.
fn access_read_config() -> RequestConfig {
    read_config().disable_auth_refresh()
}

/// One state event as the exact text the server sent (`None` without it),
/// on `access_read_config`: the one state read of this module.
async fn access_get(room: &Room, kind: &StateEventType, key: &str) -> Result<Option<String>> {
    let mut request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        kind.clone(),
        key.into(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    let response = room
        .client()
        .send(request)
        .with_request_config(access_read_config())
        .await;
    match response {
        Ok(response) => Ok(Some(response.event_or_content.get().to_owned())),
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(request_failure(error)),
    }
}

/// A history-visibility or guest-access event's content as the exact
/// stored text (`None` without the event), with the parent's own parsers
/// and categories.
async fn access_state(room: &Room, kind: StateEventType) -> Result<Option<String>> {
    let Some(text) = access_get(room, &kind, "").await? else {
        return Ok(None);
    };
    parse_state(&text, room.room_id(), &kind, "")?;
    let content = parse_raw_state(&text, room.room_id(), &kind, "")?;
    Ok(Some(content.get().to_owned()))
}

/// One state event, parsed by the parent's own parser for exactly this
/// room, type and state key; `None` without it.
async fn access_event(
    room: &Room,
    kind: StateEventType,
    key: &str,
) -> Result<Option<StateEnvelope>> {
    let text = access_get(room, &kind, key).await?;
    let event = text.map(|text| parse_state(&text, room.room_id(), &kind, key));
    event.transpose()
}

/// The room's creation and power levels, once the account's own
/// membership is found to be a join, as the parent's `read_settings`
/// reads them, but on `access_read_config`: no authentication refresh or
/// replay runs behind an access load or a change's fresh read.
pub(super) async fn access_authority(
    room: &Room,
    own: &OwnedUserId,
) -> Result<(StateEnvelope, Option<StateEnvelope>)> {
    let create = access_event(room, StateEventType::RoomCreate, "")
        .await?
        .ok_or(RoomSettingsError::InvalidState)?;
    let member = access_event(room, StateEventType::RoomMember, own.as_str())
        .await?
        .ok_or(RoomSettingsError::NotJoined)?;
    let member: RoomMemberEventContent = content(&member)?;
    if member.membership != MembershipState::Join {
        return Err(RoomSettingsError::NotJoined);
    }
    let power = access_event(room, StateEventType::RoomPowerLevels, "").await?;
    Ok((create, power))
}

/// The listing as the homeserver states it now, on `access_read_config`.
/// A failure here leaves the other settings readable.
async fn directory_listing(room: &Room) -> DirectoryListing {
    let request = get_room_visibility::v3::Request::new(room.room_id().to_owned());
    let response = room
        .client()
        .send(request)
        .with_request_config(access_read_config())
        .await;
    match response {
        Ok(response) => match response.visibility {
            Visibility::Public => DirectoryListing::Listed,
            Visibility::Private => DirectoryListing::NotListed,
            _ => DirectoryListing::Unknown,
        },
        Err(error) if not_offered(&error) => DirectoryListing::Unsupported,
        Err(_) => DirectoryListing::Unknown,
    }
}
/// The spec's answer for an endpoint or method a homeserver does not offer.
fn not_offered(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|error| {
        matches!(error.status_code.as_u16(), 404 | 405)
            && matches!(error.error_kind(), Some(ErrorKind::Unrecognized))
    })
}

/// One field of a stored content object, read from its fields in stored
/// order: content that repeats a name, which a map would quietly collapse
/// to one value, or that is not an object, is `Invalid`.
#[derive(Debug, PartialEq, Eq)]
enum Stored {
    NoEvent,
    NoValue,
    Text(String),
    Invalid,
}
fn stored_value(stored: Option<&str>, key: &str) -> Stored {
    let Some(stored) = stored else {
        return Stored::NoEvent;
    };
    let Ok(fields) = content_fields(stored) else {
        return Stored::Invalid;
    };
    match fields.iter().find(|(name, _)| name == key) {
        None => Stored::NoValue,
        Some((_, value)) => match serde_json::from_str::<String>(value.get()) {
            Ok(text) => Stored::Text(text),
            Err(_) => Stored::Invalid,
        },
    }
}
fn history_setting(stored: Option<&str>) -> RoomAccessSetting<HistoryVisibilityChoice> {
    use HistoryVisibilityChoice as Choice;
    match stored_value(stored, "history_visibility") {
        // The protocol default for a room without the event.
        Stored::NoEvent => RoomAccessSetting::Default(Choice::Shared),
        Stored::Text(text) => match text.as_str() {
            "joined" => RoomAccessSetting::Set(Choice::Joined),
            "invited" => RoomAccessSetting::Set(Choice::Invited),
            "shared" => RoomAccessSetting::Set(Choice::Shared),
            "world_readable" => RoomAccessSetting::Set(Choice::WorldReadable),
            _ => RoomAccessSetting::Unrecognized,
        },
        // Redaction keeps this value, so an event without one is not one
        // Morpheus can read.
        Stored::NoValue | Stored::Invalid => RoomAccessSetting::Unrecognized,
    }
}
fn guest_setting(stored: Option<&str>) -> RoomAccessSetting<GuestAccessChoice> {
    use GuestAccessChoice as Choice;
    match stored_value(stored, "guest_access") {
        // Absent, or redacted to no value: guests may not join.
        Stored::NoEvent | Stored::NoValue => RoomAccessSetting::Default(Choice::Forbidden),
        Stored::Text(text) => match text.as_str() {
            "can_join" => RoomAccessSetting::Set(Choice::CanJoin),
            "forbidden" => RoomAccessSetting::Set(Choice::Forbidden),
            _ => RoomAccessSetting::Unrecognized,
        },
        Stored::Invalid => RoomAccessSetting::Unrecognized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::api::{
        client::uiaa::UiaaResponse,
        error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
    };
    use serde_json::json;

    #[test]
    fn stored_settings_read_known_values_defaults_and_nothing_else() {
        use GuestAccessChoice as Guests;
        use HistoryVisibilityChoice as History;
        use RoomAccessSetting as Setting;
        let history = |text: &str| history_setting(Some(text));
        assert_eq!(history_setting(None), Setting::Default(History::Shared));
        let joined = r#"{"history_visibility":"joined"}"#;
        assert_eq!(history(joined), Setting::Set(History::Joined));
        let extended = r#"{"org.example.x":1.10,"history_visibility":"world_readable"}"#;
        assert_eq!(history(extended), Setting::Set(History::WorldReadable));
        for odd in [
            r#"{"history_visibility":"org.example.members"}"#,
            r#"{"history_visibility":1}"#,
            r#"{"history_visibility":"joined","history_visibility":"shared"}"#,
            r#"{}"#,
            "[]",
        ] {
            assert_eq!(history(odd), Setting::Unrecognized, "{odd}");
        }
        let guests = |text: &str| guest_setting(Some(text));
        assert_eq!(guest_setting(None), Setting::Default(Guests::Forbidden));
        // A redacted event keeps no value: guests stay forbidden.
        assert_eq!(guests("{}"), Setting::Default(Guests::Forbidden));
        let allowed = r#"{"guest_access":"can_join"}"#;
        assert_eq!(guests(allowed), Setting::Set(Guests::CanJoin));
        let forbidden = r#"{"guest_access":"forbidden"}"#;
        assert_eq!(guests(forbidden), Setting::Set(Guests::Forbidden));
        for odd in [
            r#"{"guest_access":"org.example.maybe"}"#,
            r#"{"guest_access":true}"#,
            r#"{"guest_access":"can_join","guest_access":"forbidden"}"#,
        ] {
            assert_eq!(guests(odd), Setting::Unrecognized, "{odd}");
        }
        assert_eq!(Setting::<History>::Unrecognized.value(), None);
    }

    #[test]
    fn a_missing_endpoint_is_unsupported_and_any_other_failure_is_not() {
        let answer = |status: u16, code: &str| {
            let body: StandardErrorBody =
                serde_json::from_value(json!({"errcode": code, "error": "x"})).unwrap();
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            let error = Error::new(status, ErrorBody::Standard(body));
            let error = FromHttpResponseError::Server(UiaaResponse::MatrixError(error));
            HttpError::Api(Box::new(error))
        };
        assert!(not_offered(&answer(404, "M_UNRECOGNIZED")));
        assert!(not_offered(&answer(405, "M_UNRECOGNIZED")));
        assert!(!not_offered(&answer(404, "M_NOT_FOUND")));
        assert!(!not_offered(&answer(403, "M_FORBIDDEN")));
        assert!(!not_offered(&answer(500, "M_UNRECOGNIZED")));
        let too_large = HttpError::ResponseTooLarge { limit: 1 };
        assert!(!not_offered(&too_large));
    }

    fn current_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// Closing the form withdraws an access change only before its request
    /// is handed over; once handed over its own answer decides, and an
    /// account change still ends the wait as unknown.
    #[test]
    fn closing_withdraws_a_change_only_until_its_request_is_handed_over() {
        use crate::account_selection::Selections;
        use std::sync::atomic::AtomicUsize;
        use tokio::sync::Notify;
        current_thread().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("a"));
            let selection = selections.for_account("a").unwrap();
            // Withdrawn before the hand-off: the change stops and nothing
            // was sent.
            let form = RoomSettingsCancellation::new();
            let dispatch = Dispatch::default();
            let polls = AtomicUsize::new(0);
            let work = async {
                polls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            };
            let closing = async {
                while polls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
                form.cancel();
            };
            let waiting = withdrawable_until_sent(&selection, &form, &dispatch, work);
            let (result, ()) = tokio::join!(waiting, closing);
            assert_eq!(result, Err(RoomSettingsError::Cancelled));
            let reported = dispatch.outcome(RoomSettingsError::Cancelled);
            assert_eq!(reported, RoomSettingsError::Cancelled, "nothing was sent");
            // Closed before it starts: never polled.
            let polls = AtomicUsize::new(0);
            let work = async {
                polls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            };
            let closed = withdrawable_until_sent(&selection, &form, &dispatch, work).await;
            assert_eq!(closed, Err(RoomSettingsError::Cancelled));
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            // Handed over, then the form closes: the change is still waited
            // for, and its own answer is what it reports.
            let form = RoomSettingsCancellation::new();
            let dispatch = Dispatch::default();
            let answer = Notify::new();
            let work = async {
                dispatch.state.store(SENT, Ordering::Release);
                answer.notified().await;
                dispatch.state.store(APPLIED, Ordering::Release);
                Ok(7)
            };
            let closing = async {
                while dispatch.state.load(Ordering::Acquire) != SENT {
                    tokio::task::yield_now().await;
                }
                form.cancel();
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    dispatch.state.load(Ordering::Acquire),
                    SENT,
                    "still waited for"
                );
                answer.notify_one();
            };
            let waiting = withdrawable_until_sent(&selection, &form, &dispatch, work);
            let (result, ()) = tokio::join!(waiting, closing);
            assert_eq!(result, Ok(7));
            // Handed over, then the account changes: no answer is known.
            let form = RoomSettingsCancellation::new();
            let dispatch = Dispatch::default();
            let work = async {
                dispatch.state.store(SENT, Ordering::Release);
                std::future::pending::<()>().await;
                Ok(())
            };
            let switching = async {
                while dispatch.state.load(Ordering::Acquire) != SENT {
                    tokio::task::yield_now().await;
                }
                selections.replace(Some("b"));
            };
            let waiting = withdrawable_until_sent(&selection, &form, &dispatch, work);
            let (result, ()) = tokio::join!(waiting, switching);
            let error = result.unwrap_err();
            assert_eq!(error, RoomSettingsError::AccountChanged);
            assert_eq!(dispatch.outcome(error), RoomSettingsError::Unknown);
        });
    }

    /// The reads this module makes, the permission check's authority
    /// reads included, use its own config, with no authentication refresh
    /// and replay; the shared one stays as it is. The loopback regression
    /// `the_access_reads_never_refresh_or_replay_a_rejected_token` shows it
    /// on the wire.
    #[test]
    fn the_access_reads_opt_out_of_authentication_refresh() {
        let source = include_str!("room_access_settings.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains("read_config().disable_auth_refresh()"));
        assert!(!production.contains("with_request_config(read_config())"));
        assert_eq!(
            production
                .matches("with_request_config(access_read_config())")
                .count(),
            2,
            "the state read (authority and settings) and the listing read"
        );
        assert!(!production.contains("state_and_raw("));
        assert!(!production.contains("joined_authority("));
        // The config itself: capped at the transport, no retry, no refresh.
        let config = format!("{:?}", access_read_config());
        let cap = format!("max_response_bytes: {}", super::super::MAX_STATE_BYTES);
        assert!(config.contains(&cap), "{config}");
        assert!(config.contains("retry_limit: 0"), "{config}");
        assert!(config.contains("disable_auth_refresh: true"), "{config}");
    }

    #[test]
    fn writes_name_the_protocol_values() {
        use HistoryVisibilityChoice as History;
        for (choice, wire) in [
            (History::Joined, "joined"),
            (History::Invited, "invited"),
            (History::Shared, "shared"),
            (History::WorldReadable, "world_readable"),
        ] {
            assert_eq!(choice.wire().as_str(), wire);
        }
        assert_eq!(GuestAccessChoice::CanJoin.wire().as_str(), "can_join");
        assert_eq!(GuestAccessChoice::Forbidden.wire().as_str(), "forbidden");
    }
}
