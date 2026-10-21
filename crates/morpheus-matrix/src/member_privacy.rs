//! The signed-in account's Matrix ignore list (`m.ignored_user_list`) and a
//! bounded, cached view of one room member.
//!
//! Ignoring is account data on the homeserver: it hides the user's messages,
//! invitations and calls from this account's clients and push. It does not
//! kick or ban anyone, does not delete or redact history, and other people
//! in the room still see the user as before.
//!
//! A change is one read-modify-write of the whole account-data object. The
//! person reviews a list read from the homeserver; immediately before the
//! write, a fresh bounded read must still equal that reviewed baseline,
//! otherwise the change is refused rather than overwriting what another
//! client wrote. The baseline is the list's exact raw JSON text, and a
//! change edits only the one `ignored_users` entry: every other top-level
//! field and every other entry is carried as raw JSON, byte for byte, so
//! numbers or values serde_json's `Value` could not represent exactly are
//! never rewritten. An object with a duplicate key is refused (Malformed),
//! never resolved by dropping one. Only an HTTP 404 with `M_NOT_FOUND` reads
//! as "no list yet"; any other failure leaves the baseline unreadable.
//! Immediately before the write the caller's admission check (the app's
//! security-gate generation) must still hold, otherwise nothing is sent
//! (SecurityBusy). Morpheus never parses a list whose raw JSON is over
//! `MAX_IGNORE_LIST_BYTES`: every read reports it unavailable, so nothing is
//! written over it, and no change grows a list past it. The write is
//! configured with no HTTP retries and with
//! `RequestConfig::disable_auth_refresh`, so an expired access token (a 401)
//! is reported instead of refreshed and replayed: the write is sent at most
//! once. That opt-in comes from root's SDK checkpoint d4c5d08; this module
//! does not compile without it. A write whose answer never came, or that was
//! abandoned after it was handed to the SDK, is reported as unknown, never as
//! not sent; the next read shows what the server has.
//!
//! Every wait is bound to one exact [`AccountSelection`] and one
//! [`MemberPrivacyCancellation`], checked before each request leaves and
//! before any result is returned. Error text is fixed: no server body, user
//! ID or account data is put into it or logged.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
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
        OwnedUserId, UserId,
        api::{
            client::config::{get_global_account_data, set_global_account_data},
            error::ErrorKind,
        },
        events::{
            AnyGlobalAccountDataEventContent, GlobalAccountDataEventType,
            room::power_levels::UserPowerLevel,
        },
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::{Map, Value, value::RawValue};
use tokio::{
    sync::{Mutex, Notify},
    time,
};

use crate::{
    AccountManager, AccountSelection, LoadedAccount, State, account_access, account_room,
    selected_loaded_for,
};

/// Entries an ignore list may hold for Morpheus to change it. A larger list
/// is refused, never truncated: a write must not drop entries.
pub const MAX_IGNORE_ENTRIES: usize = 10_000;
/// Longest display name kept for an inspected member, in characters.
pub const MAX_MEMBER_NAME_CHARS: usize = 256;
// Raw JSON bytes an ignore list may have for Morpheus to read it, checked
// before Morpheus parses it. A larger list (huge unknown fields or entries)
// is unreadable: an error, never an empty or truncated list. Compact JSON of
// MAX_IGNORE_ENTRIES entries whose user IDs are as long as the spec allows
// (255 bytes) takes 2.49 MiB; as many usual entries (23 bytes, as in
// `"@name:example.org":{},`) take 0.22 MiB.
const MAX_IGNORE_LIST_BYTES: usize = 3 * 1024 * 1024;
// A read's response body is the list's JSON. The SDK refuses a larger one;
// below this, MAX_IGNORE_LIST_BYTES decides.
const MAX_LIST_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
// A write's answer is an empty object.
const MAX_WRITE_RESPONSE_BYTES: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const MEMBER_TIMEOUT: Duration = Duration::from_secs(5);

// One ignore-list change at a time, whichever view asked for it.
static CHANGE: Mutex<()> = Mutex::const_new(());

/// Why a request did not complete. The text is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MemberPrivacyError {
    #[error("This view is closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("The ignore list is unavailable; refresh before retrying")]
    Unavailable,
    #[error("The ignore list on the server is not in a form Morpheus can change safely")]
    Malformed,
    #[error("The ignore list is too large to change safely here")]
    TooLarge,
    #[error("The ignore list changed elsewhere; refresh and review it again")]
    Changed,
    #[error("The homeserver is limiting requests; wait, then refresh")]
    RateLimited,
    #[error("The request timed out; refresh before retrying")]
    TimedOut,
    #[error("The homeserver refused this change")]
    Refused,
    #[error("No answer came back; refresh to see whether the change applied")]
    Unknown,
    #[error("This sign-in needs renewing; the homeserver did not apply the change")]
    SignInExpired,
    #[error("That is not a valid Matrix user ID")]
    InvalidUser,
    #[error("An account cannot ignore itself")]
    OwnAccount,
    #[error("That person is not in this room's member list on this device")]
    NotMember,
    /// The caller's admission check failed before the write; nothing was
    /// sent. Never retried automatically.
    #[error("Finish the account or security step in progress, then try again.")]
    SecurityBusy,
}

pub type Result<T> = std::result::Result<T, MemberPrivacyError>;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Ends one inspector or ignore-list view and everything it waits for. A
/// write already handed to the homeserver may still apply.
#[derive(Clone, Default)]
pub struct MemberPrivacyCancellation(Arc<CancellationState>);

impl MemberPrivacyCancellation {
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

/// The ignore-list object's raw JSON text exactly as a read found it
/// (`None`: the account has none yet). A change goes ahead only while the
/// server still has the same bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct IgnoreBaseline(Option<String>);

impl fmt::Debug for IgnoreBaseline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IgnoreBaseline { .. }")
    }
}

/// The ignore list as the homeserver had it at one read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoredUsersSnapshot {
    /// Ignored Matrix user IDs, sorted.
    pub users: Vec<String>,
    /// Entries kept verbatim that are not valid user IDs.
    pub unrecognized: usize,
    /// What a change must still find on the server to go ahead.
    pub baseline: IgnoreBaseline,
}

impl IgnoredUsersSnapshot {
    pub fn contains(&self, user_id: &str) -> bool {
        self.users
            .binary_search_by(|user| user.as_str().cmp(user_id))
            .is_ok()
    }
}

/// The ignore list as this device's store has it, as of the last sync.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoredUsers(BTreeSet<OwnedUserId>);

impl IgnoredUsers {
    pub fn contains(&self, user_id: &UserId) -> bool {
        self.0.contains(user_id)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What became of one ignore or unignore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IgnoreChange {
    /// The homeserver accepted the change and a fresh read shows it.
    Confirmed(IgnoredUsersSnapshot),
    /// The homeserver accepted the change; a fresh read could not show it
    /// yet. Refreshing reconciles.
    Unconfirmed,
    /// The list already was as requested; nothing was sent.
    AlreadyDone(IgnoredUsersSnapshot),
}

/// One room member as this device's store has it. Names are bounded; they
/// are the room's own display names, not verified identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDetails {
    pub user_id: String,
    pub display_name: Option<String>,
    /// `join`, `invite`, `leave`, `ban` or `knock`.
    pub membership: String,
    /// `None` for an immutable room creator with infinite power.
    pub power_level: Option<i64>,
    pub is_account_user: bool,
    /// Whether the ignore list as of the last sync names this person;
    /// `None` when it could not be read.
    pub ignored: Option<bool>,
}

impl AccountManager {
    /// The selected account's loaded client state, only while `selection`
    /// is the exact current one and the account is not being removed.
    fn privacy_loaded<'a>(
        &self,
        state: &'a State,
        selection: &AccountSelection,
    ) -> Result<&'a Arc<LoadedAccount>> {
        let current = self.account_selection(selection.account_id());
        let current = current.ok_or(MemberPrivacyError::AccountChanged)?;
        if !selection.same_epoch(&current) {
            return Err(MemberPrivacyError::AccountChanged);
        }
        account_access::ensure_eligible(state, selection.account_id())
            .map_err(|_| MemberPrivacyError::AccountChanged)?;
        selected_loaded_for(state, selection.account_id())
            .map_err(|_| MemberPrivacyError::AccountChanged)
    }

    async fn privacy_client(
        &self,
        selection: &AccountSelection,
        cancellation: &MemberPrivacyCancellation,
    ) -> Result<Client> {
        scope_error(selection, cancellation)?;
        let client = {
            let state = self.state.lock().await;
            self.privacy_loaded(&state, selection)?.client.clone()
        };
        scope_error(selection, cancellation)?;
        Ok(client)
    }

    /// The ignore list as the homeserver has it now: one bounded read.
    pub async fn ignored_users_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &MemberPrivacyCancellation,
    ) -> Result<IgnoredUsersSnapshot> {
        let client = guarded(
            selection,
            cancellation,
            READ_TIMEOUT,
            self.privacy_client(selection, cancellation),
        )
        .await?;
        let found = guarded(selection, cancellation, READ_TIMEOUT, read_list(&client)).await?;
        snapshot(found)
    }

    /// The ignore list as this device's store has it, as of the last sync,
    /// without any request. A list Morpheus cannot read is an error, never
    /// an empty list. Each wait, for the manager's state and then for the
    /// store, ends at once on an account change and after READ_TIMEOUT.
    pub async fn cached_ignored_users_selected(
        &self,
        selection: &AccountSelection,
    ) -> Result<IgnoredUsers> {
        let cancellation = MemberPrivacyCancellation::new();
        let client = guarded(
            selection,
            &cancellation,
            READ_TIMEOUT,
            self.privacy_client(selection, &cancellation),
        )
        .await?;
        let account = client.account();
        let stored = account.account_data_raw(GlobalAccountDataEventType::IgnoredUserList);
        let stored = guarded(selection, &cancellation, READ_TIMEOUT, async {
            stored.await.map_err(|_| MemberPrivacyError::Unavailable)
        })
        .await?;
        let Some(stored) = stored else {
            return Ok(IgnoredUsers::default());
        };
        stored_list(stored.json().get())
    }

    /// Ignore (`ignore` true) or stop ignoring one user. `reviewed` is the
    /// baseline of the list the person reviewed: a fresh bounded read right
    /// before the write must still equal it, otherwise nothing is sent. The
    /// write is sent at most once: no HTTP retries and no replay after a
    /// token refresh (see the module docs). Once it is handed to the SDK, a
    /// revoked selection or a closed view stops the wait and reports Unknown.
    ///
    /// `admitted` is the caller's fence (the app passes its security-gate
    /// generation, captured when the person confirmed). It is checked before
    /// the change lock is awaited and again after the lock and the fresh
    /// read, immediately before the write is handed to the SDK; if it fails
    /// either time, nothing is sent and the result is SecurityBusy. After
    /// that last check the write may leave, and a later gate change does not
    /// make it "not sent": a stop then is Unknown as above.
    pub async fn set_user_ignored_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &MemberPrivacyCancellation,
        user_id: &str,
        ignore: bool,
        reviewed: &IgnoreBaseline,
        admitted: impl Fn() -> bool + Send + Sync,
    ) -> Result<IgnoreChange> {
        let user = parse_user(user_id)?;
        let client = guarded(
            selection,
            cancellation,
            READ_TIMEOUT,
            self.privacy_client(selection, cancellation),
        )
        .await?;
        if client.user_id() == Some(&*user) {
            return Err(MemberPrivacyError::OwnAccount);
        }
        if !admitted() {
            return Err(MemberPrivacyError::SecurityBusy);
        }
        let _change = guarded(selection, cancellation, READ_TIMEOUT, async {
            Ok(CHANGE.lock().await)
        })
        .await?;
        // Revalidate: the server must still hold exactly the reviewed list.
        let fresh = guarded(selection, cancellation, READ_TIMEOUT, read_list(&client)).await?;
        if fresh != reviewed.0 {
            return Err(MemberPrivacyError::Changed);
        }
        let Some(next) = changed_list(fresh.as_deref(), &user, ignore)? else {
            return Ok(IgnoreChange::AlreadyDone(snapshot(fresh)?));
        };
        // The last checks before the write is handed to the SDK: the view,
        // the selection and the caller's fence, rechecked after every wait.
        // From here on the write may reach the homeserver, so every way of
        // stopping is Unknown.
        scope_error(selection, cancellation)?;
        if !admitted() {
            return Err(MemberPrivacyError::SecurityBusy);
        }
        let written = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(MemberPrivacyError::Unknown),
            () = selection.revoked() => Err(MemberPrivacyError::Unknown),
            written = time::timeout(WRITE_TIMEOUT, write_list(&client, &next)) => {
                written.unwrap_or(Err(MemberPrivacyError::Unknown))
            }
        };
        written?;
        // Accepted. A failed read-back is unconfirmed, never a failure.
        let after = guarded(selection, cancellation, READ_TIMEOUT, read_list(&client)).await;
        let after = match after {
            Err(error @ (MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged)) => {
                return Err(error);
            }
            Err(_) => return Ok(IgnoreChange::Unconfirmed),
            Ok(after) => after,
        };
        match snapshot(after) {
            Ok(after) if after.contains(user.as_str()) == ignore => {
                Ok(IgnoreChange::Confirmed(after))
            }
            _ => Ok(IgnoreChange::Unconfirmed),
        }
    }

    /// One member of one room of the selected account, from this device's
    /// store only (no request), with the ignore state as of the last sync.
    pub async fn member_details_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &MemberPrivacyCancellation,
        room_id: &str,
        user_id: &str,
    ) -> Result<MemberDetails> {
        let user = parse_user(user_id)?;
        // The wait for the manager's state is bounded and ends at once when
        // the view closes or the selection changes, like every other wait.
        let room = guarded(selection, cancellation, MEMBER_TIMEOUT, async {
            let state = self.state.lock().await;
            self.privacy_loaded(&state, selection)?;
            account_room(&state, selection.account_id(), room_id)
                .map_err(|_| MemberPrivacyError::Unavailable)
        })
        .await?;
        let member = guarded(selection, cancellation, MEMBER_TIMEOUT, async {
            room.get_member_no_sync(&user)
                .await
                .map_err(|_| MemberPrivacyError::Unavailable)
        })
        .await?;
        let member = member.ok_or(MemberPrivacyError::NotMember)?;
        // The cached reader has no view of its own; this view's cancellation
        // is kept beside it: closing the view drops the read at once.
        let ignored = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MemberPrivacyError::Cancelled),
            ignored = self.cached_ignored_users_selected(selection) => ignored,
        };
        let ignored = match ignored {
            Err(error @ (MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged)) => {
                return Err(error);
            }
            Err(_) => None,
            Ok(ignored) => Some(ignored.contains(&user)),
        };
        scope_error(selection, cancellation)?;
        Ok(MemberDetails {
            user_id: member.user_id().to_string(),
            display_name: member
                .display_name()
                .map(|name| name.chars().take(MAX_MEMBER_NAME_CHARS).collect()),
            membership: member.membership().as_str().to_owned(),
            power_level: match member.power_level() {
                UserPowerLevel::Int(level) => Some(level.into()),
                _ => None,
            },
            is_account_user: member.is_account_user(),
            ignored,
        })
    }
}

fn parse_user(user_id: &str) -> Result<OwnedUserId> {
    UserId::parse(user_id.trim()).map_err(|_| MemberPrivacyError::InvalidUser)
}

/// The list's raw JSON text as one read found it, checked against
/// MAX_IGNORE_LIST_BYTES before anything parses it. Only an HTTP 404 with
/// `M_NOT_FOUND` is "no list"; every other failure is an error.
async fn read_list(client: &Client) -> Result<Option<String>> {
    let user = client.user_id().ok_or(MemberPrivacyError::Unavailable)?;
    let request = get_global_account_data::v3::Request::new(
        user.to_owned(),
        GlobalAccountDataEventType::IgnoredUserList,
    );
    let config = client
        .request_config()
        .timeout(READ_TIMEOUT)
        .max_response_bytes(MAX_LIST_RESPONSE_BYTES);
    match client.send(request).with_request_config(config).await {
        Ok(response) => {
            let raw = within_cap(response.account_data.json().get())?;
            Ok(Some(raw.to_owned()))
        }
        Err(error) if not_found(&error) => Ok(None),
        Err(error) => Err(read_error(&error)),
    }
}

/// The homeserver answered 404 `M_NOT_FOUND`: no list is stored. The same
/// error code under any other status is not taken as an empty list.
fn not_found(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|answer| {
        answer.status_code.as_u16() == 404 && answer.error_kind() == Some(&ErrorKind::NotFound)
    })
}

/// The write: the whole object, sent at most once. No HTTP retries, and an
/// expired token is reported rather than refreshed and replayed (root's
/// `disable_auth_refresh`, SDK checkpoint d4c5d08). A list over
/// MAX_IGNORE_LIST_BYTES, which no read could take back, is refused as
/// TooLarge before anything is sent. `raw` is sent exactly as given.
async fn write_list(client: &Client, raw: &str) -> Result<()> {
    let user = client.user_id().ok_or(MemberPrivacyError::Unavailable)?;
    within_cap(raw).map_err(|_| MemberPrivacyError::TooLarge)?;
    let raw = RawValue::from_string(raw.to_owned());
    let raw = raw.map_err(|_| MemberPrivacyError::Malformed)?;
    let request = set_global_account_data::v3::Request::new_raw(
        user.to_owned(),
        GlobalAccountDataEventType::IgnoredUserList,
        Raw::<AnyGlobalAccountDataEventContent>::from_json(raw),
    );
    let config = RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(WRITE_TIMEOUT)
        .max_response_bytes(MAX_WRITE_RESPONSE_BYTES);
    let sent = client.send(request).with_request_config(config).await;
    sent.map(|_| ()).map_err(|error| write_error(&error))
}

/// The listing of one read's raw JSON. Only `ignored_users` must have the
/// right shape; everything else is kept, and keys that are not user IDs are
/// counted.
fn snapshot(raw: Option<String>) -> Result<IgnoredUsersSnapshot> {
    let mut users = Vec::new();
    let mut unrecognized = 0;
    if let Some(entries) = RawList::parse(raw.as_deref())?.entries {
        for key in entries.keys() {
            match UserId::parse(key.as_str()) {
                Ok(_) => users.push(key.clone()),
                Err(_) => unrecognized += 1,
            }
        }
    }
    users.sort();
    Ok(IgnoredUsersSnapshot {
        users,
        unrecognized,
        baseline: IgnoreBaseline(raw),
    })
}

// A normal map deserializer silently keeps only the last duplicate key.
// Refuse ambiguous objects rather than dropping a field on write.
struct UniqueFields(BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = UniqueFields;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate ignore-list field"));
                    }
                }
                Ok(UniqueFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// A list's raw JSON split only as far as a change needs: every top-level
/// field, and every `ignored_users` entry, stays raw JSON text.
struct RawList {
    fields: BTreeMap<String, Box<RawValue>>,
    entries: Option<BTreeMap<String, Box<RawValue>>>,
}

impl RawList {
    /// `raw` must already have passed `within_cap` (read_list does that
    /// before returning it). Not an object, an `ignored_users` that is not
    /// an object, or a duplicate key is Malformed; more than
    /// MAX_IGNORE_ENTRIES entries is TooLarge.
    fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw else {
            return Ok(Self {
                fields: BTreeMap::new(),
                entries: None,
            });
        };
        let fields: UniqueFields =
            serde_json::from_str(raw).map_err(|_| MemberPrivacyError::Malformed)?;
        let entries = match fields.0.get("ignored_users") {
            None => None,
            Some(entries) => {
                let entries: UniqueFields = serde_json::from_str(entries.get())
                    .map_err(|_| MemberPrivacyError::Malformed)?;
                if entries.0.len() > MAX_IGNORE_ENTRIES {
                    return Err(MemberPrivacyError::TooLarge);
                }
                Some(entries.0)
            }
        };
        Ok(Self {
            fields: fields.0,
            entries,
        })
    }
}

/// `raw` while it is within MAX_IGNORE_LIST_BYTES. Every read checks this
/// before it parses, so a list over the cap is never parsed: it fails closed
/// as Unavailable, never as an empty or truncated list.
fn within_cap(raw: &str) -> Result<&str> {
    if raw.len() > MAX_IGNORE_LIST_BYTES {
        return Err(MemberPrivacyError::Unavailable);
    }
    Ok(raw)
}

/// The strict reading of a stored list's raw JSON.
fn stored_list(raw: &str) -> Result<IgnoredUsers> {
    let raw = within_cap(raw)?;
    // A duplicate key anywhere fails closed: `Value` keeps only the last
    // copy, so a list whose second `ignored_users` is empty would otherwise
    // read as ignoring no one while other clients honour the first.
    if !unique_keys(raw) {
        return Err(MemberPrivacyError::Unavailable);
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| MemberPrivacyError::Unavailable)?;
    strict_list(&value)
}

/// Whether `raw` is JSON in which every object, at any depth, has unique
/// keys (compared after unescaping, as a parser compares them).
fn unique_keys(raw: &str) -> bool {
    struct Checked;
    struct CheckedVisitor;
    impl<'de> Deserialize<'de> for Checked {
        fn deserialize<D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            deserializer.deserialize_any(CheckedVisitor)
        }
    }
    impl<'de> serde::de::Visitor<'de> for CheckedVisitor {
        type Value = Checked;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("JSON whose objects have unique keys")
        }
        fn visit_bool<E>(self, _: bool) -> std::result::Result<Checked, E> {
            Ok(Checked)
        }
        fn visit_i64<E>(self, _: i64) -> std::result::Result<Checked, E> {
            Ok(Checked)
        }
        fn visit_u64<E>(self, _: u64) -> std::result::Result<Checked, E> {
            Ok(Checked)
        }
        fn visit_f64<E>(self, _: f64) -> std::result::Result<Checked, E> {
            Ok(Checked)
        }
        fn visit_str<E>(self, _: &str) -> std::result::Result<Checked, E> {
            Ok(Checked)
        }
        fn visit_unit<E>(self) -> std::result::Result<Checked, E> {
            Ok(Checked)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Checked, A::Error> {
            while seq.next_element::<Checked>()?.is_some() {}
            Ok(Checked)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Checked, A::Error> {
            let mut seen = BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !seen.insert(key) {
                    return Err(serde::de::Error::custom("duplicate key"));
                }
                map.next_value::<Checked>()?;
            }
            Ok(Checked)
        }
    }
    serde_json::from_str::<Checked>(raw).is_ok()
}

/// The `ignored_users` object of a list, `None` for no list or no key.
fn entries(value: Option<&Value>) -> Result<Option<&Map<String, Value>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let object = value.as_object().ok_or(MemberPrivacyError::Malformed)?;
    let entries = match object.get("ignored_users") {
        None => return Ok(None),
        Some(Value::Object(entries)) => entries,
        Some(_) => return Err(MemberPrivacyError::Malformed),
    };
    if entries.len() > MAX_IGNORE_ENTRIES {
        return Err(MemberPrivacyError::TooLarge);
    }
    Ok(Some(entries))
}

/// The list's raw JSON with `user` added or removed; every other top-level
/// field and every other entry is copied as raw JSON text, never through
/// `Value`. `None` when it already is as requested.
fn changed_list(current: Option<&str>, user: &UserId, ignore: bool) -> Result<Option<String>> {
    let RawList {
        mut fields,
        entries,
    } = RawList::parse(current)?;
    let mut entries = entries.unwrap_or_default();
    if ignore {
        if entries.contains_key(user.as_str()) {
            return Ok(None);
        }
        if entries.len() >= MAX_IGNORE_ENTRIES {
            return Err(MemberPrivacyError::TooLarge);
        }
        let empty = RawValue::from_string("{}".to_owned());
        let empty = empty.map_err(|_| MemberPrivacyError::Malformed)?;
        entries.insert(user.to_string(), empty);
    } else if entries.remove(user.as_str()).is_none() {
        return Ok(None);
    }
    let entries = serde_json::value::to_raw_value(&entries);
    let entries = entries.map_err(|_| MemberPrivacyError::Malformed)?;
    fields.insert("ignored_users".to_owned(), entries);
    let next = serde_json::to_string(&fields);
    next.map(Some).map_err(|_| MemberPrivacyError::Malformed)
}

/// The strict reading other features rely on (as the incoming-call policy
/// does): an unreadable list is an error, never an empty one.
fn strict_list(value: &Value) -> Result<IgnoredUsers> {
    let entries = entries(Some(value)).map_err(|_| MemberPrivacyError::Unavailable)?;
    let entries = entries.ok_or(MemberPrivacyError::Unavailable)?;
    let mut users = BTreeSet::new();
    for (key, entry) in entries {
        let user = UserId::parse(key.as_str()).map_err(|_| MemberPrivacyError::Unavailable)?;
        if !entry.is_object() {
            return Err(MemberPrivacyError::Unavailable);
        }
        users.insert(user);
    }
    Ok(IgnoredUsers(users))
}

fn read_error(error: &HttpError) -> MemberPrivacyError {
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => MemberPrivacyError::RateLimited,
        _ => MemberPrivacyError::Unavailable,
    }
}

/// A server answer below 500 means the write was not applied; anything
/// else (no answer, a server error) may have applied.
fn write_error(error: &HttpError) -> MemberPrivacyError {
    let refused = error
        .as_client_api_error()
        .is_some_and(|answer| answer.status_code.is_client_error());
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) if refused => MemberPrivacyError::RateLimited,
        // Rejected at sign-in, before the change was looked at; with
        // disable_auth_refresh it is not replayed.
        Some(ErrorKind::UnknownToken(_)) if refused => MemberPrivacyError::SignInExpired,
        Some(_) if refused => MemberPrivacyError::Refused,
        _ => MemberPrivacyError::Unknown,
    }
}

fn scope_error(
    selection: &AccountSelection,
    cancellation: &MemberPrivacyCancellation,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(MemberPrivacyError::Cancelled);
    }
    if !selection.is_current() {
        return Err(MemberPrivacyError::AccountChanged);
    }
    Ok(())
}

/// Run one wait under a view: it ends at once on cancellation, account
/// change or timeout, and is re-checked before its result is used.
async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &MemberPrivacyCancellation,
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    scope_error(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(MemberPrivacyError::Cancelled),
        _ = selection.revoked() => Err(MemberPrivacyError::AccountChanged),
        result = time::timeout(limit, work) => result.map_err(|_| MemberPrivacyError::TimedOut)?,
    }?;
    scope_error(selection, cancellation)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::tests::{fixture, runtime};
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Mutex as StdMutex, atomic::AtomicUsize},
        thread,
    };

    fn user(id: &str) -> OwnedUserId {
        UserId::parse(id).unwrap()
    }

    #[test]
    fn a_change_keeps_every_field_and_entry_it_does_not_understand() {
        let current = json!({
            "ignored_users": {
                "@a:example.org": {"com.example.note": "kept"},
                "not a user id": {},
            },
            "com.example.extra": [1, 2],
        })
        .to_string();
        let current = Some(current.as_str());
        let parsed = |raw: String| serde_json::from_str::<Value>(&raw).unwrap();
        let added = changed_list(current, &user("@b:example.org"), true);
        let added = parsed(added.unwrap().unwrap());
        assert_eq!(
            added,
            json!({
                "ignored_users": {
                    "@a:example.org": {"com.example.note": "kept"},
                    "@b:example.org": {},
                    "not a user id": {},
                },
                "com.example.extra": [1, 2],
            })
        );
        let removed = changed_list(current, &user("@a:example.org"), false);
        let removed = parsed(removed.unwrap().unwrap());
        assert_eq!(
            removed,
            json!({
                "ignored_users": {"not a user id": {}},
                "com.example.extra": [1, 2],
            })
        );
        // Already as requested: nothing to send.
        let again = changed_list(current, &user("@a:example.org"), true);
        assert_eq!(again, Ok(None));
        let absent = changed_list(current, &user("@z:example.org"), false);
        assert_eq!(absent, Ok(None));
        // No list yet: a new one with just this entry.
        let first = changed_list(None, &user("@b:example.org"), true).unwrap();
        assert_eq!(
            first.as_deref(),
            Some(r#"{"ignored_users":{"@b:example.org":{}}}"#)
        );
        // A shape Morpheus cannot change safely is refused, never replaced:
        // not an object, entries that are not an object, or a duplicate key
        // at either level (a normal parser would silently drop one).
        for malformed in [
            "[]",
            r#"{"ignored_users":[]}"#,
            r#"{"ignored_users":null}"#,
            r#""text""#,
            "not json",
            r#"{"ignored_users":{},"ignored_users":{"@a:example.org":{}}}"#,
            r#"{"ignored_users":{"@a:example.org":{},"@a:example.org":{"x":1}}}"#,
            r#"{"x":1,"x":2,"ignored_users":{}}"#,
        ] {
            let refused = changed_list(Some(malformed), &user("@b:example.org"), true);
            assert_eq!(refused, Err(MemberPrivacyError::Malformed));
            let listed = snapshot(Some(malformed.to_owned()));
            assert_eq!(listed, Err(MemberPrivacyError::Malformed));
        }
        // A list at the bound is never grown or truncated.
        let full: Map<String, Value> = (0..MAX_IGNORE_ENTRIES)
            .map(|index| (format!("@u{index}:example.org"), json!({})))
            .collect();
        let full = json!({"ignored_users": full}).to_string();
        let grown = changed_list(Some(&full), &user("@b:example.org"), true);
        assert_eq!(grown, Err(MemberPrivacyError::TooLarge));
        let listed = snapshot(Some(full)).unwrap();
        assert_eq!(listed.users.len(), MAX_IGNORE_ENTRIES);
    }

    /// Values `Value` cannot hold exactly (a number past f64 or u64, an
    /// exponent, trailing zeros, escapes, nested unknown data) are copied
    /// byte for byte, in other fields and in entries the change does not
    /// touch; and the baseline compares raw bytes, so a difference `Value`
    /// would hide still refuses the change.
    #[test]
    fn a_change_copies_untouched_raw_json_byte_for_byte() {
        let current = concat!(
            r#"{"com.example.big":123456789012345678901234567890,"#,
            r#""com.example.exp":1.50e400,"com.example.zero":1.000,"#,
            r#""com.example.text":"\u00e9\n","#,
            r#""ignored_users":{"@a:example.org":{"n":18446744073709551616},"#,
            r#""@c:example.org":{}}}"#
        );
        let added = changed_list(Some(current), &user("@b:example.org"), true);
        let added = added.unwrap().unwrap();
        assert_eq!(
            added,
            concat!(
                r#"{"com.example.big":123456789012345678901234567890,"#,
                r#""com.example.exp":1.50e400,"com.example.text":"\u00e9\n","#,
                r#""com.example.zero":1.000,"#,
                r#""ignored_users":{"@a:example.org":{"n":18446744073709551616},"#,
                r#""@b:example.org":{},"@c:example.org":{}}}"#
            )
        );
        let removed = changed_list(Some(current), &user("@c:example.org"), false);
        let removed = removed.unwrap().unwrap();
        assert!(removed.contains("123456789012345678901234567890"));
        assert!(removed.contains("1.50e400") && removed.contains("1.000"));
        assert!(removed.contains(r#"{"n":18446744073709551616}"#));
        assert!(!removed.contains("@c:example.org"));
        // The baseline is the exact text: two lists `Value` reads as equal
        // (the same f64 after rounding) are different baselines.
        let one = r#"{"ignored_users":{},"x":0.1}"#;
        let two = r#"{"ignored_users":{},"x":0.10000000000000001}"#;
        let one_value: Value = serde_json::from_str(one).unwrap();
        let two_value: Value = serde_json::from_str(two).unwrap();
        assert_eq!(one_value, two_value);
        let one = snapshot(Some(one.to_owned())).unwrap();
        let two = snapshot(Some(two.to_owned())).unwrap();
        assert_ne!(one.baseline, two.baseline);
    }

    #[test]
    fn listings_are_sorted_and_other_features_read_the_list_strictly() {
        let value = json!({
            "ignored_users": {
                "@b:example.org": {},
                "@a:example.org": {},
                "not a user id": {},
            },
        });
        let listed = snapshot(Some(value.to_string())).unwrap();
        assert_eq!(listed.users, ["@a:example.org", "@b:example.org"]);
        assert_eq!(listed.unrecognized, 1);
        assert!(listed.contains("@b:example.org") && !listed.contains("@c:example.org"));
        assert_eq!(snapshot(None).unwrap().users.len(), 0);
        // The strict reading fails closed: an unreadable list never reads as
        // empty, so a caller cannot take "unreadable" for "not ignored".
        assert_eq!(strict_list(&value), Err(MemberPrivacyError::Unavailable));
        let strict = strict_list(&json!({"ignored_users": {"@a:example.org": {}}}));
        assert!(strict.unwrap().contains(&user("@a:example.org")));
        for unreadable in [
            json!({}),
            json!({"ignored_users": {"@a:example.org": []}}),
            json!({"ignored_users": []}),
        ] {
            assert_eq!(
                strict_list(&unreadable),
                Err(MemberPrivacyError::Unavailable)
            );
        }
    }

    /// A valid list's JSON of exactly `size` bytes: one entry, and an unknown
    /// field padded to fit.
    fn sized_list(size: usize) -> String {
        let head = r#"{"ignored_users":{"@a:example.org":{}},"com.example.pad":""#;
        let pad = "x".repeat(size - head.len() - 2);
        format!("{head}{pad}\"}}")
    }

    #[test]
    fn a_list_over_the_byte_cap_fails_closed_before_it_is_parsed() {
        // One byte over the cap, with a huge unknown field: refused by its
        // length alone, though parsed it would read as a valid list.
        let over = sized_list(MAX_IGNORE_LIST_BYTES + 1);
        assert_eq!(over.len(), MAX_IGNORE_LIST_BYTES + 1);
        let parsed: Value = serde_json::from_str(&over).unwrap();
        assert!(strict_list(&parsed).is_ok());
        assert_eq!(within_cap(&over), Err(MemberPrivacyError::Unavailable));
        assert_eq!(stored_list(&over), Err(MemberPrivacyError::Unavailable));
        // At the cap, the list is read.
        let at = sized_list(MAX_IGNORE_LIST_BYTES);
        let read = stored_list(&at).unwrap();
        assert!(read.len() == 1 && read.contains(&user("@a:example.org")));
        // The cap leaves room for MAX_IGNORE_ENTRIES of the longest user IDs.
        let pad = "x".repeat(236);
        let longest: Map<String, Value> = (0..MAX_IGNORE_ENTRIES)
            .map(|index| (format!("@u{index:05}{pad}:example.org"), json!({})))
            .collect();
        assert!(longest.keys().all(|id| id.len() == 255));
        let longest = json!({"ignored_users": longest}).to_string();
        assert!(longest.len() <= MAX_IGNORE_LIST_BYTES);
        assert_eq!(stored_list(&longest).unwrap().len(), MAX_IGNORE_ENTRIES);
    }

    /// A duplicate key anywhere in a stored list fails closed: a normal
    /// parser keeps the last copy, so a second, empty `ignored_users` would
    /// read as ignoring no one while other clients honour the first list.
    #[test]
    fn the_strict_reader_fails_closed_on_any_duplicate_key() {
        let valid = r#"{"ignored_users":{"@a:example.org":{}},"x":{"y":[1,{"z":2}]}}"#;
        assert!(
            stored_list(valid)
                .unwrap()
                .contains(&user("@a:example.org"))
        );
        for duplicated in [
            // The bypass: a valid first list, an empty last one.
            r#"{"ignored_users":{"@a:example.org":{}},"ignored_users":{}}"#,
            r#"{"ignored_users":{"@a:example.org":{},"@a:example.org":{}}}"#,
            r#"{"ignored_users":{"@a:example.org":{"k":1,"k":2}}}"#,
            r#"{"ignored_users":{"@a:example.org":{}},"x":[{"y":1,"y":1}]}"#,
            r#"{"ignored_users":{"@a:example.org":{}},"x":1,"x":1}"#,
            // Equal after unescaping, as a parser compares keys.
            r#"{"ignored_users":{"@a:example.org":{}},"\u0069gnored_users":{}}"#,
        ] {
            // Value alone would accept it, keeping the last copy.
            assert!(serde_json::from_str::<Value>(duplicated).is_ok());
            assert!(!unique_keys(duplicated), "{duplicated}");
            assert_eq!(
                stored_list(duplicated),
                Err(MemberPrivacyError::Unavailable)
            );
        }
        assert!(!unique_keys("not json"));
        assert!(unique_keys(r#"[{"a":1},{"a":1}]"#));
    }

    /// The details read waits for the manager's state under the view's
    /// guard: a closed view or an account change ends that wait at once.
    #[test]
    fn member_details_waiting_on_the_manager_lock_end_promptly() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let state = manager.state.lock().await;
            let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            let target = "@b:example.org";
            let view = MemberPrivacyCancellation::new();
            let details =
                manager.member_details_selected(&selection, &view, "!r:example.org", target);
            let mut waiting = Box::pin(details);
            assert!(waiting.as_mut().poll(&mut context).is_pending());
            view.cancel();
            let cancelled = time::timeout(Duration::from_secs(2), waiting).await;
            assert_eq!(cancelled, Ok(Err(MemberPrivacyError::Cancelled)));
            let view = MemberPrivacyCancellation::new();
            let details =
                manager.member_details_selected(&selection, &view, "!r:example.org", target);
            let mut waiting = Box::pin(details);
            assert!(waiting.as_mut().poll(&mut context).is_pending());
            manager.publish_active_account(None);
            let changed = time::timeout(Duration::from_secs(2), waiting).await;
            assert_eq!(changed, Ok(Err(MemberPrivacyError::AccountChanged)));
            drop(state);
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// While the manager's state lock is held elsewhere, a cached read that
    /// waits for its client still ends promptly and fails closed: at its
    /// bound, when its view closes, and on an account change.
    #[test]
    fn a_cached_read_waiting_on_the_manager_lock_ends_promptly() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let state = manager.state.lock().await;
            let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            // Bounded: the guarded acquisition ends at its limit.
            let view = MemberPrivacyCancellation::new();
            let acquiring = manager.privacy_client(&selection, &view);
            let bounded = guarded(&selection, &view, Duration::from_millis(20), acquiring).await;
            assert!(matches!(bounded, Err(MemberPrivacyError::TimedOut)));
            // Cancelled: closing the view ends it at once.
            let acquiring = manager.privacy_client(&selection, &view);
            let mut waiting = Box::pin(guarded(&selection, &view, READ_TIMEOUT, acquiring));
            assert!(waiting.as_mut().poll(&mut context).is_pending());
            view.cancel();
            let cancelled = time::timeout(Duration::from_secs(2), waiting).await;
            assert!(matches!(cancelled, Ok(Err(MemberPrivacyError::Cancelled))));
            // The reader itself: an account change ends its wait at once.
            let mut reading = Box::pin(manager.cached_ignored_users_selected(&selection));
            assert!(reading.as_mut().poll(&mut context).is_pending());
            manager.publish_active_account(None);
            let read = time::timeout(Duration::from_secs(2), reading).await;
            assert_eq!(read, Ok(Err(MemberPrivacyError::AccountChanged)));
            drop(state);
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// A loopback homeserver holding one account's ignore list: it answers
    /// the version check, serves the current list, and records each write
    /// (which then becomes the current list). With `hold_writes` it takes a
    /// write and never answers it, which leaves the request pending inside
    /// the SDK the way a held token refresh does. With `fail_reads` every
    /// read gets that answer instead. No real account or network.
    struct ListServer {
        url: String,
        list: Arc<StdMutex<Option<String>>>,
        writes: Arc<StdMutex<Vec<String>>>,
        reads: Arc<AtomicUsize>,
        hold: Arc<AtomicBool>,
        failure: Arc<StdMutex<Option<(u16, String)>>>,
        stopped: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl ListServer {
        fn start(list: Option<Value>) -> Self {
            Self::start_raw(list.map(|value| value.to_string()))
        }

        /// The list is served as exactly this text.
        fn start_raw(list: Option<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let list = Arc::new(StdMutex::new(list));
            let writes = Arc::new(StdMutex::new(Vec::new()));
            let reads = Arc::new(AtomicUsize::new(0));
            let hold = Arc::new(AtomicBool::new(false));
            let failure = Arc::new(StdMutex::new(None::<(u16, String)>));
            let stopped = Arc::new(AtomicBool::new(false));
            let (held, written, counted, stop) =
                (list.clone(), writes.clone(), reads.clone(), stopped.clone());
            let holding = hold.clone();
            let failing = failure.clone();
            let worker = thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).unwrap();
                    let timeout = Some(Duration::from_secs(2));
                    stream.set_read_timeout(timeout).unwrap();
                    let (line, body) = read_request(&mut stream);
                    let (status, reply) = if line.contains("/_matrix/client/versions") {
                        (200, json!({"versions": ["v1.15"]}).to_string())
                    } else if line.contains("/account_data/m.ignored_user_list") {
                        if line.starts_with("PUT ") {
                            written.lock().unwrap().push(body.clone());
                            while holding.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
                                thread::sleep(Duration::from_millis(5));
                            }
                            *held.lock().unwrap() = Some(body);
                            (200, "{}".to_owned())
                        } else {
                            counted.fetch_add(1, Ordering::SeqCst);
                            let failed = failing.lock().unwrap().clone();
                            match held.lock().unwrap().clone() {
                                _ if failed.is_some() => failed.unwrap(),
                                Some(list) => (200, list),
                                None => {
                                    let missing = json!({"errcode": "M_NOT_FOUND", "error": "x"});
                                    (404, missing.to_string())
                                }
                            }
                        }
                    } else {
                        let unknown = json!({"errcode": "M_UNRECOGNIZED", "error": "x"});
                        (404, unknown.to_string())
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self {
                url,
                list,
                writes,
                reads,
                hold,
                failure,
                stopped,
                worker: Some(worker),
            }
        }

        /// Every read from now on gets this status and body.
        fn fail_reads(&self, status: u16, body: Value) {
            *self.failure.lock().unwrap() = Some((status, body.to_string()));
        }

        /// Writes are taken from now on but never answered.
        fn hold_writes(&self) {
            self.hold.store(true, Ordering::SeqCst);
        }

        /// Another client changes the list.
        fn replace(&self, list: Value) {
            *self.list.lock().unwrap() = Some(list.to_string());
        }

        /// Another client stores exactly this text.
        fn replace_raw(&self, list: &str) {
            *self.list.lock().unwrap() = Some(list.to_owned());
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Drop for ListServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// The request line and body of one HTTP/1.1 request.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(size) = stream.read(&mut chunk) {
            if size == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..size]);
            let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_string();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    let length = name.eq_ignore_ascii_case("content-length");
                    length.then(|| value.trim().parse::<usize>().ok()).flatten()
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                let line = headers.lines().next().unwrap_or("").to_owned();
                let body = String::from_utf8_lossy(&request[end + 4..end + 4 + length]);
                return (line, body.to_string());
            }
        }
        (String::new(), String::new())
    }

    /// The selected account of a fresh manager, signed in with a synthetic
    /// session against `server`.
    async fn signed_in(
        server: &ListServer,
    ) -> (std::path::PathBuf, Arc<AccountManager>, AccountSelection) {
        let account = crate::AccountInfo {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: "@casey:example.org".into(),
            device_id: "PRIVACY-FIXTURE".into(),
            homeserver: server.url.clone(),
        };
        let (directory, manager, account) = fixture(Some(account)).await;
        let client = {
            let state = manager.state.lock().await;
            state.loaded[&account.id].client.clone()
        };
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: user("@casey:example.org"),
                device_id: "PRIVACY-FIXTURE".into(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: "synthetic access token".into(),
                refresh_token: None,
            },
        };
        let restored = client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await;
        restored.unwrap();
        let selection = manager.account_selection(&account.id).unwrap();
        (directory, manager, selection)
    }

    /// This device's store holds `list` as the account's ignore list, the
    /// way sync leaves it.
    async fn store_list(manager: &AccountManager, selection: &AccountSelection, list: &str) {
        let client = {
            let state = manager.state.lock().await;
            state.loaded[selection.account_id()].client.clone()
        };
        let content: Value = serde_json::from_str(list).unwrap();
        let event = json!({"type": "m.ignored_user_list", "content": content});
        let event = Raw::from_json(serde_json::value::to_raw_value(&event).unwrap());
        let mut changes = matrix_sdk::StateChanges::default();
        let key = GlobalAccountDataEventType::IgnoredUserList;
        changes.account_data.insert(key, event);
        let saved = matrix_sdk::StateStore::save_changes(client.state_store(), &changes).await;
        saved.unwrap();
    }

    #[test]
    fn a_change_is_refused_on_a_changed_baseline_and_otherwise_written_once() {
        runtime().block_on(async {
            let server = ListServer::start(Some(json!({
                "ignored_users": {"@a:example.org": {"com.example.note": "kept"}},
                "com.example.extra": true,
            })));
            let (directory, manager, selection) = signed_in(&server).await;
            let view = MemberPrivacyCancellation::new();
            let reviewed = manager
                .ignored_users_selected(&selection, &view)
                .await
                .unwrap();
            assert_eq!(reviewed.users, ["@a:example.org"]);
            // Another client changes the list after the review: refused,
            // nothing written.
            server.replace(json!({"ignored_users": {"@c:example.org": {}}}));
            let target = "@b:example.org";
            let refused = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await;
            assert_eq!(refused, Err(MemberPrivacyError::Changed));
            assert!(server.writes().is_empty());
            // Reviewed again: one write, which keeps everything else.
            server.replace(json!({
                "ignored_users": {"@a:example.org": {"com.example.note": "kept"}},
                "com.example.extra": true,
            }));
            let reviewed = manager
                .ignored_users_selected(&selection, &view)
                .await
                .unwrap();
            let applied = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await
                .unwrap();
            let IgnoreChange::Confirmed(after) = applied else {
                panic!("the read-back did not confirm the change");
            };
            assert_eq!(after.users, ["@a:example.org", target]);
            let writes = server.writes();
            assert_eq!(writes.len(), 1);
            let written: Value = serde_json::from_str(&writes[0]).unwrap();
            assert_eq!(
                written,
                json!({
                    "ignored_users": {
                        "@a:example.org": {"com.example.note": "kept"},
                        "@b:example.org": {},
                    },
                    "com.example.extra": true,
                })
            );
            // Already ignored: nothing more is sent.
            let again = manager
                .set_user_ignored_selected(&selection, &view, target, true, &after.baseline, || {
                    true
                })
                .await;
            assert!(matches!(again, Ok(IgnoreChange::AlreadyDone(_))));
            assert_eq!(server.writes().len(), 1);
            // The account itself can never be ignored.
            let own = "@casey:example.org";
            let own = manager
                .set_user_ignored_selected(&selection, &view, own, true, &after.baseline, || true)
                .await;
            assert_eq!(own, Err(MemberPrivacyError::OwnAccount));
            // A closed view, or another account selection, sends nothing.
            let reads = server.reads.load(Ordering::SeqCst);
            let closed = MemberPrivacyCancellation::new();
            closed.cancel();
            let unignore = manager
                .set_user_ignored_selected(
                    &selection,
                    &closed,
                    target,
                    false,
                    &after.baseline,
                    || true,
                )
                .await;
            assert_eq!(unignore, Err(MemberPrivacyError::Cancelled));
            manager.publish_active_account(None);
            let switched = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    false,
                    &after.baseline,
                    || true,
                )
                .await;
            assert_eq!(switched, Err(MemberPrivacyError::AccountChanged));
            assert_eq!(server.reads.load(Ordering::SeqCst), reads);
            assert_eq!(server.writes().len(), 1);
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// Once a write is handed to the SDK it may reach the homeserver: a
    /// revoked selection or a closed view ends the wait at once (not after
    /// the 60-second limit) and reports Unknown, never "not sent".
    #[test]
    fn a_write_still_waiting_when_the_account_changes_or_view_closes_is_unknown() {
        for close_view in [false, true] {
            runtime().block_on(async {
                let server = ListServer::start(Some(json!({"ignored_users": {}})));
                let (directory, manager, selection) = signed_in(&server).await;
                let view = MemberPrivacyCancellation::new();
                let reviewed = manager
                    .ignored_users_selected(&selection, &view)
                    .await
                    .unwrap();
                server.hold_writes();
                let target = "@b:example.org";
                let change = manager.set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                );
                let interrupt = async {
                    // The homeserver has the write and holds it unanswered.
                    while server.writes().is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    if close_view {
                        view.cancel();
                    } else {
                        manager.publish_active_account(None);
                    }
                };
                let both = async { tokio::join!(change, interrupt) };
                let ended = tokio::time::timeout(Duration::from_secs(10), both).await;
                let (changed, ()) = ended.unwrap();
                assert_eq!(changed, Err(MemberPrivacyError::Unknown));
                assert_eq!(server.writes().len(), 1);
                drop(manager);
                std::fs::remove_dir_all(directory).unwrap();
            });
        }
    }

    /// The cached reader refuses a stored list over the byte cap before it
    /// parses it, and reads one at the cap; neither sends a request.
    #[test]
    fn the_cached_reader_fails_closed_on_a_stored_list_over_the_byte_cap() {
        runtime().block_on(async {
            let server = ListServer::start(None);
            let (directory, manager, selection) = signed_in(&server).await;
            let over = sized_list(MAX_IGNORE_LIST_BYTES + 1);
            store_list(&manager, &selection, &over).await;
            let over = manager.cached_ignored_users_selected(&selection).await;
            assert_eq!(over, Err(MemberPrivacyError::Unavailable));
            let at = sized_list(MAX_IGNORE_LIST_BYTES);
            store_list(&manager, &selection, &at).await;
            let at = manager.cached_ignored_users_selected(&selection).await;
            assert!(at.unwrap().contains(&user("@a:example.org")));
            assert_eq!(server.reads.load(Ordering::SeqCst), 0);
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// Under the byte cap, a list is read with every field and a change keeps
    /// them; over it, a read fails closed and nothing is written; and no
    /// change grows a list past it.
    #[test]
    fn lists_under_the_byte_cap_keep_their_fields_and_none_over_it_is_written() {
        runtime().block_on(async {
            let under = sized_list(MAX_IGNORE_LIST_BYTES - 64);
            let under: Value = serde_json::from_str(&under).unwrap();
            let server = ListServer::start(Some(under.clone()));
            let (directory, manager, selection) = signed_in(&server).await;
            let view = MemberPrivacyCancellation::new();
            let reviewed = manager
                .ignored_users_selected(&selection, &view)
                .await
                .unwrap();
            assert_eq!(reviewed.users, ["@a:example.org"]);
            assert_eq!(reviewed.baseline, IgnoreBaseline(Some(under.to_string())));
            let target = "@b:example.org";
            let applied = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await
                .unwrap();
            let IgnoreChange::Confirmed(after) = applied else {
                panic!("the read-back did not confirm the change");
            };
            let writes = server.writes();
            assert_eq!(writes.len(), 1);
            let written: Value = serde_json::from_str(&writes[0]).unwrap();
            assert_eq!(written["com.example.pad"], under["com.example.pad"]);
            let entries = json!({"@a:example.org": {}, "@b:example.org": {}});
            assert_eq!(written["ignored_users"], entries);
            // Over the cap: the read fails closed, and a change sends nothing.
            let over = sized_list(MAX_IGNORE_LIST_BYTES + 1);
            server.replace(serde_json::from_str(&over).unwrap());
            let read = manager.ignored_users_selected(&selection, &view).await;
            assert_eq!(read, Err(MemberPrivacyError::Unavailable));
            let unignore = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    false,
                    &after.baseline,
                    || true,
                )
                .await;
            assert_eq!(unignore, Err(MemberPrivacyError::Unavailable));
            assert_eq!(server.writes().len(), 1);
            // One more entry would put this list over the cap: refused before
            // anything is sent, so no change makes the list unreadable.
            let full = sized_list(MAX_IGNORE_LIST_BYTES - 10);
            server.replace(serde_json::from_str(&full).unwrap());
            let reviewed = manager
                .ignored_users_selected(&selection, &view)
                .await
                .unwrap();
            let grown = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await;
            assert_eq!(grown, Err(MemberPrivacyError::TooLarge));
            assert_eq!(server.writes().len(), 1);
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// Only a 404 with M_NOT_FOUND reads as "no list yet". The same code
    /// under another status, or another code under 404, leaves the list
    /// unreadable, and a change against a "no list" baseline sends nothing.
    /// (Server errors are not served here: the read's SDK retry policy is
    /// left as configured, and `read_error` maps them to Unavailable too.)
    #[test]
    fn only_a_404_not_found_reads_as_no_list() {
        runtime().block_on(async {
            let server = ListServer::start(None);
            let (directory, manager, selection) = signed_in(&server).await;
            let view = MemberPrivacyCancellation::new();
            let none = manager.ignored_users_selected(&selection, &view).await;
            let none = none.unwrap();
            assert_eq!(none.baseline, IgnoreBaseline(None));
            let not_found = json!({"errcode": "M_NOT_FOUND", "error": "x"});
            let unrecognized = json!({"errcode": "M_UNRECOGNIZED", "error": "x"});
            let unknown = json!({"errcode": "M_UNKNOWN", "error": "x"});
            for (status, body) in [
                (403, not_found.clone()),
                (400, not_found.clone()),
                (410, not_found),
                (404, unrecognized),
                (404, unknown.clone()),
                (405, unknown),
            ] {
                server.fail_reads(status, body);
                let read = manager.ignored_users_selected(&selection, &view).await;
                assert_eq!(read, Err(MemberPrivacyError::Unavailable), "{status}");
                let target = "@b:example.org";
                let change = manager
                    .set_user_ignored_selected(
                        &selection,
                        &view,
                        target,
                        true,
                        &none.baseline,
                        || true,
                    )
                    .await;
                assert_eq!(change, Err(MemberPrivacyError::Unavailable), "{status}");
            }
            assert!(server.writes().is_empty());
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// A list `Value` could not round-trip is reviewed and changed with
    /// every untouched byte kept, end to end; and the fresh read compares
    /// raw bytes, so a change `Value` would call equal is still refused.
    #[test]
    fn untouched_raw_json_survives_a_change_through_the_homeserver() {
        runtime().block_on(async {
            let stored = concat!(
                r#"{"com.example.big":123456789012345678901234567890,"#,
                r#""ignored_users":{"@a:example.org":{"n":1.50e400}},"x":0.1}"#
            );
            let server = ListServer::start_raw(Some(stored.to_owned()));
            let (directory, manager, selection) = signed_in(&server).await;
            let view = MemberPrivacyCancellation::new();
            let reviewed = manager.ignored_users_selected(&selection, &view).await;
            let reviewed = reviewed.unwrap();
            assert_eq!(reviewed.baseline, IgnoreBaseline(Some(stored.to_owned())));
            // Equal as `Value`, different as text: refused, nothing sent.
            server.replace_raw(&stored.replace("0.1}", "0.10000000000000001}"));
            let target = "@b:example.org";
            let refused = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await;
            assert_eq!(refused, Err(MemberPrivacyError::Changed));
            assert!(server.writes().is_empty());
            server.replace_raw(stored);
            let applied = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await;
            assert!(matches!(applied, Ok(IgnoreChange::Confirmed(_))));
            assert_eq!(
                server.writes(),
                [concat!(
                    r#"{"com.example.big":123456789012345678901234567890,"#,
                    r#""ignored_users":{"@a:example.org":{"n":1.50e400},"#,
                    r#""@b:example.org":{}},"x":0.1}"#
                )]
            );
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    /// The caller's fence is checked before the change lock is awaited and
    /// again immediately before the write: a security step that began while
    /// the lock or the fresh read was pending sends nothing (SecurityBusy),
    /// and nothing retries it.
    #[test]
    fn a_change_whose_admission_ends_before_the_write_sends_nothing() {
        runtime().block_on(async {
            let server = ListServer::start(Some(json!({"ignored_users": {}})));
            let (directory, manager, selection) = signed_in(&server).await;
            let view = MemberPrivacyCancellation::new();
            let reviewed = manager.ignored_users_selected(&selection, &view).await;
            let reviewed = reviewed.unwrap();
            let target = "@b:example.org";
            // Refused from the start: not even the fresh read is sent.
            let reads = server.reads.load(Ordering::SeqCst);
            let closed = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || false,
                )
                .await;
            assert_eq!(closed, Err(MemberPrivacyError::SecurityBusy));
            assert_eq!(server.reads.load(Ordering::SeqCst), reads);
            // Admitted at first; the gate moves during the lock and the fresh
            // read: the read happens, the write never does.
            let checks = AtomicUsize::new(0);
            let moved = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || checks.fetch_add(1, Ordering::SeqCst) == 0,
                )
                .await;
            assert_eq!(moved, Err(MemberPrivacyError::SecurityBusy));
            assert_eq!(checks.load(Ordering::SeqCst), 2);
            assert_eq!(server.reads.load(Ordering::SeqCst), reads + 1);
            assert!(server.writes().is_empty());
            // Admitted at both checks: exactly one write.
            let applied = manager
                .set_user_ignored_selected(
                    &selection,
                    &view,
                    target,
                    true,
                    &reviewed.baseline,
                    || true,
                )
                .await;
            assert!(matches!(applied, Ok(IgnoreChange::Confirmed(_))));
            assert_eq!(server.writes().len(), 1);
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }
}
