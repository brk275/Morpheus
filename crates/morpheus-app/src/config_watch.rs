//! The ordinary desktop config producer, UI bridge and refresh task are one
//! shutdown owner. Cancellation revokes queued callbacks before any UI read;
//! completion observes the producer and bridge's actual native joins.

use crate::{AppWindow, LiveUi, apply_loaded_config, apply_notification_settings};
use morpheus_config::{ConfigError, ConfigStore, LoadedConfig, OwnedWatch, WatchStop};
use slint::ComponentHandle;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigWatchError {
    Start,
    Producer,
    Bridge,
    Refresh,
    Admission,
    Ownership,
}

impl std::fmt::Display for ConfigWatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Configuration watcher shutdown could not be confirmed.")
    }
}
impl std::error::Error for ConfigWatchError {}

struct State {
    closed: bool,
    failed: bool,
    queued: bool,
    producer: Option<WatchStop>,
    refresh: Option<mpsc::Sender<()>>,
}

struct Shared(Mutex<State>);

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.failed = true;
            state.closed = true;
            state.refresh.take();
            if let Some(stop) = &state.producer {
                stop.stop();
            }
            state
        })
    }

    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        state.refresh.take();
        if let Some(stop) = &state.producer {
            stop.stop();
        }
    }

    fn queue_callback(&self) -> bool {
        let mut state = self.state();
        if state.closed || state.queued {
            return false;
        }
        state.queued = true;
        true
    }

    fn enter_callback(&self) -> bool {
        let mut state = self.state();
        state.queued = false;
        !state.closed
    }

    fn request_refresh(&self) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        // The one retained worker already exists. Admission and publication
        // share the close mutex; no check-then-spawn window creates new work.
        if let Some(sender) = &state.refresh {
            match sender.try_send(()) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                Err(mpsc::error::TrySendError::Closed(())) => state.failed = true,
            }
        }
    }
}

type BridgeSlot = Arc<Mutex<Option<JoinHandle<Result<(), ConfigWatchError>>>>>;

type ProducerSlot = Arc<Mutex<Option<OwnedWatch<Result<LoadedConfig, ConfigError>>>>>;

struct BridgeJoined {
    bridge: Result<(), ConfigWatchError>,
    producer: Result<(), ConfigWatchError>,
}

pub(crate) struct ConfigWatch {
    shared: Arc<Shared>,
    bridge: BridgeSlot,
    // The bridge only borrows this native owner. A bridge panic leaves the
    // actual producer reachable here for the same supervisor's checked join.
    producer: ProducerSlot,
    refresh: Option<tokio::task::JoinHandle<()>>,
    refresh_result: Option<Result<(), ConfigWatchError>>,
    supervisor: Option<tokio::task::JoinHandle<Result<BridgeJoined, ConfigWatchError>>>,
    completion: Option<Result<(), ConfigWatchError>>,
}

impl ConfigWatch {
    pub(crate) fn start(
        ui: &AppWindow,
        store: ConfigStore,
        live: Arc<LiveUi>,
        notifications: Arc<crate::notifications::DesktopNotifications>,
        runtime: &Handle,
    ) -> Self {
        let (requests, mut receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared(Mutex::new(State {
            closed: false,
            failed: false,
            queued: false,
            producer: None,
            refresh: Some(requests),
        })));
        let refresh_shared = shared.clone();
        let refresh_live = live.clone();
        let refresh_ui = ui.as_weak();
        let refresh = runtime.spawn(async move {
            while receiver.recv().await.is_some() {
                // An admitted refresh is awaited to completion even when close
                // subsequently revokes presentation and account admission.
                let admitted = !refresh_shared.state().closed;
                if !admitted {
                    break;
                }
                let _ = refresh_live.refresh(refresh_ui.clone()).await;
            }
        });
        let bridge_shared = shared.clone();
        let producer: ProducerSlot = Arc::new(Mutex::new(None));
        let bridge_producer = producer.clone();
        let weak = ui.as_weak();
        let bridge = std::thread::Builder::new()
            .name("morpheus-config-ui".into())
            .spawn(move || {
                {
                    let mut state = bridge_shared.state();
                    if state.closed {
                        return Ok(());
                    }
                    // Creation and stop-handle registration share close admission;
                    // an already closed application cannot start a new producer.
                    match store.watch_with_theme_owned(Duration::from_millis(700)) {
                        Ok(watch) => {
                            state.producer = Some(watch.stop_handle());
                            *bridge_producer
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner()) = Some(watch);
                        }
                        Err(_) => {
                            state.failed = true;
                            drop(state);
                            let failed_shared = bridge_shared.clone();
                            let _ = weak.upgrade_in_event_loop(move |ui| {
                                let open = !failed_shared.state().closed;
                                if open && !ui.get_application_closing() {
                                    ui.set_config_error(
                                        "Could not start external settings updates.".into(),
                                    );
                                }
                            });
                            return Err(ConfigWatchError::Start);
                        }
                    }
                }
                loop {
                    let received = {
                        let owner = bridge_producer
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        // Only the supervisor takes the producer, after joining
                        // this actual bridge. No handle is moved into the loop.
                        owner.as_ref().ok_or(ConfigWatchError::Ownership)?.recv()
                    };
                    if received.is_err() {
                        break;
                    }
                    if bridge_shared.state().closed {
                        break;
                    }
                    if post_update(
                        bridge_shared.clone(),
                        weak.clone(),
                        store.clone(),
                        live.clone(),
                        notifications.clone(),
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                Ok(())
            });
        let bridge = match bridge {
            Ok(bridge) => Some(bridge),
            Err(_) => {
                shared.state().failed = true;
                shared.close();
                ui.set_config_error("Could not start external settings updates.".into());
                None
            }
        };
        Self {
            shared,
            bridge: Arc::new(Mutex::new(bridge)),
            producer,
            refresh: Some(refresh),
            refresh_result: None,
            supervisor: None,
            completion: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn queue_latest_for_test(
        &self,
        ui: &AppWindow,
        store: ConfigStore,
        live: Arc<LiveUi>,
        notifications: Arc<crate::notifications::DesktopNotifications>,
    ) -> Result<bool, ()> {
        let posted = post_update(
            self.shared.clone(),
            ui.as_weak(),
            store,
            live,
            notifications,
        )?;
        let state = self.shared.state();
        if state.closed || !state.queued {
            Err(())
        } else {
            // Same UI thread: no event pump occurs between this receipt and
            // the test's synchronous close. An already queued producer post
            // is the same real production callback, not a missing observation.
            Ok(posted)
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shared.close();
    }

    /// Cancellation leaves this same owner and every original join reachable.
    /// Only Ownership is an unproved receipt; all other errors below follow
    /// actual refresh, bridge and producer joins (or proven startup refusal).
    pub(crate) async fn finish(&mut self) -> Result<(), ConfigWatchError> {
        self.shutdown();
        if let Some(result) = self.completion {
            return result;
        }
        if self.refresh_result.is_none() {
            let result = match self.refresh.as_mut() {
                Some(task) => settle_result(task.await),
                None => return Err(ConfigWatchError::Ownership),
            };
            self.refresh.take();
            self.refresh_result = Some(result);
        }
        if self.supervisor.is_none() {
            let slot = self.bridge.clone();
            let producer = self.producer.clone();
            let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::task::spawn_blocking(move || {
                    let worker = slot.lock().map_err(|_| ConfigWatchError::Ownership)?.take();
                    let bridge = match worker {
                        Some(worker) => worker.join().unwrap_or_else(|payload| {
                            std::mem::forget(payload);
                            Err(ConfigWatchError::Bridge)
                        }),
                        // Start never created a bridge, so there is no handle
                        // to join. The producer slot is still checked below.
                        None => Err(ConfigWatchError::Start),
                    };
                    let native = producer
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take();
                    let producer = match native {
                        Some(watch) => watch.finish().map_err(|_| ConfigWatchError::Producer),
                        None => Ok(()),
                    };
                    Ok(BridgeJoined { bridge, producer })
                })
            }));
            self.supervisor = match spawned {
                Ok(task) => Some(task),
                Err(payload) => {
                    std::mem::forget(payload);
                    return Err(ConfigWatchError::Ownership);
                }
            };
        }
        let joined = match self
            .supervisor
            .as_mut()
            .expect("supervisor was retained")
            .await
        {
            Ok(Ok(joined)) => Some(joined),
            Ok(Err(_)) => None,
            Err(error) => {
                forget_task_panic(error);
                None
            }
        };
        self.supervisor.take();
        let result = match joined {
            // An independent refresh failure must never mask missing joins.
            None => Err(ConfigWatchError::Ownership),
            Some(joined) => {
                let refresh = self.refresh_result.expect("refresh join was recorded");
                if joined.bridge == Err(ConfigWatchError::Ownership) {
                    Err(ConfigWatchError::Ownership)
                } else {
                    joined
                        .producer
                        .and(joined.bridge)
                        .and(refresh)
                        .and_then(|()| {
                            if self.shared.state().failed {
                                Err(ConfigWatchError::Admission)
                            } else {
                                Ok(())
                            }
                        })
                }
            }
        };
        self.completion = Some(result);
        result
    }
}

impl Drop for ConfigWatch {
    fn drop(&mut self) {
        self.shutdown();
        if self.completion.is_none() || self.completion == Some(Err(ConfigWatchError::Ownership)) {
            // An interrupted finish is not a receipt. Preserve all remaining
            // actual owners; no timeout, detached replacement or no-op task.
            std::mem::forget(self.bridge.clone());
            std::mem::forget(self.producer.clone());
            if let Some(task) = self.refresh.take() {
                std::mem::forget(task);
            }
            if let Some(task) = self.supervisor.take() {
                std::mem::forget(task);
            }
        }
    }
}

fn post_update(
    shared: Arc<Shared>,
    weak: slint::Weak<AppWindow>,
    store: ConfigStore,
    live: Arc<LiveUi>,
    notifications: Arc<crate::notifications::DesktopNotifications>,
) -> Result<bool, ()> {
    if !shared.queue_callback() {
        return Ok(false);
    }
    let callback_shared = shared.clone();
    if weak
        .upgrade_in_event_loop(move |ui| {
            // A real callback may have been queued before synchronous close. Do
            // not reread any current config/UI/account policy after revocation.
            if !callback_shared.enter_callback()
                || live.action_admission_closed()
                || ui.get_application_closing()
            {
                return;
            }
            // The producer sample can precede a Settings privacy save. Re-read
            // the latest file on the UI thread; a theme error cannot block policy.
            match store.load() {
                Ok(config) => {
                    notifications.apply_config(&config);
                    apply_notification_settings(&ui, &config);
                    crate::shortcuts::apply_config(&ui, &config);
                    crate::audio_devices::apply_config(&ui, &config);
                    crate::image_previews::apply_config(&ui, &config);
                    crate::room_activity::apply_config(&ui, &config);
                    live.voice_spaces.apply_config(&config);
                    match store.load_theme(&config) {
                        Ok(theme) => apply_loaded_config(
                            &ui,
                            &LoadedConfig {
                                config: config.clone(),
                                theme,
                            },
                        ),
                        Err(error) => ui.set_config_error(error.to_string().into()),
                    }
                    callback_shared.request_refresh();
                }
                Err(error) => ui.set_config_error(error.to_string().into()),
            }
        })
        .is_err()
    {
        shared.close();
        return Err(());
    }
    Ok(true)
}

fn settle_result(result: Result<(), tokio::task::JoinError>) -> Result<(), ConfigWatchError> {
    result.map_err(|error| {
        forget_task_panic(error);
        ConfigWatchError::Refresh
    })
}

fn forget_task_panic(error: tokio::task::JoinError) {
    if error.is_panic() {
        std::mem::forget(error.into_panic());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_retires_queued_callbacks_and_refresh_admission_together() {
        let (sender, mut receiver) = mpsc::channel(1);
        let shared = Shared(Mutex::new(State {
            closed: false,
            failed: false,
            queued: false,
            producer: None,
            refresh: Some(sender),
        }));
        assert!(shared.queue_callback());
        assert!(!shared.queue_callback());
        shared.request_refresh();
        shared.close();
        assert!(!shared.enter_callback());
        shared.request_refresh();
        assert_eq!(receiver.try_recv(), Ok(()));
        assert_eq!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        );
        assert!(!shared.queue_callback());
    }

    fn fixture(
        bridge: JoinHandle<Result<(), ConfigWatchError>>,
        refresh: tokio::task::JoinHandle<()>,
        producer: Option<OwnedWatch<Result<LoadedConfig, ConfigError>>>,
    ) -> ConfigWatch {
        let stop = producer.as_ref().map(OwnedWatch::stop_handle);
        ConfigWatch {
            shared: Arc::new(Shared(Mutex::new(State {
                closed: false,
                failed: false,
                queued: false,
                producer: stop,
                refresh: None,
            }))),
            bridge: Arc::new(Mutex::new(Some(bridge))),
            producer: Arc::new(Mutex::new(producer)),
            refresh: Some(refresh),
            refresh_result: None,
            supervisor: None,
            completion: None,
        }
    }

    #[tokio::test]
    async fn a_bridge_panic_still_joins_the_original_native_producer() {
        // This producer is stopped before its first sample; no fixture config
        // file or profile is opened. It is the actual owned polling thread.
        let producer = ConfigStore::at("synthetic-unused-config.toml")
            .watch_with_theme_owned(Duration::from_secs(3600))
            .unwrap();
        let bridge = std::thread::spawn(|| -> Result<(), ConfigWatchError> {
            panic!("synthetic config bridge panic")
        });
        let refresh = tokio::spawn(async { panic!("synthetic config refresh panic") });
        let mut owner = fixture(bridge, refresh, Some(producer));
        let joined = owner.finish().await;
        let repeated = owner.finish().await;
        let original_owners_joined = owner.producer.lock().unwrap().is_none()
            && owner.bridge.lock().unwrap().is_none()
            && owner.refresh.is_none()
            && owner.supervisor.is_none();
        assert_eq!(joined, Err(ConfigWatchError::Bridge));
        assert_eq!(repeated, joined);
        assert!(original_owners_joined);
    }

    #[tokio::test]
    async fn cancelled_finish_keeps_the_original_running_supervisor_for_retry() {
        let (release, released) = std::sync::mpsc::channel();
        let bridge = std::thread::spawn(move || {
            let _ = released.recv();
            Ok(())
        });
        let refresh = tokio::spawn(async {});
        let mut owner = fixture(bridge, refresh, None);
        let observed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut finishing = Box::pin(owner.finish());
                let pending = futures_util::poll!(finishing.as_mut()).is_pending();
                drop(finishing);
                if !pending {
                    return false;
                }
                if owner.supervisor.is_some() {
                    return true;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let _ = release.send(());
        let joined = owner.finish().await;
        let repeated = owner.finish().await;
        assert!(matches!(observed, Ok(true)));
        assert_eq!(joined, Ok(()));
        assert_eq!(repeated, joined);
        assert!(owner.bridge.lock().unwrap().is_none());
        assert!(owner.supervisor.is_none());
    }
}
