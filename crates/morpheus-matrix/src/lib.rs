//! Matrix account and messaging core for the Morpheus desktop client.
//!
//! One Matrix `Client` and SQLite store belong to each account/device. Only the
//! selected account has an active sync loop. The OS credential store holds the
//! session tokens and the passphrase for that account's SQLite store.

#![recursion_limit = "512"]

mod media;
pub use media::{
    AttachmentKind, AttachmentMetadata, AttachmentSendUpdate, DownloadedAttachment,
    MAX_ATTACHMENT_BYTES, QueuedAttachment,
};

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
            key::verification::VerificationMethod,
            room::message::{RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
            space::child::SpaceChildEventContent,
        },
        room::RoomType,
        serde::Raw,
    },
    send_queue::RoomSendQueueUpdate,
    store::RoomLoadSettings,
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
    #[error("room not found: {0}")]
    RoomNotFound(String),
    #[error("invalid room ID: {0}")]
    InvalidRoomId(String),
    #[error("invalid event ID: {0}")]
    InvalidEventId(String),
    #[error("invalid media: {0}")]
    InvalidMedia(String),
    #[error("attachment exceeds {maximum} bytes (received {actual} bytes)")]
    MediaTooLarge { maximum: u64, actual: u64 },
    #[error("this event is not a media attachment")]
    NotMediaAttachment,
    #[error("attachment has no size metadata")]
    MissingMediaSize,
    #[error("invalid user ID: {0}")]
    InvalidUserId(String),
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
}

struct PendingSso {
    homeserver: String,
    redirect_url: Url,
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
    session: MatrixSession,
}

struct LoadedAccount {
    client: Client,
    timelines: Mutex<HashMap<String, Arc<Timeline>>>,
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
            sync_tx,
        })
    }

    pub fn subscribe_active_account(&self) -> watch::Receiver<Option<AccountInfo>> {
        self.active_tx.subscribe()
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
        if let Err(e) = save_secret(
            &id,
            StoredSecret {
                version: SECRET_VERSION,
                store_passphrase: passphrase.clone(),
                session,
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
        stop_sync(&mut state).await;
        let loaded = Arc::new(LoadedAccount::new(client, id.clone(), passphrase));
        state.loaded.insert(id, loaded.clone());
        state.registry = next;
        self.active_tx.send_replace(Some(account.clone()));
        self.start_sync(&mut state, loaded.client.clone()).await;
        Ok(account)
    }

    /// Begin Matrix SSO with an application-owned loopback redirect URL.
    /// This is the Matrix login-token flow, not MSC3861 OIDC.
    pub async fn begin_sso(
        &self,
        homeserver: &str,
        loopback_redirect_url: &str,
        identity_provider_id: Option<&str>,
    ) -> Result<SsoAttemptInfo> {
        let homeserver = validate_homeserver(homeserver)?;
        let mut redirect_url = validate_loopback_redirect(loopback_redirect_url)?;
        let attempt_id = Uuid::new_v4().to_string();
        redirect_url
            .query_pairs_mut()
            .append_pair("morpheus_state", &attempt_id);
        let client = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        let browser_url = client
            .matrix_auth()
            .get_sso_login_url(redirect_url.as_str(), identity_provider_id)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        self.state.lock().await.pending_sso.insert(
            attempt_id.clone(),
            PendingSso {
                homeserver,
                redirect_url: redirect_url.clone(),
            },
        );
        Ok(SsoAttemptInfo {
            attempt_id,
            browser_url,
            redirect_url: redirect_url.to_string(),
        })
    }

    /// Finish a browser SSO login using the full URL received by the local
    /// callback server. The callback origin, path, and random state are checked
    /// before exchanging the one-use login token for a new Matrix device.
    pub async fn finish_sso(&self, attempt_id: &str, callback_url: &str) -> Result<AccountInfo> {
        let mut state = self.state.lock().await;
        let pending = state
            .pending_sso
            .remove(attempt_id)
            .ok_or_else(|| MatrixCoreError::InvalidSession)?;
        let callback = Url::parse(callback_url)
            .map_err(|e| MatrixCoreError::InvalidSsoCallback(e.to_string()))?;
        let expected = &pending.redirect_url;
        if callback.scheme() != expected.scheme()
            || callback.host_str() != expected.host_str()
            || callback.port_or_known_default() != expected.port_or_known_default()
            || callback.path() != expected.path()
            || callback.fragment().is_some()
        {
            return Err(MatrixCoreError::InvalidSsoCallback(
                "unexpected redirect origin or path".into(),
            ));
        }
        let mut states = callback
            .query_pairs()
            .filter(|(key, _)| key == "morpheus_state");
        if states.next().is_none_or(|(_, value)| value != attempt_id) || states.next().is_some() {
            return Err(MatrixCoreError::InvalidSsoCallback(
                "missing or mismatched state".into(),
            ));
        }
        let mut tokens = callback
            .query_pairs()
            .filter(|(key, _)| key == "loginToken");
        let token = match tokens.next() {
            Some((_, token)) if !token.is_empty() && tokens.next().is_none() => token.into_owned(),
            _ => {
                return Err(MatrixCoreError::InvalidSsoCallback(
                    "missing or repeated login token".into(),
                ));
            }
        };
        let id = Uuid::new_v4().to_string();
        let store_path = self.store_path(&id);
        fs::create_dir_all(&store_path)?;
        let passphrase = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let client = match build_client(&pending.homeserver, &store_path, &passphrase).await {
            Ok(client) => client,
            Err(e) => {
                let _ = fs::remove_dir_all(&store_path);
                return Err(e);
            }
        };
        let response = match client
            .matrix_auth()
            .login_token(&token)
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
            homeserver: pending.homeserver,
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
        if let Err(e) = save_secret(
            &id,
            StoredSecret {
                version: SECRET_VERSION,
                store_passphrase: passphrase.clone(),
                session,
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
        stop_sync(&mut state).await;
        let loaded = Arc::new(LoadedAccount::new(client, id.clone(), passphrase));
        state.loaded.insert(id, loaded.clone());
        state.registry = next;
        self.active_tx.send_replace(Some(account.clone()));
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
                    || secret.session.meta.user_id.as_str() != account.user_id
                    || secret.session.meta.device_id.as_str() != account.device_id
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
                    .matrix_auth()
                    .restore_session(secret.session, RoomLoadSettings::default())
                    .await
                    .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
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
        stop_sync(&mut state).await;
        state.registry = next;
        self.active_tx.send_replace(Some(account.clone()));
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
                            if let Err(e) = client
                                .matrix_auth()
                                .restore_session(secret.session, RoomLoadSettings::default())
                                .await
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
            self.active_tx.send_replace(None);
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
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
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

    pub async fn send_thread_text_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        root_event_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
        timeline
            .send(RoomMessageEventContent::text_plain(body.into()).into())
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(())
    }

    pub async fn edit_text_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
        body: impl Into<String>,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
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
            .send(RoomMessageEventContent::text_plain(body.into()).into())
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
        let state = self.state.lock().await;
        let room = account_room(&state, account_id, room_id)?;
        room.load_composer_draft(None)
            .await
            .map(|draft| draft.map_or_else(String::new, |draft| draft.plain_text))
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
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
        let state = self.state.lock().await;
        let room = account_room(&state, account_id, room_id)?;
        if body.is_empty() {
            room.clear_composer_draft(None)
                .await
                .map_err(|e| MatrixCoreError::Sdk(e.to_string()))
        } else {
            room.save_composer_draft(
                ComposerDraft {
                    plain_text: body.to_owned(),
                    html_text: None,
                    draft_type: ComposerDraftType::NewMessage,
                    attachments: Vec::new(),
                },
                None,
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
        find_verification_request(client, user_id, flow_id)
            .await?
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
        let token_client = client.clone();
        let mut changes = client.subscribe_to_session_changes();
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let token_task = tokio::spawn(async move {
            loop {
                let change = tokio::select! {
                    biased;
                    _ = &mut shutdown_receiver => break,
                    change = changes.recv() => change,
                };
                match change {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(session) = token_client.matrix_auth().session() {
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
    Some(MessageInfo {
        key: item.unique_id().0.clone(),
        event_id: event.event_id().map(ToString::to_string),
        sender: event.sender().to_string(),
        body: message.body().to_owned(),
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
        .homeserver_url(homeserver)
        .sqlite_store(store_path, Some(passphrase))
        // Fetch only missing room keys after recovery, avoiding a full-account backup download.
        .with_encryption_settings(EncryptionSettings {
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            ..Default::default()
        })
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

            // Re-selecting an active account is a manual recovery path for
            // a queue paused after a transient error.
            client_b.send_queue().set_enabled(false).await;
            manager.switch_to(&second.id).await.unwrap();
            assert!(client_b.send_queue().is_enabled());

            manager.switch_to(&first.id).await.unwrap();
            assert!(client_a.send_queue().is_enabled());
            assert!(!client_b.send_queue().is_enabled());

            manager.stop().await;
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
