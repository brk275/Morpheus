//! The selected account's own public Matrix profile: its display name and
//! avatar as the homeserver reports them, and explicit changes to them.
//!
//! These are public profile fields, distinct from the local account entry
//! (Matrix user ID, homeserver, device), the device display name, login
//! credentials and the local protection password, none of which are read or
//! changed here. Every step runs on the account's own SDK client under the
//! exact account selection and a caller cancellation token; a switch away
//! (also A -> B -> A), removal or a closed editor ends the step and any later
//! step. A request already handed to the homeserver is never claimed to be
//! undone: a step that returns an error after sending may still have applied,
//! and the caller re-reads the profile to learn what the server holds.
//!
//! Names and avatars are each their own request; nothing here pretends that
//! a name and an avatar change are atomic. Ordinary display-name and avatar
//! edits use the session's existing access token: no password, no
//! re-authentication and no new network client.

use std::{fmt, future::Future, time::Duration};

use matrix_sdk::{
    Client, HttpError,
    config::RequestConfig,
    ruma::{
        OwnedMxcUri, OwnedUserId,
        api::{
            Metadata,
            client::profile::{
                AvatarUrl, DisplayName, ProfileFieldName, delete_profile_field, get_profile,
                set_avatar_url, set_display_name,
            },
            error::ErrorKind,
        },
        events::room::MediaSource,
    },
};
use tokio::time;

use crate::{AccountManager, AccountSelection, AvatarPng, RoomSettingsCancellation};

/// Longest display name accepted, in characters and in UTF-8 bytes.
pub const MAX_DISPLAY_NAME_CHARS: usize = 256;
pub const MAX_DISPLAY_NAME_BYTES: usize = 1024;
/// Largest current avatar read back for the editor preview.
pub const MAX_OWN_AVATAR_PREVIEW_BYTES: usize = crate::MAX_IMAGE_PREVIEW_BYTES;
/// Largest profile response body read, for reads and writes alike. A
/// display name, avatar URL and the few other standard fields fit easily.
pub const MAX_PROFILE_RESPONSE_BYTES: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Fixed, sanitized outcomes. No server text, URL or identifier is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("The profile editor was closed.")]
    Cancelled,
    #[error("The selected account changed.")]
    AccountChanged,
    #[error("The profile could not be reached. Refresh and try again.")]
    Unavailable,
    #[error("The homeserver is limiting requests. Wait, then refresh and try again.")]
    RateLimited,
    #[error("The request timed out. Refresh to see what the server holds.")]
    TimedOut,
    #[error("The homeserver does not allow changing this profile field.")]
    NotAllowed,
    #[error("Enter a display name of 1 to 256 characters without control characters.")]
    InvalidName,
    #[error("Choose a valid bounded image.")]
    InvalidAvatar,
    #[error("The server returned an invalid profile.")]
    InvalidResponse,
}

pub type ProfileResult<T> = std::result::Result<T, ProfileError>;

/// What one public profile change did, as far as the server shows.
///
/// A change is sent exactly once. `applied` is true only when the profile
/// read back afterwards shows exactly the requested value. `send_error` is
/// the send's own failure: with `applied` the change reached the server
/// anyway; without it the outcome is unknown and `profile` (when it could
/// be read) is what the server holds now. Nothing is resent automatically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteReport {
    pub profile: Option<OwnProfile>,
    pub applied: bool,
    pub send_error: Option<ProfileError>,
}

impl WriteReport {
    /// The server confirmed the change.
    pub fn confirmed(&self) -> bool {
        self.applied
    }

    /// The send reported a failure and the read-back does not show the
    /// change: it may or may not have reached the server.
    pub fn unknown(&self) -> bool {
        !self.applied
    }
}

/// How one single-send request ended.
enum Sent {
    /// Refused before anything was handed to the homeserver.
    NotSent(ProfileError),
    /// The homeserver accepted it.
    Sent,
    /// Handed to the homeserver (or possibly so) but no success was seen.
    Unknown(ProfileError),
}

/// The profile as the homeserver reported it, with the identity it belongs
/// to so the editor can say exactly which account it changes.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnProfile {
    pub account_id: String,
    pub user_id: String,
    pub homeserver: String,
    /// For display only: bounded and single-line; `None` when the server
    /// holds none. Never used to confirm a change.
    pub display_name: Option<String>,
    /// The name exactly as the server returned it, if it fits the bound;
    /// the only value a write is confirmed against.
    raw_display_name: RawName,
    avatar: Option<OwnedMxcUri>,
}

/// The server's display name as returned, kept only within a bound.
#[derive(Clone, PartialEq, Eq)]
enum RawName {
    Absent,
    Exact(String),
    /// Longer than any name this editor sends: never equal to one.
    Oversize,
}

impl RawName {
    fn from_server(raw: Option<String>) -> Self {
        match raw {
            None => Self::Absent,
            Some(raw) if raw.len() > MAX_DISPLAY_NAME_BYTES => Self::Oversize,
            Some(raw) => Self::Exact(raw),
        }
    }
}

impl OwnProfile {
    pub fn has_avatar(&self) -> bool {
        self.avatar.is_some()
    }

    /// Whether the server holds exactly `sent` as the display name. Compared
    /// on the raw value, never on the shortened, normalized display copy.
    pub fn name_is_exactly(&self, sent: &str) -> bool {
        matches!(&self.raw_display_name, RawName::Exact(raw) if raw == sent)
    }

    /// Whether the server holds no display name (absent, or the empty value
    /// a legacy clear may leave).
    pub fn name_is_absent(&self) -> bool {
        match &self.raw_display_name {
            RawName::Absent => true,
            RawName::Exact(raw) => raw.is_empty(),
            RawName::Oversize => false,
        }
    }

    /// A profile without an avatar for synthetic UI tests. It names no real
    /// identity and can only relabel presentation in a test window.
    #[doc(hidden)]
    pub fn synthetic(
        account_id: &str,
        user_id: &str,
        homeserver: &str,
        display_name: Option<&str>,
    ) -> Self {
        Self {
            account_id: account_id.to_owned(),
            user_id: user_id.to_owned(),
            homeserver: homeserver.to_owned(),
            display_name: display_name.map(str::to_owned),
            raw_display_name: RawName::from_server(display_name.map(str::to_owned)),
            avatar: None,
        }
    }
}

impl fmt::Debug for OwnProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnProfile")
            .field("display_name", &self.display_name.is_some())
            .field("avatar", &self.avatar.is_some())
            .finish_non_exhaustive()
    }
}

/// A display name as the editor would save it, or why it cannot be saved.
/// Trims surrounding whitespace; rejects empty, oversized and control
/// characters (line breaks included) so a name stays one line everywhere.
pub fn validate_display_name(input: &str) -> ProfileResult<String> {
    let name = input.trim();
    if name.is_empty()
        || name.len() > MAX_DISPLAY_NAME_BYTES
        || name.chars().count() > MAX_DISPLAY_NAME_CHARS
        || name.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return Err(ProfileError::InvalidName);
    }
    // Runs of spaces collapse, as the editor shows a server name, so the
    // read-back of a saved name compares equal.
    Ok(name.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// A name the server returned, made safe to show: one bounded line, or
/// `None` when it is empty or unusable.
fn shown_name(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let line: String = raw
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .take(MAX_DISPLAY_NAME_CHARS)
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

fn request_failure(error: &HttpError) -> ProfileError {
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        // Over the profile bound: nothing of it is used or shown.
        return ProfileError::InvalidResponse;
    }
    match error.as_client_api_error() {
        Some(api)
            if api.status_code.as_u16() == 429
                || matches!(api.error_kind(), Some(ErrorKind::LimitExceeded(_))) =>
        {
            ProfileError::RateLimited
        }
        Some(api) if api.status_code.as_u16() == 403 => ProfileError::NotAllowed,
        _ => ProfileError::Unavailable,
    }
}

fn ensure_scope(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
) -> ProfileResult<()> {
    if cancellation.is_cancelled() {
        return Err(ProfileError::Cancelled);
    }
    if !selection.is_current() {
        return Err(ProfileError::AccountChanged);
    }
    Ok(())
}

/// One step under the exact selection and the editor's token: it ends early
/// when either ends, and its result is dropped if either ended meanwhile.
async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    limit: Duration,
    work: impl Future<Output = ProfileResult<T>>,
) -> ProfileResult<T> {
    ensure_scope(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ProfileError::Cancelled),
        _ = selection.revoked() => Err(ProfileError::AccountChanged),
        result = time::timeout(limit, work) => result.map_err(|_| ProfileError::TimedOut)?,
    }?;
    ensure_scope(selection, cancellation)?;
    Ok(result)
}

/// The request settings of every public profile change: sent once, never
/// retried by the SDK, authenticated, bounded in time and in response size.
fn single_send() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(WRITE_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_PROFILE_RESPONSE_BYTES)
}

/// The request settings of every profile read: one attempt, authenticated,
/// bounded in time and in response size. A larger body is refused before it
/// is buffered past the bound.
fn profile_read() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(READ_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_PROFILE_RESPONSE_BYTES)
}

/// Run one profile request (built with `single_send`) exactly once. Only a
/// refusal before the request leaves (closed editor, changed account) is
/// reported as not sent; any later failure means the outcome is unknown.
async fn send_once(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    request: impl Future<Output = Result<(), HttpError>>,
) -> Sent {
    if let Err(error) = ensure_scope(selection, cancellation) {
        return Sent::NotSent(error);
    }
    let result = guarded(selection, cancellation, WRITE_TIMEOUT, async {
        request.await.map_err(|error| request_failure(&error))
    })
    .await;
    match result {
        Ok(()) => Sent::Sent,
        // The request may already have left when the editor or account ended.
        Err(error) => Sent::Unknown(error),
    }
}

/// Remove one profile field with a single request: the delete endpoint when
/// the server supports it, else the legacy setter with no value. Learning
/// which endpoint applies is a read; only the chosen request is a change.
async fn clear_field(
    owned: &Owned,
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    field: ProfileFieldName,
) -> Sent {
    let versions = match guarded(selection, cancellation, READ_TIMEOUT, async {
        owned
            .client
            .supported_versions()
            .await
            .map_err(|error| request_failure(&error))
    })
    .await
    {
        Ok(versions) => versions,
        Err(error) => return Sent::NotSent(error),
    };
    let client = &owned.client;
    if delete_profile_field::v3::Request::PATH_BUILDER.is_supported(&versions) {
        let request = delete_profile_field::v3::Request::new(owned.user.clone(), field);
        return send_once(selection, cancellation, async {
            client.send(request).with_request_config(single_send()).await.map(|_| ())
        })
        .await;
    }
    match field {
        ProfileFieldName::DisplayName => {
            #[allow(deprecated)]
            let request = set_display_name::v3::Request::new(owned.user.clone(), None);
            send_once(selection, cancellation, async {
                client.send(request).with_request_config(single_send()).await.map(|_| ())
            })
            .await
        }
        _ => {
            #[allow(deprecated)]
            let request = set_avatar_url::v3::Request::new(owned.user.clone(), None);
            send_once(selection, cancellation, async {
                client.send(request).with_request_config(single_send()).await.map(|_| ())
            })
            .await
        }
    }
}

struct Owned {
    client: Client,
    user: OwnedUserId,
    account_id: String,
    homeserver: String,
}

impl AccountManager {
    /// The selected account's client and identity, only while `selection` is
    /// still exactly the active, eligible account.
    async fn own_client(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
    ) -> ProfileResult<Owned> {
        ensure_scope(selection, cancellation)?;
        let state = self.state.lock().await;
        let current = self
            .account_selection(selection.account_id())
            .ok_or(ProfileError::AccountChanged)?;
        if !selection.same_epoch(&current)
            || state.active_account_id.as_deref() != Some(selection.account_id())
        {
            return Err(ProfileError::AccountChanged);
        }
        crate::account_access::ensure_eligible(&state, selection.account_id())
            .map_err(|_| ProfileError::AccountChanged)?;
        let loaded = state
            .loaded
            .get(selection.account_id())
            .ok_or(ProfileError::AccountChanged)?;
        let info = state
            .registry
            .accounts
            .iter()
            .find(|account| account.id == selection.account_id())
            .ok_or(ProfileError::AccountChanged)?;
        let user = loaded
            .client
            .user_id()
            .ok_or(ProfileError::Unavailable)?
            .to_owned();
        // The session's user must be the registered identity being edited.
        if user.as_str() != info.user_id {
            return Err(ProfileError::AccountChanged);
        }
        Ok(Owned {
            client: loaded.client.clone(),
            user,
            account_id: info.id.clone(),
            homeserver: info.homeserver.clone(),
        })
    }

    async fn read_profile(
        owned: &Owned,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
    ) -> ProfileResult<OwnProfile> {
        // A typed request under this module's own bounds, never the SDK's
        // unbounded profile helper.
        let request = get_profile::v3::Request::new(owned.user.clone());
        let response = guarded(selection, cancellation, READ_TIMEOUT, async {
            owned
                .client
                .send(request)
                .with_request_config(profile_read())
                .await
                .map_err(|error| request_failure(&error))
        })
        .await?;
        let display_name = response
            .get_static::<DisplayName>()
            .map_err(|_| ProfileError::InvalidResponse)?;
        let avatar = response
            .get_static::<AvatarUrl>()
            .map_err(|_| ProfileError::InvalidResponse)?;
        if let Some(uri) = &avatar {
            crate::media::validate_media_address(uri)
                .map_err(|_| ProfileError::InvalidResponse)?;
        }
        Ok(OwnProfile {
            account_id: owned.account_id.clone(),
            user_id: owned.user.to_string(),
            homeserver: owned.homeserver.clone(),
            raw_display_name: RawName::from_server(display_name.clone()),
            display_name: shown_name(display_name),
            avatar,
        })
    }

    /// Read the selected account's public profile from its homeserver.
    pub async fn own_profile_for(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
    ) -> ProfileResult<OwnProfile> {
        let owned = self.own_client(selection, cancellation).await?;
        Self::read_profile(&owned, selection, cancellation).await
    }

    /// Publish a new display name: one single-send request, then a read-back.
    pub async fn set_own_display_name_for(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
        name: &str,
    ) -> ProfileResult<WriteReport> {
        let name = validate_display_name(name)?;
        let owned = self.own_client(selection, cancellation).await?;
        #[allow(deprecated)]
        let request = set_display_name::v3::Request::new(owned.user.clone(), Some(name.clone()));
        let client = &owned.client;
        let sent = send_once(selection, cancellation, async {
            client.send(request).with_request_config(single_send()).await.map(|_| ())
        })
        .await;
        Self::confirm(&owned, selection, cancellation, sent, |profile| {
            profile.name_is_exactly(&name)
        })
        .await
    }

    /// Remove the display name from the public profile, so it has none:
    /// one single-send request, then a read-back.
    pub async fn clear_own_display_name_for(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
    ) -> ProfileResult<WriteReport> {
        let owned = self.own_client(selection, cancellation).await?;
        let sent = clear_field(&owned, selection, cancellation, ProfileFieldName::DisplayName).await;
        Self::confirm(&owned, selection, cancellation, sent, |profile| {
            profile.name_is_absent()
        })
        .await
    }

    /// Upload a prepared, bounded PNG, then publish it as the avatar with one
    /// single-send request and read the profile back. The upload is not a
    /// public change: if it fails, nothing was published and this returns an
    /// error. A closed editor or account change after the upload leaves only
    /// an unused upload and never starts the profile change.
    pub async fn set_own_avatar_for(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
        image: AvatarPng,
    ) -> ProfileResult<WriteReport> {
        let owned = self.own_client(selection, cancellation).await?;
        let (bytes, _, _) = image.into_parts();
        let upload = guarded(selection, cancellation, UPLOAD_TIMEOUT, async {
            owned
                .client
                .media()
                .upload(
                    &mime::IMAGE_PNG,
                    bytes,
                    Some(RequestConfig::new().retry_limit(0).timeout(UPLOAD_TIMEOUT)),
                )
                .await
                .map_err(|_| ProfileError::Unavailable)
        })
        .await?;
        crate::media::validate_media_address(&upload.content_uri)
            .map_err(|_| ProfileError::InvalidResponse)?;
        let uri = upload.content_uri;
        #[allow(deprecated)]
        let request = set_avatar_url::v3::Request::new(owned.user.clone(), Some(uri.clone()));
        let client = &owned.client;
        let sent = send_once(selection, cancellation, async {
            client.send(request).with_request_config(single_send()).await.map(|_| ())
        })
        .await;
        Self::confirm(&owned, selection, cancellation, sent, |profile| {
            profile.avatar.as_ref() == Some(&uri)
        })
        .await
    }

    /// Remove the avatar from the public profile: one single-send request,
    /// then a read-back.
    pub async fn remove_own_avatar_for(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
    ) -> ProfileResult<WriteReport> {
        let owned = self.own_client(selection, cancellation).await?;
        let sent = clear_field(&owned, selection, cancellation, ProfileFieldName::AvatarUrl).await;
        Self::confirm(&owned, selection, cancellation, sent, |profile| !profile.has_avatar())
        .await
    }

    /// Turn one send into a report. A refusal before anything was sent is
    /// returned as an error; after a send (successful or not) the profile is
    /// read back once and the report says what the server shows. A failed
    /// send is never retried here.
    async fn confirm(
        owned: &Owned,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
        sent: Sent,
        applied: impl Fn(&OwnProfile) -> bool,
    ) -> ProfileResult<WriteReport> {
        let send_error = match sent {
            Sent::NotSent(error) => return Err(error),
            Sent::Sent => None,
            Sent::Unknown(error) => Some(error),
        };
        let profile = Self::read_profile(owned, selection, cancellation).await.ok();
        Ok(WriteReport {
            applied: profile.as_ref().is_some_and(&applied),
            profile,
            send_error,
        })
    }

    /// The bytes of the avatar `profile` names, bounded, for a local preview
    /// only. `None` when the profile has no avatar. The profile must be the
    /// selected account's own, read under the same selection.
    pub async fn own_avatar_bytes_for(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomSettingsCancellation,
        profile: &OwnProfile,
    ) -> ProfileResult<Option<Vec<u8>>> {
        let Some(uri) = profile.avatar.clone() else {
            return Ok(None);
        };
        let owned = self.own_client(selection, cancellation).await?;
        if owned.account_id != profile.account_id || owned.user.as_str() != profile.user_id {
            return Err(ProfileError::AccountChanged);
        }
        let bytes = guarded(selection, cancellation, READ_TIMEOUT, async {
            crate::media::fetch_bounded_media(
                &owned.client,
                &MediaSource::Plain(uri),
                MAX_OWN_AVATAR_PREVIEW_BYTES,
            )
            .await
            .map_err(|_| ProfileError::Unavailable)
        })
        .await?;
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_one_bounded_line() {
        assert_eq!(validate_display_name("  Synthetic Name  ").unwrap(), "Synthetic Name");
        assert_eq!(validate_display_name("Synthetic   Spaced").unwrap(), "Synthetic Spaced");
        for invalid in [
            "",
            "   ",
            "two\nlines",
            "tab\there",
            "bidi\u{202e}override",
            &"x".repeat(MAX_DISPLAY_NAME_CHARS + 1),
        ] {
            assert_eq!(validate_display_name(invalid), Err(ProfileError::InvalidName));
        }
        assert!(validate_display_name(&"x".repeat(MAX_DISPLAY_NAME_CHARS)).is_ok());
        // Multi-byte names are bounded in bytes too.
        assert_eq!(
            validate_display_name(&"\u{1f600}".repeat(MAX_DISPLAY_NAME_BYTES / 4 + 1)),
            Err(ProfileError::InvalidName)
        );
    }

    #[test]
    fn a_send_without_a_matching_read_back_is_unknown() {
        let shown = OwnProfile::synthetic("synthetic-a", "@synthetic-a:example.org", "https://example.org", None);
        let confirmed = WriteReport {
            profile: Some(shown.clone()),
            applied: true,
            send_error: Some(ProfileError::TimedOut),
        };
        // The send timed out but the server shows the change: confirmed.
        assert!(confirmed.confirmed() && !confirmed.unknown());
        let unknown = WriteReport {
            profile: Some(shown),
            applied: false,
            send_error: Some(ProfileError::TimedOut),
        };
        assert!(unknown.unknown() && !unknown.confirmed());
        // Nothing could be read back: still unknown, never "saved".
        let unread = WriteReport {
            profile: None,
            applied: false,
            send_error: None,
        };
        assert!(unread.unknown());
    }

    #[test]
    fn profile_reads_and_writes_are_bounded_single_attempts() {
        for config in [profile_read(), single_send()] {
            let shown = format!("{config:?}");
            assert!(
                shown.contains(&format!("max_response_bytes: {MAX_PROFILE_RESPONSE_BYTES}"))
                    || shown.contains(&format!("max_response_bytes: Some({MAX_PROFILE_RESPONSE_BYTES})")),
                "{shown}"
            );
            assert!(shown.contains("retry_limit: 0") || shown.contains("retry_limit: Some(0)"));
            assert!(shown.contains("force_auth: true"));
        }
    }

    #[test]
    fn an_oversize_profile_response_is_refused_without_its_contents() {
        let error = HttpError::ResponseTooLarge {
            limit: MAX_PROFILE_RESPONSE_BYTES,
        };
        assert_eq!(request_failure(&error), ProfileError::InvalidResponse);
        // The fixed message carries no limit, URL or body.
        assert!(!ProfileError::InvalidResponse.to_string().contains("65536"));
    }

    fn read_back(raw: Option<&str>) -> OwnProfile {
        let raw = raw.map(str::to_owned);
        OwnProfile {
            account_id: "synthetic-a".into(),
            user_id: "@synthetic-a:example.org".into(),
            homeserver: "https://example.org".into(),
            raw_display_name: RawName::from_server(raw.clone()),
            display_name: shown_name(raw),
            avatar: None,
        }
    }

    #[test]
    fn a_long_name_read_back_is_not_confirmed_by_its_shortened_display_copy() {
        let sent = "n".repeat(MAX_DISPLAY_NAME_CHARS);
        // The server returned a longer name; its display copy is cut to the
        // very value that was sent, but the raw value differs.
        let longer = format!("{sent}tail");
        let shown = read_back(Some(&longer));
        assert_eq!(shown.display_name.as_deref(), Some(sent.as_str()));
        assert!(!shown.name_is_exactly(&sent));
        // Oversize raw values never equal anything and are never "absent".
        let oversize = read_back(Some(&"o".repeat(MAX_DISPLAY_NAME_BYTES + 1)));
        assert!(!oversize.name_is_exactly(&"o".repeat(MAX_DISPLAY_NAME_BYTES + 1)));
        assert!(!oversize.name_is_absent());
        // The exact value confirms.
        assert!(read_back(Some(&sent)).name_is_exactly(&sent));
    }

    #[test]
    fn a_whitespace_altered_read_back_is_not_confirmed() {
        let sent = "Synthetic Name";
        for altered in ["Synthetic  Name", " Synthetic Name", "Synthetic Name ", "Synthetic\tName", "Synthetic\nName"] {
            let shown = read_back(Some(altered));
            // The display copy normalizes to the sent value...
            assert_eq!(shown.display_name.as_deref(), Some(sent), "{altered:?}");
            // ...but only the exact raw value confirms a write.
            assert!(!shown.name_is_exactly(sent), "{altered:?}");
        }
        assert!(read_back(Some(sent)).name_is_exactly(sent));
        // Cleared: absent or empty; a whitespace-only name is not cleared.
        assert!(read_back(None).name_is_absent());
        assert!(read_back(Some("")).name_is_absent());
        assert!(!read_back(Some(" ")).name_is_absent());
    }

    #[test]
    fn server_names_are_sanitized_for_display() {
        assert_eq!(shown_name(None), None);
        assert_eq!(shown_name(Some("  ".into())), None);
        assert_eq!(
            shown_name(Some("Synthetic\n\tName".into())).as_deref(),
            Some("Synthetic Name")
        );
        let long = shown_name(Some("y".repeat(10_000))).unwrap();
        assert_eq!(long.chars().count(), MAX_DISPLAY_NAME_CHARS);
    }

    #[test]
    fn profile_debug_never_shows_identity_or_name() {
        let profile = OwnProfile {
            account_id: "synthetic-local".into(),
            user_id: "@synthetic:example.org".into(),
            homeserver: "https://example.org".into(),
            display_name: Some("Synthetic Name".into()),
            avatar: None,
        };
        let shown = format!("{profile:?}");
        assert!(!shown.contains("synthetic") && !shown.contains("Synthetic"));
        assert!(!profile.has_avatar());
    }
}
