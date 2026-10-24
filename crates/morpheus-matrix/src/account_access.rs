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

/// One already-authorized local removal, never a source of new delete authority.
/// The readiness/token drain and vault/store erasure have distinct receipts.
pub(super) struct RemovalOwner {
    account_id: String,
    work: RemovalWork,
    pending: Arc<AtomicBool>,
}

enum RemovalWork {
    Draining(JoinHandle<()>),
    Deleting(JoinHandle<(bool, bool)>),
    Finished(RemovalReceipt),
}

#[derive(Clone, Copy)]
pub(super) enum RemovalReceipt {
    Drained,
    Deleted { credentials: bool, store: bool },
    Failed,
}

impl RemovalOwner {
    fn poll_completion(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<RemovalReceipt> {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::Poll;
        let receipt = match &mut self.work {
            RemovalWork::Draining(task) => match Pin::new(task).poll(cx) {
                Poll::Ready(Ok(())) => RemovalReceipt::Drained,
                Poll::Ready(Err(error)) => failed_removal(error),
                Poll::Pending => return Poll::Pending,
            },
            RemovalWork::Deleting(task) => match Pin::new(task).poll(cx) {
                Poll::Ready(Ok((credentials, store))) => {
                    RemovalReceipt::Deleted { credentials, store }
                }
                Poll::Ready(Err(error)) => failed_removal(error),
                Poll::Pending => return Poll::Pending,
            },
            RemovalWork::Finished(receipt) => return Poll::Ready(*receipt),
        };
        // The real JoinHandle output, including task destructors, has been
        // observed. No fallible drop/await precedes caching this exact receipt.
        self.work = RemovalWork::Finished(receipt);
        self.pending.store(false, Ordering::Release);
        Poll::Ready(receipt)
    }

    pub(super) async fn observe(&mut self) -> RemovalReceipt {
        std::future::poll_fn(|cx| self.poll_completion(cx)).await
    }
}

fn failed_removal(error: tokio::task::JoinError) -> RemovalReceipt {
    retain_join_panic(error);
    RemovalReceipt::Failed
}

pub(super) fn retain_join_panic(error: tokio::task::JoinError) {
    // A contribution's panic payload may itself panic on Drop. It must never
    // unwind between consuming a task output and recording its joined receipt.
    if let Ok(payload) = error.try_into_panic() {
        std::mem::forget(payload);
    }
}

impl Drop for RemovalOwner {
    fn drop(&mut self) {
        if !matches!(self.work, RemovalWork::Finished(_)) {
            // A manager abandoned without stop has no clean shutdown receipt.
            // Retain this exact task, its permit and profile lock rather than
            // detach/abort it. This terminal retention is not a retryable drain.
            let work = std::mem::replace(
                &mut self.work,
                RemovalWork::Finished(RemovalReceipt::Failed),
            );
            std::mem::forget(work);
        }
    }
}

fn incomplete_removal(id: &str) -> RemovalResult {
    RemovalResult {
        account_id: id.into(),
        credentials_removed: false,
        store_removed: false,
        complete: false,
    }
}
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
    /// Includes OS workers after a read timeout and retained removal tasks
    /// until their actual join is observed. This is refusal, not a completion
    /// receipt; updaters must not restart while it is true.
    pub fn account_access_busy(&self) -> bool {
        self.observe_ready_removal();
        self.removal_pending.load(Ordering::Acquire) || self.access_worker.available_permits() == 0
    }

    fn observe_ready_removal(&self) {
        if !self.removal_pending.load(Ordering::Acquire) {
            return;
        }
        // UI/updater refusal queries never block, advance a provider future,
        // write a registry, or start erasure. Only the actual retained task's
        // Ready output can retire the admission bit. An active async waiter
        // holds State, so this cannot replace its live waker with the noop one.
        if let Ok(mut state) = self.state.try_lock()
            && let Some(owner) = state.removal_owner.as_mut()
        {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = owner.poll_completion(&mut cx);
        }
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
        self.observe_ready_removal();
        if self.removal_pending.load(Ordering::Acquire) {
            return Err(MatrixCoreError::AccountAccessBusy);
        }
        let worker = self
            .access_worker
            .clone()
            .try_acquire_owned()
            .map_err(|_| MatrixCoreError::AccountAccessBusy)?;
        if self.removal_pending.load(Ordering::Acquire) {
            return Err(MatrixCoreError::AccountAccessBusy);
        }
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
        // Preserve the existing prompt Busy result for unrelated access work.
        // Only a retained removal may be observed without a new permit.
        self.observe_ready_removal();
        let permit = if self.removal_pending.load(Ordering::Acquire) {
            None
        } else {
            Some(self.access_permit()?)
        };
        let mut state = self.state.lock().await;
        let mut drained = false;
        if let Some(owner) = state.removal_owner.as_ref()
            && owner.account_id != id
        {
            let RemovalWork::Finished(receipt) = &owner.work else {
                return Err(MatrixCoreError::AccountAccessBusy);
            };
            let receipt = *receipt;
            let original = owner.account_id.clone();
            // This is only an already-joined A result. Settle A's authorized
            // erasure, or retain its pending row/loaded owner after drain or
            // failure. B's action never starts/retries any A deletion.
            self.apply_removal_receipt(&mut state, &original, receipt)?;
            state.removal_owner.take();
        }
        if let Some(owner) = state.removal_owner.as_mut() {
            let receipt = match tokio::time::timeout(CLEANUP_TIMEOUT, owner.observe()).await {
                Ok(receipt) => receipt,
                Err(_) => return Ok(incomplete_removal(id)),
            };
            if !matches!(receipt, RemovalReceipt::Drained) {
                return self.apply_removal_receipt(&mut state, id, receipt);
            }
            // Only this explicit remove request may advance a completed drain
            // to erasure. A completion-only waiter never enters this path.
            state.removal_owner.take();
            drained = true;
        }
        let permit = match permit {
            Some(permit) => permit,
            None => self.access_permit()?,
        };
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
            // Direct local removal bypasses stop_sync. Publish the same stop
            // intent before awaiting discovery, and keep its original owner
            // until the actual join before queues/stores can be retired.
            if let Some(sync) = state.sync_task.as_mut() {
                sync.request_stop();
            }
            stop_recovery_scan(&mut state.recovery_scan_task).await;
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
        if !drained && let Some(loaded) = state.loaded.get(id).cloned() {
            let account_id = id.to_owned();
            let drain_permit = permit.clone();
            self.removal_pending.store(true, Ordering::Release);
            let task = tokio::spawn(async move {
                let _permit = drain_permit;
                loaded.stop_owned_tasks().await;
            });
            state.removal_owner = Some(RemovalOwner {
                account_id,
                work: RemovalWork::Draining(task),
                pending: self.removal_pending.clone(),
            });
            let owner = state.removal_owner.as_mut().expect("removal drain owner");
            let receipt = match tokio::time::timeout(CLEANUP_TIMEOUT, owner.observe()).await {
                Ok(receipt) => receipt,
                Err(_) => return Ok(incomplete_removal(id)),
            };
            if !matches!(receipt, RemovalReceipt::Drained) {
                return self.apply_removal_receipt(&mut state, id, receipt);
            }
            state.removal_owner.take();
        }
        state.loaded.remove(id);
        let account_id = id.to_owned();
        let owner_id = account_id.clone();
        let account_dir = self.data_dir.join("accounts").join(id);
        self.removal_pending.store(true, Ordering::Release);
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
        // No await between native task creation and publishing its sole owner.
        state.removal_owner = Some(RemovalOwner {
            account_id: owner_id,
            work: RemovalWork::Deleting(worker),
            pending: self.removal_pending.clone(),
        });
        let owner = state.removal_owner.as_mut().expect("removal erasure owner");
        match tokio::time::timeout(CLEANUP_TIMEOUT, owner.observe()).await {
            Ok(receipt) => self.apply_removal_receipt(&mut state, id, receipt),
            Err(_) => Ok(incomplete_removal(id)),
        }
    }

    /// Observe only an already-started removal for this exact account. This
    /// never starts/retries a vault operation or deletes a store. A completed
    /// readiness/token drain is still incomplete; a later explicit remove is
    /// needed to authorize erasure. Caller cancellation retains the real task.
    pub async fn finish_pending_account_removal(&self, id: &str) -> Result<RemovalResult> {
        let mut state = self.state.lock().await;
        let owner = state
            .removal_owner
            .as_mut()
            .ok_or(MatrixCoreError::AccountRemovalPending)?;
        if owner.account_id != id {
            return Err(MatrixCoreError::AccountAccessBusy);
        }
        let receipt = owner.observe().await;
        self.apply_removal_receipt(&mut state, id, receipt)
    }

    fn apply_removal_receipt(
        &self,
        state: &mut State,
        id: &str,
        receipt: RemovalReceipt,
    ) -> Result<RemovalResult> {
        if !state
            .registry
            .pending_removals
            .iter()
            .any(|pending| pending == id)
            || !state
                .registry
                .accounts
                .iter()
                .any(|account| account.id == id)
        {
            return Err(MatrixCoreError::AccountRemovalPending);
        }
        let (credentials_removed, store_removed) = match receipt {
            RemovalReceipt::Drained => return Ok(incomplete_removal(id)),
            RemovalReceipt::Deleted { credentials, store } => (credentials, store),
            RemovalReceipt::Failed => (false, false),
        };
        let complete = credentials_removed && store_removed;
        if complete {
            let mut next = state.registry.clone();
            next.accounts.retain(|account| account.id != id);
            next.pending_removals.retain(|pending| pending != id);
            save_registry(&self.data_dir, &next)?;
            state.registry = next;
        }
        // A failed final registry save leaves the observed result reachable;
        // retry then commits that result without another vault/store deletion.
        state.removal_owner.take();
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
    use std::future::Future;
    use std::sync::{Mutex as StdMutex, OnceLock};

    // Exercise the exact fixture adapter against this module's real removal
    // owners, including cancellation, drain-only and failed-erasure receipts.
    mod fixture_removal {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/account_removal.rs"
        ));
    }

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
        before_delete: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
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
            let before_delete = self.before_delete.lock().unwrap().take();
            if let Some(before_delete) = before_delete {
                before_delete();
            }
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
                before_delete: StdMutex::new(None),
            }),
        );
        backing
    }
    fn controlled_removal_credential(
        id: &str,
        before_delete: impl FnOnce() + Send + 'static,
    ) -> Arc<keyring::mock::MockCredential> {
        let backing = Arc::new(keyring::mock::MockCredential::default());
        overrides().lock().unwrap().insert(
            (KEYRING_SERVICE.into(), id.into()),
            Arc::new(ControlledCredential {
                backing: backing.clone(),
                before_write: StdMutex::new(None),
                before_delete: StdMutex::new(Some(Box::new(before_delete))),
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
    fn draft_fixture_observes_the_original_erasure_after_cancel_without_retrying_logout() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let other = account();
            let path = directory.0.join("accounts").join(&saved.id);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("keep"), b"owned draft cleanup fixture").unwrap();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![saved.clone(), other.clone()];
                save_registry(&directory.0, &state.registry).unwrap();
            }
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let (entered, waiting) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let credential = controlled_removal_credential(&saved.id, move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = entered.send(());
                let _ = held.recv();
            });
            // Deliberately not a stored session: remote logout cannot use it.
            // The actual credential deletion is still the production worker.
            credential
                .set_password("owned draft cleanup fixture")
                .unwrap();
            let observations = async {
                let logout =
                    tokio::time::timeout(Duration::from_secs(5), manager.logout_account(&saved.id))
                        .await
                        .map_err(|_| "logout did not return its bounded result")?;
                let was_pending = matches!(&logout, Err(MatrixCoreError::AccountRemovalPending));
                tokio::time::timeout(Duration::from_secs(2), waiting)
                    .await
                    .map_err(|_| "native deletion did not enter")?
                    .map_err(|_| "native deletion observation closed")?;
                let mut completion = Box::pin(fixture_removal::finish_logout(
                    &manager, &saved.id, &path, logout,
                ));
                let was_waiting = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(completion.as_mut().poll(cx).is_pending())
                })
                .await;
                drop(completion);
                let original_owner =
                    manager
                        .state
                        .lock()
                        .await
                        .removal_owner
                        .as_ref()
                        .map(|owner| match &owner.work {
                            RemovalWork::Deleting(task) => Some(task.id()),
                            _ => None,
                        });
                Ok::<_, &'static str>((was_pending, was_waiting, original_owner.flatten()))
            }
            .await;
            // Release and really join before any observation can assert/panic.
            let released = release.send(()).is_ok();
            let completion = fixture_removal::finish_logout(
                &manager,
                &saved.id,
                &path,
                Err(MatrixCoreError::AccountRemovalPending),
            )
            .await;
            manager.stop().await;
            let accounts = manager.accounts().await;
            let pending = manager.app_password_status().await.pending_removals;
            let gone =
                !path.exists() && matches!(credential.get_secret(), Err(keyring::Error::NoEntry));
            let (was_pending, was_waiting, original_owner) = observations.unwrap();
            assert!(was_pending && was_waiting && original_owner.is_some() && released);
            completion.expect("the same actual erasure must finish");
            assert!(gone && pending.is_empty());
            assert_eq!(accounts, [other]);
            assert_eq!(calls.load(Ordering::SeqCst), 1, "no deletion retry");
        });
    }

    #[test]
    fn pending_erasure_retains_the_same_worker_through_cancel_retry_and_stop() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let other = account();
            let path = directory.0.join("accounts").join(&saved.id);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("keep"), b"owned cleanup fixture").unwrap();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![saved.clone(), other.clone()];
                save_registry(&directory.0, &state.registry).unwrap();
            }
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let (entered, waiting) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let credential = controlled_removal_credential(&saved.id, move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = entered.send(());
                // Disconnect also releases the fixture during unwinding. The
                // normal path explicitly releases and joins before asserting.
                let _ = held.recv();
            });
            credential.set_password("owned cleanup fixture").unwrap();
            let observations = async {
                let first =
                    tokio::time::timeout(Duration::from_secs(5), manager.remove_account(&saved.id))
                        .await
                        .map_err(|_| "removal did not return its bounded pending result")?
                        .map_err(|_| "initial removal failed before observation")?;
                tokio::time::timeout(Duration::from_secs(2), waiting)
                    .await
                    .map_err(|_| "native removal did not enter")?
                    .map_err(|_| "native removal observation closed")?;
                let mut waiter = Box::pin(manager.finish_pending_account_removal(&saved.id));
                let waiter_pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(waiter.as_mut().poll(cx).is_pending())
                })
                .await;
                drop(waiter);
                let mut retry = Box::pin(manager.remove_account(&saved.id));
                let retry_pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(retry.as_mut().poll(cx).is_pending())
                })
                .await;
                drop(retry);
                let foreign_refused = matches!(
                    manager.remove_account(&other.id).await,
                    Err(MatrixCoreError::AccountAccessBusy)
                );
                let still_blocked = manager
                    .app_password_status()
                    .await
                    .pending_removals
                    .contains(&saved.id);
                Ok::<_, &'static str>((
                    first,
                    waiter_pending,
                    retry_pending,
                    foreign_refused,
                    still_blocked,
                ))
            }
            .await;
            let mut stopping = Box::pin(manager.stop());
            let stop_pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(stopping.as_mut().poll(cx).is_pending())
            })
            .await;
            let released = release.send(()).is_ok();
            let stopped = tokio::time::timeout(Duration::from_secs(5), stopping).await;
            if stopped.is_err() {
                // There is no joined receipt. Keep both the actual manager
                // owner and its profile; never repair the test by deleting it.
                std::mem::forget(manager);
                std::mem::forget(directory);
                panic!("owned removal shutdown did not join");
            }
            let completion = manager.finish_pending_account_removal(&saved.id).await;
            let remaining = manager.accounts().await;
            let store_removed = !path.exists();
            let credential_removed =
                matches!(credential.get_secret(), Err(keyring::Error::NoEntry));
            let (first, waiter_pending, retry_pending, foreign_refused, still_blocked) =
                observations.expect("held removal observations failed");
            assert!(!first.complete && !first.credentials_removed && !first.store_removed);
            assert!(waiter_pending && retry_pending && foreign_refused && still_blocked);
            assert!(stop_pending && released);
            let completion = completion.expect("actual erasure receipt missing");
            assert!(
                completion.complete && completion.credentials_removed && completion.store_removed
            );
            assert_eq!(completion.account_id, saved.id);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "retry must observe the original worker"
            );
            assert!(store_removed && credential_removed);
            assert_eq!(remaining, [other]);
        });
    }

    #[test]
    fn cancelled_token_drain_is_retained_and_its_receipt_never_authorizes_erasure() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let other = account();
            let other_path = directory.0.join("accounts").join(&other.id);
            fs::create_dir(&other_path).unwrap();
            vault_entry(KEYRING_SERVICE, &other.id)
                .set_password("owned other fixture")
                .unwrap();
            let path = directory.0.join("accounts").join(&saved.id);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("keep"), b"owned drain fixture").unwrap();
            let client = Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            let loaded = Arc::new(LoadedAccount::new(
                client,
                saved.id.clone(),
                String::new(),
                manager._access_lock.clone(),
            ));
            loaded.stop_owned_tasks().await;
            let (release, held) = oneshot::channel();
            let lock = manager._access_lock.clone();
            let task = tokio::spawn(async move {
                let _lock = lock;
                let _ = held.await;
            });
            *loaded.token_task.lock().await = Some(SyncTask {
                shutdown: None,
                task,
            });
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![saved.clone(), other.clone()];
                state.loaded.insert(saved.id.clone(), loaded.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let credential = controlled_removal_credential(&saved.id, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            });
            credential.set_password("owned drain fixture").unwrap();
            let observations = async {
                let mut token_wait = Box::pin(loaded.stop_token_persistence());
                let token_pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(token_wait.as_mut().poll(cx).is_pending())
                })
                .await;
                drop(token_wait);
                let token_still_owned = loaded.token_task.lock().await.is_some();
                let removal =
                    tokio::time::timeout(Duration::from_secs(5), manager.remove_account(&saved.id))
                        .await
                        .map_err(|_| "drain did not return its bounded pending result")?
                        .map_err(|_| "drain failed before observation")?;
                let mut waiter = Box::pin(manager.finish_pending_account_removal(&saved.id));
                let waiter_pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(waiter.as_mut().poll(cx).is_pending())
                })
                .await;
                drop(waiter);
                Ok::<_, &'static str>((token_pending, token_still_owned, removal, waiter_pending))
            }
            .await;
            let released = release.send(()).is_ok();
            let completion = tokio::time::timeout(
                Duration::from_secs(5),
                manager.finish_pending_account_removal(&saved.id),
            )
            .await;
            if completion.is_err() {
                std::mem::forget(loaded);
                std::mem::forget(manager);
                std::mem::forget(directory);
                panic!("owned token drain did not join");
            }
            let completion = completion.unwrap();
            let fixture_completion = fixture_removal::finish_logout(
                &manager,
                &saved.id,
                &path,
                Err(MatrixCoreError::AccountRemovalPending),
            )
            .await;
            let credential_preserved = credential.get_secret().is_ok();
            let store_preserved = path.join("keep").is_file();
            let no_delete = calls.load(Ordering::SeqCst) == 0;
            let token_joined = loaded.token_task.lock().await.is_none();
            let unrelated = manager.remove_account(&other.id).await;
            let original_preserved_after_b = credential.get_secret().is_ok()
                && path.join("keep").is_file()
                && calls.load(Ordering::SeqCst) == 0
                && manager
                    .app_password_status()
                    .await
                    .pending_removals
                    .contains(&saved.id)
                && manager.state.lock().await.loaded.contains_key(&saved.id);
            let explicit_removal = manager.remove_account(&saved.id).await;
            manager.stop().await;
            let (token_pending, token_still_owned, first, waiter_pending) =
                observations.expect("held token observations failed");
            assert!(released && token_pending && token_still_owned && waiter_pending);
            assert!(!first.complete && !first.credentials_removed && !first.store_removed);
            let completion = completion.expect("actual drain receipt missing");
            assert!(
                !completion.complete
                    && !completion.credentials_removed
                    && !completion.store_removed
            );
            assert!(credential_preserved && store_preserved && no_delete && token_joined);
            assert_eq!(fixture_completion, Err("account erasure incomplete"));
            assert!(
                unrelated
                    .expect("unrelated removal after an actual drain failed")
                    .complete
            );
            assert!(original_preserved_after_b);
            assert!(
                explicit_removal
                    .expect("explicit post-drain removal failed")
                    .complete
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn busy_query_joins_completed_erasure_without_runtime_or_foreign_account_lockout() {
        let runtime = runtime();
        let directory = TestDirectory::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let saved = account();
        let other = account();
        for account in [&saved, &other] {
            fs::create_dir(directory.0.join("accounts").join(&account.id)).unwrap();
        }
        runtime.block_on(async {
            let mut state = manager.state.lock().await;
            state.registry.accounts = vec![saved.clone(), other.clone()];
            save_registry(&directory.0, &state.registry).unwrap();
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let (entered, waiting) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let credential = controlled_removal_credential(&saved.id, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = entered.send(());
            let _ = held.recv();
        });
        credential.set_password("owned busy query fixture").unwrap();
        vault_entry(KEYRING_SERVICE, &other.id)
            .set_password("owned unrelated fixture")
            .unwrap();
        let observations = runtime.block_on(async {
            let pending =
                tokio::time::timeout(Duration::from_secs(5), manager.remove_account(&saved.id))
                    .await
                    .map_err(|_| "initial erasure did not return its bounded result")?
                    .map_err(|_| "initial erasure failed")?;
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .map_err(|_| "owned provider did not enter")?
                .map_err(|_| "owned provider observation closed")?;
            Ok::<_, &'static str>(pending)
        });
        let before_release_busy = manager.account_access_busy();
        let released = release.send(()).is_ok();
        // This is the synchronous production UI/updater query, outside every
        // runtime context. Every iteration polls the actual retained handle;
        // the later typed receipt is mandatory, never a time/count inference.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut joined_by_query = false;
        while Instant::now() < deadline {
            if !manager.account_access_busy() {
                joined_by_query = true;
                break;
            }
            std::thread::yield_now();
        }
        let actual_receipt = runtime.block_on(async {
            let state = manager.state.lock().await;
            state.removal_owner.as_ref().is_some_and(|owner| {
                owner.account_id == saved.id
                    && matches!(
                        owner.work,
                        RemovalWork::Finished(RemovalReceipt::Deleted {
                            credentials: true,
                            store: true
                        })
                    )
            })
        });
        let unrelated = runtime.block_on(manager.remove_account(&other.id));
        let stopped = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), manager.stop()).await });
        if stopped.is_err() {
            std::mem::forget(manager);
            std::mem::forget(directory);
            std::mem::forget(runtime);
            panic!("owned busy-query cleanup did not join");
        }
        let (accounts, pending) = runtime.block_on(async {
            (
                manager.accounts().await,
                manager.app_password_status().await.pending_removals,
            )
        });
        let removed = !directory.0.join("accounts").join(&saved.id).exists()
            && !directory.0.join("accounts").join(&other.id).exists()
            && matches!(credential.get_secret(), Err(keyring::Error::NoEntry));
        let first = observations.expect("owned busy-query observations failed");
        assert!(!first.complete);
        assert!(before_release_busy && released && joined_by_query && actual_receipt);
        assert!(
            unrelated
                .expect("joined foreign receipt must not block unrelated removal")
                .complete
        );
        assert!(removed && accounts.is_empty() && pending.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panicked_erasure_stays_incomplete_and_completion_cannot_retry_it() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let path = directory.0.join("accounts").join(&saved.id);
            fs::create_dir(&path).unwrap();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(saved.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            struct PanicOnDrop(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for PanicOnDrop {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    panic!("owned panic payload must be retained");
                }
            }
            let payload_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = payload_drops.clone();
            let credential = controlled_removal_credential(&saved.id, move || {
                std::panic::panic_any(PanicOnDrop(counted));
            });
            credential.set_password("owned failure fixture").unwrap();
            let failed = manager.remove_account(&saved.id).await;
            let completion = manager.finish_pending_account_removal(&saved.id).await;
            let fixture_completion = fixture_removal::finish_logout(
                &manager,
                &saved.id,
                &path,
                Err(MatrixCoreError::AccountRemovalPending),
            )
            .await;
            let blocked = manager
                .app_password_status()
                .await
                .pending_removals
                .contains(&saved.id);
            let preserved = path.is_dir() && credential.get_secret().is_ok();
            // The failed worker has actually joined. Clean the disposable
            // fixture only through a separately requested removal.
            let explicit = manager.remove_account(&saved.id).await;
            manager.stop().await;
            let failed = failed.expect("panic must have a fixed incomplete result");
            assert!(!failed.complete && !failed.credentials_removed && !failed.store_removed);
            assert!(matches!(
                completion,
                Err(MatrixCoreError::AccountRemovalPending)
            ));
            assert!(blocked && preserved);
            assert_eq!(
                fixture_completion,
                Err("account removal observation failed")
            );
            assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
            assert!(
                explicit
                    .expect("explicit removal after a joined failure failed")
                    .complete
            );
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
    fn direct_removal_retains_discovery_until_its_actual_join() {
        runtime().block_on(async {
            let directory = TestDirectory::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let saved = account();
            let other = account();
            let path = directory.0.join("accounts").join(&saved.id);
            let other_path = directory.0.join("accounts").join(&other.id);
            fs::create_dir(&path).unwrap();
            fs::create_dir(&other_path).unwrap();
            fs::write(path.join("keep"), b"discovery removal fixture").unwrap();
            fs::write(other_path.join("keep"), b"other account fixture").unwrap();
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts = vec![saved.clone(), other.clone()];
                state.registry.active_account_id = Some(saved.id.clone());
                state.active_account_id = Some(saved.id.clone());
                save_registry(&directory.0, &state.registry).unwrap();
            }
            manager.publish_active_account(Some(saved.clone()));
            let selection = manager.account_selection(&saved.id).unwrap();
            let gate = Arc::new(Mutex::new(RecoveryGate::default()));
            let dropped = Arc::new(AtomicBool::new(false));
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let after_join = Arc::new(AtomicBool::new(false));
            let counted = calls.clone();
            let observed_drop = dropped.clone();
            let observed_gate = gate.clone();
            let observed_order = after_join.clone();
            let credential = controlled_removal_credential(&saved.id, move || {
                counted.fetch_add(1, Ordering::SeqCst);
                observed_order.store(
                    observed_drop.load(Ordering::Acquire) && observed_gate.try_lock().is_ok(),
                    Ordering::Release,
                );
            });
            credential
                .set_password("discovery removal fixture")
                .unwrap();
            struct ScanDrop(Arc<AtomicBool>);
            impl Drop for ScanDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let worker_gate = gate.clone();
            let worker_drop = dropped.clone();
            let (entered, started) = oneshot::channel();
            let scan = tokio::spawn(async move {
                let _coverage = worker_gate.lock().await;
                let _drop = ScanDrop(worker_drop);
                let _ = entered.send(());
                std::future::pending::<()>().await;
            });
            let original = scan.id();
            let entered = started.await.is_ok();
            let (shutdown, stopped) = oneshot::channel();
            {
                let mut state = manager.state.lock().await;
                state.recovery_scan_task = Some(scan);
                state.sync_task = Some(SyncTask {
                    shutdown: Some(shutdown),
                    task: tokio::spawn(async move {
                        let _ = stopped.await;
                    }),
                });
            }
            let mut first = Box::pin(manager.remove_account(&saved.id));
            let was_pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(first.as_mut().poll(cx).is_pending())
            })
            .await;
            drop(first);
            let (retained, stop_requested) = {
                let state = manager.state.lock().await;
                (
                    state.recovery_scan_task.as_ref().map(JoinHandle::id),
                    state
                        .sync_task
                        .as_ref()
                        .is_some_and(|task| task.shutdown.is_none()),
                )
            };
            let held_before_join = gate.try_lock().is_err();
            // Resume the explicit local removal. Only the original discovery
            // is joined; completion observation never retries native erasure.
            let completion = match manager.remove_account(&saved.id).await {
                Ok(removal) if removal.complete => Ok(removal),
                Ok(_) => manager.finish_pending_account_removal(&saved.id).await,
                Err(error) => Err(error),
            };
            manager.stop().await;
            let scan_retired = manager.state.lock().await.recovery_scan_task.is_none();
            let accounts = manager.accounts().await;
            let pending = manager.app_password_status().await.pending_removals;
            let store_gone = matches!(path.try_exists(), Ok(false));
            let credential_gone = matches!(credential.get_secret(), Err(keyring::Error::NoEntry));
            let other_store = fs::read(other_path.join("keep"));
            // Observe all original async/native handles before assertions.
            assert!(entered && was_pending && stop_requested && held_before_join);
            assert_eq!(retained, Some(original));
            assert!(!selection.is_current());
            let completion = completion.expect("the original local removal must complete");
            assert!(
                completion.complete && completion.credentials_removed && completion.store_removed
            );
            assert!(scan_retired && after_join.load(Ordering::Acquire));
            assert!(store_gone && credential_gone && pending.is_empty());
            assert_eq!(accounts, [other]);
            assert_eq!(other_store.unwrap(), b"other account fixture");
            assert_eq!(calls.load(Ordering::SeqCst), 1, "one native erasure");
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
            let e2ee = crate::account_recovery::E2eeReadiness::of(&client);
            let loaded = Arc::new(LoadedAccount {
                client: client.clone(),
                e2ee,
                timelines: Mutex::new(HashMap::new()),
                composed_edits: Mutex::new(HashMap::new()),
                pending_sends: Mutex::new(HashMap::new()),
                originals: Mutex::new(HashMap::new()),
                recovered: Mutex::new(Vec::new()),
                recovery_gate: Mutex::new(RecoveryGate::default()),
                recovery_ready: watch::channel(false).0,
                incoming_verification_tx,
                verification_book: Default::default(),
                peer_verifications: Default::default(),
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
