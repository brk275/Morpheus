//! Finding people on the selected account's homeserver and opening or
//! starting a direct conversation with one of them.
//!
//! The search is the homeserver's own user directory, asked through the
//! account's authenticated SDK client with a typed request: one attempt, a
//! bounded query, result count, response body and time. No other server or
//! URL is contacted, no avatar is fetched, and nothing about the query is
//! logged. A complete Matrix user ID needs no directory result.
//!
//! An existing direct conversation is found from this device's joined rooms
//! and the account's `m.direct` data; "several" and "none known here" are
//! reported as such, never guessed. A new conversation is created with one
//! request with the SDK's retries off and its token-refresh replay disabled
//! (`RequestConfig::disable_auth_refresh`), so it is a single attempt on the
//! wire; it is encrypted from creation and invites exactly the chosen user.
//! A failure after that request left is reported as unknown: the
//! conversation may exist, so this code never creates it again.
//!
//! Every step runs under the exact account selection and a cancellation
//! token; a switch away (also A -> B -> A), lock, removal or a closed
//! window ends it, and a result that arrives afterwards is dropped.

use std::{
    collections::HashSet,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    Client, HttpError,
    config::RequestConfig,
    ruma::{
        DirectUserIdentifier, OwnedUserId, RoomId, UInt, UserId,
        api::{
            client::{room::create_room, user_directory::search_users},
            error::ErrorKind,
        },
        events::{InitialStateEvent, room::encryption::RoomEncryptionEventContent},
    },
};
use tokio::{sync::Notify, time};

use crate::{
    AccountManager, AccountSelection, account_access,
    direct_account_data::{DirectListing, DirectWriteError, DirectWriteGuard},
    selected_loaded_for,
};

/// Longest search accepted, in characters (and four times that in bytes).
pub const MAX_USER_QUERY_CHARS: usize = 128;
/// Results asked for and shown at most.
pub const MAX_USER_RESULTS: usize = 20;
/// Longest display name shown, in characters.
pub const MAX_PERSON_NAME_CHARS: usize = 80;
/// Existing direct conversations reported by ID at most.
pub const MAX_KNOWN_DMS: usize = 8;
const MAX_DIRECTORY_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_CREATE_RESPONSE_BYTES: u64 = 16 * 1024;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);
const SYNC_TIMEOUT: Duration = Duration::from_secs(20);

/// Fixed, sanitized outcomes. No server text, URL, query or identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeopleError {
    #[error("This window was closed.")]
    Cancelled,
    #[error("The selected account changed.")]
    AccountChanged,
    #[error("Enter 1 to 128 characters without line breaks.")]
    InvalidQuery,
    #[error("Enter a complete Matrix user ID such as @name:example.org.")]
    InvalidUser,
    #[error("That is your own account.")]
    OwnAccount,
    #[error("The user directory is unavailable. Try again later.")]
    Unavailable,
    #[error("The homeserver is limiting requests. Wait, then try again.")]
    RateLimited,
    #[error("The request timed out.")]
    TimedOut,
    #[error("The homeserver refused this request.")]
    NotAllowed,
    #[error("The server returned an invalid answer.")]
    InvalidResponse,
    #[error(
        "The conversation may have been created. Check your conversations before trying again."
    )]
    CreateUnknown,
}

pub type PeopleResult<T> = std::result::Result<T, PeopleError>;

/// Ends one people window and everything it waits for. App callers install
/// their security-epoch admission using `with_admission`; `new()` alone only
/// supplies explicit cancellation plus each operation's account-selection
/// checks. Admission is checked again at SDK handoff. A request already handed
/// to the SDK may still apply; an external admission change alone is noticed
/// on the next poll/check, while explicit cancellation wakes pending waits.
#[derive(Clone, Default)]
pub struct PeopleCancellation {
    state: Arc<CancellationState>,
    admission: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl PeopleCancellation {
    pub fn new() -> Self {
        Self::default()
    }
    /// Add an application-owned admission check. It runs synchronously on
    /// worker threads and must only inspect thread-safe state, never the UI.
    /// App-origin operations install their security epoch here; callers using
    /// `new()` alone receive only explicit cancellation and account scoping.
    pub fn with_admission(mut self, admitted: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.admission = Some(admitted);
        self
    }
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.state.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
            || self.admission.as_ref().is_some_and(|admitted| !admitted())
    }
    async fn cancelled(&self) {
        loop {
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// One person from the directory. Text is bounded and single-line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryPerson {
    pub user_id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeoplePage {
    /// At most `MAX_USER_RESULTS`, in the server's order, without
    /// duplicates or the account itself.
    pub people: Vec<DirectoryPerson>,
    /// The server said more matched than it returned.
    pub limited: bool,
}

/// Direct conversations with one person this device knows of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnownDirect {
    /// None is known here. One may still exist elsewhere.
    NoneKnown,
    One(String),
    /// More than one; at most `MAX_KNOWN_DMS` IDs and the full count.
    Several { rooms: Vec<String>, count: usize },
}

/// A newly created direct conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDirect {
    pub room_id: String,
    /// Listing it under the invitee in the account's `m.direct` data. The
    /// room exists either way; an error only means that listing is missing
    /// or unconfirmed (`Unknown`), never that the creation failed.
    pub direct_listing: Result<DirectListing, DirectWriteError>,
    /// This device has synced it, so it can be opened now.
    pub synced: bool,
}

/// The people window's step guard for the shared `m.direct` writer: the
/// exact account selection and this window's cancellation.
struct PeopleGuard<'a> {
    selection: &'a AccountSelection,
    cancellation: &'a PeopleCancellation,
}

impl DirectWriteGuard for PeopleGuard<'_> {
    fn stopped(&self) -> bool {
        !self.selection.is_current() || self.cancellation.is_cancelled()
    }
    fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
        async move {
            tokio::select! {
                _ = self.cancellation.cancelled() => {}
                _ = self.selection.revoked() => {}
            }
        }
    }
}

/// A search term as it will be sent, or why not.
pub fn validate_people_query(input: &str) -> PeopleResult<String> {
    let query = input.trim();
    if query.is_empty()
        || query.chars().count() > MAX_USER_QUERY_CHARS
        || query.len() > MAX_USER_QUERY_CHARS * 4
        || query.chars().any(char::is_control)
    {
        return Err(PeopleError::InvalidQuery);
    }
    Ok(query.to_owned())
}

/// A complete Matrix user ID typed as is, if the input is one.
pub fn exact_user_id(input: &str) -> Option<String> {
    let input = input.trim();
    if !input.starts_with('@') || input.len() > 255 {
        return None;
    }
    UserId::parse(input).ok().map(|user| user.to_string())
}

/// A display name made safe to show: one bounded line, or `None`.
fn shown_name(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let line: String = raw
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                ' '
            } else {
                character
            }
        })
        .take(MAX_PERSON_NAME_CHARS * 2)
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = line.chars().take(MAX_PERSON_NAME_CHARS).collect();
    (!line.is_empty()).then_some(line)
}

/// The directory's answer, bounded, deduplicated and without this account.
fn page_from(results: Vec<search_users::v3::User>, limited: bool, own: &UserId) -> PeoplePage {
    let mut seen = HashSet::new();
    let mut people = Vec::new();
    let mut more = limited;
    for user in results {
        if user.user_id.as_str() == own.as_str() || !seen.insert(user.user_id.clone()) {
            continue;
        }
        if people.len() == MAX_USER_RESULTS {
            more = true;
            break;
        }
        people.push(DirectoryPerson {
            user_id: user.user_id.to_string(),
            display_name: shown_name(user.display_name),
        });
    }
    PeoplePage {
        people,
        limited: more,
    }
}

fn known_direct(mut rooms: Vec<String>) -> KnownDirect {
    rooms.sort();
    match rooms.len() {
        0 => KnownDirect::NoneKnown,
        1 => KnownDirect::One(rooms.remove(0)),
        count => {
            rooms.truncate(MAX_KNOWN_DMS);
            KnownDirect::Several { rooms, count }
        }
    }
}

fn request_failure(error: &HttpError) -> PeopleError {
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        return PeopleError::InvalidResponse;
    }
    match error.as_client_api_error() {
        Some(api)
            if api.status_code.as_u16() == 429
                || matches!(api.error_kind(), Some(ErrorKind::LimitExceeded(_))) =>
        {
            PeopleError::RateLimited
        }
        Some(api) if api.status_code.as_u16() == 403 => PeopleError::NotAllowed,
        _ => PeopleError::Unavailable,
    }
}

fn ensure_scope(selection: &AccountSelection, cancellation: &PeopleCancellation) -> PeopleResult<()> {
    if cancellation.is_cancelled() {
        return Err(PeopleError::Cancelled);
    }
    if !selection.is_current() {
        return Err(PeopleError::AccountChanged);
    }
    Ok(())
}

async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &PeopleCancellation,
    limit: Duration,
    work: impl Future<Output = PeopleResult<T>>,
) -> PeopleResult<T> {
    ensure_scope(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(PeopleError::Cancelled),
        _ = selection.revoked() => Err(PeopleError::AccountChanged),
        result = time::timeout(limit, work) => result.map_err(|_| PeopleError::TimedOut)?,
    }?;
    ensure_scope(selection, cancellation)?;
    Ok(result)
}

/// Retries off, authenticated, bounded: for the directory search, a read.
/// A 401 token refresh may replay it once, which is harmless for a read.
fn single_attempt(timeout: Duration, max_bytes: u64) -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(timeout)
        .force_auth()
        .max_response_bytes(max_bytes)
}

/// The creation's settings: `single_attempt` plus no token-refresh replay,
/// so a 401 ends the attempt instead of resending a mutation that may have
/// applied.
fn single_wire_attempt(timeout: Duration, max_bytes: u64) -> RequestConfig {
    single_attempt(timeout, max_bytes).disable_auth_refresh()
}

impl AccountManager {
    /// The selected account's client and user, only while `selection` is
    /// still exactly the active, eligible account.
    async fn people_client(
        &self,
        selection: &AccountSelection,
        cancellation: &PeopleCancellation,
    ) -> PeopleResult<(Client, OwnedUserId)> {
        guarded(selection, cancellation, SEARCH_TIMEOUT, async {
            ensure_scope(selection, cancellation)?;
            let state = self.state.lock().await;
            ensure_scope(selection, cancellation)?;
            let current = self
                .account_selection(selection.account_id())
                .ok_or(PeopleError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(PeopleError::AccountChanged);
            }
            account_access::ensure_eligible(&state, selection.account_id())
                .map_err(|_| PeopleError::AccountChanged)?;
            let loaded = selected_loaded_for(&state, selection.account_id())
                .map_err(|_| PeopleError::AccountChanged)?;
            let own = loaded
                .client
                .user_id()
                .ok_or(PeopleError::Unavailable)?
                .to_owned();
            Ok((loaded.client.clone(), own))
        })
        .await
    }

    /// Search the selected account's homeserver user directory once.
    pub async fn search_people_for(
        &self,
        selection: &AccountSelection,
        cancellation: &PeopleCancellation,
        query: &str,
    ) -> PeopleResult<PeoplePage> {
        let query = validate_people_query(query)?;
        let (client, own) = self.people_client(selection, cancellation).await?;
        let mut request = search_users::v3::Request::new(query);
        // One extra, so a full page with the account itself removed still
        // fills the bound.
        request.limit = UInt::from(u32::try_from(MAX_USER_RESULTS + 1).unwrap_or(21));
        let response = guarded(selection, cancellation, SEARCH_TIMEOUT, async {
            ensure_scope(selection, cancellation)?;
            client
                .send(request)
                .with_request_config(single_attempt(SEARCH_TIMEOUT, MAX_DIRECTORY_RESPONSE_BYTES))
                .await
                .map_err(|error| request_failure(&error))
        })
        .await?;
        Ok(page_from(response.results, response.limited, &own))
    }

    /// Direct conversations with `user_id` that this device knows of: joined
    /// rooms whose `m.direct` targets are exactly that user. Local only.
    pub async fn known_direct_for(
        &self,
        selection: &AccountSelection,
        cancellation: &PeopleCancellation,
        user_id: &str,
    ) -> PeopleResult<KnownDirect> {
        let user = UserId::parse(user_id).map_err(|_| PeopleError::InvalidUser)?;
        let (client, own) = self.people_client(selection, cancellation).await?;
        if user == own {
            return Err(PeopleError::OwnAccount);
        }
        let target = <&DirectUserIdentifier>::from(&*user);
        let rooms = client
            .joined_rooms()
            .into_iter()
            .filter(|room| {
                let targets = room.direct_targets();
                targets.len() == 1 && targets.contains(target)
            })
            .map(|room| room.room_id().to_string())
            .collect();
        ensure_scope(selection, cancellation)?;
        Ok(known_direct(rooms))
    }

    /// Create one encrypted direct conversation inviting exactly `user_id`.
    /// The creation is a single wire attempt (retries off, no token-refresh
    /// replay); an error after it left, including a close or account change
    /// racing it (the send is raced against both and dropped), is
    /// `CreateUnknown`. Listing it in `m.direct` (the shared bounded writer:
    /// fresh read, re-read before one write, unknown fields kept, a changed
    /// baseline refused) and waiting for it to sync follow; their outcome is
    /// reported, never retried, and never turns the creation into a failure.
    pub async fn create_direct_for(
        &self,
        selection: &AccountSelection,
        cancellation: &PeopleCancellation,
        user_id: &str,
    ) -> PeopleResult<CreatedDirect> {
        let user = UserId::parse(user_id).map_err(|_| PeopleError::InvalidUser)?;
        let (client, own) = self.people_client(selection, cancellation).await?;
        if user == own {
            return Err(PeopleError::OwnAccount);
        }
        let mut request = create_room::v3::Request::new();
        request.invite = vec![user.clone()];
        request.is_direct = true;
        request.preset = Some(create_room::v3::RoomPreset::TrustedPrivateChat);
        // Encrypted from creation: a rejection fails the creation; never a
        // cleartext fallback.
        request.initial_state.push(
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        );
        ensure_scope(selection, cancellation)?;
        let created = guarded(selection, cancellation, CREATE_TIMEOUT, async {
            // Final synchronous application/security and selection check at
            // SDK handoff, after client lookup and request preparation.
            ensure_scope(selection, cancellation)?;
            client
                .send(request)
                .with_request_config(single_wire_attempt(CREATE_TIMEOUT, MAX_CREATE_RESPONSE_BYTES))
                .await
                .map_err(|error| request_failure(&error))
        })
        .await;
        let room_id = match created {
            Ok(response) => response.room_id,
            // The homeserver answered with a refusal: nothing was created.
            Err(PeopleError::NotAllowed) => return Err(PeopleError::NotAllowed),
            Err(PeopleError::RateLimited) => return Err(PeopleError::RateLimited),
            // Anything else after the request may have left, including a
            // close or account change while it was in flight: unknown.
            Err(_) => return Err(PeopleError::CreateUnknown),
        };
        let guard = PeopleGuard {
            selection,
            cancellation,
        };
        let direct_listing =
            crate::direct_account_data::list_direct_room(&client, &own, &user, &room_id, &guard)
                .await;
        let synced = guarded(selection, cancellation, SYNC_TIMEOUT, async {
            Ok(synced_room(&client, &room_id).await)
        })
        .await
        .unwrap_or(false);
        Ok(CreatedDirect {
            room_id: room_id.to_string(),
            direct_listing,
            synced,
        })
    }
}

async fn synced_room(client: &Client, room_id: &RoomId) -> bool {
    client.await_room_remote_echo(room_id).await;
    true
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::user_id;

    use super::*;

    fn person(id: &str, name: Option<&str>) -> search_users::v3::User {
        let mut user = search_users::v3::User::new(UserId::parse(id).unwrap());
        user.display_name = name.map(str::to_owned);
        user
    }

    #[test]
    fn cancelled_or_revoked_people_lookup_releases_a_held_manager_wait() {
        crate::history::tests::runtime().block_on(async {
            let (directory, manager, account) = crate::history::tests::fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let held = manager.state.lock().await;
            {
                let cancellation = PeopleCancellation::new();
                let lookup = manager.people_client(&selection, &cancellation);
                tokio::pin!(lookup);
                assert!(time::timeout(Duration::ZERO, lookup.as_mut()).await.is_err());
                cancellation.cancel();
                let result = time::timeout(Duration::from_secs(1), lookup.as_mut()).await.unwrap();
                assert_eq!(result.err(), Some(PeopleError::Cancelled));
            }
            {
                let cancellation = PeopleCancellation::new();
                let lookup = manager.people_client(&selection, &cancellation);
                tokio::pin!(lookup);
                assert!(time::timeout(Duration::ZERO, lookup.as_mut()).await.is_err());
                manager.publish_active_account(None);
                let result = time::timeout(Duration::from_secs(1), lookup.as_mut()).await.unwrap();
                assert_eq!(result.err(), Some(PeopleError::AccountChanged));
            }
            drop(held);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn ended_application_admission_refuses_before_request_work_is_polled() {
        let runtime = crate::history::tests::runtime();
        runtime.block_on(async {
            let selections = crate::account_selection::Selections::default();
            selections.replace(Some("synthetic"));
            let selection = selections.for_account("synthetic").unwrap();
            let allowed = Arc::new(AtomicBool::new(true));
            let admitted = allowed.clone();
            let cancellation = PeopleCancellation::new().with_admission(Arc::new(
                move || admitted.load(Ordering::Acquire),
            ));
            assert!(!cancellation.is_cancelled());
            allowed.store(false, Ordering::Release);
            let polled = AtomicBool::new(false);
            let result = guarded(&selection, &cancellation, SEARCH_TIMEOUT, async {
                polled.store(true, Ordering::Release);
                Ok(())
            }).await;
            assert_eq!(result, Err(PeopleError::Cancelled));
            assert!(!polled.load(Ordering::Acquire));
            assert!(PeopleGuard { selection: &selection, cancellation: &cancellation }.stopped());
        });
    }

    #[test]
    fn queries_and_exact_ids_are_bounded() {
        assert_eq!(validate_people_query("  synthetic ").unwrap(), "synthetic");
        for invalid in ["", "   ", "two\nlines", &"q".repeat(MAX_USER_QUERY_CHARS + 1)] {
            assert_eq!(validate_people_query(invalid), Err(PeopleError::InvalidQuery));
        }
        assert_eq!(
            exact_user_id(" @synthetic:example.org ").as_deref(),
            Some("@synthetic:example.org")
        );
        assert_eq!(exact_user_id("synthetic"), None);
        assert_eq!(exact_user_id("@incomplete"), None);
        assert_eq!(exact_user_id("#room:example.org"), None);
    }

    #[test]
    fn directory_pages_are_bounded_deduplicated_and_never_list_this_account() {
        let own = user_id!("@me:example.org");
        let mut results = vec![
            person("@me:example.org", Some("Me")),
            person("@a:example.org", Some("Synthetic\nA\u{202e}")),
            person("@a:example.org", Some("Duplicate")),
        ];
        for index in 0..30 {
            results.push(person(&format!("@u{index}:example.org"), None));
        }
        let page = page_from(results, false, own);
        assert_eq!(page.people.len(), MAX_USER_RESULTS);
        assert!(page.limited, "more matched than shown");
        assert_eq!(page.people[0].user_id, "@a:example.org");
        assert_eq!(page.people[0].display_name.as_deref(), Some("Synthetic A"));
        assert!(page.people.iter().all(|p| p.user_id != "@me:example.org"));
        let long = shown_name(Some("n".repeat(1000))).unwrap();
        assert_eq!(long.chars().count(), MAX_PERSON_NAME_CHARS);
        assert_eq!(shown_name(Some("  ".into())), None);
    }

    #[test]
    fn several_and_none_known_are_distinguished() {
        assert_eq!(known_direct(vec![]), KnownDirect::NoneKnown);
        assert_eq!(
            known_direct(vec!["!a:example.org".into()]),
            KnownDirect::One("!a:example.org".into())
        );
        let many: Vec<String> = (0..12).map(|i| format!("!r{i:02}:example.org")).collect();
        match known_direct(many) {
            KnownDirect::Several { rooms, count } => {
                assert_eq!(count, 12);
                assert_eq!(rooms.len(), MAX_KNOWN_DMS);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn requests_are_single_bounded_authenticated_attempts() {
        let shown = format!("{:?}", single_attempt(SEARCH_TIMEOUT, MAX_DIRECTORY_RESPONSE_BYTES));
        assert!(shown.contains("retry_limit: 0") || shown.contains("retry_limit: Some(0)"));
        assert!(shown.contains("force_auth: true"));
        assert!(shown.contains("65536"));
        // The creation alone also refuses the token-refresh replay.
        let create = format!("{:?}", single_wire_attempt(CREATE_TIMEOUT, MAX_CREATE_RESPONSE_BYTES));
        assert!(create.contains("retry_limit: 0") || create.contains("retry_limit: Some(0)"));
        assert!(create.contains("16384"));
        assert_ne!(create, format!("{:?}", single_attempt(CREATE_TIMEOUT, MAX_CREATE_RESPONSE_BYTES)));
        assert_eq!(
            request_failure(&HttpError::ResponseTooLarge { limit: 1 }),
            PeopleError::InvalidResponse
        );
        // Fixed wording carries no query or identifier.
        assert!(!PeopleError::CreateUnknown.to_string().contains('@'));
    }
}
