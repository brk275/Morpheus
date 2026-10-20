//! Optional local switch protection. This is an application access boundary,
//! not protection against another process already running as the OS user.

use super::*;
use argon2::{Algorithm, Argon2, Block, Params, Version};
use std::sync::atomic::{AtomicBool, Ordering};
use subtle::ConstantTimeEq;
use tokio::sync::OwnedSemaphorePermit;
use zeroize::Zeroize;

const ACCESS_SERVICE: &str = "Morpheus App Password";
const ACCESS_FILE: &str = "account-access.json";
const GRANT_LIFETIME: Duration = Duration::from_secs(120);
const VAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) struct AccessPermit {
    _worker: OwnedSemaphorePermit,
    _profile_lock: Arc<fs::File>,
}

const KDF_MEMORY_KIB: u32 = 65_536;
const KDF_PASSES: u32 = 3;
const KDF_LANES: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPasswordStatus {
    pub enabled: bool,
    pub pending_removals: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalResult {
    pub account_id: String,
    pub credentials_removed: bool,
    pub store_removed: bool,
    pub complete: bool,
}

#[derive(Clone)]
pub struct ActivationCancellation(Arc<AtomicBool>);
impl ActivationCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A single request for one account in one manager/selection/policy epoch.
/// Fields are private and the capability cannot be cloned or deserialized.
pub struct ActivationChallenge {
    account_id: String,
    instance: Uuid,
    epoch: u64,
    policy_generation: u64,
    expires: Instant,
    cancelled: ActivationCancellation,
}
impl ActivationChallenge {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
    pub fn cancellation(&self) -> ActivationCancellation {
        self.cancelled.clone()
    }
}

/// Successful verification permits exactly one activation of the requested
/// account. Consuming it cannot authorize another target or policy epoch.
pub struct ActivationGrant(ActivationChallenge);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccessFence {
    version: u8,
    profile: String,
    pub(super) enabled: bool,
    generation: u64,
}
impl AccessFence {
    pub(super) fn open(data_dir: &Path) -> Result<Self> {
        Self::read_or_create(data_dir, true)
    }
    pub(super) fn read(data_dir: &Path) -> Result<Self> {
        Self::read_or_create(data_dir, false)
    }
    fn read_or_create(data_dir: &Path, create: bool) -> Result<Self> {
        match fs::read(data_dir.join(ACCESS_FILE)) {
            Ok(bytes) => {
                if bytes.len() > 4096 {
                    return Err(MatrixCoreError::AppPasswordUnavailable);
                }
                let fence: Self = serde_json::from_slice(&bytes)
                    .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
                if fence.version != 1 || Uuid::parse_str(&fence.profile).is_err() {
                    return Err(MatrixCoreError::AppPasswordUnavailable);
                }
                Ok(fence)
            }
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                // Losing the policy file must not turn an existing profile off.
                // This also refuses implicit migration of pre-policy profiles.
                let registry_absent = matches!(
                    fs::symlink_metadata(data_dir.join("accounts.json")),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                );
                let accounts_empty = fs::read_dir(data_dir.join("accounts"))
                    .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?
                    .next()
                    .is_none();
                if !registry_absent || !accounts_empty {
                    return Err(MatrixCoreError::AppPasswordUnavailable);
                }
                let fence = Self {
                    version: 1,
                    profile: Uuid::new_v4().to_string(),
                    enabled: false,
                    generation: 0,
                };
                fence.save(data_dir)?;
                Ok(fence)
            }
            Err(_) => Err(MatrixCoreError::AppPasswordUnavailable),
        }
    }
    fn save(&self, data_dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        let temporary = data_dir.join(format!("account-access-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            use std::io::Write;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, data_dir.join(ACCESS_FILE))?;
            #[cfg(unix)]
            fs::File::open(data_dir)?.sync_all()?;
            Ok::<(), std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.map_err(|_| MatrixCoreError::AppPasswordUnavailable)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Verifier {
    version: u8,
    // A closed identifier, not attacker-controlled PHC parameters.
    algorithm: String,
    salt: [u8; 16],
    hash: [u8; 32],
}
impl Drop for Verifier {
    fn drop(&mut self) {
        self.hash.zeroize();
    }
}

fn derive(password: &str, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
    if password.len() > 1024 {
        return Err(MatrixCoreError::AppPasswordInvalid);
    }
    let params = Params::new(KDF_MEMORY_KIB, KDF_PASSES, KDF_LANES, Some(32))
        .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    let mut result = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password.as_bytes(), salt, &mut *result, &mut *memory)
        .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
    Ok(result)
}

fn read_verifier(entry: &keyring::Entry) -> Result<Verifier> {
    let payload = Zeroizing::new(
        entry
            .get_password()
            .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?,
    );
    if payload.len() > 4096 {
        return Err(MatrixCoreError::AppPasswordUnavailable);
    }
    let verifier: Verifier =
        serde_json::from_str(&payload).map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
    if verifier.version != 1 || verifier.algorithm != "argon2id-v19-m65536-t3-p1" {
        return Err(MatrixCoreError::AppPasswordUnavailable);
    }
    Ok(verifier)
}

fn verify(entry: &keyring::Entry, password: &str) -> Result<()> {
    let verifier = read_verifier(entry)?;
    let calculated = derive(password, &verifier.salt)?;
    if bool::from(calculated.as_ref().ct_eq(&verifier.hash)) {
        Ok(())
    } else {
        Err(MatrixCoreError::AppPasswordIncorrect)
    }
}

pub(super) fn vault_entry(
    service: &str,
    id: &str,
) -> std::result::Result<keyring::Entry, keyring::Error> {
    #[cfg(test)]
    {
        Ok(tests::vault_entry(service, id))
    }
    #[cfg(not(test))]
    {
        keyring::Entry::new(service, id)
    }
}

fn entry(profile: &str) -> Result<keyring::Entry> {
    vault_entry(ACCESS_SERVICE, profile).map_err(|_| MatrixCoreError::AppPasswordUnavailable)
}

impl AccountManager {
    /// Includes detached OS workers after a read or removal timeout. Updaters
    /// must not restart while this is true.
    pub fn account_access_busy(&self) -> bool {
        self.access_worker.available_permits() == 0
    }

    /// Explicit lost-password recovery is permitted only after verified local
    /// removal of every account. It never authorizes an existing account.
    pub async fn reset_app_password_after_removal(&self) -> Result<()> {
        let permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        state.access = AccessFence::read(&self.data_dir)?;
        if !state.registry.accounts.is_empty()
            || !state.registry.pending_removals.is_empty()
            || !state.pending_sso.is_empty()
            || !state.loaded.is_empty()
            || state.active_account_id.is_some()
            || fs::read_dir(self.data_dir.join("accounts"))?
                .next()
                .is_some()
        {
            return Err(MatrixCoreError::AccountRemovalPending);
        }
        self.access_epoch.fetch_add(1, Ordering::AcqRel);
        let mut fence = state.access.clone();
        let data_dir = self.data_dir.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            fence.generation = fence
                .generation
                .checked_add(1)
                .ok_or(MatrixCoreError::AppPasswordUnavailable)?;
            match entry(&fence.profile)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(_) => return Err(MatrixCoreError::AppPasswordUnavailable),
            }
            fence.enabled = false;
            fence.save(&data_dir)?;
            Ok::<_, MatrixCoreError>(fence)
        });
        state.access = worker
            .await
            .map_err(|_| MatrixCoreError::AppPasswordUnavailable)??;
        Ok(())
    }

    pub async fn app_password_status(&self) -> AppPasswordStatus {
        let mut state = self.state.lock().await;
        // If a detached OS operation completed after its UI future was dropped,
        // refresh from its durable fence. A read error always reports protected.
        let enabled = match AccessFence::read(&self.data_dir) {
            Ok(fence) => {
                let enabled = fence.enabled;
                state.access = fence;
                enabled
            }
            Err(_) => true,
        };
        AppPasswordStatus {
            enabled,
            pending_removals: state.registry.pending_removals.clone(),
        }
    }

    pub async fn last_active_account_id(&self) -> Option<String> {
        let state = self.state.lock().await;
        state
            .registry
            .active_account_id
            .clone()
            .filter(|id| !state.registry.pending_removals.contains(id))
    }

    pub async fn begin_activation(&self, id: &str) -> Result<ActivationChallenge> {
        let _permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        state.access = AccessFence::read(&self.data_dir)?;
        ensure_eligible(&state, id)?;
        Ok(ActivationChallenge {
            account_id: id.into(),
            instance: self.access_instance,
            epoch: self.access_epoch.load(Ordering::Acquire),
            policy_generation: state.access.generation,
            expires: Instant::now() + GRANT_LIFETIME,
            cancelled: ActivationCancellation(Arc::new(AtomicBool::new(false))),
        })
    }

    pub async fn authorize_activation(
        &self,
        challenge: ActivationChallenge,
        password: Zeroizing<String>,
    ) -> Result<ActivationGrant> {
        let permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        state.access = AccessFence::read(&self.data_dir)?;
        self.validate_challenge(&state, &challenge)?;
        let enabled = state.access.enabled;
        let profile = state.access.profile.clone();
        drop(state);
        let cancellation = challenge.cancellation();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if cancellation.0.load(Ordering::Acquire) {
                return Err(MatrixCoreError::ActivationExpired);
            }
            if enabled {
                verify(&entry(&profile)?, &password)?;
            }
            if cancellation.0.load(Ordering::Acquire) {
                return Err(MatrixCoreError::ActivationExpired);
            }
            Ok(())
        });
        tokio::time::timeout(VAULT_TIMEOUT, worker)
            .await
            .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?
            .map_err(|_| MatrixCoreError::AppPasswordUnavailable)??;
        let state = self.state.lock().await;
        self.validate_challenge(&state, &challenge)?;
        Ok(ActivationGrant(challenge))
    }

    pub async fn activate(&self, grant: ActivationGrant) -> Result<AccountInfo> {
        let _permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        state.access = AccessFence::read(&self.data_dir)?;
        self.validate_challenge(&state, &grant.0)?;
        self.switch_locked(&mut state, &grant.0.account_id, Some(&grant.0))
            .await
    }

    pub async fn set_app_password(
        &self,
        current: Option<Zeroizing<String>>,
        new: Zeroizing<String>,
    ) -> Result<()> {
        if new.chars().count() < 12 || new.len() > 1024 {
            return Err(MatrixCoreError::AppPasswordInvalid);
        }
        let permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        state.access = AccessFence::read(&self.data_dir)?;
        self.access_epoch.fetch_add(1, Ordering::AcqRel);
        let mut fence = state.access.clone();
        let data_dir = self.data_dir.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let entry = entry(&fence.profile)?;
            if fence.enabled {
                verify(
                    &entry,
                    current
                        .as_deref()
                        .ok_or(MatrixCoreError::AppPasswordRequired)?,
                )?;
            }
            let mut salt = [0; 16];
            getrandom::fill(&mut salt).map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
            let hash = derive(&new, &salt)?;
            let verifier = Verifier {
                version: 1,
                algorithm: "argon2id-v19-m65536-t3-p1".into(),
                salt,
                hash: *hash,
            };
            let payload = Zeroizing::new(serde_json::to_string(&verifier)?);
            // For first enable, the existing state is unprotected until BOTH
            // writes succeed. There is never an enabled state without a fence.
            fence.generation = fence
                .generation
                .checked_add(1)
                .ok_or(MatrixCoreError::AppPasswordUnavailable)?;
            entry
                .set_password(&payload)
                .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
            fence.enabled = true;
            fence.save(&data_dir)?;
            Ok::<_, MatrixCoreError>(fence)
        });
        state.access = worker
            .await
            .map_err(|_| MatrixCoreError::AppPasswordUnavailable)??;
        Ok(())
    }

    pub async fn disable_app_password(&self, current: Zeroizing<String>) -> Result<()> {
        let permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        state.access = AccessFence::read(&self.data_dir)?;
        if !state.access.enabled {
            return Ok(());
        }
        self.access_epoch.fetch_add(1, Ordering::AcqRel);
        let mut fence = state.access.clone();
        let data_dir = self.data_dir.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let entry = entry(&fence.profile)?;
            verify(&entry, &current)?;
            // A crash or write failure after deletion leaves the enabled fence
            // in place and therefore fails closed.
            fence.generation = fence
                .generation
                .checked_add(1)
                .ok_or(MatrixCoreError::AppPasswordUnavailable)?;
            entry
                .delete_credential()
                .map_err(|_| MatrixCoreError::AppPasswordUnavailable)?;
            fence.enabled = false;
            fence.save(&data_dir)?;
            Ok::<_, MatrixCoreError>(fence)
        });
        state.access = worker
            .await
            .map_err(|_| MatrixCoreError::AppPasswordUnavailable)??;
        Ok(())
    }

    pub(super) fn access_permit(&self) -> Result<Arc<AccessPermit>> {
        let worker = self
            .access_worker
            .clone()
            .try_acquire_owned()
            .map_err(|_| MatrixCoreError::AccountAccessBusy)?;
        Ok(Arc::new(AccessPermit {
            _worker: worker,
            _profile_lock: self._access_lock.clone(),
        }))
    }

    pub(super) fn validate_challenge(
        &self,
        state: &State,
        challenge: &ActivationChallenge,
    ) -> Result<()> {
        if challenge.instance != self.access_instance
            || challenge.epoch != self.access_epoch.load(Ordering::Acquire)
            || challenge.policy_generation != state.access.generation
            || challenge.expires <= Instant::now()
            || challenge.cancelled.0.load(Ordering::Acquire)
        {
            return Err(MatrixCoreError::ActivationExpired);
        }
        ensure_eligible(state, &challenge.account_id)
    }

    /// Remove only this device's stored account. No network request or other
    /// account activation is performed. Incomplete cleanup remains visible and
    /// cannot be activated, including after restart.
    pub async fn remove_account(&self, id: &str) -> Result<RemovalResult> {
        let permit = self.access_permit()?;
        let mut state = self.state.lock().await;
        if !state
            .registry
            .accounts
            .iter()
            .any(|account| account.id == id)
        {
            return Err(MatrixCoreError::AccountNotFound(id.into()));
        }
        let mut next = state.registry.clone();
        if !next.pending_removals.iter().any(|pending| pending == id) {
            next.pending_removals.push(id.into());
        }
        if next.active_account_id.as_deref() == Some(id) {
            next.active_account_id = None;
        }
        save_registry(&self.data_dir, &next)?;
        state.registry = next;
        if state.active_account_id.as_deref() == Some(id) {
            self.selections.replace(None);
            if let Some(loaded) = state.loaded.get(id) {
                loaded.client.send_queue().set_enabled(false).await;
            }
            if let Some(recovery) = state.queue_recovery_task.take() {
                recovery.abort();
                let _ = recovery.await;
            }
            if let Some(sync) = state.sync_task.take() {
                sync.task.abort();
                let _ = sync.task.await;
            }
            state.active_account_id = None;
            self.publish_active_account(None);
            self.sync_tx.send_replace(SyncStatus::Stopped);
        }
        // Do not delete a credential while an already-started refresh write
        // can still recreate it. Retain the join handle for the next retry.
        if let Some(loaded) = state.loaded.get(id) {
            if !loaded.try_stop_token_persistence(CLEANUP_TIMEOUT).await {
                let loaded = loaded.clone();
                // The API can report pending cleanup promptly, while the
                // retained permit keeps updater restart and another cleanup
                // from racing the in-flight vault write.
                tokio::spawn(async move {
                    let _permit = permit;
                    loaded.stop_token_persistence().await;
                });
                return Ok(RemovalResult {
                    account_id: id.into(),
                    credentials_removed: false,
                    store_removed: false,
                    complete: false,
                });
            }
        }
        state.loaded.remove(id);
        let account_id = id.to_owned();
        let account_dir = self.data_dir.join("accounts").join(id);
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let credentials_removed = vault_entry(KEYRING_SERVICE, &account_id)
                .map(|entry| {
                    matches!(
                        entry.delete_credential(),
                        Ok(()) | Err(keyring::Error::NoEntry)
                    )
                })
                .unwrap_or(false);
            // External SDK/timeline/call clones may still hold SQLite open on
            // Windows. A sharing violation stays pending rather than being
            // presented as successful erasure.
            let store_removed = match fs::remove_dir_all(&account_dir) {
                Ok(()) => true,
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            };
            (credentials_removed, store_removed)
        });
        let result = tokio::time::timeout(CLEANUP_TIMEOUT, worker).await;
        let (credentials_removed, store_removed) = match result {
            Ok(Ok(value)) => value,
            _ => (false, false),
        };
        let complete = credentials_removed && store_removed;
        if complete {
            let mut next = state.registry.clone();
            next.accounts.retain(|account| account.id != id);
            next.pending_removals.retain(|pending| pending != id);
            save_registry(&self.data_dir, &next)?;
            state.registry = next;
        }
        Ok(RemovalResult {
            account_id: id.into(),
            credentials_removed,
            store_removed,
            complete,
        })
    }
}

pub(super) fn ensure_eligible(state: &State, id: &str) -> Result<()> {
    if state
        .registry
        .pending_removals
        .iter()
        .any(|pending| pending == id)
    {
        return Err(MatrixCoreError::AccountRemovalPending);
    }
    if !state
        .registry
        .accounts
        .iter()
        .any(|account| account.id == id)
    {
        return Err(MatrixCoreError::AccountNotFound(id.into()));
    }
    Ok(())
}

pub(super) fn lock_profile(data_dir: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(data_dir.join("account-access.lock"))?;
    file.try_lock()
        .map_err(|_| MatrixCoreError::AccountAccessBusy)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    use keyring::credential::CredentialApi;
    use std::sync::{Mutex as StdMutex, OnceLock};

    type MockEntries = HashMap<(String, String), Arc<keyring::mock::MockCredential>>;
    fn entries() -> &'static StdMutex<MockEntries> {
        static ENTRIES: OnceLock<StdMutex<MockEntries>> = OnceLock::new();
        ENTRIES.get_or_init(|| StdMutex::new(HashMap::new()))
    }
    type CredentialOverrides = HashMap<(String, String), Arc<keyring::credential::Credential>>;
    fn overrides() -> &'static StdMutex<CredentialOverrides> {
        static OVERRIDES: OnceLock<StdMutex<CredentialOverrides>> = OnceLock::new();
        OVERRIDES.get_or_init(|| StdMutex::new(HashMap::new()))
    }
    struct SharedMock(Arc<keyring::credential::Credential>);
    impl CredentialApi for SharedMock {
        fn set_secret(&self, secret: &[u8]) -> std::result::Result<(), keyring::Error> {
            self.0.set_secret(secret)
        }
        fn get_secret(&self) -> std::result::Result<Vec<u8>, keyring::Error> {
            self.0.get_secret()
        }
        fn delete_credential(&self) -> std::result::Result<(), keyring::Error> {
            self.0.delete_credential()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    pub(super) fn vault_entry(service: &str, id: &str) -> keyring::Entry {
        if let Some(credential) = overrides()
            .lock()
            .unwrap()
            .get(&(service.into(), id.into()))
            .cloned()
        {
            return keyring::Entry::new_with_credential(Box::new(SharedMock(credential)));
        }
        let credential = entries()
            .lock()
            .unwrap()
            .entry((service.into(), id.into()))
            .or_insert_with(|| Arc::new(keyring::mock::MockCredential::default()))
            .clone();
        keyring::Entry::new_with_credential(Box::new(SharedMock(credential)))
    }

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("morpheus-access-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn account() -> AccountInfo {
        AccountInfo {
            id: Uuid::new_v4().to_string(),
            user_id: "@casey:example.org".into(),
            device_id: "TEST".into(),
            homeserver: "https://example.org".into(),
        }
    }
    fn mock_entry() -> keyring::Entry {
        keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()))
    }
    fn store_verifier(entry: &keyring::Entry, password: &str) {
        let salt = [23; 16];
        let hash = derive(password, &salt).unwrap();
        let verifier = Verifier {
            version: 1,
            algorithm: "argon2id-v19-m65536-t3-p1".into(),
            salt,
            hash: *hash,
        };
        entry
            .set_password(&serde_json::to_string(&verifier).unwrap())
            .unwrap();
    }

    struct ControlledCredential {
        backing: Arc<keyring::mock::MockCredential>,
        before_write: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl CredentialApi for ControlledCredential {
        fn set_secret(&self, value: &[u8]) -> std::result::Result<(), keyring::Error> {
            if let Some(before_write) = self.before_write.lock().unwrap().take() {
                before_write();
            }
            self.backing.set_secret(value)
        }
        fn get_secret(&self) -> std::result::Result<Vec<u8>, keyring::Error> {
            self.backing.get_secret()
        }
        fn delete_credential(&self) -> std::result::Result<(), keyring::Error> {
            self.backing.delete_credential()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    fn controlled_credential(
        id: &str,
        before_write: impl FnOnce() + Send + 'static,
    ) -> Arc<keyring::mock::MockCredential> {
        let backing = Arc::new(keyring::mock::MockCredential::default());
        overrides().lock().unwrap().insert(
            (KEYRING_SERVICE.into(), id.into()),
            Arc::new(ControlledCredential {
                backing: backing.clone(),
                before_write: StdMutex::new(Some(Box::new(before_write))),
            }),
        );
        backing
    }
    fn login_secret(account: &AccountInfo) -> StoredSecret {
        StoredSecret {
            version: SECRET_VERSION,
            store_passphrase: "synthetic store passphrase".into(),
            session: StoredSession::Matrix(MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: UserId::parse(&account.user_id).unwrap(),
                    device_id: account.device_id.as_str().into(),
                },
                tokens: matrix_sdk::SessionTokens {
                    access_token: "synthetic access token".into(),
                    refresh_token: None,
                },
            }),
        }
    }
    fn pending_login(manager: &AccountManager) -> (authentication::PendingStore, AccountInfo) {
        let store = authentication::PendingStore::new(manager).unwrap();
        let mut saved = account();
        saved.id = store.id.clone();
        (store, saved)
    }
    async fn assert_blocked_login(manager: &AccountManager, saved: &AccountInfo) {
        assert!(manager.accounts().await.contains(saved));
        assert!(
            manager
                .app_password_status()
                .await
                .pending_removals
                .contains(&saved.id)
        );
        assert!(matches!(
            manager.switch_to(&saved.id).await,
            Err(MatrixCoreError::AccountRemovalPending)
        ));
    }

    #[test]
    fn cancelled_login_write_retains_cleanup_owner_and_cannot_resurrect_after_removal() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = Arc::new(AccountManager::open(&directory.0).unwrap());
            let (mut store, saved) = pending_login(&manager);
            let (entered, waiting) = oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let credential = controlled_credential(&saved.id, move || {
                let _ = entered.send(());
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            let task_manager = manager.clone();
            let task_account = saved.clone();
            let writer = tokio::spawn(async move {
                let permit = task_manager.access_permit().unwrap();
                let mut state = task_manager.state.lock().await;
                task_manager
                    .persist_new_account(
                        &mut state,
                        &task_account,
                        &mut store,
                        login_secret(&task_account),
                        permit,
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap();
            writer.abort();
            assert!(writer.await.unwrap_err().is_cancelled());
            assert!(manager.account_access_busy());
            assert!(
                manager
                    .app_password_status()
                    .await
                    .pending_removals
                    .contains(&saved.id)
            );
            assert!(matches!(
                manager.remove_account(&saved.id).await,
                Err(MatrixCoreError::AccountAccessBusy)
            ));
            assert!(
                directory
                    .0
                    .join("accounts")
                    .join(&saved.id)
                    .join(".pending-browser-login")
                    .is_file()
            );
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while manager.account_access_busy() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(credential.get_secret().is_ok());
            assert_blocked_login(&manager, &saved).await;
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
            assert!(matches!(
                credential.get_secret(),
                Err(keyring::Error::NoEntry)
            ));
            assert!(!directory.0.join("accounts").join(&saved.id).exists());
            assert!(manager.accounts().await.is_empty());
        });
    }

    #[test]
    fn pending_registry_sync_failure_never_starts_a_vault_write() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let (mut store, saved) = pending_login(&manager);
            let result = {
                let permit = manager.access_permit().unwrap();
                let mut state = manager.state.lock().await;
                manager
                    .persist_new_account_with_sync(
                        &mut state,
                        &saved,
                        &mut store,
                        login_secret(&saved),
                        permit,
                        |_| {
                            Err(
                                std::io::Error::other("injected pending directory sync failure")
                                    .into(),
                            )
                        },
                    )
                    .await
            };
            assert!(result.is_err());
            drop(store);
            assert_blocked_login(&manager, &saved).await;
            assert!(matches!(
                vault_entry(KEYRING_SERVICE, &saved.id).get_password(),
                Err(keyring::Error::NoEntry)
            ));
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
        });
    }

    #[test]
    fn final_registry_sync_failure_recovers_blocked_row_after_restart() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let (mut store, saved) = pending_login(&manager);
            let result = {
                let permit = manager.access_permit().unwrap();
                let mut state = manager.state.lock().await;
                let mut syncs = 0;
                manager
                    .persist_new_account_with_sync(
                        &mut state,
                        &saved,
                        &mut store,
                        login_secret(&saved),
                        permit,
                        |path| {
                            syncs += 1;
                            if syncs == 2 {
                                Err(
                                    std::io::Error::other("injected final directory sync failure")
                                        .into(),
                                )
                            } else {
                                sync_registry_directory(path)
                            }
                        },
                    )
                    .await
            };
            assert!(result.is_err());
            assert_blocked_login(&manager, &saved).await;
            let disk: Registry =
                serde_json::from_slice(&fs::read(directory.0.join("accounts.json")).unwrap())
                    .unwrap();
            assert!(
                !disk.pending_removals.contains(&saved.id),
                "test must cross the successful final rename"
            );
            drop(store);
            manager.stop().await;
            drop(manager);
            let manager = AccountManager::open(&directory.0).unwrap();
            assert!(manager.active_account().await.is_none());
            assert!(manager.last_active_account_id().await.is_none());
            assert_blocked_login(&manager, &saved).await;
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
        });
    }

    #[test]
    fn final_registry_save_failure_and_vault_delete_failure_remain_retryable() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let (mut store, saved) = pending_login(&manager);
            let registry_path = directory.0.join("accounts.json");
            let pending_bytes = Arc::new(StdMutex::new(Vec::new()));
            let captured = pending_bytes.clone();
            let broken_path = registry_path.clone();
            let credential = controlled_credential(&saved.id, move || {
                *captured.lock().unwrap() = fs::read(&broken_path).unwrap();
                fs::remove_file(&broken_path).unwrap();
                fs::create_dir(&broken_path).unwrap();
            });
            let result = {
                let permit = manager.access_permit().unwrap();
                let mut state = manager.state.lock().await;
                manager
                    .persist_new_account(
                        &mut state,
                        &saved,
                        &mut store,
                        login_secret(&saved),
                        permit,
                    )
                    .await
            };
            assert!(result.is_err());
            drop(store);
            assert!(
                directory
                    .0
                    .join("accounts")
                    .join(&saved.id)
                    .join("store")
                    .is_dir()
            );
            assert_blocked_login(&manager, &saved).await;
            fs::remove_dir(&registry_path).unwrap();
            fs::write(&registry_path, &*pending_bytes.lock().unwrap()).unwrap();
            credential.inner.lock().unwrap().get_mut().error =
                Some(keyring::Error::PlatformFailure(Box::new(
                    std::io::Error::other("fixture vault unavailable"),
                )));
            let removal = manager.remove_account(&saved.id).await.unwrap();
            assert!(!removal.complete && !removal.credentials_removed);
            assert_blocked_login(&manager, &saved).await;
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
            assert!(matches!(
                credential.get_secret(),
                Err(keyring::Error::NoEntry)
            ));
        });
    }

    #[test]
    fn successful_login_commit_disarms_journal_without_changing_active_authority() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let previous = account();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(previous.clone());
                state.registry.active_account_id = Some(previous.id.clone());
                state.active_account_id = Some(previous.id.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            manager.publish_active_account(Some(previous.clone()));
            let selection = manager.account_selection(&previous.id).unwrap();
            let (mut store, saved) = pending_login(&manager);
            {
                let permit = manager.access_permit().unwrap();
                let mut state = manager.state.lock().await;
                manager
                    .persist_new_account(
                        &mut state,
                        &saved,
                        &mut store,
                        login_secret(&saved),
                        permit,
                    )
                    .await
                    .unwrap();
            }
            drop(store);
            assert_eq!(manager.active_account().await, Some(previous));
            assert!(selection.is_current());
            assert!(
                manager
                    .app_password_status()
                    .await
                    .pending_removals
                    .is_empty()
            );
            assert!(
                directory
                    .0
                    .join("accounts")
                    .join(&saved.id)
                    .join("store")
                    .is_dir()
            );
            assert!(
                !directory
                    .0
                    .join("accounts")
                    .join(&saved.id)
                    .join(".pending-browser-login")
                    .exists()
            );
            manager.stop().await;
            drop(manager);
            let manager = AccountManager::open(&directory.0).unwrap();
            assert!(
                manager
                    .app_password_status()
                    .await
                    .pending_removals
                    .is_empty()
            );
            assert!(manager.accounts().await.contains(&saved));
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
        });
    }

    #[test]
    fn password_verification_rejects_wrong_password_and_missing_vault_entry() {
        let entry = mock_entry();
        assert!(matches!(
            verify(&entry, "a longer fixture password"),
            Err(MatrixCoreError::AppPasswordUnavailable)
        ));
        store_verifier(&entry, "a longer fixture password");
        verify(&entry, "a longer fixture password").unwrap();
        assert!(matches!(
            verify(&entry, "a different fixture password"),
            Err(MatrixCoreError::AppPasswordIncorrect)
        ));
    }

    #[test]
    fn verifier_rejects_unknown_schema_and_unbounded_kdf_inputs() {
        let entry = mock_entry();
        for payload in [
            "{}".to_string(),
            "x".repeat(4097),
            serde_json::json!({"version":1,"algorithm":"argon2id-v19-m4294967295-t4294967295-p1","salt":vec![0_u8;16],"hash":vec![0_u8;32]}).to_string(),
            serde_json::json!({"version":1,"algorithm":"argon2id-v19-m65536-t3-p1","salt":vec![0_u8;16],"hash":vec![0_u8;32],"memory":0}).to_string(),
        ] {
            entry.set_password(&payload).unwrap();
            assert!(matches!(read_verifier(&entry), Err(MatrixCoreError::AppPasswordUnavailable)));
        }
    }

    #[test]
    fn protected_startup_exposes_only_a_restore_hint() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let saved = account();
            let mut registry = Registry::default();
            registry.accounts.push(saved.clone());
            registry.active_account_id = Some(saved.id.clone());
            fs::create_dir(directory.0.join("accounts")).unwrap();
            let mut fence = AccessFence::open(&directory.0).unwrap();
            save_registry(&directory.0, &registry).unwrap();
            fence.enabled = true;
            fence.save(&directory.0).unwrap();
            let manager = AccountManager::open(&directory.0).unwrap();
            assert!(manager.active_account().await.is_none());
            assert_eq!(
                manager.last_active_account_id().await.as_deref(),
                Some(saved.id.as_str())
            );
            assert!(matches!(
                manager.active_client().await,
                Err(MatrixCoreError::NoActiveAccount)
            ));
            assert!(matches!(
                manager.restore_last_active().await,
                Err(MatrixCoreError::AppPasswordRequired)
            ));
            assert!(matches!(
                manager.switch_to(&saved.id).await,
                Err(MatrixCoreError::AppPasswordRequired)
            ));
        });
    }

    #[test]
    fn cancellation_epoch_policy_and_foreign_manager_each_revoke_grants() {
        runtime().block_on(async {
            let first_dir = TestDirectory::new();
            let second_dir = TestDirectory::new();
            let first = AccountManager::open(&first_dir.0).unwrap();
            let second = AccountManager::open(&second_dir.0).unwrap();
            let saved = account();
            first
                .state
                .lock()
                .await
                .registry
                .accounts
                .push(saved.clone());
            second
                .state
                .lock()
                .await
                .registry
                .accounts
                .push(saved.clone());
            let cancelled = first.begin_activation(&saved.id).await.unwrap();
            cancelled.cancellation().cancel();
            assert!(matches!(
                first
                    .authorize_activation(cancelled, Zeroizing::new(String::new()))
                    .await,
                Err(MatrixCoreError::ActivationExpired)
            ));
            let grant = first
                .authorize_activation(
                    first.begin_activation(&saved.id).await.unwrap(),
                    Zeroizing::new(String::new()),
                )
                .await
                .unwrap();
            assert!(matches!(
                second.activate(grant).await,
                Err(MatrixCoreError::ActivationExpired)
            ));
            let grant = first
                .authorize_activation(
                    first.begin_activation(&saved.id).await.unwrap(),
                    Zeroizing::new(String::new()),
                )
                .await
                .unwrap();
            first.publish_active_account(None);
            assert!(matches!(
                first.activate(grant).await,
                Err(MatrixCoreError::ActivationExpired)
            ));
            let grant = first
                .authorize_activation(
                    first.begin_activation(&saved.id).await.unwrap(),
                    Zeroizing::new(String::new()),
                )
                .await
                .unwrap();
            let mut fence = AccessFence::read(&first_dir.0).unwrap();
            fence.generation += 1;
            fence.save(&first_dir.0).unwrap();
            assert!(matches!(
                first.activate(grant).await,
                Err(MatrixCoreError::ActivationExpired)
            ));
            let mut expired = first.begin_activation(&saved.id).await.unwrap();
            expired.expires = Instant::now();
            assert!(matches!(
                first
                    .authorize_activation(expired, Zeroizing::new(String::new()))
                    .await,
                Err(MatrixCoreError::ActivationExpired)
            ));
        });
    }

    #[test]
    fn pending_removal_survives_restart_and_cannot_restore_or_activate() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let saved = account();
            let registry = Registry {
                accounts: vec![saved.clone()],
                pending_removals: vec![saved.id.clone()],
                active_account_id: Some(saved.id.clone()),
                ..Registry::default()
            };
            let initial = AccountManager::open(&directory.0).unwrap();
            drop(initial);
            save_registry(&directory.0, &registry).unwrap();
            let manager = AccountManager::open(&directory.0).unwrap();
            assert!(manager.last_active_account_id().await.is_none());
            assert!(manager.restore_last_active().await.unwrap().is_none());
            assert!(matches!(
                manager.switch_to(&saved.id).await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            assert!(matches!(
                manager.begin_activation(&saved.id).await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            assert_eq!(
                manager.app_password_status().await.pending_removals,
                [saved.id.clone()]
            );
            assert_eq!(manager.accounts().await, [saved]);
        });
    }

    #[test]
    fn unfinished_worker_retains_exclusive_profile_and_operation_gate() {
        let directory = TestDirectory::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let worker = manager.access_permit().unwrap();
        assert!(matches!(
            manager.access_permit(),
            Err(MatrixCoreError::AccountAccessBusy)
        ));
        drop(manager);
        assert!(matches!(
            AccountManager::open(&directory.0),
            Err(MatrixCoreError::AccountAccessBusy)
        ));
        drop(worker);
        assert!(AccountManager::open(&directory.0).is_ok());
    }

    #[test]
    fn missing_or_malformed_enabled_fence_never_becomes_disabled_in_memory() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            manager
                .state
                .lock()
                .await
                .registry
                .accounts
                .push(saved.clone());
            fs::remove_file(directory.0.join(ACCESS_FILE)).unwrap();
            assert!(manager.app_password_status().await.enabled);
            assert!(matches!(
                manager.switch_to(&saved.id).await,
                Err(MatrixCoreError::AppPasswordUnavailable)
            ));
            fs::write(directory.0.join(ACCESS_FILE), "{}").unwrap();
            assert!(matches!(
                manager.begin_activation(&saved.id).await,
                Err(MatrixCoreError::AppPasswordUnavailable)
            ));
        });
    }

    #[test]
    fn missing_fence_refuses_existing_profiles_without_replacing_data() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(saved.clone());
                state.registry.active_account_id = Some(saved.id.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            manager
                .set_app_password(None, Zeroizing::new("fixture profile password".into()))
                .await
                .unwrap();
            let registry_before = fs::read(directory.0.join("accounts.json")).unwrap();
            let store = directory.0.join("accounts").join(&saved.id);
            fs::create_dir(&store).unwrap();
            fs::write(store.join("preserved"), b"account fixture").unwrap();
            manager.stop().await;
            drop(manager);
            fs::remove_file(directory.0.join(ACCESS_FILE)).unwrap();
            assert!(matches!(
                AccountManager::open(&directory.0),
                Err(MatrixCoreError::AppPasswordUnavailable)
            ));
            assert!(!directory.0.join(ACCESS_FILE).exists());
            assert_eq!(
                fs::read(directory.0.join("accounts.json")).unwrap(),
                registry_before
            );
            assert_eq!(
                fs::read(store.join("preserved")).unwrap(),
                b"account fixture"
            );
        });
    }

    #[test]
    fn missing_fence_creation_requires_no_registry_and_an_empty_account_directory() {
        for case in [
            "empty_registry",
            "invalid_registry",
            "unreadable_registry",
            "unknown_directory",
        ] {
            let directory = TestDirectory::new();
            fs::create_dir(directory.0.join("accounts")).unwrap();
            match case {
                "empty_registry" => save_registry(&directory.0, &Registry::default()).unwrap(),
                "invalid_registry" => {
                    fs::write(directory.0.join("accounts.json"), b"invalid").unwrap()
                }
                "unreadable_registry" => fs::create_dir(directory.0.join("accounts.json")).unwrap(),
                "unknown_directory" => {
                    fs::create_dir(directory.0.join("accounts").join("unknown")).unwrap()
                }
                _ => unreachable!(),
            }
            assert!(AccountManager::open(&directory.0).is_err(), "{case}");
            assert!(!directory.0.join(ACCESS_FILE).exists(), "{case}");
        }
        let directory = TestDirectory::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        assert!(!AccessFence::read(&directory.0).unwrap().enabled);
        drop(manager);
        assert!(AccountManager::open(&directory.0).is_ok());
    }

    #[test]
    fn new_password_length_is_checked_before_any_vault_operation() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            for password in ["short".into(), "é".repeat(11), "界".repeat(342)] {
                assert!(matches!(
                    manager
                        .set_app_password(None, Zeroizing::new(password))
                        .await,
                    Err(MatrixCoreError::AppPasswordInvalid)
                ));
            }
            assert!(!manager.app_password_status().await.enabled);
        });
    }

    #[test]
    fn enable_change_disable_and_restart_enforce_the_current_password() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(saved.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            manager
                .set_app_password(None, Zeroizing::new("first fixture password".into()))
                .await
                .unwrap();
            let grant = manager
                .authorize_activation(
                    manager.begin_activation(&saved.id).await.unwrap(),
                    Zeroizing::new("first fixture password".into()),
                )
                .await
                .unwrap();
            assert!(matches!(
                manager
                    .set_app_password(
                        Some(Zeroizing::new("incorrect password".into())),
                        Zeroizing::new("second fixture password".into())
                    )
                    .await,
                Err(MatrixCoreError::AppPasswordIncorrect)
            ));
            assert!(manager.app_password_status().await.enabled);
            manager
                .set_app_password(
                    Some(Zeroizing::new("first fixture password".into())),
                    Zeroizing::new("second fixture password".into()),
                )
                .await
                .unwrap();
            assert!(matches!(
                manager.activate(grant).await,
                Err(MatrixCoreError::ActivationExpired)
            ));
            drop(manager);
            let manager = AccountManager::open(&directory.0).unwrap();
            assert!(manager.app_password_status().await.enabled);
            assert!(manager.active_account().await.is_none());
            assert!(matches!(
                manager
                    .authorize_activation(
                        manager.begin_activation(&saved.id).await.unwrap(),
                        Zeroizing::new("first fixture password".into())
                    )
                    .await,
                Err(MatrixCoreError::AppPasswordIncorrect)
            ));
            assert!(matches!(
                manager
                    .disable_app_password(Zeroizing::new("incorrect password".into()))
                    .await,
                Err(MatrixCoreError::AppPasswordIncorrect)
            ));
            manager
                .disable_app_password(Zeroizing::new("second fixture password".into()))
                .await
                .unwrap();
            assert!(!manager.app_password_status().await.enabled);
        });
    }

    #[test]
    fn removal_is_local_and_partial_cleanup_stays_retryable() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let first = account();
            let second = account();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![first.clone(), second.clone()];
                state.registry.active_account_id = Some(second.id.clone());
                state.active_account_id = Some(second.id.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            vault_entry(KEYRING_SERVICE, &first.id)
                .set_password("disposable test token")
                .unwrap();
            // A file in place of the account directory is an I/O error, not successful cleanup.
            let path = directory.0.join("accounts").join(&first.id);
            fs::write(&path, b"fixture").unwrap();
            let removed = manager.remove_account(&first.id).await.unwrap();
            assert!(removed.credentials_removed && !removed.store_removed && !removed.complete);
            assert_eq!(manager.active_account().await, Some(second.clone()));
            assert!(matches!(
                manager.begin_activation(&first.id).await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            fs::remove_file(path).unwrap();
            let removed = manager.remove_account(&first.id).await.unwrap();
            assert!(removed.complete);
            assert_eq!(manager.accounts().await, [second.clone()]);
            assert_eq!(manager.active_account().await, Some(second));
        });
    }

    #[test]
    fn pending_removal_blocks_drafts_even_with_a_retained_loaded_client() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let client = Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(saved.clone());
                state.loaded.insert(
                    saved.id.clone(),
                    Arc::new(LoadedAccount::new(
                        client,
                        saved.id.clone(),
                        String::new(),
                        manager._access_lock.clone(),
                    )),
                );
            }
            let room_id = "!draft:example.org";
            // Prove this fixture reaches the loaded client's room lookup first.
            assert!(matches!(
                manager
                    .load_composed_draft_for(&saved.id, room_id, None)
                    .await,
                Err(MatrixCoreError::RoomNotFound(_))
            ));
            // This is the retained-client state after a refresh worker prevents
            // removal from completing promptly. Delayed draft work must stop
            // at the tombstone, before consulting any SDK room or store.
            manager
                .state
                .lock()
                .await
                .registry
                .pending_removals
                .push(saved.id.clone());
            assert!(matches!(
                manager
                    .load_composed_draft_for(&saved.id, room_id, None)
                    .await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            assert!(matches!(
                manager
                    .save_composed_draft_for(
                        &saved.id,
                        room_id,
                        None,
                        &ComposeText::plain("delayed draft"),
                        &DraftEnvelope::fresh(),
                    )
                    .await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            manager.stop().await;
        });
    }

    #[test]
    fn vault_failure_during_removal_is_not_reported_as_complete() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            manager
                .state
                .lock()
                .await
                .registry
                .accounts
                .push(saved.clone());
            let credential = vault_entry(KEYRING_SERVICE, &saved.id);
            credential.set_password("disposable test token").unwrap();
            // A platform failure must retain the tombstone even after store cleanup.
            let backing = entries()
                .lock()
                .unwrap()
                .get(&(KEYRING_SERVICE.into(), saved.id.clone()))
                .unwrap()
                .clone();
            backing.inner.lock().unwrap().get_mut().error = Some(keyring::Error::PlatformFailure(
                Box::new(std::io::Error::other("fixture")),
            ));
            let result = manager.remove_account(&saved.id).await.unwrap();
            assert!(!result.credentials_removed && result.store_removed && !result.complete);
            assert_eq!(manager.accounts().await, [saved.clone()]);
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
        });
    }

    #[test]
    fn explicit_reset_requires_every_account_and_store_to_be_gone() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            manager
                .set_app_password(None, Zeroizing::new("forgotten fixture password".into()))
                .await
                .unwrap();
            let saved = account();
            manager
                .state
                .lock()
                .await
                .registry
                .accounts
                .push(saved.clone());
            assert!(matches!(
                manager.reset_app_password_after_removal().await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            assert!(manager.remove_account(&saved.id).await.unwrap().complete);
            fs::create_dir(directory.0.join("accounts").join("leftover")).unwrap();
            assert!(matches!(
                manager.reset_app_password_after_removal().await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            fs::remove_dir(directory.0.join("accounts").join("leftover")).unwrap();
            manager.reset_app_password_after_removal().await.unwrap();
            assert!(!manager.app_password_status().await.enabled);
        });
    }

    #[test]
    fn selected_logout_rejects_retargeted_aba_foreign_and_stopped_intents_without_effects() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let first = account();
            let second = account();
            for saved in [&first, &second] {
                let store = directory.0.join("accounts").join(&saved.id);
                fs::create_dir_all(&store).unwrap();
                fs::write(store.join("keep"), b"owned history fixture").unwrap();
                vault_entry(KEYRING_SERVICE, &saved.id)
                    .set_password("owned credential fixture")
                    .unwrap();
            }
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![first.clone(), second.clone()];
                state.registry.active_account_id = Some(first.id.clone());
                state.active_account_id = Some(first.id.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            manager.publish_active_account(Some(first.clone()));
            // This is the synchronous token captured by the initial UI click.
            let queued = manager.account_selection(&first.id).unwrap();
            let foreign_owner = crate::account_selection::Selections::default();
            foreign_owner.replace(Some(&first.id));
            let foreign = foreign_owner.for_account(&first.id).unwrap();
            // A queued switch can win the transition lock; returning to the
            // same ID, or a token from another manager, must not revive intent.
            for (active, token) in [
                (Some(second.clone()), queued.clone()),
                (Some(first.clone()), queued.clone()),
                (Some(first.clone()), foreign),
                (None, queued),
            ] {
                {
                    let mut state = manager.state.lock().await;
                    state.active_account_id = active.as_ref().map(|a| a.id.clone());
                    state.registry.active_account_id = state.active_account_id.clone();
                    save_registry(&directory.0, &state.registry).unwrap();
                }
                manager.publish_active_account(active.clone());
                let before = fs::read(directory.0.join("accounts.json")).unwrap();
                let published = manager.subscribe_active_account().borrow().clone();
                assert!(matches!(
                    manager.logout_selected_account(&token).await,
                    Err(MatrixCoreError::SelectedAccountChanged)
                ));
                assert_eq!(fs::read(directory.0.join("accounts.json")).unwrap(), before);
                assert_eq!(manager.active_account().await, active);
                assert_eq!(*manager.subscribe_active_account().borrow(), published);
                assert!(
                    manager
                        .app_password_status()
                        .await
                        .pending_removals
                        .is_empty()
                );
                assert_eq!(manager.accounts().await, [first.clone(), second.clone()]);
                for saved in [&first, &second] {
                    assert_eq!(
                        fs::read(directory.0.join("accounts").join(&saved.id).join("keep"))
                            .unwrap(),
                        b"owned history fixture"
                    );
                    assert_eq!(
                        vault_entry(KEYRING_SERVICE, &saved.id)
                            .get_password()
                            .unwrap(),
                        "owned credential fixture"
                    );
                }
            }
        });
    }

    #[test]
    fn selected_logout_removes_only_current_target_and_reports_unconfirmed_remote_logout() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let first = account();
            let second = account();
            for saved in [&first, &second] {
                let store = directory.0.join("accounts").join(&saved.id);
                fs::create_dir_all(&store).unwrap();
                fs::write(store.join("keep"), b"owned history fixture").unwrap();
            }
            // No first-account credential exists in this isolated mock vault:
            // remote logout is unavailable, but explicit local removal works.
            vault_entry(KEYRING_SERVICE, &second.id)
                .set_password("other account credential fixture")
                .unwrap();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![first.clone(), second.clone()];
                state.registry.active_account_id = Some(first.id.clone());
                state.active_account_id = Some(first.id.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            manager.publish_active_account(Some(first.clone()));
            let selected = manager.account_selection(&first.id).unwrap();
            let result = manager.logout_selected_account(&selected).await.unwrap();
            assert!(!result.remote_logout_succeeded);
            assert!(!selected.is_current());
            assert!(manager.active_account().await.is_none());
            assert!(manager.account_selection(&first.id).is_none());
            assert_eq!(manager.accounts().await, [second.clone()]);
            assert!(!directory.0.join("accounts").join(&first.id).exists());
            assert!(
                directory
                    .0
                    .join("accounts")
                    .join(&second.id)
                    .join("keep")
                    .is_file()
            );
            assert_eq!(
                vault_entry(KEYRING_SERVICE, &second.id)
                    .get_password()
                    .unwrap(),
                "other account credential fixture"
            );
        });
    }

    #[test]
    fn failed_legacy_logout_never_retains_live_authority_or_same_id_bypass() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(saved.clone());
                state.registry.active_account_id = Some(saved.id.clone());
                state.active_account_id = Some(saved.id.clone());
                state.access.enabled = true;
                state.access.save(&directory.0).unwrap();
            }
            manager.publish_active_account(Some(saved.clone()));
            // Vault deletion succeeds, but local store cleanup must fail.
            fs::write(directory.0.join("accounts").join(&saved.id), b"fixture").unwrap();
            assert!(manager.logout_account(&saved.id).await.is_err());
            assert!(manager.active_account().await.is_none());
            assert!(manager.subscribe_active_account().borrow().is_none());
            assert!(manager.account_selection(&saved.id).is_none());
            assert!(matches!(
                manager.switch_to(&saved.id).await,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            assert_eq!(manager.accounts().await, [saved]);
        });
    }

    #[test]
    fn selected_pause_retains_token_owner_but_stop_drains_before_profile_reopen() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let lock = manager._access_lock.clone();
            let (shutdown, mut receiver) = oneshot::channel();
            let (probe_tx, mut probes) =
                tokio::sync::mpsc::unbounded_channel::<oneshot::Sender<()>>();
            let task = tokio::spawn(async move {
                let _lock = lock;
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut receiver => break,
                        probe = probes.recv() => {
                            let Some(probe) = probe else { break };
                            let _ = probe.send(());
                        }
                    }
                }
            });
            let client = Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            // This manual LoadedAccount fixture must match build_client.
            client.send_queue().set_enabled(false).await;
            let (incoming_verification_tx, _) = watch::channel(Vec::new());
            let loaded = Arc::new(LoadedAccount {
                client: client.clone(),
                timelines: Mutex::new(HashMap::new()),
                composed_edits: Mutex::new(HashMap::new()),
                pending_sends: Mutex::new(HashMap::new()),
                originals: Mutex::new(HashMap::new()),
                recovered: Mutex::new(Vec::new()),
                recovery_gate: Mutex::new(RecoveryGate::default()),
                recovery_ready: watch::channel(false).0,
                incoming_verification_tx,
                verification_book: Default::default(),
                notification_handler_registered: Mutex::new(false),
                token_task: Mutex::new(Some(SyncTask {
                    shutdown: Some(shutdown),
                    task,
                })),
            });
            let lifetime = Arc::downgrade(&loaded);
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(saved.clone());
                state.registry.active_account_id = Some(saved.id.clone());
                state.active_account_id = Some(saved.id.clone());
                state.loaded.insert(saved.id.clone(), loaded);
                let (shutdown, stopped) = oneshot::channel();
                state.sync_task = Some(SyncTask {
                    shutdown: Some(shutdown),
                    task: tokio::spawn(async {
                        let _ = stopped.await;
                    }),
                });
            }
            manager.publish_active_account(Some(saved.clone()));
            let stale = manager.account_selection(&saved.id).unwrap();
            manager.publish_active_account(Some(saved.clone()));
            let current = manager.account_selection(&saved.id).unwrap();
            let foreign = crate::account_selection::Selections::default();
            foreign.replace(Some(&saved.id));
            let foreign_selection = foreign.for_account(&saved.id).unwrap();
            for rejected in [&stale, &foreign_selection] {
                assert!(matches!(
                    manager.pause_active_sync_for(rejected).await,
                    Err(MatrixCoreError::SelectedAccountChanged)
                ));
                // The client-wide queue is never enabled; the live sync task
                // and the current selection show the account is still active.
                assert!(!client.send_queue().is_enabled());
                assert!(manager.state.lock().await.sync_task.is_some());
                assert!(current.is_current());
            }
            manager.pause_active_sync_for(&current).await.unwrap();
            assert!(!client.send_queue().is_enabled());
            assert!(current.is_current());
            assert_eq!(manager.active_account().await, Some(saved.clone()));
            assert_eq!(
                *manager.subscribe_sync_status().borrow(),
                SyncStatus::Stopped
            );
            assert!(manager.state.lock().await.sync_task.is_none());
            assert!(lifetime.upgrade().is_some());
            // The token owner's task remains able to handle a new event after
            // pause. Actual rotating-token persistence is covered by OAuth QA.
            let (probe, observed) = oneshot::channel();
            probe_tx.send(probe).unwrap();
            tokio::time::timeout(Duration::from_secs(1), observed)
                .await
                .unwrap()
                .unwrap();
            manager.stop().await;
            assert!(!current.is_current());
            assert!(manager.active_account().await.is_none());
            assert!(probe_tx.is_closed());
            assert!(lifetime.upgrade().is_none());
            drop(manager);
            assert!(AccountManager::open(&directory.0).is_ok());
        });
    }

    #[test]
    fn valid_grant_switches_cached_account_and_active_removal_has_no_fallback() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let first = account();
            let second = account();
            let client_a = Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            let client_b = Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            client_b.send_queue().set_enabled(false).await;
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![first.clone(), second.clone()];
                state.registry.active_account_id = Some(first.id.clone());
                state.active_account_id = Some(first.id.clone());
                state.loaded.insert(
                    first.id.clone(),
                    Arc::new(LoadedAccount::new(
                        client_a.clone(),
                        first.id.clone(),
                        String::new(),
                        manager._access_lock.clone(),
                    )),
                );
                state.loaded.insert(
                    second.id.clone(),
                    Arc::new(LoadedAccount::new(
                        client_b.clone(),
                        second.id.clone(),
                        String::new(),
                        manager._access_lock.clone(),
                    )),
                );
            }
            manager.publish_active_account(Some(first.clone()));
            let previous = manager.account_selection(&first.id).unwrap();
            manager
                .set_app_password(None, Zeroizing::new("fixture app password".into()))
                .await
                .unwrap();
            assert!(matches!(
                manager.switch_to(&second.id).await,
                Err(MatrixCoreError::AppPasswordRequired)
            ));
            assert!(matches!(
                manager
                    .authorize_activation(
                        manager.begin_activation(&second.id).await.unwrap(),
                        Zeroizing::new("wrong fixture password".into())
                    )
                    .await,
                Err(MatrixCoreError::AppPasswordIncorrect)
            ));
            assert_eq!(manager.active_account().await, Some(first.clone()));
            let challenge = manager.begin_activation(&second.id).await.unwrap();
            let cancellation = challenge.cancellation();
            let grant = manager
                .authorize_activation(challenge, Zeroizing::new("fixture app password".into()))
                .await
                .unwrap();
            cancellation.cancel();
            assert!(matches!(
                manager.activate(grant).await,
                Err(MatrixCoreError::ActivationExpired)
            ));
            assert!(previous.is_current());
            let grant = manager
                .authorize_activation(
                    manager.begin_activation(&second.id).await.unwrap(),
                    Zeroizing::new("fixture app password".into()),
                )
                .await
                .unwrap();
            assert_eq!(manager.activate(grant).await.unwrap(), second.clone());
            assert!(!previous.is_current());
            assert!(!client_a.send_queue().is_enabled());
            // Never client-wide: the selected account is known by its
            // discovery readiness, not by a global queue flag.
            assert!(!client_b.send_queue().is_enabled());
            assert!(
                manager
                    .take_recovered_drafts(&manager.account_selection(&second.id).unwrap())
                    .await
                    .unwrap()
                    .is_empty()
            );
            let removal = manager.remove_account(&second.id).await.unwrap();
            assert!(removal.complete);
            assert!(manager.active_account().await.is_none());
            assert!(manager.subscribe_active_account().borrow().is_none());
            assert_eq!(manager.accounts().await, [first]);
            manager.stop().await;
        });
    }
}
