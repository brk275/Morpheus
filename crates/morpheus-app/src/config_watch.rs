//! The ordinary desktop config producer, UI bridge and refresh task are one
//! shutdown owner. Cancellation revokes queued callbacks before any UI read;
//! completion observes the producer and bridge's actual native joins.

use crate::{AppWindow, LiveUi, apply_loaded_config, apply_notification_settings};
use morpheus_config::{ConfigStore, LoadedConfig, WatchStop};
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

struct BridgeJoined {
    result: Result<(), ConfigWatchError>,
}

pub(crate) struct ConfigWatch {
    shared: Arc<Shared>,
    bridge: BridgeSlot,
    refresh: Option<tokio::task::JoinHandle<()>>,
    supervisor: Option<tokio::task::JoinHandle<Result<BridgeJoined, ConfigWatchError>>>,
    finished: bool,
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
        let weak = ui.as_weak();
        let bridge = std::thread::Builder::new()
            .name("morpheus-config-ui".into())
            .spawn(move || {
                let watch = {
                    let mut state = bridge_shared.state();
                    if state.closed {
                        return Ok(());
                    }
                    // Creation and stop-handle registration share close admission;
                    // an already closed application cannot start a new producer.
                    match store.watch_with_theme_owned(Duration::from_millis(700)) {
                        Ok(watch) => {
                            state.producer = Some(watch.stop_handle());
                            watch
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
                };
                while watch.recv().is_ok() {
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
                let result = watch.finish().map_err(|_| ConfigWatchError::Producer);
                bridge_shared.state().producer.take();
                result
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
            refresh: Some(refresh),
            supervisor: None,
            finished: false,
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

    pub(crate) async fn finish(mut self) -> Result<(), ConfigWatchError> {
        self.shutdown();
        let refresh_result = match self.refresh.as_mut() {
            Some(task) => settle_result(task.await),
            None => Err(ConfigWatchError::Ownership),
        };
        self.refresh.take();
        // The actual std handle remains in this retained slot until a running
        // supervisor takes it. Pool-spawn failure cannot drop the sole handle.
        let slot = self.bridge.clone();
        let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::task::spawn_blocking(move || {
                let worker = slot
                    .lock()
                    .map_err(|_| ConfigWatchError::Ownership)?
                    .take()
                    .ok_or(ConfigWatchError::Start)?;
                let result = worker
                    .join()
                    .map_err(|payload| {
                        std::mem::forget(payload);
                        ConfigWatchError::Bridge
                    })
                    .and_then(|result| result);
                Ok(BridgeJoined { result })
            })
        }));
        self.supervisor = match spawned {
            Ok(task) => Some(task),
            Err(payload) => {
                std::mem::forget(payload);
                return Err(ConfigWatchError::Ownership);
            }
        };
        let bridge_result = match self
            .supervisor
            .as_mut()
            .expect("supervisor was retained")
            .await
        {
            Ok(Ok(joined)) => {
                self.finished = true;
                joined.result
            }
            Ok(Err(error)) => Err(error),
            Err(error) => {
                forget_task_panic(error);
                Err(ConfigWatchError::Ownership)
            }
        };
        self.supervisor.take();
        refresh_result?;
        bridge_result?;
        if self.shared.state().failed {
            return Err(ConfigWatchError::Ownership);
        }
        Ok(())
    }
}

impl Drop for ConfigWatch {
    fn drop(&mut self) {
        self.shutdown();
        if !self.finished {
            // An interrupted finish is not a receipt. Preserve all remaining
            // actual owners; no timeout, detached replacement or no-op task.
            std::mem::forget(self.bridge.clone());
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
}
