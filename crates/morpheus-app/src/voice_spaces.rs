//! Account-scoped voice-space presentation. The shared marker is a room state
//! hint; the MatrixRTC slot remains the source of truth for a call.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, Instant};

#[cfg(feature = "native-calls")]
use futures_util::{StreamExt, stream};
use morpheus_config::{Config, ConfigError, ConfigStore, RoomCallMode};
use morpheus_matrix::AccountManager;
#[cfg(feature = "native-calls")]
use morpheus_rtc::RtcService;
use tokio::sync::Mutex;

const MARKER_REFRESH: Duration = Duration::from_secs(300);
const FAILED_REFRESH: Duration = Duration::from_secs(45);

#[derive(Clone, Copy)]
struct CachedMarker {
    enabled: bool,
    checked_at: Instant,
    retry_after: Duration,
}

pub struct VoiceSpaces {
    store: ConfigStore,
    config: RwLock<Config>,
    shared: Mutex<HashMap<(String, String), CachedMarker>>,
    scan: Mutex<()>,
    #[cfg(feature = "native-calls")]
    policy_changed: tokio::sync::watch::Sender<()>,
    #[cfg(feature = "native-calls")]
    ring_policy_epoch: std::sync::atomic::AtomicU64,
    #[cfg(feature = "native-calls")]
    policy_invalidator: std::sync::Mutex<Option<IncomingPolicyCallback>>,
}

#[cfg(feature = "native-calls")]
type IncomingPolicyCallback = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Weak back-reference prevents a worker/config lifetime cycle. Releasing the
/// exact registration cannot remove a later worker's authorization hook.
#[cfg(feature = "native-calls")]
pub(crate) struct IncomingPolicyRegistration {
    spaces: std::sync::Weak<VoiceSpaces>,
    callback: IncomingPolicyCallback,
}
#[cfg(feature = "native-calls")]
impl Drop for IncomingPolicyRegistration {
    fn drop(&mut self) {
        if let Some(spaces) = self.spaces.upgrade() {
            let mut registered = spaces
                .policy_invalidator
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if registered
                .as_ref()
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &self.callback))
            {
                *registered = None;
            }
        }
    }
}

impl VoiceSpaces {
    pub fn new(store: ConfigStore) -> Self {
        let config = store.load().unwrap_or_default();
        Self {
            store,
            config: RwLock::new(config),
            shared: Mutex::new(HashMap::new()),
            scan: Mutex::new(()),
            #[cfg(feature = "native-calls")]
            policy_changed: tokio::sync::watch::Sender::new(()),
            #[cfg(feature = "native-calls")]
            ring_policy_epoch: std::sync::atomic::AtomicU64::new(1),
            #[cfg(feature = "native-calls")]
            policy_invalidator: std::sync::Mutex::new(None),
        }
    }

    pub fn apply_config(&self, config: &Config) {
        let mut current = self.config.write().expect("voice config lock poisoned");
        #[cfg(feature = "native-calls")]
        let policy_changed = incoming_policy_changed(&current, config);
        #[cfg(feature = "native-calls")]
        if policy_changed {
            // Serialize revocation before the new preferences become visible.
            // A delayed watch receiver cannot undo an A→B→A transition.
            if let Some(invalidate) = self
                .policy_invalidator
                .lock()
                .expect("policy hook lock poisoned")
                .as_ref()
            {
                invalidate();
            }
        }
        #[cfg(feature = "native-calls")]
        if incoming_policy_changed(&current, config)
            || current.notifications.sounds != config.notifications.sounds
            || current.media.speaker_device != config.media.speaker_device
        {
            let _ = self.ring_policy_epoch.fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| Some(n.saturating_add(1)),
            );
        }
        *current = config.clone();
        drop(current);
        #[cfg(feature = "native-calls")]
        if policy_changed {
            self.policy_changed.send_replace(());
        }
    }

    /// Install the one process incoming worker's authorization invalidator.
    /// It must only invalidate its own signal/presentation; it must never read
    /// or mutate VoiceSpaces, because it runs under the config write lock.
    /// A duplicate registration is rejected without replacing the first guard.
    #[cfg(feature = "native-calls")]
    pub(crate) fn watch_incoming_invalidation(
        self: &std::sync::Arc<Self>,
        callback: IncomingPolicyCallback,
    ) -> Option<IncomingPolicyRegistration> {
        let mut registered = self.policy_invalidator.lock().ok()?;
        if registered.is_some() {
            return None;
        }
        *registered = Some(callback.clone());
        Some(IncomingPolicyRegistration {
            spaces: std::sync::Arc::downgrade(self),
            callback,
        })
    }

    /// Subscribe before reading preferences. A wake revokes pending incoming
    /// offers even if a later edit restores the previous values.
    #[cfg(feature = "native-calls")]
    pub(crate) fn subscribe_incoming_policy(&self) -> tokio::sync::watch::Receiver<()> {
        self.policy_changed.subscribe()
    }

    #[cfg(feature = "native-calls")]
    pub fn notifications_enabled(&self, account_id: &str) -> bool {
        self.config
            .read()
            .expect("voice config lock poisoned")
            .notifications_enabled_for(account_id)
    }

    #[cfg(feature = "native-calls")]
    pub(crate) fn ringing_policy(&self, account: &str) -> (u64, bool, Option<String>) {
        let config = self.config.read().expect("voice config lock poisoned");
        let epoch = self
            .ring_policy_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        (
            epoch,
            epoch != u64::MAX
                && config.notifications_enabled_for(account)
                && config.notifications.sounds,
            config.media.speaker_device.clone(),
        )
    }
    #[cfg(feature = "native-calls")]
    pub(crate) fn ringing_policy_current(&self, epoch: u64) -> bool {
        epoch != u64::MAX
            && self
                .ring_policy_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                == epoch
    }

    pub fn local_rooms(&self, account_id: &str) -> HashSet<String> {
        let config = self.config.read().expect("voice config lock poisoned");
        config
            .account_overrides
            .get(account_id)
            .map(|overrides| overrides.local_voice_rooms.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn room_call_mode(&self, account_id: &str, room_id: &str) -> RoomCallMode {
        self.config
            .read()
            .expect("voice config lock poisoned")
            .room_call_mode(account_id, room_id)
    }

    pub fn set_room_call_mode(
        &self,
        account_id: &str,
        room_id: &str,
        mode: RoomCallMode,
    ) -> Result<(), ConfigError> {
        // Invalid external edits remain untouched. Only an atomic successful
        // save changes the preferences used by the next join.
        let mut config = self.store.load()?;
        if config.set_room_call_mode(account_id, room_id, mode)? {
            self.store.save(&config)?;
        }
        self.apply_config(&config);
        Ok(())
    }

    pub fn set_local(
        &self,
        account_id: &str,
        room_id: &str,
        enabled: bool,
    ) -> Result<bool, ConfigError> {
        let mut config = self.store.load()?;
        let changed = config.set_local_voice_room(account_id, room_id, enabled)?;
        if changed {
            self.store.save(&config)?;
        }
        self.apply_config(&config);
        Ok(changed)
    }

    pub fn clear_logged_out_account(&self, account_id: &str) -> Result<(), ConfigError> {
        let mut config = self.store.load()?;
        let changed =
            config.clear_local_voice_rooms(account_id) | config.clear_room_call_modes(account_id);
        if changed {
            self.store.save(&config)?;
        }
        self.apply_config(&config);
        Ok(())
    }

    pub async fn shared_rooms(&self, account_id: &str) -> HashSet<String> {
        self.shared
            .lock()
            .await
            .iter()
            .filter(|((account, _), marker)| account == account_id && marker.enabled)
            .map(|((_, room), _)| room.clone())
            .collect()
    }

    pub async fn forget_account(&self, account_id: &str) {
        self.shared
            .lock()
            .await
            .retain(|(account, _), _| account != account_id);
    }

    #[cfg(feature = "native-calls")]
    pub async fn set_shared(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
        enabled: bool,
    ) -> Result<(), String> {
        if manager
            .active_account()
            .await
            .is_none_or(|account| account.id != account_id)
        {
            return Err("The active Matrix account changed".into());
        }
        let client = manager
            .active_client()
            .await
            .map_err(|error| error.to_string())?;
        let rtc = RtcService::new(client);
        tokio::time::timeout(
            Duration::from_secs(15),
            rtc.set_voice_space_marker(room_id, enabled),
        )
        .await
        .map_err(|_| "Writing the shared voice-space marker timed out".to_owned())?
        .map_err(|error| error.to_string())?;
        self.shared.lock().await.insert(
            (account_id.to_owned(), room_id.to_owned()),
            CachedMarker {
                enabled,
                checked_at: Instant::now(),
                retry_after: MARKER_REFRESH,
            },
        );
        Ok(())
    }

    #[cfg(feature = "native-calls")]
    pub async fn scan_shared(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_ids: &[String],
    ) -> bool {
        // A refresh for the next account can arrive while the previous
        // account's marker reads are still finishing. Queue that refresh and
        // recheck the selected account after acquiring the lock; otherwise its
        // only discovery pass can be silently lost until another UI refresh.
        let _scan = self.scan.lock().await;
        if manager
            .active_account()
            .await
            .is_none_or(|account| account.id != account_id)
        {
            return false;
        }
        let now = Instant::now();
        let due = {
            let cache = self.shared.lock().await;
            room_ids
                .iter()
                .filter(|room_id| {
                    cache
                        .get(&(account_id.to_owned(), (*room_id).clone()))
                        .is_none_or(|marker| {
                            now.duration_since(marker.checked_at) >= marker.retry_after
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if due.is_empty() {
            return false;
        }
        let Ok(client) = manager.active_client().await else {
            return false;
        };
        let rtc = RtcService::new(client);
        let mut checks = stream::iter(due.into_iter().map(|room_id| {
            let rtc = &rtc;
            async move {
                let result =
                    tokio::time::timeout(Duration::from_secs(10), rtc.voice_space_marker(&room_id))
                        .await;
                (room_id, result)
            }
        }))
        .buffer_unordered(4);
        let mut changed = false;
        while let Some((room_id, result)) = checks.next().await {
            if manager
                .active_account()
                .await
                .is_none_or(|account| account.id != account_id)
            {
                break;
            }
            let (enabled, retry_after) = match result {
                Ok(Ok(marker)) => (marker.is_some_and(|marker| marker.enabled), MARKER_REFRESH),
                _ => (false, FAILED_REFRESH),
            };
            let key = (account_id.to_owned(), room_id);
            let mut cache = self.shared.lock().await;
            // A user action may have written a newer marker while this
            // background request was in flight. Keep the explicit result.
            if cache
                .get(&key)
                .is_some_and(|marker| marker.checked_at > now)
            {
                continue;
            }
            let prior = cache.get(&key).is_some_and(|marker| marker.enabled);
            if retry_after == MARKER_REFRESH && prior != enabled {
                changed = true;
            }
            cache.insert(
                key,
                CachedMarker {
                    enabled: if retry_after == MARKER_REFRESH {
                        enabled
                    } else {
                        prior
                    },
                    checked_at: Instant::now(),
                    retry_after,
                },
            );
        }
        changed
    }

    #[cfg(not(feature = "native-calls"))]
    pub async fn scan_shared(
        &self,
        _manager: &AccountManager,
        _account_id: &str,
        _room_ids: &[String],
    ) -> bool {
        false
    }
}

#[cfg(feature = "native-calls")]
fn incoming_policy_changed(before: &Config, after: &Config) -> bool {
    if before.notifications.enabled != after.notifications.enabled {
        return true;
    }
    before
        .account_overrides
        .keys()
        .chain(after.account_overrides.keys())
        .any(|id| {
            let old = before.account_overrides.get(id);
            let new = after.account_overrides.get(id);
            old.and_then(|value| value.notifications_enabled)
                != new.and_then(|value| value.notifications_enabled)
                || old
                    .map(|value| value.local_voice_rooms.as_slice())
                    .unwrap_or_default()
                    != new
                        .map(|value| value.local_voice_rooms.as_slice())
                        .unwrap_or_default()
        })
}

#[cfg(all(test, feature = "native-calls"))]
mod tests {
    use super::*;

    #[test]
    fn incoming_policy_wakes_for_quiet_or_notification_changes_only() {
        let directory = tempfile::tempdir().unwrap();
        let spaces = VoiceSpaces::new(ConfigStore::at(directory.path().join("config.toml")));
        let mut updates = spaces.subscribe_incoming_policy();
        let mut config = Config::default();
        spaces.apply_config(&config);
        assert!(!updates.has_changed().unwrap());
        config.appearance.scale_percent = 125;
        spaces.apply_config(&config);
        assert!(!updates.has_changed().unwrap());
        config.notifications.enabled = false;
        spaces.apply_config(&config);
        assert!(updates.has_changed().unwrap());
        updates.borrow_and_update();
        // A→B→A still emits a wake; a stale offer cannot reuse old admission.
        config.notifications.enabled = true;
        spaces.apply_config(&config);
        assert!(updates.has_changed().unwrap());
        updates.borrow_and_update();
        config
            .account_overrides
            .entry("account-a".into())
            .or_default()
            .local_voice_rooms
            .push("!quiet:example.org".into());
        spaces.apply_config(&config);
        assert!(updates.has_changed().unwrap());
        updates.borrow_and_update();
        config
            .account_overrides
            .get_mut("account-a")
            .unwrap()
            .local_voice_rooms
            .clear();
        spaces.apply_config(&config);
        assert!(updates.has_changed().unwrap());
    }

    #[test]
    fn policy_reversion_invalidates_synchronously_and_registration_is_owned() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let directory = tempfile::tempdir().unwrap();
        let spaces = Arc::new(VoiceSpaces::new(ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let weak = Arc::downgrade(&spaces);
        let registration = spaces
            .watch_incoming_invalidation(Arc::new(move || {
                let spaces = weak.upgrade().unwrap();
                assert!(
                    spaces.config.try_read().is_err(),
                    "hook must precede config publication"
                );
                seen.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap();
        assert!(
            spaces
                .watch_incoming_invalidation(Arc::new(|| panic!("duplicate hook installed")))
                .is_none()
        );
        let mut config = Config::default();
        spaces.apply_config(&config);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        config.notifications.enabled = false;
        spaces.apply_config(&config);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        config.notifications.enabled = true;
        spaces.apply_config(&config);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "no watcher poll was required"
        );
        drop(registration);
        config.notifications.enabled = false;
        spaces.apply_config(&config);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            spaces
                .watch_incoming_invalidation(Arc::new(|| {}))
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_new_scan_waits_for_the_previous_account_scan() {
        let directory = tempfile::tempdir().expect("temporary account directory");
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let spaces = std::sync::Arc::new(VoiceSpaces::new(store));
        let manager = std::sync::Arc::new(
            AccountManager::open(directory.path().join("accounts")).expect("account manager"),
        );
        let previous_scan = spaces.scan.lock().await;
        let (started, ready) = tokio::sync::oneshot::channel();
        let next_spaces = spaces.clone();
        let next_manager = manager.clone();
        let mut next_scan = tokio::spawn(async move {
            let _ = started.send(());
            next_spaces
                .scan_shared(&next_manager, "account-b", &[])
                .await
        });
        ready.await.expect("next scan did not start");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut next_scan)
                .await
                .is_err(),
            "next account's scan must wait instead of being dropped"
        );
        drop(previous_scan);
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), next_scan)
                .await
                .expect("queued scan did not finish")
                .expect("queued scan panicked")
        );
    }
    #[test]
    fn sound_policy_off_on_revokes_old_audio_without_changing_visible_offer_policy() {
        let dir = tempfile::tempdir().unwrap();
        let spaces = VoiceSpaces::new(ConfigStore::at(dir.path().join("config.toml")));
        let mut config = Config::default();
        spaces.apply_config(&config);
        let (epoch, enabled, _) = spaces.ringing_policy("account");
        assert!(enabled);
        config.notifications.sounds = false;
        spaces.apply_config(&config);
        assert!(!spaces.ringing_policy_current(epoch));
        assert!(!spaces.ringing_policy("account").1);
        config.notifications.sounds = true;
        spaces.apply_config(&config);
        assert!(!spaces.ringing_policy_current(epoch));
        assert!(spaces.ringing_policy("account").1);
    }
    #[test]
    fn audio_policy_honors_account_override_and_output_changes_revoke_old_ring() {
        let dir = tempfile::tempdir().unwrap();
        let spaces = VoiceSpaces::new(ConfigStore::at(dir.path().join("config.toml")));
        let mut config = Config::default();
        config
            .account_overrides
            .entry("account".into())
            .or_default()
            .notifications_enabled = Some(false);
        spaces.apply_config(&config);
        assert!(!spaces.ringing_policy("account").1);
        assert!(spaces.ringing_policy("other-account").1);
        let mut notifications = spaces.subscribe_incoming_policy();
        notifications.borrow_and_update();
        let epoch = spaces.ringing_policy("other-account").0;
        config.media.speaker_device = Some("wasapi:test-output".into());
        spaces.apply_config(&config);
        assert!(!spaces.ringing_policy_current(epoch));
        assert_eq!(
            spaces.ringing_policy("other-account").2.as_deref(),
            Some("wasapi:test-output")
        );
        assert!(!notifications.has_changed().unwrap());
    }
}
