//! Encrypted-account recovery for one exact account selection: a truthful
//! overview of this device's identity, recovery and key backup, setting up
//! recovery (a new recovery key), and recovering this device with an
//! existing key or passphrase.
//!
//! Every call takes the selected client under the manager's state lock and
//! releases it before any SDK, store or network await (the legacy methods
//! held it for the whole operation).
//!
//! Reading the overview sends nothing that changes the account and is
//! bounded; a part that does not answer in time is reported as not known,
//! never as absent.
//!
//! Setup and recovery are critical: once the SDK starts them they are never
//! cancelled or timed out here, because an interrupted setup can leave the
//! server depending on a key this device never kept, and an interrupted
//! import leaves secrets half applied. Before they start, the selection, the
//! loaded client's own encryption setup and the caller's admission fence
//! (the app's security-gate generation) are checked; a failure there sends
//! nothing. Both use the SDK's own recovery code and cryptography unchanged,
//! and are never retried here: a failure after the start is reported as
//! unconfirmed and the overview is read again so the person sees what the
//! account has.
//!
//! Setup keeps every key the SDK generates. The pinned SDK hands the new key
//! over before it stores anything (`GeneratedKeyHandoff`, a reviewed vendor
//! seam), so a setup that fails after the server may already use the key
//! still returns it, marked as not confirmed.
//!
//! Recovery imports the stored secrets only (`Recovery::recover`): unlike
//! the legacy `recover_and_fix_backup`, it never deletes or replaces the
//! server-side key backup. When the backup's key in secret storage is
//! missing or does not match, the identity may still have been recovered;
//! that is reported as such, with nothing deleted.
//!
//! Secrets arrive as `Zeroizing` and the recovery key leaves only inside a
//! `RecoveryKey`; neither is logged or put into an error.

use std::{
    future::{Future, IntoFuture},
    time::Duration,
};

use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    Client, HttpError,
    config::RequestConfig,
    encryption::{
        VerificationState,
        backups::BackupState,
        recovery::{EnableProgress, RecoveryError, RecoveryState},
        secret_storage::{GeneratedKeyHandoff, ImportError, SecretStorageError},
    },
    ruma::{
        api::{client::backup::get_latest_backup_info, error::ErrorKind},
        events::secret::request::SecretName,
    },
};
use tokio::{sync::watch, time};
use zeroize::Zeroizing;

use crate::{
    AccountManager, AccountSelection, BackupStatus, DeviceVerificationStatus, RecoveryKey,
    RecoveryStatus, account_access, selected_loaded_for,
};

// Taking the client: the manager's state lock and nothing else.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
// One non-critical read of the overview (local crypto state or one request).
const READ_TIMEOUT: Duration = Duration::from_secs(15);
// The latest key backup's info, for the overview only: a version and its
// auth data (a public key and signatures), far below this.
const MAX_BACKUP_INFO_BYTES: u64 = 64 * 1024;
// Waiting for the SDK's encryption setup after sign-in, before setup starts.
const E2EE_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Why an operation did not complete. The text is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccountRecoveryError {
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Finish the account or security step in progress, then try again.")]
    SecurityBusy,
    #[error("Encryption is not ready on this device yet. Nothing was changed; try again shortly.")]
    NotReady,
    #[error(
        "This account already has a key backup on the server. Recover with your existing key instead; nothing was changed."
    )]
    BackupExistsOnServer,
    #[error(
        "That recovery key or passphrase could not be used with this account's recovery data. Check it and try again; nothing was imported."
    )]
    KeyNotAccepted,
    #[error(
        "This account's recovery data on the server is incomplete or in a form Morpheus cannot use. Nothing was imported."
    )]
    RecoveryDataUnusable,
    #[error(
        "This account's recovery data could not be read from the server. Nothing was imported; try again."
    )]
    RecoveryDataUnreadable,
    #[error("This account has no recovery set up on the server.")]
    NotSetUp,
    #[error(
        "This device does not hold the account's cross-signing keys yet. Verify or recover this device first; nothing was changed."
    )]
    IdentityNotReady,
    #[error(
        "Your identity may have been recovered, but the key backup could not be restored with this key. Nothing was deleted."
    )]
    BackupNotRestored,
    #[error(
        "Your identity may have been recovered on this device, but turning on the key backup did not finish. Check the status below before trying again; nothing was deleted."
    )]
    BackupNotEnabled,
    #[error(
        "The operation did not finish and may have partly applied. Check the status below before trying again."
    )]
    Unconfirmed,
    #[error("The security status could not be read. Try Refresh.")]
    Unavailable,
}

pub type AccountRecoveryResult<T> = std::result::Result<T, AccountRecoveryError>;

/// The cross-signing private keys this device holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrossSigningKeys {
    pub master: bool,
    pub self_signing: bool,
    pub user_signing: bool,
}

impl CrossSigningKeys {
    pub fn complete(self) -> bool {
        self.master && self.self_signing && self.user_signing
    }

    pub fn count(self) -> u8 {
        u8::from(self.master) + u8::from(self.self_signing) + u8::from(self.user_signing)
    }
}

/// This device's encryption state as the SDK reports it now. `None` means
/// not known (no answer in time, or the SDK has no state yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryOverview {
    pub device: DeviceVerificationStatus,
    pub recovery: RecoveryStatus,
    pub backup: BackupStatus,
    /// Whether a key backup exists on the homeserver.
    pub backup_on_server: Option<bool>,
    pub cross_signing: Option<CrossSigningKeys>,
}

/// How far setup has come, from the SDK's own progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryProgress {
    Starting,
    CreatingBackup,
    CreatingRecoveryKey,
    BackingUp {
        backed_up: usize,
        total: usize,
    },
    /// Some room keys could not be uploaded yet; the SDK retries later.
    UploadIncomplete,
    Finishing,
}

/// How far a setup that produced a key is known to have got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupConfirmation {
    /// The SDK finished setup without an error.
    Confirmed,
    /// The SDK reported setup done with this key, then its own final step
    /// (updating the recovery state) failed.
    FinalStepFailed,
    /// Setup failed while storing this key's secrets or making it the
    /// default key. The server may already depend on this key, or not.
    StorageUnknown,
}

/// A setup that produced a key: the key to show once, and the overview
/// after it. Unless confirmed, the key must still be saved: the server may
/// already depend on it.
pub struct EnabledRecovery {
    pub key: RecoveryKey,
    pub overview: Option<RecoveryOverview>,
    pub confirmation: SetupConfirmation,
}

/// The SDK's encryption setup after sign-in, for one loaded client. The SDK
/// hands that setup's task to the first caller of
/// `wait_for_e2ee_initialization_tasks` and forgets it, so a later call
/// returns at once and proves nothing. This owner is started when the
/// account's client is loaded, before anything else can take the task, and
/// runs that one wait detached to its end; every reader of the client's
/// readiness waits on this outcome with its own bound and never takes a
/// timeout or a cancel as done. It belongs to its `LoadedAccount`: a client
/// restored later for the same account and device has its own.
pub(crate) struct E2eeReadiness(watch::Receiver<bool>);

impl E2eeReadiness {
    /// Starts the one wait for `client`'s encryption setup.
    pub(crate) fn of(client: &Client) -> Self {
        let client = client.clone();
        Self::start(async move {
            let encryption = client.encryption();
            encryption.wait_for_e2ee_initialization_tasks().await;
        })
    }

    /// The outcome of `setup`, which runs detached to its end; dropping
    /// this owner or a waiter does not stop it.
    fn start(setup: impl Future<Output = ()> + Send + 'static) -> Self {
        let (done, ready) = watch::channel(false);
        tokio::spawn(async move {
            setup.await;
            let _ = done.send(true);
        });
        Self(ready)
    }

    fn waiter(&self) -> watch::Receiver<bool> {
        self.0.clone()
    }

    /// Whether the setup finished. Dropping this future only stops this
    /// wait.
    pub(crate) async fn settled(&self) -> bool {
        until_ready(self.waiter()).await
    }
}

/// Waits until `ready` reports true. Dropping this future only stops this
/// wait; the setup itself and its detached wait go on.
async fn until_ready(mut ready: watch::Receiver<bool>) -> bool {
    ready.wait_for(|done| *done).await.is_ok()
}

/// The key carried by the SDK's final progress update, kept as soon as it
/// is seen so that a failure of the SDK's last step cannot lose it.
fn done_key(update: &EnableProgress) -> Option<Zeroizing<String>> {
    match update {
        EnableProgress::Done { recovery_key } => Some(Zeroizing::new(recovery_key.clone())),
        _ => None,
    }
}

fn device_status(state: VerificationState) -> DeviceVerificationStatus {
    match state {
        VerificationState::Unknown => DeviceVerificationStatus::Unknown,
        VerificationState::Verified => DeviceVerificationStatus::Verified,
        VerificationState::Unverified => DeviceVerificationStatus::Unverified,
    }
}

fn recovery_status(state: RecoveryState) -> RecoveryStatus {
    match state {
        RecoveryState::Unknown => RecoveryStatus::Unknown,
        RecoveryState::Enabled => RecoveryStatus::Enabled,
        RecoveryState::Disabled => RecoveryStatus::Disabled,
        RecoveryState::Incomplete => RecoveryStatus::Incomplete,
    }
}

fn backup_status(state: BackupState) -> BackupStatus {
    match state {
        BackupState::Unknown => BackupStatus::Unknown,
        BackupState::Creating => BackupStatus::Creating,
        BackupState::Enabling => BackupStatus::Enabling,
        BackupState::Resuming => BackupStatus::Resuming,
        BackupState::Enabled => BackupStatus::Enabled,
        BackupState::Downloading => BackupStatus::Downloading,
        BackupState::Disabling => BackupStatus::Disabling,
    }
}

/// The SDK's setup progress without the key it carries at the end.
fn progress(update: &EnableProgress) -> RecoveryProgress {
    match update {
        EnableProgress::Starting => RecoveryProgress::Starting,
        EnableProgress::CreatingBackup => RecoveryProgress::CreatingBackup,
        EnableProgress::CreatingRecoveryKey => RecoveryProgress::CreatingRecoveryKey,
        EnableProgress::BackingUp(counts) => RecoveryProgress::BackingUp {
            backed_up: counts.backed_up,
            total: counts.total,
        },
        EnableProgress::RoomKeyUploadError => RecoveryProgress::UploadIncomplete,
        EnableProgress::Done { .. } => RecoveryProgress::Finishing,
    }
}

fn setup_error(error: &RecoveryError) -> AccountRecoveryError {
    match error {
        // Checked before anything is created.
        RecoveryError::BackupExistsOnServer => AccountRecoveryError::BackupExistsOnServer,
        _ => AccountRecoveryError::Unconfirmed,
    }
}

/// The key a setup produced, and how far the setup is known to have got.
/// Every key the SDK generated is kept: the one it returned, else the one
/// its Done update reported, else the one it handed over before storing
/// anything. Without any, no key was generated and the error stands.
fn setup_key(
    result: Result<String, RecoveryError>,
    done: Option<Zeroizing<String>>,
    handed_over: Option<Zeroizing<String>>,
) -> AccountRecoveryResult<(Zeroizing<String>, SetupConfirmation)> {
    match (result, done, handed_over) {
        (Ok(key), _, _) => Ok((Zeroizing::new(key), SetupConfirmation::Confirmed)),
        (Err(_), Some(key), _) => Ok((key, SetupConfirmation::FinalStepFailed)),
        (Err(_), None, Some(key)) => Ok((key, SetupConfirmation::StorageUnknown)),
        (Err(error), None, None) => Err(setup_error(&error)),
    }
}

/// A recovery failure in fixed words. Only the absence of any default
/// secret-storage key is "not set up". Failing to derive or check the typed
/// key is "could not be used": the SDK's key-decode error also covers
/// unsupported algorithms and malformed key metadata, and its variants are
/// not reachable from this crate, so a wrong key is never asserted. Missing
/// referenced key metadata and undecodable key metadata come before any
/// import and are unusable recovery data; a secret that could not be
/// imported is described by when it can fail (`import_error`). "Nothing was
/// imported" is said only where nothing can have been. No secret name, key
/// ID, algorithm or server text is reflected.
fn recover_error(error: &RecoveryError) -> AccountRecoveryError {
    match error {
        RecoveryError::SecretStorage(SecretStorageError::SecretStorageKey(_)) => {
            AccountRecoveryError::KeyNotAccepted
        }
        RecoveryError::SecretStorage(SecretStorageError::MissingKeyInfo { key_id: None }) => {
            AccountRecoveryError::NotSetUp
        }
        RecoveryError::SecretStorage(
            SecretStorageError::MissingKeyInfo { key_id: Some(_) }
            | SecretStorageError::Decryption(_)
            | SecretStorageError::Json(_),
        ) => AccountRecoveryError::RecoveryDataUnusable,
        RecoveryError::SecretStorage(SecretStorageError::ImportError { name, error }) => {
            import_error(name, error)
        }
        RecoveryError::SecretStorage(
            SecretStorageError::InconsistentBackupDecryptionKey
            | SecretStorageError::MissingOrInvalidBackupDecryptionKey,
        ) => AccountRecoveryError::BackupNotRestored,
        _ => AccountRecoveryError::Unconfirmed,
    }
}

/// A secret that could not be imported, by when the pinned SDK's
/// `SecretStore::import_secrets` can report it. The cross-signing keys are
/// all fetched, decrypted and checked against the public identity before
/// any of them is stored, so an error naming one comes before any import:
/// a failed fetch is unreadable data, anything else unusable data. The key
/// backup's key is imported last, after the identity and possibly a
/// signature on this device, and its errors, like any other secret's, may
/// follow those changes.
fn import_error(name: &SecretName, error: &ImportError) -> AccountRecoveryError {
    let cross_signing = matches!(
        name,
        SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningSelfSigningKey
            | SecretName::CrossSigningUserSigningKey
    );
    match error {
        _ if !cross_signing => AccountRecoveryError::BackupNotEnabled,
        ImportError::Sdk(_) => AccountRecoveryError::RecoveryDataUnreadable,
        ImportError::Json(_)
        | ImportError::Key(_)
        | ImportError::MismatchedPublicKeys
        | ImportError::Decryption(_) => AccountRecoveryError::RecoveryDataUnusable,
    }
}

/// Whether a key backup exists on the homeserver, for the overview only:
/// one bounded read of the latest backup's info (capped response, no HTTP
/// retry, no token-refresh replay), never the SDK's unbounded backup
/// checks. `None` (not known) on any failure other than 404 M_NOT_FOUND.
/// The client's own server-version discovery, if it runs first, is outside
/// this cap.
async fn backup_exists_on_server(client: &Client) -> Option<bool> {
    let config = RequestConfig::new()
        .retry_limit(0)
        .timeout(READ_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_BACKUP_INFO_BYTES)
        .disable_auth_refresh();
    let request = get_latest_backup_info::v3::Request::new();
    match client.send(request).with_request_config(config).await {
        Ok(_) => Some(true),
        Err(error) if no_backup(&error) => Some(false),
        Err(_) => None,
    }
}

fn no_backup(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|answer| {
        answer.status_code.as_u16() == 404 && answer.error_kind() == Some(&ErrorKind::NotFound)
    })
}

/// `None` when `work` does not finish within `limit`.
async fn bounded<T>(limit: Duration, work: impl std::future::Future<Output = T>) -> Option<T> {
    time::timeout(limit, work).await.ok()
}

/// This device's state now. Each part is bounded on its own; one that does
/// not answer is not known.
async fn overview(client: &Client) -> RecoveryOverview {
    let encryption = client.encryption();
    let cross_signing = bounded(READ_TIMEOUT, encryption.cross_signing_status()).await;
    let cross_signing = cross_signing.flatten().map(|status| CrossSigningKeys {
        master: status.has_master,
        self_signing: status.has_self_signing,
        user_signing: status.has_user_signing,
    });
    let backups = encryption.backups();
    let on_server = bounded(READ_TIMEOUT, backup_exists_on_server(client)).await;
    RecoveryOverview {
        device: device_status(encryption.verification_state().get()),
        recovery: recovery_status(encryption.recovery().state()),
        backup: backup_status(backups.state()),
        backup_on_server: on_server.flatten(),
        cross_signing,
    }
}

impl AccountManager {
    /// The selected account's client while `selection` still owns it, with
    /// that loaded client's own encryption-setup readiness. The manager's
    /// state lock is released before this returns.
    async fn recovery_client(
        &self,
        selection: &AccountSelection,
    ) -> AccountRecoveryResult<(Client, watch::Receiver<bool>)> {
        if !selection.is_current() {
            return Err(AccountRecoveryError::AccountChanged);
        }
        let taken = selection.guard(bounded(CLIENT_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self.account_selection(selection.account_id());
            let current = current.ok_or(AccountRecoveryError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(AccountRecoveryError::AccountChanged);
            }
            account_access::ensure_eligible(&state, selection.account_id())
                .map_err(|_| AccountRecoveryError::AccountChanged)?;
            let loaded = selected_loaded_for(&state, selection.account_id())
                .map_err(|_| AccountRecoveryError::AccountChanged)?;
            Ok((loaded.client.clone(), loaded.e2ee.waiter()))
        }));
        let taken = match taken.await {
            None => return Err(AccountRecoveryError::AccountChanged),
            Some(None) => return Err(AccountRecoveryError::Unavailable),
            Some(Some(taken)) => taken?,
        };
        if !selection.is_current() {
            return Err(AccountRecoveryError::AccountChanged);
        }
        Ok(taken)
    }

    /// The selected account's identity, recovery and key backup state, read
    /// again from the SDK. Bounded; sends nothing that changes the account.
    pub async fn recovery_overview_selected(
        &self,
        selection: &AccountSelection,
    ) -> AccountRecoveryResult<RecoveryOverview> {
        let (client, _) = self.recovery_client(selection).await?;
        let read = selection.guard(overview(&client)).await;
        let read = read.ok_or(AccountRecoveryError::AccountChanged)?;
        if !selection.is_current() {
            return Err(AccountRecoveryError::AccountChanged);
        }
        Ok(read)
    }

    /// Set up recovery: a key backup if none exists, and secret storage with
    /// a new recovery key, returned once. `progress` receives the SDK's
    /// progress (never the key). Checked before the start; once started it
    /// runs to its end and is never retried here.
    pub async fn enable_recovery_selected(
        &self,
        selection: &AccountSelection,
        admitted: &(dyn Fn() -> bool + Send + Sync),
        progress_to: &(dyn Fn(RecoveryProgress) + Send + Sync),
    ) -> AccountRecoveryResult<EnabledRecovery> {
        let (client, ready) = self.recovery_client(selection).await?;
        // Cross-signing is bootstrapped by the SDK after sign-in; export its
        // secrets only after that settled for this exact loaded client (its
        // own readiness owner, never an earlier client's, and never the
        // keys a store already holds). Nothing is sent before this ends, so
        // bounding the wait cannot lose a key; the setup's own wait runs on
        // detached, so a timeout here never counts as done.
        let ready = until_ready(ready);
        let ready = selection.guard(bounded(E2EE_READY_TIMEOUT, ready)).await;
        match ready {
            None => return Err(AccountRecoveryError::AccountChanged),
            Some(None | Some(false)) => return Err(AccountRecoveryError::NotReady),
            Some(Some(true)) => {}
        }
        // Once settled, a side-effect-free local read checks the
        // precondition that this device holds the cross-signing keys setup
        // will store. It is a precondition, not proof of readiness.
        let encryption = client.encryption();
        let identity = bounded(READ_TIMEOUT, encryption.cross_signing_status()).await;
        let identity = identity.flatten();
        if !identity.is_some_and(|status| status.has_master && status.has_self_signing) {
            return Err(AccountRecoveryError::IdentityNotReady);
        }
        if !selection.is_current() {
            return Err(AccountRecoveryError::AccountChanged);
        }
        if !admitted() {
            return Err(AccountRecoveryError::SecurityBusy);
        }
        // Critical from here: not cancelled, not timed out. The SDK hands
        // the new key over before it stores anything.
        let encryption = client.encryption();
        let recovery = encryption.recovery();
        let handoff = GeneratedKeyHandoff::new();
        let enable = recovery.enable().hand_key_to(handoff.clone());
        let updates = enable.subscribe_to_progress();
        let mut updates = std::pin::pin!(updates);
        let mut setup = enable.into_future();
        progress_to(RecoveryProgress::Starting);
        // The SDK reports the key in its Done update before its last step;
        // it is kept from that moment, whatever follows.
        let mut seen_key: Option<Zeroizing<String>> = None;
        let mut take = |update: EnableProgress| {
            if let Some(key) = done_key(&update) {
                seen_key = Some(key);
            }
            progress_to(progress(&update));
        };
        let result = loop {
            tokio::select! {
                biased;
                result = &mut setup => break result,
                Some(update) = updates.next() => {
                    if let Ok(update) = update {
                        take(update);
                    }
                }
            }
        };
        // Updates already sent but not yet read, the Done one included.
        while let Some(Some(update)) = updates.next().now_or_never() {
            if let Ok(update) = update {
                take(update);
            }
        }
        // Never drop a generated key, whatever failed after it existed.
        let (key, confirmation) = setup_key(result, seen_key, handoff.take())?;
        let key = RecoveryKey { secret: key };
        // The key is kept whatever the overview read does.
        let overview = bounded(READ_TIMEOUT * 2, overview(&client)).await;
        Ok(EnabledRecovery {
            key,
            overview,
            confirmation,
        })
    }

    /// Recover this device with an existing recovery key or passphrase:
    /// import the cross-signing keys and the key backup's key from secret
    /// storage. Never deletes or replaces the server's key backup. Checked
    /// before the start; once started it runs to its end. The overview read
    /// afterwards comes with every outcome, for a truthful result.
    pub async fn recover_selected(
        &self,
        selection: &AccountSelection,
        admitted: &(dyn Fn() -> bool + Send + Sync),
        secret: Zeroizing<String>,
    ) -> (AccountRecoveryResult<()>, Option<RecoveryOverview>) {
        let client = match self.recovery_client(selection).await {
            Ok((client, _)) => client,
            Err(error) => return (Err(error), None),
        };
        if !admitted() {
            return (Err(AccountRecoveryError::SecurityBusy), None);
        }
        // Critical from here: not cancelled, not timed out.
        let encryption = client.encryption();
        let recovery = encryption.recovery();
        let result = recovery.recover(secret.as_str()).await;
        drop(secret);
        let result = result.map_err(|error| recover_error(&error));
        let overview = bounded(READ_TIMEOUT * 2, overview(&client)).await;
        (result, overview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_progress_never_carries_the_key() {
        let done = EnableProgress::Done {
            recovery_key: "synthetic key".into(),
        };
        assert_eq!(progress(&done), RecoveryProgress::Finishing);
        assert!(!format!("{:?}", progress(&done)).contains("synthetic"));
        assert_eq!(
            progress(&EnableProgress::RoomKeyUploadError),
            RecoveryProgress::UploadIncomplete
        );
        assert_eq!(
            progress(&EnableProgress::Starting),
            RecoveryProgress::Starting
        );
    }

    /// The Done update's key is kept the moment it is seen, so the SDK
    /// failing its last step afterwards cannot lose it; other updates carry
    /// none.
    #[test]
    fn the_done_key_is_kept_as_soon_as_it_is_reported() {
        let done = EnableProgress::Done {
            recovery_key: "synthetic key".into(),
        };
        assert_eq!(
            done_key(&done).as_deref().map(String::as_str),
            Some("synthetic key")
        );
        for other in [
            EnableProgress::Starting,
            EnableProgress::CreatingBackup,
            EnableProgress::CreatingRecoveryKey,
            EnableProgress::RoomKeyUploadError,
        ] {
            assert!(done_key(&other).is_none());
        }
    }

    /// The readiness wait never counts a timeout as done: its outcome only
    /// turns true when the detached setup wait finished, and dropping a
    /// waiter leaves the shared outcome waiting for the next one.
    #[test]
    fn a_timed_out_readiness_wait_is_not_taken_as_done() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (done, ready) = watch::channel(false);
            let first = time::timeout(Duration::from_millis(20), until_ready(ready.clone())).await;
            assert!(first.is_err(), "timed out, not done");
            assert!(!*ready.borrow());
            let again = tokio::spawn(until_ready(ready.clone()));
            done.send(true).unwrap();
            assert!(again.await.unwrap());
            // The setup's wait ended without a result: not done.
            let (gone, ready) = watch::channel(false);
            drop(gone);
            assert!(!until_ready(ready).await);
            // The production owner is done only once its setup finished.
            let (finish, setup) = tokio::sync::oneshot::channel::<()>();
            let owner = E2eeReadiness::start(async move {
                let _ = setup.await;
            });
            let early = time::timeout(Duration::from_millis(20), owner.settled()).await;
            assert!(early.is_err(), "timed out, not done");
            finish.send(()).unwrap();
            assert!(owner.settled().await);
        });
    }

    /// A manager whose one account is selected and loaded with a fresh
    /// client without a session; `readiness` stands for that loaded
    /// client's encryption setup.
    async fn loaded_manager(
        root: &std::path::Path,
        id: &str,
        readiness: E2eeReadiness,
    ) -> AccountManager {
        let manager = AccountManager::open(root).unwrap();
        let homeserver = "http://127.0.0.1:9";
        let client = Client::builder()
            .homeserver_url(homeserver)
            .build()
            .await
            .unwrap();
        let lock = manager._access_lock.clone();
        let mut loaded = crate::LoadedAccount::new(client, id.to_owned(), String::new(), lock);
        loaded.e2ee = readiness;
        {
            let mut state = manager.state.lock().await;
            state.registry.accounts = vec![crate::AccountInfo {
                id: id.to_owned(),
                user_id: "@casey:localhost".into(),
                device_id: "RECOVERY".into(),
                homeserver: homeserver.into(),
            }];
            state.active_account_id = Some(id.to_owned());
            let loaded = std::sync::Arc::new(loaded);
            state.loaded.insert(id.to_owned(), loaded);
        }
        manager.selections.replace(Some(id));
        manager
    }

    /// Readiness belongs to the exact loaded client. After a client of the
    /// same account and device finished its setup, a replacement manager's
    /// client is still setting up: setup waits for that client's own setup
    /// and goes no further before it settles, never taking the earlier
    /// client's outcome or the keys a store holds as proof.
    #[test]
    fn a_replaced_client_waits_for_its_own_encryption_setup() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let base = format!("morpheus-account-readiness-{}", uuid::Uuid::new_v4());
            let roots = [0, 1].map(|n| std::env::temp_dir().join(format!("{base}-{n}")));
            let id = "account";
            let first = loaded_manager(&roots[0], id, E2eeReadiness::start(async {})).await;
            {
                let state = first.state.lock().await;
                assert!(state.loaded[id].e2ee.settled().await);
            }
            first.selections.replace(None);
            drop(first);
            let (finish, setup) = tokio::sync::oneshot::channel::<()>();
            let held = E2eeReadiness::start(async move {
                let _ = setup.await;
            });
            let second = loaded_manager(&roots[1], id, held).await;
            let selection = second.account_selection(id).unwrap();
            let open = || true;
            let report = |_: RecoveryProgress| {};
            let enable = second.enable_recovery_selected(&selection, &open, &report);
            let mut enable = Box::pin(enable);
            for _ in 0..8 {
                assert!(futures_util::poll!(enable.as_mut()).is_pending());
                tokio::task::yield_now().await;
            }
            // Once this client's own setup settled, setup goes on to its
            // checks: this client holds no identity, so it stops there,
            // before anything is enabled.
            finish.send(()).unwrap();
            let result = enable.await;
            assert!(matches!(
                result,
                Err(AccountRecoveryError::IdentityNotReady)
            ));
            second.selections.replace(None);
            drop(second);
            for root in roots {
                let _ = std::fs::remove_dir_all(root);
            }
        });
    }

    /// A setup that fails before it reports Done still returns the key the
    /// SDK handed over before storing anything: never dropped, and never
    /// called confirmed. With no key at all, the error stands.
    #[test]
    fn every_generated_key_is_kept_with_how_far_setup_got() {
        use SetupConfirmation::{Confirmed, FinalStepFailed, StorageUnknown};
        let key = |text: &str| Some(Zeroizing::new(text.to_owned()));
        let failed = || Err(RecoveryError::Sdk(matrix_sdk::Error::InsufficientData));
        let kept = |result: AccountRecoveryResult<(Zeroizing<String>, SetupConfirmation)>| {
            let (key, confirmation) = result.unwrap();
            (key.as_str().to_owned(), confirmation)
        };
        let handed = kept(setup_key(failed(), None, key("handed")));
        assert_eq!(handed, ("handed".to_owned(), StorageUnknown));
        let done = kept(setup_key(failed(), key("done"), key("done")));
        assert_eq!(done, ("done".to_owned(), FinalStepFailed));
        let returned = kept(setup_key(Ok("returned".into()), None, key("returned")));
        assert_eq!(returned, ("returned".to_owned(), Confirmed));
        let refused = setup_key(Err(RecoveryError::BackupExistsOnServer), None, None);
        let refused = refused.err();
        assert_eq!(refused, Some(AccountRecoveryError::BackupExistsOnServer));
        let unknown = setup_key(failed(), None, None);
        assert_eq!(unknown.err(), Some(AccountRecoveryError::Unconfirmed));
    }

    /// The key backup's key is imported after the identity and possibly a
    /// signature on this device, so its failure, an HTTP or JSON error
    /// while turning the backup on, never says nothing was imported. Only
    /// failures naming a cross-signing key, which come before any import,
    /// say so. No text names a secret.
    #[test]
    fn a_late_backup_import_failure_never_claims_nothing_was_imported() {
        let failed = |name: SecretName, error: ImportError| {
            let error = SecretStorageError::ImportError { name, error };
            recover_error(&RecoveryError::SecretStorage(error))
        };
        let http = || {
            let error = HttpError::ResponseTooLarge { limit: 64 };
            ImportError::Sdk(matrix_sdk::Error::Http(Box::new(error)))
        };
        let json = || {
            let error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
            ImportError::Json(error)
        };
        for error in [http(), json(), ImportError::MismatchedPublicKeys] {
            let mapped = failed(SecretName::RecoveryKey, error);
            assert_eq!(mapped, AccountRecoveryError::BackupNotEnabled);
            let text = mapped.to_string().to_lowercase();
            assert!(!text.contains("nothing was imported"), "{text}");
            assert!(text.contains("may have been recovered"), "{text}");
            assert!(text.contains("nothing was deleted"), "{text}");
        }
        let custom = failed(SecretName::from("org.example.secret"), json());
        assert_eq!(custom, AccountRecoveryError::BackupNotEnabled);
        // Cross-signing keys fail before anything is imported.
        let unread = failed(SecretName::CrossSigningMasterKey, http());
        assert_eq!(unread, AccountRecoveryError::RecoveryDataUnreadable);
        let mismatched = ImportError::MismatchedPublicKeys;
        let unusable = failed(SecretName::CrossSigningSelfSigningKey, mismatched);
        assert_eq!(unusable, AccountRecoveryError::RecoveryDataUnusable);
        let undecodable = failed(SecretName::CrossSigningUserSigningKey, json());
        assert_eq!(undecodable, AccountRecoveryError::RecoveryDataUnusable);
        for error in [unread, unusable, AccountRecoveryError::BackupNotEnabled] {
            let text = error.to_string();
            assert!(!text.contains("m."), "{text}");
            assert!(!text.contains("cross_signing"), "{text}");
        }
    }

    #[test]
    fn errors_are_fixed_and_only_a_refusal_before_the_start_is_definite() {
        assert_eq!(
            setup_error(&RecoveryError::BackupExistsOnServer),
            AccountRecoveryError::BackupExistsOnServer
        );
        let sdk = RecoveryError::Sdk(matrix_sdk::Error::InsufficientData);
        assert_eq!(setup_error(&sdk), AccountRecoveryError::Unconfirmed);
        assert_eq!(recover_error(&sdk), AccountRecoveryError::Unconfirmed);
        // No default key at all: not set up. A referenced key whose metadata
        // is missing, undecodable account data, and secrets that do not
        // decrypt with an accepted key: unusable data, never "not set up"
        // or "wrong key".
        let missing =
            RecoveryError::SecretStorage(SecretStorageError::MissingKeyInfo { key_id: None });
        assert_eq!(recover_error(&missing), AccountRecoveryError::NotSetUp);
        let referenced = RecoveryError::SecretStorage(SecretStorageError::MissingKeyInfo {
            key_id: Some("synthetic-key-id".into()),
        });
        assert_eq!(
            recover_error(&referenced),
            AccountRecoveryError::RecoveryDataUnusable
        );
        let json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let json = RecoveryError::SecretStorage(SecretStorageError::Json(json));
        assert_eq!(
            recover_error(&json),
            AccountRecoveryError::RecoveryDataUnusable
        );
        for error in [
            AccountRecoveryError::KeyNotAccepted,
            AccountRecoveryError::RecoveryDataUnusable,
            AccountRecoveryError::NotSetUp,
        ] {
            let text = error.to_string();
            assert!(!text.contains("synthetic-key-id") && !text.contains("does not match"));
        }
        for backup in [
            SecretStorageError::InconsistentBackupDecryptionKey,
            SecretStorageError::MissingOrInvalidBackupDecryptionKey,
        ] {
            let error = RecoveryError::SecretStorage(backup);
            assert_eq!(
                recover_error(&error),
                AccountRecoveryError::BackupNotRestored
            );
        }
        // No error text names a key, a passphrase value or a server body.
        for error in [
            AccountRecoveryError::KeyNotAccepted,
            AccountRecoveryError::BackupNotRestored,
            AccountRecoveryError::Unconfirmed,
        ] {
            assert!(!error.to_string().contains('{'));
        }
    }

    #[test]
    fn cross_signing_counts_are_exact() {
        let none = CrossSigningKeys {
            master: false,
            self_signing: false,
            user_signing: false,
        };
        assert_eq!((none.count(), none.complete()), (0, false));
        let all = CrossSigningKeys {
            master: true,
            self_signing: true,
            user_signing: true,
        };
        assert_eq!((all.count(), all.complete()), (3, true));
        let partial = CrossSigningKeys {
            user_signing: false,
            ..all
        };
        assert_eq!((partial.count(), partial.complete()), (2, false));
    }

    /// Nothing starts for a superseded selection or a closed admission
    /// fence; a recover refused there returns no overview and touches no
    /// secret store. No homeserver is contacted.
    #[test]
    fn critical_operations_are_fenced_before_they_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let name = format!("morpheus-account-recovery-{}", uuid::Uuid::new_v4());
            let root = std::env::temp_dir().join(name);
            let manager = AccountManager::open(&root).unwrap();
            let id = "account".to_owned();
            manager.selections.replace(Some(&id));
            let stale = manager.account_selection(&id).unwrap();
            manager.selections.replace(None);
            manager.selections.replace(Some(&id));
            let open = || true;
            let report = |_: RecoveryProgress| {};
            let secret = Zeroizing::new("synthetic secret".to_owned());
            let (recovered, overview) = manager.recover_selected(&stale, &open, secret).await;
            assert_eq!(recovered, Err(AccountRecoveryError::AccountChanged));
            assert!(overview.is_none());
            let enabled = manager
                .enable_recovery_selected(&stale, &open, &report)
                .await;
            assert!(matches!(enabled, Err(AccountRecoveryError::AccountChanged)));
            let read = manager.recovery_overview_selected(&stale).await;
            assert_eq!(read, Err(AccountRecoveryError::AccountChanged));
            // The current selection with no loaded account: refused before
            // any client is used.
            let current = manager.account_selection(&id).unwrap();
            let secret = Zeroizing::new("synthetic secret".to_owned());
            let (recovered, _) = manager.recover_selected(&current, &open, secret).await;
            assert_eq!(recovered, Err(AccountRecoveryError::AccountChanged));
            manager.selections.replace(None);
            drop(manager);
            let _ = std::fs::remove_dir_all(&root);
        });
    }
}
