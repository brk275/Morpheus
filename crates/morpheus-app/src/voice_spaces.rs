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
}

impl VoiceSpaces {
    pub fn new(store: ConfigStore) -> Self {
        let config = store.load().unwrap_or_default();
        Self {
            store,
            config: RwLock::new(config),
            shared: Mutex::new(HashMap::new()),
            scan: Mutex::new(()),
        }
    }

    pub fn apply_config(&self, config: &Config) {
        *self.config.write().expect("voice config lock poisoned") = config.clone();
    }

    #[cfg(feature = "native-calls")]
    pub fn notifications_enabled(&self, account_id: &str) -> bool {
        self.config
            .read()
            .expect("voice config lock poisoned")
            .notifications_enabled_for(account_id)
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

#[cfg(all(test, feature = "native-calls"))]
mod tests {
    use super::*;

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
}
