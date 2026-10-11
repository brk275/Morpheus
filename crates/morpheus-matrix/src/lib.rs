//! Matrix account and messaging core for the Morpheus desktop client.
//!
//! One Matrix `Client` and SQLite store belong to each account/device. Only the
//! selected account has an active sync loop. The OS credential store holds the
//! session tokens and the passphrase for that account's SQLite store.

#![recursion_limit = "512"]

mod authentication;
use authentication::{PendingSso, StoredSession};

mod media;
pub use media::{
    AttachmentKind, AttachmentMetadata, AttachmentSendUpdate, DownloadedAttachment,
    MAX_ATTACHMENT_BYTES, QueuedAttachment,
};
mod discovery;
pub use discovery::{
    MatrixLinkTarget, PublicRoomInfo, PublicRoomsPage, PublicRoomsQuery, ResolvedRoomAlias,
    parse_matrix_link_target,
};
mod search;
pub use search::MessageSearchResult;
mod formatting;
pub use formatting::TextFormat;
mod compose;
pub use compose::{ComposeError, ComposeText, MentionSpan, MentionTarget, RestoredCompose};
mod account_selection;
mod admin;
mod rich_text;
pub use account_selection::AccountSelection;
pub use admin::RoomMemberInfo;

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use eyeball_im::Vector;
use futures_util::StreamExt;
use matrix_sdk::{
    Client, ComposerDraft, ComposerDraftType, RoomState,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{
        BackupDownloadStrategy, EncryptionSettings, VerificationState,
        backups::BackupState,
        recovery::RecoveryState,
        verification::{SasVerification, VerificationRequest},
    },
    room::Room,
    room::edit::EditedContent,
    ruma::{
        EventId, RoomId, RoomOrAliasId, ServerName, UserId,
        api::{
            FeatureFlag,
            client::{room::create_room, space::get_hierarchy},
        },
        events::{
            InitialStateEvent,
            key::verification::{VerificationMethod, request::ToDeviceKeyVerificationRequestEvent},
            room::encryption::RoomEncryptionEventContent,
            room::message::{
                MessageFormat, MessageType, RoomMessageEventContent,
                RoomMessageEventContentWithoutRelation,
            },
            space::child::SpaceChildEventContent,
        },
        room::RoomType,
        serde::Raw,
    },
    search_index::SearchIndexStoreKind,
    send_queue::RoomSendQueueUpdate,
};
use matrix_sdk_ui::sync_service::{Error as ServiceError, State as ServiceState, SyncService};
use matrix_sdk_ui::timeline::{
    EventSendState, RoomExt, Timeline, TimelineDetails, TimelineEventItemId, TimelineFocus,
    TimelineItem,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, broadcast, oneshot, watch},
    task::JoinHandle,
};
use tracing::warn;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const REGISTRY_VERSION: u8 = 1;
const SECRET_VERSION: u8 = 1;
const KEYRING_SERVICE: &str = "Morpheus Matrix";

pub type Result<T> = std::result::Result<T, MatrixCoreError>;

#[derive(Debug, thiserror::Error)]
pub enum MatrixCoreError {
    #[error(transparent)]
    Compose(#[from] ComposeError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid homeserver URL: {0}")]
    InvalidHomeserver(String),
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("this Matrix account and device is already saved")]
    DuplicateAccount,
    #[error("no active Matrix account")]
    NoActiveAccount,
    #[error("the selected Matrix account changed before this action could run")]
    SelectedAccountChanged,
    #[error("the selected room changed before this action could run")]
    SelectedRoomChanged,
    #[error(
        "The message may already be queued. Keep the draft and check the send queue before retrying."
    )]
    ComposedSendUncertain,
    #[error("room not found: {0}")]
    RoomNotFound(String),
    #[error("invalid room ID: {0}")]
    InvalidRoomId(String),
    #[error("invalid Matrix room link: {0}")]
    InvalidMatrixLink(String),
    #[error("invalid public room directory request: {0}")]
    InvalidDirectoryQuery(String),
    #[error("invalid message search query: {0}")]
    InvalidSearchQuery(String),
    #[error("invalid event ID: {0}")]
    InvalidEventId(String),
    #[error("invalid media: {0}")]
    InvalidMedia(String),
    #[error("attachment exceeds {maximum} bytes (received {actual} bytes)")]
    MediaTooLarge { maximum: u64, actual: u64 },
    #[error("this event is not a media attachment")]
    NotMediaAttachment,
    #[error("invalid user ID: {0}")]
    InvalidUserId(String),
    #[error("invalid room power level: {0}")]
    InvalidPowerLevel(String),
    #[error("account registry is invalid: {0}")]
    InvalidRegistry(String),
    #[error("stored Matrix session is invalid or does not match this account")]
    InvalidSession,
    #[error("SSO callback is invalid: {0}")]
    InvalidSsoCallback(String),
    #[error("OS credential store error: {0}")]
    Keyring(String),
    #[error("Matrix SDK error: {0}")]
    Sdk(String),
    #[error("{0} login is not implemented yet")]
    UnsupportedLogin(&'static str),
}

/// Public identity shown in the account picker. Contains no access tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// Stable local identifier; never derived from a user-controlled path.
    pub id: String,
    pub user_id: String,
    pub device_id: String,
    pub homeserver: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomInfo {
    pub room_id: String,
    pub name: String,
    pub topic: Option<String>,
    pub membership: RoomMembership,
    pub is_direct: bool,
    pub is_space: bool,
    pub avatar_mxc: Option<String>,
    pub unread_messages: u64,
    pub unread_notifications: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomMembership {
    Joined,
    Invited,
    Knocked,
    Left,
    Banned,
}

impl From<RoomState> for RoomMembership {
    fn from(state: RoomState) -> Self {
        match state {
            RoomState::Joined => Self::Joined,
            RoomState::Invited => Self::Invited,
            RoomState::Knocked => Self::Knocked,
            RoomState::Left => Self::Left,
            RoomState::Banned => Self::Banned,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Stopped,
    Starting,
    Running { backend: SyncBackend },
    Retrying { after_seconds: u64, reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncBackend {
    /// Sliding Sync with the MSC4354 sticky extension, required by native RTC.
    SlidingSticky,
    /// Broadly compatible chat sync; native RTC sticky memberships are absent.
    Classic,
}

/// Select the sync adapter for an account manager. `ClassicOnly` is useful
/// when diagnosing a homeserver's sliding-sync implementation and for
/// exercising the fallback path against a modern test server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncPolicy {
    #[default]
    Auto,
    ClassicOnly,
}

/// Plain data for a native UI. Non-message timeline entries are omitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageInfo {
    pub key: String,
    pub event_id: Option<String>,
    pub sender: String,
    pub body: String,
    /// A narrow, presentation-only Markdown projection of Matrix formatted
    /// HTML. Unsupported markup leaves this empty so the UI uses `body`.
    #[serde(default)]
    pub rich_markdown: Option<String>,
    /// Plain accessibility text derived from the accepted rich projection.
    /// Incoming Matrix `body` may differ from `formatted_body`.
    #[serde(default)]
    pub rich_accessible: Option<String>,
    /// A display-only basename for file, image, audio, and video messages.
    /// Media URLs and decryption keys never enter the UI projection.
    #[serde(default)]
    pub attachment_name: Option<String>,
    pub reply_preview: Option<String>,
    pub reaction_summary: String,
    pub timestamp_ms: u64,
    pub is_own: bool,
    pub is_local_echo: bool,
    pub send_state: MessageSendState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub event_id: String,
    pub relevance_score: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceChildInfo {
    pub room_id: String,
    pub via_servers: Vec<String>,
    pub suggested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceNodeInfo {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_mxc: Option<String>,
    pub is_space: bool,
    pub children: Vec<SpaceChildInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceHierarchyPage {
    pub nodes: Vec<SpaceNodeInfo>,
    pub next_batch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceVerificationStatus {
    Unknown,
    Verified,
    Unverified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    Unknown,
    Enabled,
    Disabled,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    Unknown,
    Creating,
    Enabling,
    Resuming,
    Enabled,
    Downloading,
    Disabling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityInfo {
    pub device_verification: DeviceVerificationStatus,
    pub recovery: RecoveryStatus,
    pub backup: BackupStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationInfo {
    pub flow_id: String,
    pub other_user_id: String,
    /// Only the request initiator should start the SAS method. The other
    /// device accepts that method once its start event arrives.
    pub we_started: bool,
    pub is_ready: bool,
    pub is_done: bool,
    pub is_cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SasEmojiInfo {
    pub symbol: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SasPresentation {
    pub emoji: Option<Vec<SasEmojiInfo>>,
    pub decimals: Option<[u16; 3]>,
}

/// Recovery keys are intentionally excluded from Debug and serialization and
/// zeroized when this value is dropped. Show once and ask the user to save it.
pub struct RecoveryKey {
    secret: Zeroizing<String>,
}

impl RecoveryKey {
    pub fn reveal(&self) -> &str {
        self.secret.as_str()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSendState {
    Pending,
    Failed,
    Sent,
}

/// Outcome of a single queued room message, associated by its SDK
/// transaction ID rather than by message body or room timeline position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedSendOutcome {
    Sent,
    /// The user explicitly cancelled the retained request.
    Failed,
    /// The observer ended; the SDK may still hold a persistent request.
    Unknown,
}

/// A send error does not remove an SDK request. Callers must keep observing
/// its transaction instead of asking the user to send a duplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedSendUpdate {
    Sent,
    Retained { recoverable: bool },
    Cancelled,
    Unknown,
}

pub struct QueuedMessage {
    transaction_id: String,
    handle: matrix_sdk::send_queue::SendHandle,
    updates: broadcast::Receiver<RoomSendQueueUpdate>,
    finished: bool,
}

impl QueuedMessage {
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub async fn abort(&self) -> Result<bool> {
        self.handle
            .abort()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }

    pub async fn retry_wedged(&self) -> Result<()> {
        self.handle
            .unwedge()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }

    pub async fn next_update(&mut self) -> QueuedSendUpdate {
        if self.finished {
            return QueuedSendUpdate::Unknown;
        }
        let update = next_queued_send_update(&mut self.updates, &self.transaction_id).await;
        self.finished = matches!(
            update,
            QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled | QueuedSendUpdate::Unknown
        );
        update
    }

    pub async fn wait_outcome(mut self) -> QueuedSendOutcome {
        loop {
            match self.next_update().await {
                QueuedSendUpdate::Sent => return QueuedSendOutcome::Sent,
                QueuedSendUpdate::Retained { .. } => {}
                QueuedSendUpdate::Cancelled => return QueuedSendOutcome::Failed,
                QueuedSendUpdate::Unknown => return QueuedSendOutcome::Unknown,
            }
        }
    }
}

async fn next_queued_send_update(
    updates: &mut broadcast::Receiver<RoomSendQueueUpdate>,
    transaction_id: &str,
) -> QueuedSendUpdate {
    loop {
        match updates.recv().await {
            Ok(RoomSendQueueUpdate::SentEvent {
                transaction_id: event_transaction_id,
                ..
            }) if event_transaction_id.as_str() == transaction_id => {
                return QueuedSendUpdate::Sent;
            }
            Ok(RoomSendQueueUpdate::SendError {
                transaction_id: event_transaction_id,
                is_recoverable,
                ..
            }) if event_transaction_id.as_str() == transaction_id => {
                return QueuedSendUpdate::Retained {
                    recoverable: is_recoverable,
                };
            }
            Ok(RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: event_transaction_id,
            }) if event_transaction_id.as_str() == transaction_id => {
                return QueuedSendUpdate::Cancelled;
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return QueuedSendUpdate::Unknown,
        }
    }
}

/// A selected account's live message list. Drop it when the room or account
/// changes; the underlying SDK timeline subscriber is then stopped.
pub struct MessageFeed {
    pub account_id: String,
    pub room_id: String,
    updates: watch::Receiver<Vec<MessageInfo>>,
    task: JoinHandle<()>,
}

impl MessageFeed {
    pub fn subscribe(&self) -> watch::Receiver<Vec<MessageInfo>> {
        self.updates.clone()
    }

    pub fn snapshot(&self) -> Vec<MessageInfo> {
        self.updates.borrow().clone()
    }
}

impl Drop for MessageFeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogoutResult {
    /// False if the homeserver could not be reached. Local credentials and
    /// store are still removed; the old device may remain visible remotely.
    pub remote_logout_succeeded: bool,
}

/// Open `browser_url` in the system browser after the application starts a
/// loopback listener at `redirect_url`. Pass the received full callback URL
/// and `attempt_id` to `finish_sso`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsoAttemptInfo {
    pub attempt_id: String,
    pub browser_url: String,
    pub redirect_url: String,
    /// Callback state field chosen by the negotiated authentication protocol.
    pub state_parameter: &'static str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Registry {
    version: u8,
    active_account_id: Option<String>,
    accounts: Vec<AccountInfo>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            active_account_id: None,
            accounts: Vec::new(),
        }
    }
}

impl Registry {
    fn validate(&self) -> Result<()> {
        if self.version != REGISTRY_VERSION {
            return Err(MatrixCoreError::InvalidRegistry(
                "unsupported registry version".into(),
            ));
        }
        let mut ids = HashSet::new();
        for account in &self.accounts {
            if Uuid::parse_str(&account.id).is_err() || !ids.insert(account.id.as_str()) {
                return Err(MatrixCoreError::InvalidRegistry(
                    "invalid or repeated account ID".into(),
                ));
            }
            validate_homeserver(&account.homeserver)?;
        }
        if let Some(id) = &self.active_account_id {
            if !ids.contains(id.as_str()) {
                return Err(MatrixCoreError::InvalidRegistry(
                    "active account is missing".into(),
                ));
            }
        }
        Ok(())
    }
}

// Stored only in Keychain, Credential Manager, or Secret Service.
#[derive(Serialize, Deserialize)]
struct StoredSecret {
    version: u8,
    store_passphrase: String,
    session: StoredSession,
}

struct LoadedAccount {
    client: Client,
    timelines: Mutex<HashMap<String, Arc<Timeline>>>,
    composed_edits: Mutex<HashMap<(String, String), composed_adapter::PendingEdit>>,
    incoming_verification_tx: watch::Sender<Vec<VerificationInfo>>,
    token_task: Mutex<Option<SyncTask>>,
}

impl Drop for LoadedAccount {
    fn drop(&mut self) {
        if let Some(token_task) = self.token_task.get_mut().take() {
            token_task.task.abort();
        }
    }
}

struct State {
    registry: Registry,
    loaded: HashMap<String, Arc<LoadedAccount>>,
    pending_sso: HashMap<String, PendingSso>,
    sync_task: Option<SyncTask>,
    queue_recovery_task: Option<JoinHandle<()>>,
}

struct SyncTask {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// Owns account switching and the sole live Matrix sync loop.
///
/// `open` only reads the non-secret account list. Call `restore_last_active`
/// after constructing the UI so a locked/unavailable keyring can be handled
/// without losing access to the account picker.
pub struct AccountManager {
    data_dir: PathBuf,
    sync_policy: SyncPolicy,
    state: Mutex<State>,
    active_tx: watch::Sender<Option<AccountInfo>>,
    selections: account_selection::Selections,
    sync_tx: watch::Sender<SyncStatus>,
}

impl AccountManager {
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_sync_policy(data_dir, SyncPolicy::Auto)
    }

    pub fn open_with_sync_policy(
        data_dir: impl Into<PathBuf>,
        sync_policy: SyncPolicy,
    ) -> Result<Self> {
        let data_dir = data_dir.into();
        fs::create_dir_all(data_dir.join("accounts"))?;
        let registry_path = data_dir.join("accounts.json");
        let registry: Registry = match fs::read(registry_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e.into()),
        };
        registry.validate()?;
        authentication::cleanup_pending_stores(&data_dir, &registry)?;
        let (active_tx, _) = watch::channel(None);
        let (sync_tx, _) = watch::channel(SyncStatus::Stopped);
        Ok(Self {
            data_dir,
            sync_policy,
            state: Mutex::new(State {
                registry,
                loaded: HashMap::new(),
                pending_sso: HashMap::new(),
                sync_task: None,
                queue_recovery_task: None,
            }),
            active_tx,
            selections: account_selection::Selections::default(),
            sync_tx,
        })
    }

    pub fn subscribe_active_account(&self) -> watch::Receiver<Option<AccountInfo>> {
        self.active_tx.subscribe()
    }

    /// Capture ownership of queued work synchronously, before its first await.
    /// A watch receiver can coalesce A → B → A; this token is still revoked
    /// by that sequence. None while restoring, switching, logging out or stopped.
    pub fn account_selection(&self, account_id: &str) -> Option<AccountSelection> {
        self.selections.for_account(account_id)
    }

    fn publish_active_account(&self, account: Option<AccountInfo>) {
        self.selections
            .replace(account.as_ref().map(|account| account.id.as_str()));
        self.active_tx.send_replace(account);
    }

    pub fn subscribe_sync_status(&self) -> watch::Receiver<SyncStatus> {
        self.sync_tx.subscribe()
    }

    pub async fn accounts(&self) -> Vec<AccountInfo> {
        self.state.lock().await.registry.accounts.clone()
    }

    pub async fn active_account(&self) -> Option<AccountInfo> {
        let state = self.state.lock().await;
        let id = state.registry.active_account_id.as_ref()?;
        state
            .registry
            .accounts
            .iter()
            .find(|a| &a.id == id)
            .cloned()
    }

    /// Restore the account selected during the previous launch. A missing or
    /// locked OS credential store is returned as an error, never replaced by a
    /// plaintext token file.
    pub async fn restore_last_active(&self) -> Result<Option<AccountInfo>> {
        let id = self.state.lock().await.registry.active_account_id.clone();
        match id {
            Some(id) => self.switch_to(&id).await.map(Some),
            None => Ok(None),
        }
    }

    /// Password login creates a new Matrix device and its own encrypted local
    /// store. The password itself is never persisted.
    pub async fn add_password_account(
        &self,
        homeserver: &str,
        username: &str,
        password: &str,
    ) -> Result<AccountInfo> {
        let homeserver = validate_homeserver(homeserver)?;
        let mut state = self.state.lock().await;
        let id = Uuid::new_v4().to_string();
        let store_path = self.store_path(&id);
        fs::create_dir_all(&store_path)?;
        let passphrase = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let client = match build_client(&homeserver, &store_path, &passphrase).await {
            Ok(client) => client,
            Err(e) => {
                let _ = fs::remove_dir_all(&store_path);
                return Err(e);
            }
        };
        let response = match client
            .matrix_auth()
            .login_username(username, password)
            .initial_device_display_name("Morpheus Desktop")
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                let _ = fs::remove_dir_all(&store_path);
                return Err(MatrixCoreError::Sdk(e.to_string()));
            }
        };
        let session: MatrixSession = (&response).into();
        let account = AccountInfo {
            id: id.clone(),
            user_id: session.meta.user_id.to_string(),
            device_id: session.meta.device_id.to_string(),
            homeserver,
        };
        if state
            .registry
            .accounts
            .iter()
            .any(|a| a.user_id == account.user_id && a.device_id == account.device_id)
        {
            let _ = client.logout().await;
            let _ = fs::remove_dir_all(&store_path);
            return Err(MatrixCoreError::DuplicateAccount);
        }
        if let Err(error) = start_event_cache(&client) {
            let _ = client.logout().await;
            let _ = fs::remove_dir_all(&store_path);
            return Err(error);
        }
        if let Err(e) = save_secret(
            &id,
            StoredSecret {
                version: SECRET_VERSION,
                store_passphrase: passphrase.clone(),
                session: StoredSession::Matrix(session),
            },
        )
        .await
        {
            let _ = client.logout().await;
            let _ = fs::remove_dir_all(&store_path);
            return Err(e);
        }
        let mut next = state.registry.clone();
        next.accounts.push(account.clone());
        next.active_account_id = Some(id.clone());
        if let Err(e) = save_registry(&self.data_dir, &next) {
            let _ = delete_secret(&id).await;
            let _ = client.logout().await;
            let _ = fs::remove_dir_all(&store_path);
            return Err(e);
        }
        self.selections.replace(None);
        stop_sync(&mut state).await;
        let loaded = Arc::new(LoadedAccount::new(client, id.clone(), passphrase));
        state.loaded.insert(id, loaded.clone());
        state.registry = next;
        self.publish_active_account(Some(account.clone()));
        self.start_sync(&mut state, loaded.client.clone()).await;
        Ok(account)
    }

    /// Select an existing account. This completes only after the previous
    /// account's sync task has stopped; the new one then starts in background.
    pub async fn switch_to(&self, id: &str) -> Result<AccountInfo> {
        let mut state = self.state.lock().await;
        let account = state
            .registry
            .accounts
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| MatrixCoreError::AccountNotFound(id.into()))?;
        if state.registry.active_account_id.as_deref() == Some(id)
            && state
                .sync_task
                .as_ref()
                .is_some_and(|sync| !sync.task.is_finished())
        {
            // Re-selecting the active account is an explicit retry of any
            // room queue paused by a recoverable send error. No new message
            // is enqueued, so this cannot duplicate a transaction.
            if let Some(loaded) = state.loaded.get(id) {
                loaded.client.send_queue().set_enabled(true).await;
            }
            return Ok(account);
        }
        let loaded = match state.loaded.get(id) {
            Some(loaded) => loaded.clone(),
            None => {
                let secret = load_secret(id).await?;
                if secret.version != SECRET_VERSION
                    || secret.store_passphrase.is_empty()
                    || secret.session.meta().user_id.as_str() != account.user_id
                    || secret.session.meta().device_id.as_str() != account.device_id
                {
                    return Err(MatrixCoreError::InvalidSession);
                }
                let client = build_client(
                    &account.homeserver,
                    &self.store_path(id),
                    &secret.store_passphrase,
                )
                .await?;
                client
                    .restore_session(secret.session.into_auth())
                    .await
                    .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
                start_event_cache(&client)?;
                let loaded = Arc::new(LoadedAccount::new(
                    client,
                    id.into(),
                    secret.store_passphrase,
                ));
                state.loaded.insert(id.into(), loaded.clone());
                loaded
            }
        };
        let mut next = state.registry.clone();
        next.active_account_id = Some(id.into());
        save_registry(&self.data_dir, &next)?;
        self.selections.replace(None);
        stop_sync(&mut state).await;
        state.registry = next;
        self.publish_active_account(Some(account.clone()));
        self.start_sync(&mut state, loaded.client.clone()).await;
        Ok(account)
    }

    /// Remove an account from this device. Remote logout is attempted first,
    /// but a network failure cannot prevent removal of local credentials.
    /// The result reports whether the server accepted the logout.
    pub async fn logout_account(&self, id: &str) -> Result<LogoutResult> {
        let mut state = self.state.lock().await;
        let account = state
            .registry
            .accounts
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| MatrixCoreError::AccountNotFound(id.into()))?;
        let was_active = state.registry.active_account_id.as_deref() == Some(id);
        let mut next = state.registry.clone();
        next.accounts.retain(|a| a.id != id);
        if was_active {
            next.active_account_id = None;
        }
        if was_active {
            self.selections.replace(None);
            stop_sync(&mut state).await;
            self.sync_tx.send_replace(SyncStatus::Stopped);
        }
        let client = if let Some(loaded) = state.loaded.remove(id) {
            // Drain any in-flight token write before deleting the credential.
            // Otherwise a late session refresh could recreate it after logout.
            loaded.stop_token_persistence().await;
            Some(loaded.client.clone())
        } else {
            match load_secret(id).await {
                Ok(secret) if secret.version == SECRET_VERSION => {
                    match build_client(
                        &account.homeserver,
                        &self.store_path(id),
                        &secret.store_passphrase,
                    )
                    .await
                    {
                        Ok(client) => {
                            if let Err(e) = client.restore_session(secret.session.into_auth()).await
                            {
                                warn!(account_id = id, error = %e, "could not restore session for remote logout");
                                None
                            } else {
                                Some(client)
                            }
                        }
                        Err(e) => {
                            warn!(account_id = id, error = %e, "could not open client for remote logout");
                            None
                        }
                    }
                }
                Ok(_) => None,
                Err(e) => {
                    warn!(account_id = id, error = %e, "could not read session for remote logout");
                    None
                }
            }
        };
        let remote_logout_succeeded = if let Some(client) = client {
            match tokio::time::timeout(Duration::from_secs(15), client.logout()).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    warn!(account_id = id, error = %e, "remote Matrix logout failed");
                    false
                }
                Err(_) => {
                    warn!(account_id = id, "remote Matrix logout timed out");
                    false
                }
            }
        } else {
            false
        };
        // Keep the account in the registry until the credential is gone. A
        // keyring failure may leave a valid local token, so the user must be
        // able to find this account and retry the logout.
        delete_secret(id).await?;
        save_registry(&self.data_dir, &next)?;
        state.registry = next;
        if was_active {
            self.publish_active_account(None);
        }
        let store_result = fs::remove_dir_all(self.data_dir.join("accounts").join(id));
        if let Err(e) = store_result {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
        }
        Ok(LogoutResult {
            remote_logout_succeeded,
        })
    }

    /// Clone of the selected SDK client for native RTC and advanced Matrix
    /// features. Callers must discard it when `subscribe_active_account` changes.
    pub async fn active_client(&self) -> Result<Client> {
        Ok(self.active_loaded().await?.client.clone())
    }

    /// Room updates are scoped to the returned account ID. Resubscribe after
    /// an account change; the old receiver belongs to the old client.
    pub async fn subscribe_room_updates(
        &self,
    ) -> Result<(String, broadcast::Receiver<matrix_sdk::sync::RoomUpdates>)> {
        let state = self.state.lock().await;
        let id = state
            .registry
            .active_account_id
            .as_ref()
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let loaded = state
            .loaded
            .get(id)
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        Ok((id.clone(), loaded.client.subscribe_to_all_room_updates()))
    }

    /// Snapshot of joined and invited rooms. Re-read after room updates.
    pub async fn rooms_snapshot(&self) -> Result<Vec<RoomInfo>> {
        let loaded = self.active_loaded().await?;
        let mut rooms = Vec::new();
        for room in loaded.client.rooms() {
            if matches!(room.state(), RoomState::Left | RoomState::Banned) {
                continue;
            }
            rooms.push(room_info(&room).await);
        }
        rooms.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.room_id.cmp(&b.room_id))
        });
        Ok(rooms)
    }

    pub async fn invites_snapshot(&self) -> Result<Vec<RoomInfo>> {
        let loaded = self.active_loaded().await?;
        let mut rooms = Vec::new();
        for room in loaded.client.invited_rooms() {
            rooms.push(room_info(&room).await);
        }
        rooms.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.room_id.cmp(&b.room_id))
        });
        Ok(rooms)
    }

    /// A live SDK timeline, with initial items, vector diffs, pagination, and
    /// local echoes via `Timeline::subscribe`. It is scoped to the current
    /// account; discard on account change.
    pub async fn timeline(&self, room_id: &str) -> Result<Arc<Timeline>> {
        let loaded = self.active_loaded().await?;
        loaded.timeline(room_id).await
    }

    /// A thread-focused timeline. Sending on it automatically adds the
    /// correct thread relationship and reply fallback.
    pub async fn thread_timeline(
        &self,
        room_id: &str,
        root_event_id: &str,
    ) -> Result<Arc<Timeline>> {
        let loaded = self.active_loaded().await?;
        loaded.thread_timeline(room_id, root_event_id).await
    }

    /// Native UI friendly message snapshot for the selected room. This omits
    /// virtual markers and non-message events; the raw `timeline` remains
    /// available for richer rendering.
    pub async fn messages_snapshot(&self, room_id: &str) -> Result<Vec<MessageInfo>> {
        let state = self.state.lock().await;
        let id = state
            .registry
            .active_account_id
            .as_ref()
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let loaded = state
            .loaded
            .get(id)
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let timeline = loaded.timeline(room_id).await?;
        let (items, _) = timeline.subscribe().await;
        Ok(items.iter().filter_map(|item| message_info(item)).collect())
    }

    /// Live message snapshots for the selected room. The stream tracks SDK
    /// vector diffs, so edits, send states, redactions, and removals update it.
    pub async fn message_feed(&self, room_id: &str) -> Result<MessageFeed> {
        let state = self.state.lock().await;
        let account_id = state
            .registry
            .active_account_id
            .as_ref()
            .ok_or(MatrixCoreError::NoActiveAccount)?
            .clone();
        let loaded = state
            .loaded
            .get(&account_id)
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let timeline = loaded.timeline(room_id).await?;
        Ok(message_feed_from_timeline(account_id, room_id, timeline).await)
    }

    /// A native UI friendly stream of the root and replies in one thread.
    /// Both account and room are checked before the subscriber is created.
    pub async fn thread_message_feed_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        root_event_id: &str,
    ) -> Result<MessageFeed> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
        Ok(message_feed_from_timeline(expected_account_id.to_owned(), room_id, timeline).await)
    }

    /// Queue a reply only for the account the user saw when composing it.
    pub async fn send_reply_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        reply_to: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        self.send_reply_formatted_for(
            expected_account_id,
            room_id,
            reply_to,
            body,
            TextFormat::Plain,
        )
        .await
    }

    pub async fn send_reply_formatted_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        reply_to: &str,
        body: impl Into<String>,
        format: TextFormat,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let timeline = loaded.timeline(room_id).await?;
        let event_id = parse_event_id(reply_to)?;
        timeline
            .send_reply(formatting::related_text(body.into(), format), event_id)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(())
    }

    pub async fn send_thread_text_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        root_event_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        self.send_thread_formatted_for(
            expected_account_id,
            room_id,
            root_event_id,
            body,
            TextFormat::Plain,
        )
        .await
    }

    pub async fn send_thread_formatted_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        root_event_id: &str,
        body: impl Into<String>,
        format: TextFormat,
    ) -> Result<()> {
        self.send_thread_formatted_tracked_for(
            expected_account_id,
            room_id,
            root_event_id,
            body,
            format,
        )
        .await
        .map(|_| ())
    }

    /// Subscribe before queueing the thread reply so even an immediate server
    /// acknowledgement is observed. The returned observer owns no account lock.
    pub async fn send_thread_formatted_tracked_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        root_event_id: &str,
        body: impl Into<String>,
        format: TextFormat,
    ) -> Result<QueuedMessage> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let room = account_room(&state, expected_account_id, room_id)?;
        let (_, updates) = room
            .send_queue()
            .subscribe()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
        let handle = timeline
            .send(formatting::room_text(body.into(), format).into())
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(QueuedMessage {
            transaction_id: handle.transaction_id().to_string(),
            handle,
            updates,
            finished: false,
        })
    }

    pub async fn edit_text_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        self.edit_formatted_for(
            expected_account_id,
            room_id,
            event_id,
            body,
            TextFormat::Plain,
        )
        .await
    }

    pub async fn edit_formatted_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
        body: impl Into<String>,
        format: TextFormat,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let timeline = loaded.timeline(room_id).await?;
        let item_id = TimelineEventItemId::EventId(parse_event_id(event_id)?);
        timeline
            .edit(
                &item_id,
                EditedContent::RoomMessage(formatting::related_text(body.into(), format)),
            )
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn toggle_reaction_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
        reaction: &str,
    ) -> Result<bool> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let timeline = loaded.timeline(room_id).await?;
        let item_id = TimelineEventItemId::EventId(parse_event_id(event_id)?);
        timeline
            .toggle_reaction(&item_id, reaction)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Queue a plain text message in the selected room. Observe the timeline
    /// send state for remote acknowledgement or failure.
    pub async fn send_text(&self, room_id: &str, body: impl Into<String>) -> Result<()> {
        let active = self
            .active_account()
            .await
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        self.send_text_for(&active.id, room_id, body).await
    }

    /// Queue a message only if the account the user saw is still selected.
    /// This prevents a delayed UI callback from sending into a similarly named
    /// room after an account switch.
    pub async fn send_text_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        self.send_text_tracked_for(expected_account_id, room_id, body)
            .await
            .map(|_| ())
    }

    /// Queue a message and return a receiver for the server's result. The
    /// subscription starts before queueing, so a fast homeserver response is
    /// still associated with this exact transaction ID.
    pub async fn send_text_tracked_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        body: impl Into<String>,
    ) -> Result<QueuedMessage> {
        self.send_formatted_tracked_for(expected_account_id, room_id, body, TextFormat::Plain)
            .await
    }

    pub async fn send_formatted_tracked_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        body: impl Into<String>,
        format: TextFormat,
    ) -> Result<QueuedMessage> {
        // Hold the switch lock through queueing the send. A switch cannot
        // redirect a command issued for the previously selected account.
        let state = self.state.lock().await;
        if state.registry.active_account_id.as_deref() != Some(expected_account_id) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let id = state
            .registry
            .active_account_id
            .as_ref()
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let loaded = state
            .loaded
            .get(id)
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let room = account_room(&state, expected_account_id, room_id)?;
        let (_, updates) = room
            .send_queue()
            .subscribe()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        let timeline = loaded.timeline(room_id).await?;
        let handle = timeline
            .send(formatting::room_text(body.into(), format).into())
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(QueuedMessage {
            transaction_id: handle.transaction_id().to_string(),
            handle,
            updates,
            finished: false,
        })
    }

    /// Read a room draft from this account's encrypted Matrix SQLite store.
    /// The account must already be loaded on this device. A missing draft is
    /// represented by an empty string, matching the native composer.
    pub async fn load_composer_draft_for(&self, account_id: &str, room_id: &str) -> Result<String> {
        self.load_scoped_composer_draft_for(account_id, room_id, None)
            .await
    }

    /// The optional thread root is a separate encrypted SDK storage key, never
    /// part of the room draft body or a plaintext application preference.
    pub async fn load_scoped_composer_draft_for(
        &self,
        account_id: &str,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> Result<String> {
        self.load_composed_draft_for(account_id, room_id, thread_root)
            .await
            .map(|draft| draft.text.body)
    }

    pub async fn load_composed_draft_for(
        &self,
        account_id: &str,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> Result<RestoredCompose> {
        let thread_root = thread_root.map(parse_event_id).transpose()?;
        let state = self.state.lock().await;
        let room = account_room(&state, account_id, room_id)?;
        let draft = room
            .load_composer_draft(thread_root.as_deref())
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(draft.map_or_else(
            || ComposeText::restore_draft(String::new(), None),
            |draft| ComposeText::restore_draft(draft.plain_text, draft.html_text.as_deref()),
        ))
    }

    /// Save or clear a room draft in that account's encrypted Matrix SQLite
    /// store. Holding the account lock excludes logout while the write runs;
    /// a delayed write cannot recreate data after an account is removed.
    pub async fn save_composer_draft_for(
        &self,
        account_id: &str,
        room_id: &str,
        body: &str,
    ) -> Result<()> {
        self.save_scoped_composer_draft_for(account_id, room_id, None, body)
            .await
    }

    pub async fn save_scoped_composer_draft_for(
        &self,
        account_id: &str,
        room_id: &str,
        thread_root: Option<&str>,
        body: &str,
    ) -> Result<()> {
        self.save_composed_draft_for(account_id, room_id, thread_root, &ComposeText::plain(body))
            .await
    }

    pub async fn save_composed_draft_for(
        &self,
        account_id: &str,
        room_id: &str,
        thread_root: Option<&str>,
        text: &ComposeText,
    ) -> Result<()> {
        let html = text.draft_html()?;
        let thread_root = thread_root.map(parse_event_id).transpose()?;
        let state = self.state.lock().await;
        let room = account_room(&state, account_id, room_id)?;
        if text.body.is_empty() {
            room.clear_composer_draft(thread_root.as_deref())
                .await
                .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
        } else {
            room.save_composer_draft(
                ComposerDraft {
                    plain_text: text.body.clone(),
                    html_text: Some(html),
                    draft_type: ComposerDraftType::NewMessage,
                    attachments: Vec::new(),
                },
                thread_root.as_deref(),
            )
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
        }
    }

    pub async fn send_reply(
        &self,
        room_id: &str,
        reply_to: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let timeline = loaded.timeline(room_id).await?;
        let event_id = parse_event_id(reply_to)?;
        timeline
            .send_reply(
                RoomMessageEventContentWithoutRelation::text_plain(body.into()),
                event_id,
            )
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(())
    }

    pub async fn send_thread_text(
        &self,
        room_id: &str,
        root_event_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
        timeline
            .send(RoomMessageEventContent::text_plain(body.into()).into())
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(())
    }

    pub async fn edit_text(
        &self,
        room_id: &str,
        event_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let timeline = loaded.timeline(room_id).await?;
        let item_id = TimelineEventItemId::EventId(parse_event_id(event_id)?);
        timeline
            .edit(
                &item_id,
                EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                    body.into(),
                )),
            )
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Returns true if added, false if an existing reaction was removed.
    pub async fn toggle_reaction(
        &self,
        room_id: &str,
        event_id: &str,
        reaction: &str,
    ) -> Result<bool> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let timeline = loaded.timeline(room_id).await?;
        let item_id = TimelineEventItemId::EventId(parse_event_id(event_id)?);
        timeline
            .toggle_reaction(&item_id, reaction)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn redact_message(
        &self,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let timeline = loaded.timeline(room_id).await?;
        let item_id = TimelineEventItemId::EventId(parse_event_id(event_id)?);
        timeline
            .redact(&item_id, reason)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Load older items. Returns true when the start of the room was reached.
    pub async fn paginate_backwards(&self, room_id: &str, count: u16) -> Result<bool> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let timeline = loaded.timeline(room_id).await?;
        timeline
            .paginate_backwards(count)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Account-bound pagination for a desktop request that may outlive the
    /// selected room or account. A late click cannot fetch another account's
    /// timeline after a switch.
    pub async fn paginate_backwards_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        count: u16,
    ) -> Result<bool> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let timeline = loaded.timeline(room_id).await?;
        timeline
            .paginate_backwards(count)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Search locally indexed events in a room. More history is indexed as it
    /// syncs or paginates; this does not query a homeserver search endpoint.
    pub async fn search_room(
        &self,
        room_id: &str,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchHit>> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        let result = room
            .search(query, limit.min(100), Some(offset))
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(result
            .events
            .into_iter()
            .map(|(relevance_score, event_id)| SearchHit {
                event_id: event_id.to_string(),
                relevance_score,
            })
            .collect())
    }

    pub async fn create_private_room(&self, name: &str) -> Result<String> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let mut request = create_room::v3::Request::new();
        request.name = Some(name.into());
        request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
        // Install encryption in the creation request, before any members can
        // join or messages can be sent. A homeserver rejection fails creation;
        // never return a cleartext room as a private-room fallback.
        request.initial_state.push(
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        );
        let room = loaded
            .client
            .create_room(request)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(room.room_id().to_string())
    }

    pub async fn create_space(&self, name: &str) -> Result<String> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let mut request = create_room::v3::Request::new();
        request.name = Some(name.into());
        request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
        let mut creation = create_room::v3::CreationContent::new();
        creation.room_type = Some(RoomType::Space);
        request.creation_content = Some(Raw::new(&creation)?);
        let room = loaded
            .client
            .create_room(request)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(room.room_id().to_string())
    }

    /// One page of the server's space hierarchy, including each node's child
    /// links. Follow `next_batch` until it is `None` for the whole graph.
    pub async fn space_hierarchy(
        &self,
        space_id: &str,
        from: Option<&str>,
        limit: u32,
    ) -> Result<SpaceHierarchyPage> {
        let client = {
            let state = self.state.lock().await;
            selected_loaded(&state)?.client.clone()
        };
        let id =
            RoomId::parse(space_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let mut request = get_hierarchy::v1::Request::new(id);
        request.from = from.map(str::to_owned);
        request.limit = Some(limit.clamp(1, 100).into());
        let response = client
            .send(request)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(SpaceHierarchyPage {
            nodes: response.rooms.into_iter().map(space_node_info).collect(),
            next_batch: response.next_batch,
        })
    }

    pub async fn link_space_child(
        &self,
        space_id: &str,
        child_id: &str,
        via_servers: &[String],
        suggested: bool,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let space = selected_room(&state, space_id)?;
        if !space.is_space() {
            return Err(MatrixCoreError::InvalidRoomId(
                "parent room is not a space".into(),
            ));
        }
        let child =
            RoomId::parse(child_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        if via_servers.is_empty() {
            return Err(MatrixCoreError::InvalidRegistry(
                "space child must have at least one via server".into(),
            ));
        }
        let via = via_servers
            .iter()
            .map(|server| {
                ServerName::parse(server)
                    .map_err(|e| MatrixCoreError::InvalidRegistry(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut content = SpaceChildEventContent::new(via);
        content.suggested = suggested;
        space
            .send_state_event_for_key(&child, content)
            .await
            .map(|_| ())
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn unlink_space_child(&self, space_id: &str, child_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let space = selected_room(&state, space_id)?;
        let child =
            RoomId::parse(child_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        space
            .send_state_event_for_key(&child, SpaceChildEventContent::new(vec![]))
            .await
            .map(|_| ())
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn create_dm(&self, user_id: &str) -> Result<String> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let user_id = parse_user_id(user_id)?;
        let room = loaded
            .client
            .create_dm(&user_id)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(room.room_id().to_string())
    }

    pub async fn join_room(&self, room_or_alias_id: &str) -> Result<String> {
        let state = self.state.lock().await;
        let loaded = selected_loaded(&state)?;
        let id = RoomOrAliasId::parse(room_or_alias_id)
            .map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let room = loaded
            .client
            .join_room_by_id_or_alias(&id, &[])
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(room.room_id().to_string())
    }

    pub async fn leave_room(&self, room_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        selected_room(&state, room_id)?
            .leave()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn accept_invite(&self, room_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        if room.state() != RoomState::Invited {
            return Err(MatrixCoreError::InvalidRegistry(
                "room is not an invitation".into(),
            ));
        }
        room.join()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn reject_invite(&self, room_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        if room.state() != RoomState::Invited {
            return Err(MatrixCoreError::InvalidRegistry(
                "room is not an invitation".into(),
            ));
        }
        room.leave()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn invite_user(&self, room_id: &str, user_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        let user_id = parse_user_id(user_id)?;
        room.invite_user_by_id(&user_id)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn kick_user(
        &self,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        let user_id = parse_user_id(user_id)?;
        room.kick_user(&user_id, reason)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn ban_user(&self, room_id: &str, user_id: &str, reason: Option<&str>) -> Result<()> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        let user_id = parse_user_id(user_id)?;
        room.ban_user(&user_id, reason)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn unban_user(
        &self,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let room = selected_room(&state, room_id)?;
        let user_id = parse_user_id(user_id)?;
        room.unban_user(&user_id, reason)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn rename_room(&self, room_id: &str, name: &str) -> Result<()> {
        let state = self.state.lock().await;
        selected_room(&state, room_id)?
            .set_name(name.into())
            .await
            .map(|_| ())
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn set_room_topic(&self, room_id: &str, topic: &str) -> Result<()> {
        let state = self.state.lock().await;
        selected_room(&state, room_id)?
            .set_room_topic(topic)
            .await
            .map(|_| ())
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn security_info(&self) -> Result<SecurityInfo> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let encryption = client.encryption();
        let device_verification = match encryption.verification_state().get() {
            VerificationState::Unknown => DeviceVerificationStatus::Unknown,
            VerificationState::Verified => DeviceVerificationStatus::Verified,
            VerificationState::Unverified => DeviceVerificationStatus::Unverified,
        };
        let recovery = match encryption.recovery().state() {
            RecoveryState::Unknown => RecoveryStatus::Unknown,
            RecoveryState::Enabled => RecoveryStatus::Enabled,
            RecoveryState::Disabled => RecoveryStatus::Disabled,
            RecoveryState::Incomplete => RecoveryStatus::Incomplete,
        };
        let backup = match encryption.backups().state() {
            BackupState::Unknown => BackupStatus::Unknown,
            BackupState::Creating => BackupStatus::Creating,
            BackupState::Enabling => BackupStatus::Enabling,
            BackupState::Resuming => BackupStatus::Resuming,
            BackupState::Enabled => BackupStatus::Enabled,
            BackupState::Downloading => BackupStatus::Downloading,
            BackupState::Disabling => BackupStatus::Disabling,
        };
        Ok(SecurityInfo {
            device_verification,
            recovery,
            backup,
        })
    }

    /// Ask an existing verified device to verify this device using SAS.
    /// The app must display and compare SAS values before calling confirm.
    pub async fn request_own_verification(&self) -> Result<VerificationInfo> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let user_id = client.user_id().ok_or(MatrixCoreError::NoActiveAccount)?;
        let identity = client
            .encryption()
            .get_user_identity(user_id)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?
            .ok_or_else(|| {
                MatrixCoreError::Sdk("own cross-signing identity is not available yet".into())
            })?;
        let request = identity
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(verification_info(&request))
    }

    pub async fn verification_request_info(
        &self,
        user_id: &str,
        flow_id: &str,
    ) -> Result<VerificationInfo> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let request = find_verification_request(client, user_id, flow_id).await?;
        Ok(verification_info(&request))
    }

    /// Incoming requests from other devices on the selected Matrix account.
    /// The event handler only records request IDs; this refreshes each from
    /// the SDK's crypto state and removes completed or cancelled flows.
    pub async fn pending_own_verification_requests(&self) -> Result<Vec<VerificationInfo>> {
        let loaded = self.active_loaded().await?;
        let known = loaded.incoming_verification_tx.borrow().clone();
        let mut pending = Vec::new();
        for candidate in known {
            let Ok(user_id) = UserId::parse(candidate.other_user_id.as_str()) else {
                continue;
            };
            let Some(request) = loaded
                .client
                .encryption()
                .get_verification_request(&user_id, &candidate.flow_id)
                .await
            else {
                continue;
            };
            let info = verification_info(&request);
            if !info.is_done && !info.is_cancelled {
                pending.push(info);
            }
        }
        loaded
            .incoming_verification_tx
            .send_replace(pending.clone());
        Ok(pending)
    }

    /// Subscribe to request arrival for the selected account. Resubscribe
    /// after an account switch; the receiver belongs to the old client.
    pub async fn subscribe_incoming_verification_requests(
        &self,
    ) -> Result<(String, watch::Receiver<Vec<VerificationInfo>>)> {
        let state = self.state.lock().await;
        let id = state
            .registry
            .active_account_id
            .as_ref()
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let loaded = state
            .loaded
            .get(id)
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        Ok((id.clone(), loaded.incoming_verification_tx.subscribe()))
    }

    pub async fn accept_verification_request(&self, user_id: &str, flow_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        find_verification_request(client, user_id, flow_id)
            .await?
            .accept_with_methods(vec![VerificationMethod::SasV1])
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn start_sas_verification(&self, user_id: &str, flow_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let request = find_verification_request(client, user_id, flow_id).await?;
        if !request.we_started() {
            return Err(MatrixCoreError::Sdk(
                "only the requesting device may start this SAS flow".into(),
            ));
        }
        if client
            .encryption()
            .get_verification(&parse_user_id(user_id)?, flow_id)
            .await
            .and_then(|verification| verification.sas())
            .is_some()
        {
            // A delayed or repeated UI callback must not create a competing
            // SAS start event for a request already in progress.
            return Ok(());
        }
        request
            .start_sas()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?
            .ok_or_else(|| {
                MatrixCoreError::Sdk("verification request is not ready for SAS".into())
            })?;
        Ok(())
    }

    pub async fn accept_sas_verification(&self, user_id: &str, flow_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        find_sas(client, user_id, flow_id)
            .await?
            .accept()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Accept an incoming SAS start after accepting the verification request.
    /// A missing SAS is normal while the initiating device is still deciding.
    pub async fn accept_sas_if_available(&self, user_id: &str, flow_id: &str) -> Result<bool> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let request = find_verification_request(client, user_id, flow_id).await?;
        if request.we_started() {
            return Err(MatrixCoreError::Sdk(
                "the requesting device cannot accept its own SAS start".into(),
            ));
        }
        let user_id = parse_user_id(user_id)?;
        let Some(sas) = client
            .encryption()
            .get_verification(&user_id, flow_id)
            .await
            .and_then(|verification| verification.sas())
        else {
            return Ok(false);
        };
        sas.accept()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        Ok(true)
    }

    /// Returns `None` while waiting for the other device. Never auto-confirm
    /// based on these values; require the user to compare them out of band.
    pub async fn sas_presentation(
        &self,
        user_id: &str,
        flow_id: &str,
    ) -> Result<Option<SasPresentation>> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let sas = find_sas(client, user_id, flow_id).await?;
        if !sas.can_be_presented() {
            return Ok(None);
        }
        let emoji = sas.emoji().map(|items| {
            items
                .into_iter()
                .map(|item| SasEmojiInfo {
                    symbol: item.symbol.into(),
                    description: item.description.into(),
                })
                .collect()
        });
        let decimals = sas.decimals().map(|(a, b, c)| [a, b, c]);
        Ok(Some(SasPresentation { emoji, decimals }))
    }

    /// Call only after the user confirms the SAS values match on both devices.
    pub async fn confirm_sas_verification(&self, user_id: &str, flow_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        let sas = find_sas(client, user_id, flow_id).await?;
        if !sas.can_be_presented() {
            return Err(MatrixCoreError::Sdk(
                "SAS values are not ready to compare".into(),
            ));
        }
        sas.confirm()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn cancel_verification(&self, user_id: &str, flow_id: &str) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        find_verification_request(client, user_id, flow_id)
            .await?
            .cancel()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    /// Enable secret storage and encrypted room-key backup. The recovery key
    /// is returned only here; show it to the user and require a saved copy.
    pub async fn enable_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backup_upload: bool,
    ) -> Result<RecoveryKey> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        // Cross-signing is bootstrapped by the SDK after login. Export its
        // secrets into recovery only after that initialization has settled.
        client
            .encryption()
            .wait_for_e2ee_initialization_tasks()
            .await;
        let recovery = client.encryption().recovery();
        let mut enable = recovery.enable();
        if wait_for_backup_upload {
            enable = enable.wait_for_backups_to_upload();
        }
        if let Some(passphrase) = passphrase {
            enable = enable.with_passphrase(passphrase);
        }
        let key = enable
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(RecoveryKey {
            secret: Zeroizing::new(key),
        })
    }

    /// Import secret storage and backup keys on a newly signed-in device.
    /// Do not persist or log the supplied key or passphrase.
    pub async fn recover_account(&self, recovery_key_or_passphrase: &str) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded(&state)?.client;
        client
            .encryption()
            .recovery()
            .recover_and_fix_backup(recovery_key_or_passphrase)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
    }

    pub async fn stop(&self) {
        let mut state = self.state.lock().await;
        self.selections.replace(None);
        stop_sync(&mut state).await;
        self.sync_tx.send_replace(SyncStatus::Stopped);
    }

    async fn start_sync(&self, state: &mut State, client: Client) {
        // Send queues run independently of /sync. Resume only the selected
        // account, including persisted messages waiting from a prior session.
        // Subscribe before resuming so a fast persisted send cannot lose its
        // recoverable-error signal. Only this selected account gets a retry
        // worker; stop_sync aborts it before another account is selected.
        let queue = client.send_queue();
        let errors = queue.subscribe_errors();
        let updates = queue.subscribe();
        queue.set_enabled(true).await;
        state.queue_recovery_task = Some(tokio::spawn(recover_room_queues(
            client.clone(),
            errors,
            updates,
        )));
        let sync_policy = self.sync_policy;
        self.sync_tx.send_replace(SyncStatus::Starting);
        let status = self.sync_tx.clone();
        let (shutdown, receiver) = oneshot::channel();
        state.sync_task = Some(SyncTask {
            shutdown,
            task: tokio::spawn(
                async move { run_sync(client, status, receiver, sync_policy).await },
            ),
        });
    }

    async fn active_loaded(&self) -> Result<Arc<LoadedAccount>> {
        let state = self.state.lock().await;
        let id = state
            .registry
            .active_account_id
            .as_ref()
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        state
            .loaded
            .get(id)
            .cloned()
            .ok_or(MatrixCoreError::NoActiveAccount)
    }

    fn store_path(&self, id: &str) -> PathBuf {
        self.data_dir.join("accounts").join(id).join("store")
    }
}

/// True when a sync update changes Space child links. Callers can use this to
/// invalidate a cached `space_hierarchy` after a room update. State can arrive
/// before or after the timeline, and older servers may put it in the timeline.
pub fn room_updates_change_space_graph(updates: &matrix_sdk::sync::RoomUpdates) -> bool {
    fn is_space_child<T>(event: &Raw<T>) -> bool {
        event.get_field::<&str>("type").ok().flatten() == Some("m.space.child")
            && event
                .get_field::<&str>("state_key")
                .ok()
                .flatten()
                .is_some()
    }

    fn joined_or_left(
        state: &matrix_sdk::sync::State,
        timeline: &matrix_sdk::sync::Timeline,
    ) -> bool {
        let state_events = match state {
            matrix_sdk::sync::State::Before(events) | matrix_sdk::sync::State::After(events) => {
                events
            }
        };
        state_events.iter().any(is_space_child)
            || timeline
                .events
                .iter()
                .any(|event| is_space_child(event.raw()))
    }

    updates
        .joined
        .values()
        .any(|room| joined_or_left(&room.state, &room.timeline))
        || updates
            .left
            .values()
            .any(|room| joined_or_left(&room.state, &room.timeline))
        || updates
            .invited
            .values()
            .any(|room| room.invite_state.events.iter().any(is_space_child))
        || updates
            .knocked
            .values()
            .any(|room| room.knock_state.events.iter().any(is_space_child))
}

impl LoadedAccount {
    fn new(client: Client, account_id: String, store_passphrase: String) -> Self {
        let (incoming_verification_tx, _) = watch::channel(Vec::<VerificationInfo>::new());
        if let Some(own_user_id) = client.user_id().map(ToOwned::to_owned) {
            let incoming_tx = incoming_verification_tx.clone();
            client.add_event_handler(
                move |event: ToDeviceKeyVerificationRequestEvent, client: Client| {
                    let own_user_id = own_user_id.clone();
                    let incoming_tx = incoming_tx.clone();
                    async move {
                        if event.sender != own_user_id {
                            return;
                        }
                        let Some(request) = client
                            .encryption()
                            .get_verification_request(&event.sender, &event.content.transaction_id)
                            .await
                        else {
                            return;
                        };
                        let info = verification_info(&request);
                        if info.is_done || info.is_cancelled {
                            return;
                        }
                        incoming_tx.send_modify(|known| {
                            if !known
                                .iter()
                                .any(|existing| existing.flow_id == info.flow_id)
                            {
                                known.push(info);
                            }
                        });
                    }
                },
            );
        }
        let token_client = client.clone();
        let mut changes = client.subscribe_to_session_changes();
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let token_task = tokio::spawn(async move {
            // Restore/login can refresh tokens before this watcher subscribes.
            // Save one current snapshot after subscription, then serialize all
            // subsequent writes so an older snapshot cannot win the race.
            let mut initial_snapshot = true;
            loop {
                let change = if initial_snapshot {
                    initial_snapshot = false;
                    Ok(())
                } else {
                    tokio::select! {
                        biased;
                        _ = &mut shutdown_receiver => break,
                        change = changes.recv() => change.map(|_| ()),
                    }
                };
                match change {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(session) = StoredSession::from_client(&token_client) {
                            if let Err(e) = save_secret(
                                &account_id,
                                StoredSecret {
                                    version: SECRET_VERSION,
                                    store_passphrase: store_passphrase.clone(),
                                    session,
                                },
                            )
                            .await
                            {
                                warn!(account_id, error = %e, "could not persist refreshed Matrix session");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self {
            client,
            timelines: Mutex::new(HashMap::new()),
            composed_edits: Mutex::new(HashMap::new()),
            incoming_verification_tx,
            token_task: Mutex::new(Some(SyncTask {
                shutdown,
                task: token_task,
            })),
        }
    }

    async fn stop_token_persistence(&self) {
        if let Some(token_task) = self.token_task.lock().await.take() {
            let _ = token_task.shutdown.send(());
            let _ = token_task.task.await;
        }
    }

    async fn timeline(&self, room_id: &str) -> Result<Arc<Timeline>> {
        let id =
            RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let mut timelines = self.timelines.lock().await;
        if let Some(timeline) = timelines.get(room_id) {
            return Ok(timeline.clone());
        }
        let room = self
            .client
            .get_room(&id)
            .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))?;
        let timeline = Arc::new(
            room.timeline_builder()
                .with_focus(TimelineFocus::Live {
                    hide_threaded_events: true,
                })
                .build()
                .await
                .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?,
        );
        timelines.insert(room_id.into(), timeline.clone());
        Ok(timeline)
    }

    async fn thread_timeline(&self, room_id: &str, root_event_id: &str) -> Result<Arc<Timeline>> {
        let id =
            RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let root = parse_event_id(root_event_id)?;
        let key = format!("thread:{room_id}:{root_event_id}");
        let mut timelines = self.timelines.lock().await;
        if let Some(timeline) = timelines.get(&key) {
            return Ok(timeline.clone());
        }
        let room = self
            .client
            .get_room(&id)
            .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))?;
        let timeline = Arc::new(
            room.timeline_builder()
                .with_focus(TimelineFocus::Thread {
                    root_event_id: root,
                })
                .build()
                .await
                .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?,
        );
        timelines.insert(key, timeline.clone());
        Ok(timeline)
    }
}

async fn message_feed_from_timeline(
    account_id: String,
    room_id: &str,
    timeline: Arc<Timeline>,
) -> MessageFeed {
    let (items, mut stream) = timeline.subscribe().await;
    let mut mapped: Vector<Option<MessageInfo>> =
        items.iter().map(|item| message_info(item)).collect();
    let initial = mapped.iter().filter_map(Clone::clone).collect();
    let (updates, receiver) = watch::channel(initial);
    let task = tokio::spawn(async move {
        while let Some(diffs) = stream.next().await {
            for diff in diffs {
                diff.map(|item| message_info(&item)).apply(&mut mapped);
            }
            updates.send_replace(mapped.iter().filter_map(Clone::clone).collect());
        }
    });
    MessageFeed {
        account_id,
        room_id: room_id.into(),
        updates: receiver,
        task,
    }
}

fn selected_loaded_for<'a>(
    state: &'a State,
    expected_account_id: &str,
) -> Result<&'a Arc<LoadedAccount>> {
    if state.registry.active_account_id.as_deref() != Some(expected_account_id) {
        return Err(MatrixCoreError::SelectedAccountChanged);
    }
    selected_loaded(state)
}

fn selected_loaded(state: &State) -> Result<&Arc<LoadedAccount>> {
    let id = state
        .registry
        .active_account_id
        .as_ref()
        .ok_or(MatrixCoreError::NoActiveAccount)?;
    state.loaded.get(id).ok_or(MatrixCoreError::NoActiveAccount)
}

fn selected_room(state: &State, room_id: &str) -> Result<Room> {
    let id = RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
    selected_loaded(state)?
        .client
        .get_room(&id)
        .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))
}

fn account_room(state: &State, account_id: &str, room_id: &str) -> Result<Room> {
    if !state
        .registry
        .accounts
        .iter()
        .any(|account| account.id == account_id)
    {
        return Err(MatrixCoreError::AccountNotFound(account_id.into()));
    }
    let loaded = state
        .loaded
        .get(account_id)
        .ok_or_else(|| MatrixCoreError::AccountNotFound(account_id.into()))?;
    let id = RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
    loaded
        .client
        .get_room(&id)
        .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))
}

fn parse_event_id(raw: &str) -> Result<matrix_sdk::ruma::OwnedEventId> {
    EventId::parse(raw).map_err(|e| MatrixCoreError::InvalidEventId(e.to_string()))
}

fn parse_user_id(raw: &str) -> Result<matrix_sdk::ruma::OwnedUserId> {
    UserId::parse(raw).map_err(|e| MatrixCoreError::InvalidUserId(e.to_string()))
}

async fn find_verification_request(
    client: &Client,
    user_id: &str,
    flow_id: &str,
) -> Result<VerificationRequest> {
    let user_id = parse_user_id(user_id)?;
    client
        .encryption()
        .get_verification_request(&user_id, flow_id)
        .await
        .ok_or_else(|| MatrixCoreError::Sdk("verification request was not found".into()))
}

async fn find_sas(client: &Client, user_id: &str, flow_id: &str) -> Result<SasVerification> {
    let user_id = parse_user_id(user_id)?;
    client
        .encryption()
        .get_verification(&user_id, flow_id)
        .await
        .and_then(|verification| verification.sas())
        .ok_or_else(|| MatrixCoreError::Sdk("SAS verification was not found".into()))
}

fn verification_info(request: &VerificationRequest) -> VerificationInfo {
    VerificationInfo {
        flow_id: request.flow_id().into(),
        other_user_id: request.other_user_id().to_string(),
        we_started: request.we_started(),
        is_ready: request.is_ready(),
        is_done: request.is_done(),
        is_cancelled: request.is_cancelled(),
    }
}

async fn room_info(room: &Room) -> RoomInfo {
    let room_id = room.room_id().to_string();
    let name = room
        .display_name()
        .await
        .map(|name| name.to_string())
        .unwrap_or_else(|_| room_id.clone());
    RoomInfo {
        room_id,
        name,
        topic: room.topic(),
        membership: room.state().into(),
        is_direct: room.is_direct().await.unwrap_or(false),
        is_space: room.is_space(),
        avatar_mxc: room.avatar_url().map(|url| url.to_string()),
        unread_messages: room.num_unread_messages(),
        unread_notifications: room.num_unread_notifications(),
    }
}

fn message_info(item: &TimelineItem) -> Option<MessageInfo> {
    let event = item.as_event()?;
    let message = event.content().as_message()?;
    let reply_preview =
        event
            .content()
            .in_reply_to()
            .as_ref()
            .map(|details| match &details.event {
                TimelineDetails::Ready(replied_to) => {
                    let sender = replied_to
                        .sender
                        .as_str()
                        .trim_start_matches('@')
                        .split(':')
                        .next()
                        .unwrap_or("someone");
                    let body = replied_to
                        .content
                        .as_message()
                        .map(|message| message.body().lines().next().unwrap_or("message"))
                        .unwrap_or("message");
                    format!("{sender}: {}", body.chars().take(72).collect::<String>())
                }
                _ => format!("message {}", details.event_id),
            });
    let send_state = match event.send_state() {
        Some(EventSendState::NotSentYet { .. }) => MessageSendState::Pending,
        Some(EventSendState::SendingFailed { .. }) => MessageSendState::Failed,
        Some(EventSendState::Sent { .. }) | None => MessageSendState::Sent,
    };
    let formatted = match message.msgtype() {
        MessageType::Text(content) => content.formatted.as_ref(),
        MessageType::Notice(content) => content.formatted.as_ref(),
        MessageType::Emote(content) => content.formatted.as_ref(),
        _ => None,
    };
    let rich_markdown = formatted
        .filter(|content| content.format == MessageFormat::Html)
        .and_then(|content| rich_text::matrix_html_to_markdown(&content.body));
    let rich_accessible = rich_markdown
        .as_deref()
        .map(rich_text::markdown_to_accessible_text);
    Some(MessageInfo {
        key: item.unique_id().0.clone(),
        event_id: event.event_id().map(ToString::to_string),
        sender: event.sender().to_string(),
        body: message.body().to_owned(),
        rich_markdown,
        rich_accessible,
        attachment_name: media::attachment_name(message.msgtype()),
        reply_preview,
        reaction_summary: event
            .reactions()
            .iter()
            .take(3)
            .map(|(key, senders)| format!("{key} {}", senders.len()))
            .collect::<Vec<_>>()
            .join("   "),
        timestamp_ms: event.timestamp().get().into(),
        is_own: event.is_own(),
        is_local_echo: event.is_local_echo(),
        send_state,
    })
}

fn space_node_info(
    chunk: matrix_sdk::ruma::api::client::space::SpaceHierarchyRoomsChunk,
) -> SpaceNodeInfo {
    let children = chunk
        .children_state
        .into_iter()
        .filter_map(|raw| raw.deserialize().ok())
        .filter(|event| !event.content.via.is_empty())
        .map(|event| SpaceChildInfo {
            room_id: event.state_key.to_string(),
            via_servers: event
                .content
                .via
                .into_iter()
                .map(|server| server.to_string())
                .collect(),
            suggested: event.content.suggested,
        })
        .collect();
    SpaceNodeInfo {
        room_id: chunk.summary.room_id.to_string(),
        name: chunk.summary.name,
        topic: chunk.summary.topic,
        avatar_mxc: chunk.summary.avatar_url.map(|url| url.to_string()),
        is_space: matches!(chunk.summary.room_type, Some(RoomType::Space)),
        children,
    }
}

async fn build_client(homeserver: &str, store_path: &Path, passphrase: &str) -> Result<Client> {
    fs::create_dir_all(store_path)?;
    let client = Client::builder()
        .http_client(authentication::http_client()?)
        .homeserver_url(homeserver)
        .sqlite_store(store_path, Some(passphrase))
        .search_index_store(SearchIndexStoreKind::EncryptedDirectory(
            store_path.join("search"),
            passphrase.to_owned(),
        ))
        // Fetch only missing room keys after recovery, avoiding a full-account backup download.
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            ..Default::default()
        })
        // MSC4268 invite-history bundles are an experimental SDK default.
        // Morpheus has no explicit history-sharing consent/verification flow
        // yet, so invitations must not upload or distribute historic keys.
        // This also disables accepting those bundles; ordinary key backup,
        // recovery and encrypted messaging retain their existing policies.
        .with_enable_share_history_on_invite(false)
        .handle_refresh_tokens()
        .build()
        .await
        .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
    // A restored client can contain persisted queued sends. Keep it dormant
    // until the account is actually selected and sync starts.
    client.send_queue().enable_upload_progress(true);
    client.send_queue().set_enabled(false).await;
    Ok(client)
}

fn start_event_cache(client: &Client) -> Result<()> {
    // The event-cache redecryptor subscribes to the Olm machine when it
    // starts. Starting it before login/restore can permanently end that
    // worker, leaving recovered room keys unable to refresh UTD timeline
    // items. All callers invoke this after authentication and before sync.
    client
        .event_cache()
        .subscribe()
        .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
}

async fn stop_sync(state: &mut State) {
    if let Some(loaded) = state
        .registry
        .active_account_id
        .as_ref()
        .and_then(|id| state.loaded.get(id))
    {
        // The SDK lets an already in-flight request finish, then holds all
        // later queued requests. This is the closest safe pause boundary.
        loaded.client.send_queue().set_enabled(false).await;
    }
    if let Some(recovery) = state.queue_recovery_task.take() {
        recovery.abort();
        let _ = recovery.await;
    }
    if let Some(sync) = state.sync_task.take() {
        let _ = sync.shutdown.send(());
        let _ = sync.task.await;
    }
}

/// The pinned SDK pauses a room queue after a recoverable send error. Retry
/// retained requests with bounded backoff while their account stays selected.
/// Unrecoverable errors stay wedged for an explicit retry or cancellation.
async fn recover_room_queues(
    client: Client,
    mut errors: broadcast::Receiver<matrix_sdk::send_queue::SendQueueRoomError>,
    mut updates: broadcast::Receiver<matrix_sdk::send_queue::SendQueueUpdate>,
) {
    let mut attempts: HashMap<String, u32> = HashMap::new();
    let mut due: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            received = errors.recv() => match received {
                Ok(error) if error.is_recoverable => {
                    let room_id = error.room_id.to_string();
                    let attempts = attempts.entry(room_id.clone()).or_default();
                    *attempts = attempts.saturating_add(1).min(6);
                    // 2, 4, 8, 16, 32, then 60 seconds.
                    let delay = (1_u64 << *attempts).min(60);
                    due.insert(room_id, Instant::now() + Duration::from_secs(delay));
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            received = updates.recv() => match received {
                Ok(update) if matches!(update.update, RoomSendQueueUpdate::SentEvent { .. }) => {
                    attempts.remove(update.room_id.as_str());
                    due.remove(update.room_id.as_str());
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                if !client.send_queue().is_enabled() {
                    break;
                }
                let now = Instant::now();
                let ready: Vec<_> = due.iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(room_id, _)| room_id.clone())
                    .collect();
                for room_id in ready {
                    if let Ok(parsed) = RoomId::parse(&room_id)
                        && let Some(room) = client.get_room(&parsed)
                    {
                        room.send_queue().set_enabled(true);
                        due.remove(&room_id);
                    }
                }
            }
        }
    }
}

async fn run_sync(
    client: Client,
    status: watch::Sender<SyncStatus>,
    mut shutdown: oneshot::Receiver<()>,
    sync_policy: SyncPolicy,
) {
    // Sliding Sync is the source of room sticky events used by native
    // MatrixRTC. The UI crate's unstable-msc4354 feature configures that
    // extension. Older homeservers can still use classic /sync for chat.
    let mut retry_seconds = 2;
    let supports_sliding = if sync_policy == SyncPolicy::ClassicOnly {
        false
    } else {
        loop {
            let versions = tokio::select! {
                _ = &mut shutdown => {
                    status.send_replace(SyncStatus::Stopped);
                    return;
                }
                result = client.supported_versions() => result,
            };
            match versions {
                Ok(versions) => break versions.features.contains(&FeatureFlag::Msc4186),
                Err(e) => {
                    status.send_replace(SyncStatus::Retrying {
                        after_seconds: retry_seconds,
                        reason: e.to_string(),
                    });
                    tokio::select! {
                        _ = &mut shutdown => {
                            status.send_replace(SyncStatus::Stopped);
                            return;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(retry_seconds)) => {},
                    }
                    retry_seconds = (retry_seconds * 2).min(60);
                }
            }
        }
    };
    if supports_sliding {
        retry_seconds = 2;
        let service = loop {
            let result = tokio::select! {
                _ = &mut shutdown => {
                    status.send_replace(SyncStatus::Stopped);
                    return;
                }
                result = SyncService::builder(client.clone()).build() => result,
            };
            match result {
                Ok(service) => break Some(service),
                Err(e) if service_error_is_unsupported(&e) => {
                    warn!(error = %e, "sliding sync endpoint unavailable; using classic sync");
                    break None;
                }
                Err(e) => {
                    status.send_replace(SyncStatus::Retrying {
                        after_seconds: retry_seconds,
                        reason: e.to_string(),
                    });
                    tokio::select! {
                        _ = &mut shutdown => {
                            status.send_replace(SyncStatus::Stopped);
                            return;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(retry_seconds)) => {},
                    }
                    retry_seconds = (retry_seconds * 2).min(60);
                }
            }
        };
        if let Some(service) = service {
            let mut states = service.state();
            service.start().await;
            retry_seconds = 2;
            let mut should_fallback = false;
            loop {
                tokio::select! {
                    _ = &mut shutdown => break,
                    state = states.next() => match state {
                        Some(ServiceState::Running) => {
                            retry_seconds = 2;
                            status.send_replace(SyncStatus::Running { backend: SyncBackend::SlidingSticky });
                        }
                        Some(ServiceState::Idle) => {}
                        Some(ServiceState::Offline) => {
                            status.send_replace(SyncStatus::Retrying {
                                after_seconds: 0,
                                reason: "sliding sync is offline".into(),
                            });
                        }
                        Some(ServiceState::Terminated) => {
                            status.send_replace(SyncStatus::Retrying {
                                after_seconds: retry_seconds,
                                reason: "sliding sync ended".into(),
                            });
                            tokio::select! {
                                _ = &mut shutdown => break,
                                _ = tokio::time::sleep(Duration::from_secs(retry_seconds)) => {},
                            }
                            retry_seconds = (retry_seconds * 2).min(60);
                            service.start().await;
                        }
                        None => { should_fallback = true; break; }
                        Some(ServiceState::Error(e)) if service_error_is_unsupported(&e) => {
                            warn!(error = %e, "sliding sync endpoint unavailable; using classic sync");
                            should_fallback = true;
                            break;
                        }
                        Some(ServiceState::Error(e)) => {
                            status.send_replace(SyncStatus::Retrying {
                                after_seconds: retry_seconds,
                                reason: e.to_string(),
                            });
                            tokio::select! {
                                _ = &mut shutdown => break,
                                _ = tokio::time::sleep(Duration::from_secs(retry_seconds)) => {},
                            }
                            retry_seconds = (retry_seconds * 2).min(60);
                            service.start().await;
                        }
                    },
                }
            }
            service.stop().await;
            if !should_fallback {
                status.send_replace(SyncStatus::Stopped);
                return;
            }
        }
    }

    status.send_replace(SyncStatus::Starting);
    let mut retry_seconds = 2;
    loop {
        let settings = SyncSettings::default().timeout(Duration::from_secs(30));
        let result = tokio::select! {
            _ = &mut shutdown => break,
            result = client.sync_once(settings) => result,
        };
        match result {
            Ok(_) => {
                retry_seconds = 2;
                if *status.borrow()
                    != (SyncStatus::Running {
                        backend: SyncBackend::Classic,
                    })
                {
                    status.send_replace(SyncStatus::Running {
                        backend: SyncBackend::Classic,
                    });
                }
            }
            Err(e) => {
                status.send_replace(SyncStatus::Retrying {
                    after_seconds: retry_seconds,
                    reason: e.to_string(),
                });
                tokio::select! {
                    _ = &mut shutdown => break,
                    _ = tokio::time::sleep(Duration::from_secs(retry_seconds)) => {},
                }
                retry_seconds = (retry_seconds * 2).min(60);
            }
        }
    }
    status.send_replace(SyncStatus::Stopped);
}

fn service_error_is_unsupported(error: &ServiceError) -> bool {
    match error {
        ServiceError::RoomList(matrix_sdk_ui::room_list_service::Error::SlidingSync(
            matrix_sdk::Error::Http(http),
        ))
        | ServiceError::EncryptionSync(
            matrix_sdk_ui::encryption_sync_service::Error::SlidingSync(matrix_sdk::Error::Http(
                http,
            )),
        )
        | ServiceError::EncryptionSync(
            matrix_sdk_ui::encryption_sync_service::Error::ClientError(matrix_sdk::Error::Http(
                http,
            )),
        ) => http.is_endpoint_not_implemented(),
        _ => false,
    }
}

fn validate_homeserver(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw).map_err(|e| MatrixCoreError::InvalidHomeserver(e.to_string()))?;
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MatrixCoreError::InvalidHomeserver(
            "URL cannot contain credentials, query, or fragment".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| MatrixCoreError::InvalidHomeserver("missing host".into()))?;
    if url.scheme() != "https"
        && !(url.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1"))
    {
        return Err(MatrixCoreError::InvalidHomeserver(
            "HTTPS is required except for loopback development servers".into(),
        ));
    }
    if url.path() != "/" {
        return Err(MatrixCoreError::InvalidHomeserver(
            "use the homeserver base URL without a path".into(),
        ));
    }
    url.set_path("");
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn validate_loopback_redirect(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| MatrixCoreError::InvalidSsoCallback(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| MatrixCoreError::InvalidSsoCallback("missing host".into()))?;
    if url.scheme() != "http"
        || !matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MatrixCoreError::InvalidSsoCallback(
            "use a loopback HTTP URL with an explicit port and no credentials, query, or fragment"
                .into(),
        ));
    }
    Ok(url)
}

fn save_registry(data_dir: &Path, registry: &Registry) -> Result<()> {
    registry.validate()?;
    let path = data_dir.join("accounts.json");
    let temp = data_dir.join(format!("accounts-{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(registry)?;
    let write_result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

async fn save_secret(id: &str, secret: StoredSecret) -> Result<()> {
    let id = id.to_owned();
    let payload = serde_json::to_string(&secret)?;
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &id)
            .map_err(|e| MatrixCoreError::Keyring(e.to_string()))?;
        entry
            .set_password(&payload)
            .map_err(|e| MatrixCoreError::Keyring(e.to_string()))
    })
    .await
    .map_err(|e| MatrixCoreError::Keyring(e.to_string()))?
}

async fn load_secret(id: &str) -> Result<StoredSecret> {
    let id = id.to_owned();
    let payload = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(KEYRING_SERVICE, &id)
                .map_err(|e| MatrixCoreError::Keyring(e.to_string()))?;
            entry
                .get_password()
                .map_err(|e| MatrixCoreError::Keyring(e.to_string()))
        }),
    )
    .await
    .map_err(|_| {
        MatrixCoreError::Keyring(
            "OS credential vault did not respond within 30 seconds; check its unlock or access prompt, then restart Morpheus and sign in again".into(),
        )
    })?
    .map_err(|e| MatrixCoreError::Keyring(e.to_string()))??;
    serde_json::from_str(&payload).map_err(Into::into)
}

async fn delete_secret(id: &str) -> Result<()> {
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &id)
            .map_err(|e| MatrixCoreError::Keyring(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(MatrixCoreError::Keyring(e.to_string())),
        }
    })
    .await
    .map_err(|e| MatrixCoreError::Keyring(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_selected_account_has_an_active_send_queue() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = std::env::temp_dir().join(format!("morpheus-queue-{}", Uuid::new_v4()));
            let manager = AccountManager::open(&root).unwrap();
            let homeserver = "http://127.0.0.1:9";
            let first = AccountInfo {
                id: Uuid::new_v4().to_string(),
                user_id: "@first:localhost".into(),
                device_id: "FIRST".into(),
                homeserver: homeserver.into(),
            };
            let second = AccountInfo {
                id: Uuid::new_v4().to_string(),
                user_id: "@second:localhost".into(),
                device_id: "SECOND".into(),
                homeserver: homeserver.into(),
            };
            let client_a = Client::builder()
                .homeserver_url(homeserver)
                .build()
                .await
                .unwrap();
            let client_b = Client::builder()
                .homeserver_url(homeserver)
                .build()
                .await
                .unwrap();
            client_b.send_queue().set_enabled(false).await;
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![first.clone(), second.clone()];
                state.registry.active_account_id = Some(first.id.clone());
                state.loaded.insert(
                    first.id.clone(),
                    Arc::new(LoadedAccount::new(
                        client_a.clone(),
                        first.id.clone(),
                        String::new(),
                    )),
                );
                state.loaded.insert(
                    second.id.clone(),
                    Arc::new(LoadedAccount::new(
                        client_b.clone(),
                        second.id.clone(),
                        String::new(),
                    )),
                );
            }
            assert!(client_a.send_queue().is_enabled());
            assert!(!client_b.send_queue().is_enabled());

            manager.switch_to(&second.id).await.unwrap();
            assert!(!client_a.send_queue().is_enabled());
            assert!(client_b.send_queue().is_enabled());
            let previous_selection = manager.account_selection(&second.id).unwrap();
            assert!(previous_selection.is_current());

            // Re-selecting an active account is a manual recovery path for
            // a queue paused after a transient error.
            client_b.send_queue().set_enabled(false).await;
            manager.switch_to(&second.id).await.unwrap();
            assert!(client_b.send_queue().is_enabled());

            manager.switch_to(&first.id).await.unwrap();
            assert!(client_a.send_queue().is_enabled());
            assert!(!client_b.send_queue().is_enabled());
            assert!(!previous_selection.is_current());
            let first_selection = manager.account_selection(&first.id).unwrap();
            manager.switch_to(&second.id).await.unwrap();
            assert!(!first_selection.is_current());
            assert!(!previous_selection.is_current());
            let current_selection = manager.account_selection(&second.id).unwrap();
            assert!(current_selection.is_current());

            manager.stop().await;
            assert!(!current_selection.is_current());
            assert!(!client_a.send_queue().is_enabled());
            assert!(!client_b.send_queue().is_enabled());
            drop(manager);
            fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    fn queued_send_updates_keep_retained_transactions_observable() {
        use matrix_sdk::ruma::TransactionId;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let wanted = TransactionId::new();
            let wanted_id = wanted.to_string();
            let other = TransactionId::new();
            let (sender, updates) = broadcast::channel(1);
            let mut updates = updates;
            sender
                .send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: other,
                })
                .unwrap();
            sender
                .send(RoomSendQueueUpdate::SentEvent {
                    transaction_id: wanted,
                    event_id: EventId::parse("$sent:example.org").unwrap(),
                })
                .unwrap();
            assert_eq!(
                next_queued_send_update(&mut updates, &wanted_id).await,
                QueuedSendUpdate::Sent
            );

            let wanted = TransactionId::new();
            let wanted_id = wanted.to_string();
            let (sender, mut updates) = broadcast::channel(4);
            sender
                .send(RoomSendQueueUpdate::SendError {
                    transaction_id: wanted.clone(),
                    error: Arc::new(matrix_sdk::Error::UnknownError(Box::new(
                        std::io::Error::other("test transient failure"),
                    ))),
                    is_recoverable: true,
                })
                .unwrap();
            sender
                .send(RoomSendQueueUpdate::SentEvent {
                    transaction_id: wanted,
                    event_id: EventId::parse("$eventually-sent:example.org").unwrap(),
                })
                .unwrap();
            assert_eq!(
                next_queued_send_update(&mut updates, &wanted_id).await,
                QueuedSendUpdate::Retained { recoverable: true }
            );
            assert_eq!(
                next_queued_send_update(&mut updates, &wanted_id).await,
                QueuedSendUpdate::Sent
            );

            let wanted = TransactionId::new();
            let wanted_id = wanted.to_string();
            let (sender, updates) = broadcast::channel(1);
            let mut updates = updates;
            sender
                .send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: wanted,
                })
                .unwrap();
            assert_eq!(
                next_queued_send_update(&mut updates, &wanted_id).await,
                QueuedSendUpdate::Cancelled
            );

            let (sender, updates) = broadcast::channel(1);
            let mut updates = updates;
            drop(sender);
            assert_eq!(
                next_queued_send_update(&mut updates, TransactionId::new().as_str()).await,
                QueuedSendUpdate::Unknown
            );
        });
    }

    #[test]
    fn homeserver_must_not_send_password_over_plain_http() {
        assert!(validate_homeserver("http://example.org").is_err());
        assert_eq!(
            validate_homeserver("http://localhost:8008").unwrap(),
            "http://localhost:8008"
        );
        assert!(validate_homeserver("https://example.org/path").is_err());
        assert!(validate_homeserver("https://user:secret@example.org").is_err());
    }

    #[test]
    fn registry_rejects_path_like_account_ids() {
        let registry = Registry {
            version: REGISTRY_VERSION,
            active_account_id: Some("../outside".into()),
            accounts: vec![AccountInfo {
                id: "../outside".into(),
                user_id: "@a:b".into(),
                device_id: "D".into(),
                homeserver: "https://b".into(),
            }],
        };
        assert!(registry.validate().is_err());
    }

    #[test]
    fn sso_redirect_must_be_private_loopback_listener() {
        assert!(validate_loopback_redirect("http://127.0.0.1:49152/callback").is_ok());
        assert!(validate_loopback_redirect("http://localhost:49152/callback").is_ok());
        assert!(validate_loopback_redirect("https://example.org/callback").is_err());
        assert!(validate_loopback_redirect("http://192.168.1.12:49152/callback").is_err());
        assert!(
            validate_loopback_redirect("http://127.0.0.1:49152/callback?loginToken=leak").is_err()
        );
    }

    #[test]
    fn space_hierarchy_maps_child_links_without_guessing_membership() {
        let chunk: matrix_sdk::ruma::api::client::space::SpaceHierarchyRoomsChunk = serde_json::from_value(serde_json::json!({
            "room_id": "!space:example.org",
            "name": "Community",
            "num_joined_members": 4,
            "world_readable": false,
            "guest_can_join": false,
            "room_type": "m.space",
            "children_state": [
                {"type": "m.space.child", "state_key": "!general:example.org", "sender": "@admin:example.org", "origin_server_ts": 1, "content": {"via": ["example.org"], "suggested": true}},
                {"type": "m.space.child", "state_key": "!removed:example.org", "sender": "@admin:example.org", "origin_server_ts": 2, "content": {"via": []}}
            ]
        })).unwrap();
        let node = space_node_info(chunk);
        assert_eq!(node.room_id, "!space:example.org");
        assert!(node.is_space);
        assert_eq!(node.children.len(), 1);
        assert_eq!(node.children[0].room_id, "!general:example.org");
        assert!(node.children[0].suggested);
    }

    #[test]
    fn space_graph_change_detects_state_before_after_and_timeline() {
        use matrix_sdk::{
            deserialized_responses::TimelineEvent,
            ruma::events::{AnySyncStateEvent, AnySyncTimelineEvent},
            sync::{JoinedRoomUpdate, LeftRoomUpdate, RoomUpdates, State},
        };

        let room_id = RoomId::parse("!space:example.org").unwrap();
        let child_json = serde_json::json!({
            "type": "m.space.child",
            "state_key": "!child:example.org",
            "sender": "@admin:example.org",
            "event_id": "$child",
            "origin_server_ts": 1,
            "content": {"via": ["example.org"]}
        });
        let raw_state: Raw<AnySyncStateEvent> = serde_json::from_value(child_json.clone()).unwrap();
        let mut updates = RoomUpdates::default();
        assert!(!room_updates_change_space_graph(&updates));

        let mut joined = JoinedRoomUpdate {
            state: State::Before(vec![raw_state.clone()]),
            ..Default::default()
        };
        updates.joined.insert(room_id.clone(), joined.clone());
        assert!(room_updates_change_space_graph(&updates));

        joined.state = State::After(vec![raw_state]);
        updates.joined.insert(room_id.clone(), joined);
        assert!(room_updates_change_space_graph(&updates));

        updates.joined.clear();
        let raw_timeline: Raw<AnySyncTimelineEvent> = serde_json::from_value(child_json).unwrap();
        updates.left.insert(
            room_id.clone(),
            LeftRoomUpdate {
                timeline: matrix_sdk::sync::Timeline {
                    events: vec![TimelineEvent::from_plaintext(raw_timeline)],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(room_updates_change_space_graph(&updates));

        updates.left.clear();
        let raw_timeline: Raw<AnySyncTimelineEvent> = serde_json::from_value(serde_json::json!({
            "type": "m.space.child",
            "sender": "@admin:example.org",
            "event_id": "$fake",
            "origin_server_ts": 1,
            "content": {}
        }))
        .unwrap();
        updates.left.insert(
            room_id,
            LeftRoomUpdate {
                timeline: matrix_sdk::sync::Timeline {
                    events: vec![TimelineEvent::from_plaintext(raw_timeline)],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(!room_updates_change_space_graph(&updates));
    }
}

/// Selected Matrix identities and immutable edit baselines for the composer.
pub use composed_adapter::{ComposeSendTarget, EditBaseline, EditComposition, MentionCandidate};

mod composed_adapter {
    use super::*;
    use matrix_sdk::{
        config::RequestConfig,
        ruma::events::{
            AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent, Mentions,
            SyncMessageLikeEvent,
            room::{member::MembershipState, message::Relation},
        },
        send_queue::LocalEchoContent,
    };
    use matrix_sdk_ui::timeline::EventTimelineItem;
    use std::fmt;

    const MAX_CANDIDATE_MEMBERS: usize = 4096;
    const MAX_PENDING_EDITS: usize = 32;
    const MAX_EVENT_BYTES: usize = 512 * 1024;
    // Leave room for event headers and encrypted/base64 wrapping. A server can
    // impose a lower limit; queue failure still retains the user's draft.
    const MAX_ENCODED_COMPOSITION: usize = 44 * 1024;

    #[derive(Clone)]
    pub struct MentionCandidate {
        pub target: MentionTarget,
        pub label: String,
    }
    impl fmt::Debug for MentionCandidate {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("MentionCandidate([redacted])")
        }
    }

    #[derive(Clone)]
    pub enum ComposeSendTarget {
        Room,
        Reply(String),
        Thread(String),
        Edit(String),
    }
    impl fmt::Debug for ComposeSendTarget {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                Self::Room => "Room",
                Self::Reply(_) => "Reply([redacted])",
                Self::Thread(_) => "Thread([redacted])",
                Self::Edit(_) => "Edit([redacted])",
            })
        }
    }

    /// RAM-only baseline. A new selection, revision or pending send invalidates
    /// it; callers cannot manufacture a baseline from presentation text.
    #[derive(Clone)]
    pub struct EditBaseline {
        selection: AccountSelection,
        room: String,
        event: String,
        revision: EditRevision,
        inherited_reply_sender: Option<matrix_sdk::ruma::OwnedUserId>,
    }
    impl EditBaseline {
        fn matches(&self, selection: &AccountSelection, room: &str, event: &str) -> bool {
            self.selection.is_current()
                && selection.is_current()
                && self.selection.account_id() == selection.account_id()
                && self.room == room
                && self.event == event
        }
    }
    impl fmt::Debug for EditBaseline {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("EditBaseline([redacted])")
        }
    }
    pub struct EditComposition {
        pub text: ComposeText,
        pub baseline: EditBaseline,
    }

    #[derive(Clone)]
    struct EditRevision {
        original: serde_json::Value,
        effective: serde_json::Value,
        latest_edit: Option<String>,
        mentions: Mentions,
    }

    impl PartialEq for EditRevision {
        fn eq(&self, other: &Self) -> bool {
            self.original == other.original
                && self.effective == other.effective
                && self.latest_edit == other.latest_edit
        }
    }
    fn same_mentions(left: Option<&Mentions>, right: &Mentions) -> bool {
        left.is_some_and(|left| left.room == right.room && left.user_ids == right.user_ids)
    }

    /// Reserved before entering enqueue, so dropping an uncertain enqueue
    /// cannot release the target. Retained across account switches. There is
    /// deliberately no eviction of unresolved records and no background task.
    pub(super) struct PendingEdit {
        transaction: Option<String>,
        updates: broadcast::Receiver<RoomSendQueueUpdate>,
        sent: Option<String>,
        uncertain: bool,
    }
    impl PendingEdit {
        fn resolved(&mut self, revision: &EditRevision) -> bool {
            if self.uncertain {
                return false;
            }
            let Some(transaction) = self.transaction.as_deref() else {
                return false;
            };
            for _ in 0..256 {
                match self.updates.try_recv() {
                    Ok(RoomSendQueueUpdate::CancelledLocalEvent { transaction_id })
                        if transaction_id.as_str() == transaction =>
                    {
                        return true;
                    }
                    Ok(RoomSendQueueUpdate::SentEvent {
                        transaction_id,
                        event_id,
                    }) if transaction_id.as_str() == transaction => {
                        self.sent = Some(event_id.to_string())
                    }
                    Ok(_) => {}
                    Err(broadcast::error::TryRecvError::Empty) => {
                        return self
                            .sent
                            .as_ref()
                            .is_some_and(|sent| revision.latest_edit.as_ref() == Some(sent));
                    }
                    Err(_) => {
                        self.uncertain = true;
                        return false;
                    }
                }
            }
            // A later call can continue draining; no state is guessed on this call.
            false
        }
    }

    fn unavailable() -> MatrixCoreError {
        MatrixCoreError::Sdk(
            "This edit is unavailable or changed. Reopen it before sending.".into(),
        )
    }
    fn pending() -> MatrixCoreError {
        MatrixCoreError::Sdk(
            "The previous edit is still pending. Keep this draft until its outcome is known."
                .into(),
        )
    }
    fn mention_unavailable() -> MatrixCoreError {
        MatrixCoreError::Sdk(
            "A selected mention is unavailable or is no longer allowed in this room.".into(),
        )
    }
    fn context(selection: &AccountSelection, room: &Room) -> Result<()> {
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        if room.state() != RoomState::Joined {
            return Err(mention_unavailable());
        }
        Ok(())
    }
    fn label(source: &str) -> String {
        source
            .chars()
            .filter(|ch| {
                !ch.is_control()
                    && !matches!(*ch,
            '\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
            })
            .take(80)
            .collect()
    }

    impl AccountManager {
        /// At most seven joined users plus an explicitly permitted room row.
        /// Only this room's local member cache is read: no directory lookup or
        /// implicit members request occurs per keystroke. Very large or not yet
        /// loaded rooms may have no suggestions; explicit sends recheck targets.
        pub async fn mention_candidates_for(
            &self,
            selection: &AccountSelection,
            room_id: &str,
            query: &str,
        ) -> Result<Vec<MentionCandidate>> {
            if query.len() > 128 || query.chars().any(char::is_control) {
                return Err(mention_unavailable());
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                let state = self.state.lock().await;
                selected_loaded_for(&state, selection.account_id())?;
                let room = account_room(&state, selection.account_id(), room_id)?;
                context(selection, &room)?;
                let query = query.to_lowercase();
                let mut result = Vec::new();
                if room.joined_members_count() <= MAX_CANDIDATE_MEMBERS as u64 {
                    let mut ids = room
                        .joined_user_ids()
                        .await
                        .map_err(|_| mention_unavailable())?;
                    context(selection, &room)?;
                    if ids.len() <= MAX_CANDIDATE_MEMBERS {
                        ids.sort();
                        for id in ids {
                            if id == room.own_user_id() || id.as_str().len() > 255 {
                                continue;
                            }
                            let Some(member) = room
                                .get_member_no_sync(&id)
                                .await
                                .map_err(|_| mention_unavailable())?
                            else {
                                continue;
                            };
                            context(selection, &room)?;
                            if member.membership() != &MembershipState::Join {
                                continue;
                            }
                            let display = label(member.display_name().unwrap_or(id.as_str()));
                            if id.as_str().to_lowercase().contains(&query)
                                || display.to_lowercase().contains(&query)
                            {
                                result.push(MentionCandidate {
                                    target: MentionTarget::User(id.to_string()),
                                    label: display,
                                });
                                if result.len() == 7 {
                                    break;
                                }
                            }
                        }
                    }
                }
                if "room".starts_with(&query) {
                    // Missing/malformed power state never becomes permission.
                    let allowed = room_mention_allowed(&room).await.unwrap_or(false);
                    context(selection, &room)?;
                    if allowed {
                        result.push(MentionCandidate {
                            target: MentionTarget::Room,
                            label: "Notify room".into(),
                        });
                    }
                }
                Ok(result)
            })
            .await
            .map_err(|_| mention_unavailable())?
        }

        pub async fn edit_composition_for(
            &self,
            selection: &AccountSelection,
            room_id: &str,
            event_id: &str,
        ) -> Result<EditComposition> {
            tokio::time::timeout(Duration::from_secs(15), async {
                let state = self.state.lock().await;
                let loaded = selected_loaded_for(&state, selection.account_id())?;
                let room = account_room(&state, selection.account_id(), room_id)?;
                context(selection, &room)?;
                let event_id = parse_event_id(event_id)?;
                let revision = current_revision(loaded, &room, &event_id).await?;
                context(selection, &room)?;
                pending_ready(loaded, &room, &event_id, &revision).await?;
                context(selection, &room)?;
                let inherited = inherited_reply_sender(selection, &room, &revision).await?;
                context(selection, &room)?;
                let text = restore_edit(&revision, room.own_user_id(), inherited.as_deref())?;
                if current_revision(loaded, &room, &event_id).await? != revision {
                    return Err(unavailable());
                }
                context(selection, &room)?;
                Ok(EditComposition {
                    text,
                    baseline: EditBaseline {
                        selection: selection.clone(),
                        room: room_id.into(),
                        event: event_id.to_string(),
                        revision,
                        inherited_reply_sender: inherited,
                    },
                })
            })
            .await
            .map_err(|_| unavailable())?
        }

        /// All modes subscribe before enqueue. The account lock keeps a switch
        /// from redirecting the send; the revocable token also rejects A→B→A.
        /// Once SDK enqueue has begun it is not an atomic server compare-and-set:
        /// an external edit arriving afterwards cannot be recalled.
        pub async fn send_composed_tracked_for(
            &self,
            selection: &AccountSelection,
            room_id: &str,
            target: ComposeSendTarget,
            text: &ComposeText,
            baseline: Option<&EditBaseline>,
        ) -> Result<QueuedMessage> {
            let text = text.trimmed_for_send()?;
            if text.body.is_empty() {
                return Err(mention_unavailable());
            }
            let enqueue_entered = std::sync::atomic::AtomicBool::new(false);
            tokio::time::timeout(Duration::from_secs(20), async {
                let state = self.state.lock().await;
                let loaded = selected_loaded_for(&state, selection.account_id())?;
                let room = account_room(&state, selection.account_id(), room_id)?;
                context(selection, &room)?;
                validate_mentions(selection, &room, &text).await?;
                let queue = room.send_queue();
                let (_, updates) = queue.subscribe().await.map_err(|_| mention_unavailable())?;
                context(selection, &room)?;
                let handle = match target {
                    ComposeSendTarget::Edit(ref event_id) => {
                        let baseline = baseline.ok_or_else(unavailable)?;
                        if !baseline.matches(selection, room_id, event_id) {
                            return Err(unavailable());
                        }
                        let event_id = parse_event_id(event_id)?;
                        let revision = current_revision(loaded, &room, &event_id).await?;
                        if revision != baseline.revision {
                            return Err(unavailable());
                        }
                        pending_ready(loaded, &room, &event_id, &revision).await?;
                        context(selection, &room)?;
                        let inherited = inherited_reply_sender(selection, &room, &revision).await?;
                        if inherited != baseline.inherited_reply_sender {
                            return Err(unavailable());
                        }
                        context(selection, &room)?;
                        let mut new_content =
                            formatting::compose_related(&text, room.own_user_id())?;
                        if let Some(sender) = inherited {
                            new_content
                                .mentions
                                .as_mut()
                                .ok_or_else(unavailable)?
                                .user_ids
                                .insert(sender);
                        }
                        let mut content = room
                            .make_edit_event(&event_id, EditedContent::RoomMessage(new_content))
                            .await
                            .map_err(|_| unavailable())?;
                        context(selection, &room)?;
                        correct_edit_mentions(&mut content, &event_id, &revision.mentions)?;
                        bounded_event(&content)?;
                        // Re-read after the SDK's asynchronous author/type/relation
                        // work; never silently diff against a changed effective edit.
                        if current_revision(loaded, &room, &event_id).await? != revision {
                            return Err(unavailable());
                        }
                        validate_mentions(selection, &room, &text).await?;
                        let (echoes, edit_updates) =
                            queue.subscribe().await.map_err(|_| pending())?;
                        reject_pending_echoes(&echoes, &event_id)?;
                        reconcile_pending_capacity(loaded, selection).await?;
                        if current_revision(loaded, &room, &event_id).await? != revision {
                            return Err(unavailable());
                        }
                        context(selection, &room)?;
                        let key = (room_id.to_owned(), event_id.to_string());
                        let mut pending = loaded.composed_edits.lock().await;
                        context(selection, &room)?;
                        reserve_pending(&mut pending, key.clone(), edit_updates)?;
                        let record = pending.get_mut(&key).ok_or_else(self::pending)?;
                        // This guard is held only across enqueue, never the earlier
                        // timeline/network preparation. Record the known handle
                        // without another await that could lose it to the deadline.
                        enqueue_entered.store(true, std::sync::atomic::Ordering::Release);
                        let handle = queue
                            .send(content)
                            .await
                            .map_err(|_| MatrixCoreError::ComposedSendUncertain)?;
                        record.transaction = Some(handle.transaction_id().to_string());
                        handle
                    }
                    ComposeSendTarget::Room => {
                        if baseline.is_some() {
                            return Err(unavailable());
                        }
                        let content = formatting::compose_room(&text, room.own_user_id())?;
                        bounded_event(&AnyMessageLikeEventContent::RoomMessage(content.clone()))?;
                        context(selection, &room)?;
                        enqueue_entered.store(true, std::sync::atomic::Ordering::Release);
                        queue
                            .send(content.into())
                            .await
                            .map_err(|_| MatrixCoreError::ComposedSendUncertain)?
                    }
                    ComposeSendTarget::Reply(ref event) => {
                        if baseline.is_some() {
                            return Err(unavailable());
                        }
                        let content = room
                            .make_reply_event(
                                formatting::compose_related(&text, room.own_user_id())?,
                                matrix_sdk::room::reply::Reply {
                                    event_id: parse_event_id(event)?,
                                    enforce_thread:
                                        matrix_sdk::room::reply::EnforceThread::MaybeThreaded,
                                    add_mentions:
                                        matrix_sdk::ruma::events::room::message::AddMentions::Yes,
                                },
                            )
                            .await
                            .map_err(|_| mention_unavailable())?;
                        context(selection, &room)?;
                        validate_mentions(selection, &room, &text).await?;
                        bounded_event(&AnyMessageLikeEventContent::RoomMessage(content.clone()))?;
                        enqueue_entered.store(true, std::sync::atomic::Ordering::Release);
                        queue
                            .send(content.into())
                            .await
                            .map_err(|_| MatrixCoreError::ComposedSendUncertain)?
                    }
                    ComposeSendTarget::Thread(ref root) => {
                        if baseline.is_some() {
                            return Err(unavailable());
                        }
                        let timeline = loaded.thread_timeline(room_id, root).await?;
                        context(selection, &room)?;
                        validate_mentions(selection, &room, &text).await?;
                        let content = formatting::compose_room(&text, room.own_user_id())?;
                        bounded_event(&AnyMessageLikeEventContent::RoomMessage(content.clone()))?;
                        // SDK adds the correct current-thread fallback and applies
                        // its own reply-author policy; do not forge relations.
                        enqueue_entered.store(true, std::sync::atomic::Ordering::Release);
                        timeline
                            .send(content.into())
                            .await
                            .map_err(|_| MatrixCoreError::ComposedSendUncertain)?
                    }
                };
                // A known handle is still needed for acknowledgement/cleanup
                // if selection was revoked during enqueue. It grants no right
                // to update a stale UI; the caller retains its selection guard.
                Ok(QueuedMessage {
                    transaction_id: handle.transaction_id().to_string(),
                    handle,
                    updates,
                    finished: false,
                })
            })
            .await
            .map_err(|_| preparation_timeout(&enqueue_entered))?
        }
    }

    fn preparation_timeout(entered: &std::sync::atomic::AtomicBool) -> MatrixCoreError {
        if entered.load(std::sync::atomic::Ordering::Acquire) {
            MatrixCoreError::ComposedSendUncertain
        } else {
            MatrixCoreError::Sdk(
                "Message preparation timed out before enqueue. The draft is unchanged.".into(),
            )
        }
    }

    async fn room_mention_allowed(room: &Room) -> Result<bool> {
        use matrix_sdk::{
            deserialized_responses::RawSyncOrStrippedState,
            ruma::events::room::power_levels::RoomPowerLevelsEventContent,
        };
        let state = room
            .get_state_event_static::<RoomPowerLevelsEventContent>()
            .await
            .map_err(|_| mention_unavailable())?
            .ok_or_else(mention_unavailable)?;
        let RawSyncOrStrippedState::Sync(raw) = state else {
            return Err(mention_unavailable());
        };
        if raw.json().get().len() > MAX_EVENT_BYTES {
            return Err(mention_unavailable());
        }
        let content = raw
            .get_field::<serde_json::Value>("content")
            .map_err(|_| mention_unavailable())?
            .ok_or_else(mention_unavailable)?;
        power_content_shape(&content)?;
        let event = raw.deserialize().map_err(|_| mention_unavailable())?;
        let rules = room
            .version()
            .and_then(|version| version.rules())
            .ok_or_else(mention_unavailable)?;
        let creators = room.creators().ok_or_else(mention_unavailable)?;
        // Compute from this exact validated snapshot; a second cache fetch could
        // otherwise validate one revision and authorize using another.
        Ok(event
            .power_levels(&rules.authorization, creators)
            .user_can_trigger_room_notification(room.own_user_id()))
    }
    fn power_content_shape(content: &serde_json::Value) -> Result<()> {
        let object = content.as_object().ok_or_else(mention_unavailable)?;
        for field in ["users", "events", "notifications"] {
            if object.get(field).is_some_and(|value| !value.is_object()) {
                return Err(mention_unavailable());
            }
        }
        Ok(())
    }

    async fn validate_mentions(
        selection: &AccountSelection,
        room: &Room,
        text: &ComposeText,
    ) -> Result<()> {
        context(selection, room)?;
        for span in &text.mentions {
            match &span.target {
                MentionTarget::User(id) => {
                    let id = parse_user_id(id)?;
                    let member = room
                        .get_member_no_sync(&id)
                        .await
                        .map_err(|_| mention_unavailable())?;
                    if !member.is_some_and(|member| member.membership() == &MembershipState::Join) {
                        return Err(mention_unavailable());
                    }
                }
                MentionTarget::Room => {
                    if !room_mention_allowed(room).await? {
                        return Err(mention_unavailable());
                    }
                }
            }
            context(selection, room)?;
        }
        Ok(())
    }

    fn reserve_pending(
        entries: &mut HashMap<(String, String), PendingEdit>,
        key: (String, String),
        updates: broadcast::Receiver<RoomSendQueueUpdate>,
    ) -> Result<()> {
        if entries.contains_key(&key) || entries.len() >= MAX_PENDING_EDITS {
            return Err(pending());
        }
        entries.insert(
            key,
            PendingEdit {
                transaction: None,
                updates,
                sent: None,
                uncertain: false,
            },
        );
        Ok(())
    }

    fn retire_resolved(
        records: &mut HashMap<(String, String), PendingEdit>,
        key: &(String, String),
        revision: &EditRevision,
    ) -> bool {
        let resolved = records
            .get_mut(key)
            .is_some_and(|record| record.resolved(revision));
        if resolved {
            records.remove(key);
        }
        resolved
    }

    async fn reconcile_pending_capacity(
        loaded: &LoadedAccount,
        selection: &AccountSelection,
    ) -> Result<()> {
        let keys: Vec<_> = {
            let records = loaded.composed_edits.lock().await;
            if records.len() < MAX_PENDING_EDITS {
                return Ok(());
            }
            records.keys().cloned().collect()
        };
        // Reclaim only records whose SDK acknowledgement and actual current
        // effective edit agree. Unknown/lagged results are never capacity victims.
        // No pending-map guard is retained over a timeline/store await.
        tokio::time::timeout(Duration::from_secs(3), async {
            for key in keys.into_iter().take(MAX_PENDING_EDITS) {
                if !selection.is_current() {
                    return Err(MatrixCoreError::SelectedAccountChanged);
                }
                let Ok(room_id) = RoomId::parse(&key.0) else {
                    continue;
                };
                let Some(room) = loaded.client.get_room(&room_id) else {
                    continue;
                };
                if room.state() != RoomState::Joined {
                    continue;
                }
                let Ok(event) = parse_event_id(&key.1) else {
                    continue;
                };
                let revision = current_revision(loaded, &room, &event).await;
                if !selection.is_current() {
                    return Err(MatrixCoreError::SelectedAccountChanged);
                }
                if room.state() != RoomState::Joined {
                    continue;
                }
                if let Ok(revision) = revision {
                    let mut records = loaded.composed_edits.lock().await;
                    if !selection.is_current() {
                        return Err(MatrixCoreError::SelectedAccountChanged);
                    }
                    retire_resolved(&mut records, &key, &revision);
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| pending())?
    }

    async fn pending_ready(
        loaded: &LoadedAccount,
        room: &Room,
        event: &EventId,
        revision: &EditRevision,
    ) -> Result<()> {
        let (echoes, _) = room.send_queue().subscribe().await.map_err(|_| pending())?;
        reject_pending_echoes(&echoes, event)?;
        let key = (room.room_id().to_string(), event.to_string());
        let mut pending = loaded.composed_edits.lock().await;
        if pending.contains_key(&key) && !retire_resolved(&mut pending, &key, revision) {
            return Err(self::pending());
        }
        Ok(())
    }
    fn reject_pending_echoes(
        echoes: &[matrix_sdk::send_queue::LocalEcho],
        event: &EventId,
    ) -> Result<()> {
        if echoes.len() > 256 {
            return Err(pending());
        }
        for echo in echoes {
            if let LocalEchoContent::Event {
                serialized_event, ..
            } = &echo.content
            {
                if let AnyMessageLikeEventContent::RoomMessage(content) =
                    serialized_event.deserialize().map_err(|_| pending())?
                {
                    if matches!(content.relates_to, Some(Relation::Replacement(ref replacement)) if replacement.event_id == event)
                    {
                        return Err(pending());
                    }
                }
            }
        }
        Ok(())
    }
    fn bounded_event(content: &AnyMessageLikeEventContent) -> Result<()> {
        let encoded = serde_json::to_vec(content).map_err(|_| mention_unavailable())?;
        if encoded.len() > MAX_ENCODED_COMPOSITION {
            return Err(ComposeError::TooLarge.into());
        }
        Ok(())
    }

    async fn current_revision(
        loaded: &LoadedAccount,
        room: &Room,
        event: &EventId,
    ) -> Result<EditRevision> {
        // The ordinary room view hides thread messages. Also inspect already
        // open thread timelines, but never fetch a target into a new view merely
        // to invent an effective replacement baseline.
        let timelines: Vec<_> = loaded
            .timelines
            .lock()
            .await
            .values()
            .filter(|timeline| timeline.room().room_id() == room.room_id())
            .cloned()
            .collect();
        if timelines.len() > 64 {
            return Err(unavailable());
        }
        let mut found = None;
        let mut examined = 0usize;
        for timeline in timelines {
            let (items, _) = timeline.subscribe().await;
            examined = examined.saturating_add(items.len());
            if examined > 16384 {
                return Err(unavailable());
            }
            for item in items.iter().filter_map(|item| item.as_event()) {
                if item.event_id() != Some(event) {
                    continue;
                }
                let revision = revision_from_item(item, room.own_user_id(), event)?;
                if found.as_ref().is_some_and(|previous| previous != &revision) {
                    return Err(unavailable());
                }
                found = Some(revision);
            }
        }
        found.ok_or_else(unavailable)
    }
    fn revision_from_item(
        item: &EventTimelineItem,
        own: &UserId,
        event: &EventId,
    ) -> Result<EditRevision> {
        if !item.is_own() || !item.is_editable() || item.sender() != own {
            return Err(unavailable());
        }
        let message = item.content().as_message().ok_or_else(unavailable)?;
        let revision = revision_from_raw(
            item.original_json().ok_or_else(unavailable)?,
            item.latest_edit_json(),
            own,
            event,
        )?;
        let effective: RoomMessageEventContentWithoutRelation =
            serde_json::from_value(revision.effective.clone()).map_err(|_| unavailable())?;
        if !matches!(message.msgtype(), MessageType::Text(_))
            || message.body() != effective.msgtype.body()
            || !same_mentions(message.mentions(), &revision.mentions)
            || message.is_edited() != revision.latest_edit.is_some()
        {
            return Err(unavailable());
        }
        Ok(revision)
    }
    fn original_message(
        raw: &Raw<AnySyncTimelineEvent>,
    ) -> Result<matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent> {
        if raw.json().get().len() > MAX_EVENT_BYTES {
            return Err(unavailable());
        }
        let content = raw
            .get_field::<serde_json::Value>("content")
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        content_shape(&content)?;
        if let Some(replacement) = content.get("m.new_content") {
            content_shape(replacement)?;
        }
        match raw.deserialize().map_err(|_| unavailable())? {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncMessageLikeEvent::Original(event),
            )) => Ok(event),
            _ => Err(unavailable()),
        }
    }
    fn content_shape(content: &serde_json::Value) -> Result<()> {
        let object = content.as_object().ok_or_else(unavailable)?;
        if !object.get("body").is_some_and(serde_json::Value::is_string)
            || !object
                .get("msgtype")
                .is_some_and(serde_json::Value::is_string)
        {
            return Err(unavailable());
        }
        if let Some(mentions) = object.get("m.mentions") {
            let mentions = mentions.as_object().ok_or_else(unavailable)?;
            if let Some(users) = mentions.get("user_ids") {
                let users = users.as_array().ok_or_else(unavailable)?;
                if users.len() > compose::MAX_MENTION_SPANS
                    || users.iter().any(|value| !value.is_string())
                {
                    return Err(unavailable());
                }
            }
            if mentions
                .get("room")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(unavailable());
            }
        }
        Ok(())
    }

    fn revision_from_raw(
        original: &Raw<AnySyncTimelineEvent>,
        edit: Option<&Raw<AnySyncTimelineEvent>>,
        own: &UserId,
        event_id: &EventId,
    ) -> Result<EditRevision> {
        let original = original_message(original)?;
        if original.event_id != event_id
            || original.sender != own
            || !matches!(original.content.msgtype, MessageType::Text(_))
            || matches!(original.content.relates_to, Some(Relation::Replacement(_)))
        {
            return Err(unavailable());
        }
        let original_content =
            serde_json::to_value(&original.content).map_err(|_| unavailable())?;
        let (effective, latest_edit) = if let Some(edit) = edit {
            let edit = original_message(edit)?;
            if edit.sender != own || edit.event_id == event_id {
                return Err(unavailable());
            }
            let Some(Relation::Replacement(replacement)) = edit.content.relates_to else {
                return Err(unavailable());
            };
            if replacement.event_id != event_id {
                return Err(unavailable());
            }
            (replacement.new_content, Some(edit.event_id.to_string()))
        } else {
            (
                RoomMessageEventContentWithoutRelation::from(original.content),
                None,
            )
        };
        if !matches!(effective.msgtype, MessageType::Text(_))
            || effective.msgtype.body().len() > compose::MAX_COMPOSE_BYTES
        {
            return Err(unavailable());
        }
        let mentions = effective.mentions.clone().ok_or_else(unavailable)?;
        if mentions.user_ids.len() > compose::MAX_MENTION_SPANS {
            return Err(unavailable());
        }
        Ok(EditRevision {
            original: original_content,
            effective: serde_json::to_value(effective).map_err(|_| unavailable())?,
            latest_edit,
            mentions,
        })
    }

    async fn inherited_reply_sender(
        selection: &AccountSelection,
        room: &Room,
        revision: &EditRevision,
    ) -> Result<Option<matrix_sdk::ruma::OwnedUserId>> {
        let body = revision
            .effective
            .get("body")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(unavailable)?;
        let hidden: Vec<_> = revision
            .mentions
            .user_ids
            .iter()
            .filter(|id| id.as_ref() != room.own_user_id() && !body.contains(id.as_str()))
            .collect();
        if hidden.is_empty() {
            return Ok(None);
        }
        if hidden.len() != 1 {
            return Err(unavailable());
        }
        let original: RoomMessageEventContent =
            serde_json::from_value(revision.original.clone()).map_err(|_| unavailable())?;
        let reply = match original.relates_to {
            Some(Relation::Reply(reply)) => reply.in_reply_to.event_id,
            Some(Relation::Thread(thread)) if !thread.is_falling_back => {
                thread.in_reply_to.ok_or_else(unavailable)?.event_id
            }
            _ => return Err(unavailable()),
        };
        context(selection, room)?;
        let encryption = room
            .latest_encryption_state()
            .await
            .map_err(|_| unavailable())?;
        context(selection, room)?;
        if encryption.is_unknown() {
            return Err(unavailable());
        }
        let encrypted = encryption.is_encrypted();
        let event = room
            .event(
                &reply,
                Some(
                    RequestConfig::new()
                        .retry_limit(0)
                        .timeout(Duration::from_secs(5)),
                ),
            )
            .await
            .map_err(|_| unavailable())?;
        context(selection, room)?;
        let raw = event.raw().deserialize().map_err(|_| unavailable())?;
        let AnySyncTimelineEvent::MessageLike(ref message) = raw else {
            return Err(unavailable());
        };
        if message.original_content().is_none()
            || raw.event_id() != reply
            || raw.sender().as_str() != hidden[0].as_str()
        {
            return Err(unavailable());
        }
        if !inherited_reply_is_authenticated(
            encrypted,
            message,
            raw.sender(),
            event.encryption_info().map(|info| info.as_ref()),
        ) {
            return Err(unavailable());
        }
        Ok(Some(raw.sender().to_owned()))
    }

    fn inherited_reply_is_authenticated(
        encrypted_room: bool,
        message: &AnySyncMessageLikeEvent,
        sender: &UserId,
        info: Option<&matrix_sdk::deserialized_responses::EncryptionInfo>,
    ) -> bool {
        // An undecrypted event cannot establish inherited intent, even when the
        // current room state says cleartext. Decrypted metadata also requires
        // verification independently of that current room state.
        if matches!(message, AnySyncMessageLikeEvent::RoomEncrypted(_)) {
            return false;
        }
        if !encrypted_room && info.is_none() {
            return true;
        }
        info.is_some_and(|info| {
            info.sender.as_str() == sender.as_str()
                && matches!(
                    info.verification_state,
                    matrix_sdk::deserialized_responses::VerificationState::Verified
                )
        })
    }

    fn restore_edit(
        revision: &EditRevision,
        own: &UserId,
        inherited: Option<&UserId>,
    ) -> Result<ComposeText> {
        let content: RoomMessageEventContentWithoutRelation =
            serde_json::from_value(revision.effective.clone()).map_err(|_| unavailable())?;
        let MessageType::Text(message) = content.msgtype else {
            return Err(unavailable());
        };
        for format in [TextFormat::Plain, TextFormat::Markdown] {
            let mut text = ComposeText::new(message.body.clone(), format);
            let mut targets: Vec<_> = revision
                .mentions
                .user_ids
                .iter()
                .filter(|id| {
                    let id: &UserId = id.as_ref();
                    id != own && Some(id) != inherited
                })
                .map(|id| MentionTarget::User(id.to_string()))
                .collect();
            if revision.mentions.room {
                targets.push(MentionTarget::Room);
            }
            let mut valid = true;
            for target in targets {
                let mut spans = Vec::new();
                for (start, _) in text.body.match_indices(target.text()) {
                    let span = MentionSpan {
                        range: start..start + target.text().len(),
                        target: target.clone(),
                    };
                    let mut single = ComposeText::new(text.body.clone(), format);
                    single.mentions.push(span.clone());
                    if single.validate().is_ok() {
                        if spans.len() + text.mentions.len() == compose::MAX_MENTION_SPANS {
                            valid = false;
                            break;
                        }
                        spans.push(span);
                    }
                }
                if spans.is_empty() {
                    valid = false;
                    break;
                }
                text.mentions.extend(spans);
            }
            text.mentions.sort_by_key(|span| span.range.start);
            if !valid || text.validate().is_err() {
                continue;
            }
            let encoded = formatting::compose_related(&text, own)?;
            let MessageType::Text(encoded) = encoded.msgtype else {
                return Err(unavailable());
            };
            // Plain Matrix metadata can be intentional without presentation HTML.
            // Rich content must reproduce our deterministic encoder exactly;
            // links alone never become mention intent.
            if message.formatted.is_none() && format == TextFormat::Plain
                || match (&encoded.formatted, &message.formatted) {
                    (None, None) => true,
                    (Some(encoded), Some(original)) => {
                        encoded.format == original.format && encoded.body == original.body
                    }
                    _ => false,
                }
            {
                return Ok(text);
            }
        }
        Err(unavailable())
    }

    fn correct_edit_mentions(
        content: &mut AnyMessageLikeEventContent,
        event: &EventId,
        prior: &Mentions,
    ) -> Result<()> {
        let AnyMessageLikeEventContent::RoomMessage(content) = content else {
            return Err(unavailable());
        };
        let Some(Relation::Replacement(replacement)) = &content.relates_to else {
            return Err(unavailable());
        };
        if replacement.event_id != event {
            return Err(unavailable());
        }
        let full = replacement
            .new_content
            .mentions
            .as_ref()
            .ok_or_else(unavailable)?;
        let mut delta = Mentions::new();
        delta.user_ids = full.user_ids.difference(&prior.user_ids).cloned().collect();
        delta.room = full.room && !prior.room;
        // Only this field changes after SDK construction. In particular the
        // target, fallback and full replacement payload remain SDK-authored.
        content.mentions = Some(delta);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use matrix_sdk::ruma::{event_id, events::room::message::ReplacementMetadata, user_id};
        use serde_json::json;

        fn raw(content: serde_json::Value, id: &str, sender: &str) -> Raw<AnySyncTimelineEvent> {
            Raw::from_json(
                serde_json::value::to_raw_value(&json!({
                    "type": "m.room.message", "event_id": id, "sender": sender,
                    "origin_server_ts": 1, "content": content,
                }))
                .unwrap(),
            )
        }
        #[test]
        fn inherited_reply_requires_event_trust_even_in_cleartext_room() {
            use matrix_sdk::deserialized_responses::{
                AlgorithmInfo, EncryptionInfo, VerificationLevel,
                VerificationState as EventVerificationState,
            };

            let sender = user_id!("@riley:example.org");
            let message: AnySyncMessageLikeEvent = serde_json::from_value(json!({
                "type": "m.room.message", "event_id": "$reply:example.org",
                "sender": sender, "origin_server_ts": 1,
                "content": {"msgtype": "m.text", "body": "Fixture reply"},
            }))
            .unwrap();
            let ciphertext: AnySyncMessageLikeEvent = serde_json::from_value(json!({
                "type": "m.room.encrypted", "event_id": "$reply:example.org",
                "sender": sender, "origin_server_ts": 1,
                "content": {
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "ciphertext": "fixture", "session_id": "fixture",
                },
            }))
            .unwrap();
            let mut info = EncryptionInfo {
                sender: sender.to_owned(),
                sender_device: None,
                forwarder: None,
                algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                    curve25519_key: "fixture".into(),
                    sender_claimed_keys: Default::default(),
                    session_id: None,
                },
                verification_state: EventVerificationState::Unverified(
                    VerificationLevel::UnverifiedIdentity,
                ),
            };
            assert!(inherited_reply_is_authenticated(
                false, &message, sender, None
            ));
            assert!(!inherited_reply_is_authenticated(
                true, &message, sender, None
            ));
            assert!(!inherited_reply_is_authenticated(
                false,
                &message,
                sender,
                Some(&info)
            ));
            info.verification_state = EventVerificationState::Verified;
            assert!(inherited_reply_is_authenticated(
                false,
                &message,
                sender,
                Some(&info)
            ));
            assert!(inherited_reply_is_authenticated(
                true,
                &message,
                sender,
                Some(&info)
            ));
            assert!(!inherited_reply_is_authenticated(
                false,
                &ciphertext,
                sender,
                None
            ));
            assert!(!inherited_reply_is_authenticated(
                false,
                &ciphertext,
                sender,
                Some(&info)
            ));
            info.sender = user_id!("@jules:example.org").to_owned();
            assert!(!inherited_reply_is_authenticated(
                false,
                &message,
                sender,
                Some(&info)
            ));
        }

        fn original(content: serde_json::Value) -> Raw<AnySyncTimelineEvent> {
            raw(content, "$original:example.org", "@casey:example.org")
        }
        fn revision(content: serde_json::Value) -> EditRevision {
            revision_from_raw(
                &original(content),
                None,
                user_id!("@casey:example.org"),
                event_id!("$original:example.org"),
            )
            .unwrap()
        }
        fn users(ids: &[&str], room: bool) -> Mentions {
            let mut mentions = Mentions::new();
            mentions.user_ids = ids.iter().map(|id| UserId::parse(*id).unwrap()).collect();
            mentions.room = room;
            mentions
        }
        fn text(body: &str, format: TextFormat, ids: &[&str]) -> ComposeText {
            let mut text = ComposeText::new(body, format);
            for id in ids {
                let start = body.find(id).unwrap();
                text.mentions.push(MentionSpan {
                    range: start..start + id.len(),
                    target: MentionTarget::User((*id).into()),
                });
            }
            text.mentions.sort_by_key(|span| span.range.start);
            text
        }
        fn pending_record(
            capacity: usize,
        ) -> (broadcast::Sender<RoomSendQueueUpdate>, PendingEdit) {
            let (sender, updates) = broadcast::channel(capacity);
            (
                sender,
                PendingEdit {
                    transaction: Some("edit-transaction".into()),
                    updates,
                    sent: None,
                    uncertain: false,
                },
            )
        }

        #[test]
        fn edit_delta_uses_effective_mentions_and_preserves_sdk_replacement() {
            let full = RoomMessageEventContentWithoutRelation::text_plain("new body")
                .add_mentions(users(&["@riley:example.org", "@morgan:example.org"], true));
            let event = event_id!("$original:example.org");
            // SDK construction can only see original metadata. The current
            // effective edit has already added Riley and room, so neither rings again.
            let mut content = AnyMessageLikeEventContent::RoomMessage(full.make_replacement(
                ReplacementMetadata::new(event.to_owned(), Some(Mentions::new())),
            ));
            let before = serde_json::to_value(&content).unwrap();
            correct_edit_mentions(&mut content, event, &users(&["@riley:example.org"], true))
                .unwrap();
            let after = serde_json::to_value(&content).unwrap();
            assert_eq!(after["m.new_content"], before["m.new_content"]);
            assert_eq!(after["m.relates_to"], before["m.relates_to"]);
            assert_eq!(after["body"], before["body"]);
            assert_eq!(
                after["m.mentions"],
                json!({"user_ids":["@morgan:example.org"]})
            );
        }

        #[test]
        fn edits_can_reintroduce_removed_mentions_and_remove_without_notifications() {
            let event = event_id!("$original:example.org");
            let riley = users(&["@riley:example.org"], true);
            let mut content = AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContentWithoutRelation::text_plain("again")
                    .add_mentions(riley.clone())
                    .make_replacement(ReplacementMetadata::new(
                        event.to_owned(),
                        Some(riley.clone()),
                    )),
            );
            correct_edit_mentions(&mut content, event, &Mentions::new()).unwrap();
            assert_eq!(
                serde_json::to_value(&content).unwrap()["m.mentions"],
                json!({"user_ids":["@riley:example.org"],"room":true})
            );
            let mut removal = AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContentWithoutRelation::text_plain("removed")
                    .add_mentions(Mentions::new())
                    .make_replacement(ReplacementMetadata::new(
                        event.to_owned(),
                        Some(riley.clone()),
                    )),
            );
            correct_edit_mentions(&mut removal, event, &riley).unwrap();
            let value = serde_json::to_value(&removal).unwrap();
            assert_eq!(value["m.mentions"], json!({}));
            assert_eq!(value["m.new_content"]["m.mentions"], json!({}));
            assert!(
                correct_edit_mentions(&mut removal, event_id!("$wrong:example.org"), &riley)
                    .is_err()
            );
        }

        #[test]
        fn revision_is_full_latest_content_not_top_level_delta() {
            let original = original(json!({"msgtype":"m.text","body":"old","m.mentions":{}}));
            let edit = raw(
                json!({"msgtype":"m.text","body":"* replacement","m.mentions":{},
                "m.relates_to":{"rel_type":"m.replace","event_id":"$original:example.org"},
                "m.new_content":{"msgtype":"m.text","body":"@riley:example.org","m.mentions":{"user_ids":["@riley:example.org"]}}}),
                "$edit:example.org",
                "@casey:example.org",
            );
            let revision = revision_from_raw(
                &original,
                Some(&edit),
                user_id!("@casey:example.org"),
                event_id!("$original:example.org"),
            )
            .unwrap();
            assert_eq!(revision.latest_edit.as_deref(), Some("$edit:example.org"));
            assert_eq!(revision.effective["body"], "@riley:example.org");
            assert!(
                revision
                    .mentions
                    .user_ids
                    .contains(user_id!("@riley:example.org"))
            );
        }

        #[test]
        fn revision_rejects_wrong_author_target_type_redaction_and_missing_metadata() {
            let content = json!({"msgtype":"m.text","body":"old","m.mentions":{}});
            let original = original(content.clone());
            for (sender, target, msgtype) in [
                ("@riley:example.org", "$original:example.org", "m.text"),
                ("@casey:example.org", "$wrong:example.org", "m.text"),
                ("@casey:example.org", "$original:example.org", "m.notice"),
            ] {
                let edit = raw(
                    json!({"msgtype":"m.text","body":"* replacement","m.mentions":{},
                    "m.relates_to":{"rel_type":"m.replace","event_id":target},
                    "m.new_content":{"msgtype":msgtype,"body":"new","m.mentions":{}}}),
                    "$edit:example.org",
                    sender,
                );
                assert!(
                    revision_from_raw(
                        &original,
                        Some(&edit),
                        user_id!("@casey:example.org"),
                        event_id!("$original:example.org")
                    )
                    .is_err()
                );
            }
            for bad in [
                json!({}),
                json!({"msgtype":"m.text","body":"legacy"}),
                json!({"msgtype":"m.text","body":"bad","m.mentions":[]}),
            ] {
                assert!(
                    revision_from_raw(
                        &self::original(bad),
                        None,
                        user_id!("@casey:example.org"),
                        event_id!("$original:example.org")
                    )
                    .is_err()
                );
            }
            assert!(
                revision_from_raw(
                    &raw(content, "$original:example.org", "@riley:example.org"),
                    None,
                    user_id!("@casey:example.org"),
                    event_id!("$original:example.org")
                )
                .is_err()
            );
        }

        #[test]
        fn malformed_content_and_mentions_shapes_do_not_use_serde_defaults() {
            for value in [
                json!([]),
                json!({"body":1,"msgtype":"m.text"}),
                json!({"body":"text","msgtype":"m.text","m.mentions":[]}),
                json!({"body":"text","msgtype":"m.text","m.mentions":{"user_ids":{}}}),
                json!({"body":"text","msgtype":"m.text","m.mentions":{"user_ids":[1]}}),
                json!({"body":"text","msgtype":"m.text","m.mentions":{"room":"true"}}),
            ] {
                assert!(content_shape(&value).is_err());
            }
            assert!(
                content_shape(
                    &json!({"body":"text","msgtype":"m.text","m.mentions":{},"extension":[]})
                )
                .is_ok()
            );
        }

        #[test]
        fn plain_and_markdown_roundtrip_only_explicit_metadata() {
            let own = user_id!("@casey:example.org");
            for format in [TextFormat::Plain, TextFormat::Markdown] {
                let expected = text(
                    "Hello @riley:example.org **literal-or-bold**",
                    format,
                    &["@riley:example.org"],
                );
                let content = formatting::compose_room(&expected, own).unwrap();
                let baseline = revision(serde_json::to_value(content).unwrap());
                let restored = restore_edit(&baseline, own, None).unwrap();
                assert_eq!(restored, expected);
            }
        }

        #[test]
        fn plain_metadata_restores_intent_but_links_do_not_create_it() {
            let own = user_id!("@casey:example.org");
            let baseline = revision(
                json!({"msgtype":"m.text","body":"Hello @riley:example.org","m.mentions":{"user_ids":["@riley:example.org"]}}),
            );
            assert_eq!(
                restore_edit(&baseline, own, None).unwrap().mentions.len(),
                1
            );
            let pasted = revision(
                json!({"msgtype":"m.text","body":"https://matrix.to/#/@riley:example.org","m.mentions":{}}),
            );
            assert!(
                restore_edit(&pasted, own, None)
                    .unwrap()
                    .mentions
                    .is_empty()
            );
            let arbitrary = revision(
                json!({"msgtype":"m.text","body":"Hello","format":"org.matrix.custom.html","formatted_body":"<a href=\"https://matrix.to/#/@riley:example.org\">Hello</a>","m.mentions":{}}),
            );
            assert!(restore_edit(&arbitrary, own, None).is_err());
        }

        #[test]
        fn hidden_reply_sender_requires_separate_authenticated_baseline_proof() {
            let own = user_id!("@casey:example.org");
            let baseline = revision(
                json!({"msgtype":"m.text","body":"A reply","m.mentions":{"user_ids":["@riley:example.org"]},"m.relates_to":{"m.in_reply_to":{"event_id":"$reply:example.org"}}}),
            );
            assert!(restore_edit(&baseline, own, None).is_err());
            let restored =
                restore_edit(&baseline, own, Some(user_id!("@riley:example.org"))).unwrap();
            assert!(restored.mentions.is_empty());
            let full = formatting::compose_related(&restored, own)
                .unwrap()
                .add_mentions(users(&["@riley:example.org"], false));
            let event = event_id!("$original:example.org");
            let mut content = AnyMessageLikeEventContent::RoomMessage(
                full.make_replacement(ReplacementMetadata::new(event.to_owned(), None)),
            );
            correct_edit_mentions(&mut content, event, &baseline.mentions).unwrap();
            let value = serde_json::to_value(content).unwrap();
            assert_eq!(value["m.mentions"], json!({}));
            assert_eq!(
                value["m.new_content"]["m.mentions"]["user_ids"],
                json!(["@riley:example.org"])
            );
        }

        #[test]
        fn unrepresentable_intent_and_span_overflow_fail_closed() {
            let own = user_id!("@casey:example.org");
            for body in [
                "a@riley:example.org",
                "https://matrix.to/#/@riley:example.org",
            ] {
                let baseline = revision(
                    json!({"msgtype":"m.text","body":body,"m.mentions":{"user_ids":["@riley:example.org"]}}),
                );
                assert!(restore_edit(&baseline, own, None).is_err());
            }
            let body = "@riley:example.org ".repeat(compose::MAX_MENTION_SPANS + 1);
            let baseline = revision(
                json!({"msgtype":"m.text","body":body,"m.mentions":{"user_ids":["@riley:example.org"]}}),
            );
            assert!(restore_edit(&baseline, own, None).is_err());
        }

        #[test]
        fn edit_ack_requires_matching_effective_revision_not_just_http_success() {
            let mut revision = revision(json!({"msgtype":"m.text","body":"text","m.mentions":{}}));
            let (sender, mut record) = pending_record(4);
            sender
                .send(RoomSendQueueUpdate::SentEvent {
                    transaction_id: "edit-transaction".into(),
                    event_id: event_id!("$sent:example.org").to_owned(),
                })
                .unwrap();
            assert!(!record.resolved(&revision));
            revision.latest_edit = Some("$another:example.org".into());
            assert!(!record.resolved(&revision));
            revision.latest_edit = Some("$sent:example.org".into());
            assert!(record.resolved(&revision));
        }

        #[test]
        fn pending_unknown_lagged_and_closed_receivers_never_rearm() {
            let revision = revision(json!({"msgtype":"m.text","body":"text","m.mentions":{}}));
            let (sender, mut unknown) = pending_record(4);
            unknown.transaction = None;
            sender
                .send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: "edit-transaction".into(),
                })
                .unwrap();
            assert!(!unknown.resolved(&revision));
            let (sender, mut lagged) = pending_record(1);
            for _ in 0..3 {
                sender
                    .send(RoomSendQueueUpdate::CancelledLocalEvent {
                        transaction_id: "unrelated".into(),
                    })
                    .unwrap();
            }
            assert!(!lagged.resolved(&revision));
            sender
                .send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: "edit-transaction".into(),
                })
                .unwrap();
            assert!(!lagged.resolved(&revision));
            let (sender, mut closed) = pending_record(1);
            drop(sender);
            assert!(!closed.resolved(&revision));
            assert!(closed.uncertain);
        }

        #[test]
        fn known_queue_cancellation_releases_only_matching_transaction() {
            let revision = revision(json!({"msgtype":"m.text","body":"text","m.mentions":{}}));
            let (sender, mut record) = pending_record(4);
            sender
                .send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: "other".into(),
                })
                .unwrap();
            assert!(!record.resolved(&revision));
            sender
                .send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: "edit-transaction".into(),
                })
                .unwrap();
            assert!(record.resolved(&revision));
        }

        #[test]
        fn edit_reservation_is_bounded_and_never_evicts_unknown_outcomes() {
            let mut records = HashMap::new();
            for index in 0..MAX_PENDING_EDITS {
                let (_, receiver) = broadcast::channel(1);
                reserve_pending(
                    &mut records,
                    (
                        "!room:example.org".into(),
                        format!("$event{index}:example.org"),
                    ),
                    receiver,
                )
                .unwrap();
            }
            let (_, receiver) = broadcast::channel(1);
            assert!(
                reserve_pending(
                    &mut records,
                    ("!room:example.org".into(), "$new:example.org".into()),
                    receiver
                )
                .is_err()
            );
            let (_, receiver) = broadcast::channel(1);
            assert!(
                reserve_pending(
                    &mut records,
                    ("!room:example.org".into(), "$event0:example.org".into()),
                    receiver
                )
                .is_err()
            );
            assert_eq!(records.len(), MAX_PENDING_EDITS);
            assert!(records.values().all(|record| record.transaction.is_none()));
        }

        #[test]
        fn successful_distinct_edits_reclaim_capacity_without_discarding_uncertainty() {
            let mut records = HashMap::new();
            let mut acknowledged = Vec::new();
            let mut retained_senders = Vec::new();
            // One reservation whose enqueue outcome is unknown must survive
            // successful acknowledgements for every other target.
            let unknown_key = ("!room:example.org".into(), "$unknown:example.org".into());
            let (sender, updates) = broadcast::channel(4);
            retained_senders.push(sender);
            reserve_pending(&mut records, unknown_key.clone(), updates).unwrap();
            for index in 0..(MAX_PENDING_EDITS * 2 + 1) {
                if records.len() == MAX_PENDING_EDITS {
                    for (key, revision) in &acknowledged {
                        retire_resolved(&mut records, key, revision);
                    }
                    assert_eq!(records.len(), 1);
                    assert!(records.contains_key(&unknown_key));
                }
                let key = (
                    "!room:example.org".into(),
                    format!("$event{index}:example.org"),
                );
                let (sender, updates) = broadcast::channel(4);
                reserve_pending(&mut records, key.clone(), updates).unwrap();
                records.get_mut(&key).unwrap().transaction = Some("edit-transaction".into());
                let event_id = EventId::parse(format!("$edit{index}:example.org")).unwrap();
                sender
                    .send(RoomSendQueueUpdate::SentEvent {
                        transaction_id: "edit-transaction".into(),
                        event_id: event_id.clone(),
                    })
                    .unwrap();
                retained_senders.push(sender);
                let mut observed =
                    revision(json!({"msgtype":"m.text","body":"changed","m.mentions":{}}));
                observed.latest_edit = Some(event_id.to_string());
                acknowledged.push((key, observed));
            }
            assert!(records.contains_key(&unknown_key));
            assert!(records.len() <= MAX_PENDING_EDITS);
            assert_eq!(acknowledged.len(), MAX_PENDING_EDITS * 2 + 1);
        }

        #[test]
        fn edit_baseline_is_revoked_by_account_aba_and_debug_is_redacted() {
            let selections = account_selection::Selections::default();
            selections.replace(Some("account-one"));
            let first = selections.for_account("account-one").unwrap();
            let baseline = EditBaseline {
                selection: first.clone(),
                room: "!private:example.org".into(),
                event: "$private:example.org".into(),
                revision: revision(
                    json!({"msgtype":"m.text","body":"private source","m.mentions":{}}),
                ),
                inherited_reply_sender: None,
            };
            assert!(baseline.matches(&first, "!private:example.org", "$private:example.org"));
            assert_eq!(format!("{baseline:?}"), "EditBaseline([redacted])");
            selections.replace(Some("account-two"));
            selections.replace(Some("account-one"));
            assert!(!baseline.matches(
                &selections.for_account("account-one").unwrap(),
                "!private:example.org",
                "$private:example.org"
            ));
        }

        #[test]
        fn power_state_sequence_shapes_never_grant_room_permission() {
            use matrix_sdk::ruma::events::room::power_levels::RoomPowerLevelsEventContent;
            let malformed = json!({"users":{"@casey:example.org":100},"notifications":[]});
            // Pinned Ruma accepts this struct sequence as default notifications.
            assert!(
                serde_json::from_value::<RoomPowerLevelsEventContent>(malformed.clone()).is_ok()
            );
            for value in [
                json!([]),
                malformed,
                json!({"users":[]}),
                json!({"events":[]}),
            ] {
                assert!(power_content_shape(&value).is_err());
            }
        }

        #[test]
        fn valid_power_state_preserves_real_notification_threshold() {
            use matrix_sdk::ruma::{
                RoomVersionId, events::room::power_levels::RoomPowerLevelsEventContent,
            };
            let content = json!({"users":{"@casey:example.org":50},"notifications":{"room":75},"extension":[]});
            power_content_shape(&content).unwrap();
            let content: RoomPowerLevelsEventContent = serde_json::from_value(content).unwrap();
            let rules = RoomVersionId::V10.rules().unwrap();
            let levels = matrix_sdk::ruma::events::room::power_levels::RoomPowerLevels::new(
                content.into(),
                &rules.authorization,
                [user_id!("@casey:example.org").to_owned()],
            );
            assert!(!levels.user_can_trigger_room_notification(user_id!("@casey:example.org")));
            power_content_shape(&json!({})).unwrap();
            assert!(serde_json::from_value::<RoomPowerLevelsEventContent>(json!({})).is_ok());
        }

        #[test]
        fn enqueue_entry_distinguishes_uncertainty_from_safe_preparation_failure() {
            let entered = std::sync::atomic::AtomicBool::new(false);
            assert!(!matches!(
                preparation_timeout(&entered),
                MatrixCoreError::ComposedSendUncertain
            ));
            entered.store(true, std::sync::atomic::Ordering::Release);
            assert!(matches!(
                preparation_timeout(&entered),
                MatrixCoreError::ComposedSendUncertain
            ));
            // The state is terminal for this attempt; a later cancellation
            // cannot reclassify a possibly persisted request as safe to retry.
            assert!(matches!(
                preparation_timeout(&entered),
                MatrixCoreError::ComposedSendUncertain
            ));
        }

        #[test]
        fn final_encoded_event_limit_and_safe_labels_are_bounded() {
            let oversized =
                RoomMessageEventContent::text_plain("x".repeat(MAX_ENCODED_COMPOSITION))
                    .add_mentions(Mentions::new());
            assert!(bounded_event(&AnyMessageLikeEventContent::RoomMessage(oversized)).is_err());
            assert_eq!(label("Casey\u{202e}\n\u{2067}"), "Casey");
            assert_eq!(label(&"界".repeat(100)).chars().count(), 80);
        }
    }
}
