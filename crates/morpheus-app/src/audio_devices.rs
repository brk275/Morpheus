//! Persisted audio routing for new joins. Catalog discovery opens no stream.
//! The UI holds checked preferences; each call receives its own owned copy,
//! and a call in progress changes only on an explicit Use for this call
//! (`CallController::set_audio_route`), never on a save or a file edit.

use morpheus_config::{Config, ConfigStore};
use sha2::{Digest, Sha256};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::AppWindow;
#[cfg(feature = "native-calls")]
use crate::AudioDeviceItem;

#[cfg(any(feature = "native-calls", test))]
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[cfg(any(feature = "native-calls", test))]
#[derive(Default)]
struct DiscoveryRequests {
    running: AtomicBool,
    generation: AtomicU64,
}

#[cfg(any(feature = "native-calls", test))]
struct DiscoveryRequest {
    owner: Arc<DiscoveryRequests>,
    generation: u64,
}

#[cfg(any(feature = "native-calls", test))]
impl DiscoveryRequests {
    fn begin(self: &Arc<Self>) -> Option<DiscoveryRequest> {
        self.running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(DiscoveryRequest {
            owner: self.clone(),
            generation: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
        })
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }
}

#[cfg(any(feature = "native-calls", test))]
impl Drop for DiscoveryRequest {
    fn drop(&mut self) {
        self.owner.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(feature = "native-calls")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryReleaseError {
    TimedOut,
    WorkerPanicked,
    SupervisorFailed,
}
#[cfg(feature = "native-calls")]
impl std::fmt::Display for DiscoveryReleaseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::TimedOut => "Audio discovery release is still pending.",
            Self::WorkerPanicked => "Audio discovery worker failed.",
            Self::SupervisorFailed => "Audio discovery ownership could not be confirmed.",
        })
    }
}
#[cfg(feature = "native-calls")]
impl std::error::Error for DiscoveryReleaseError {}
#[cfg(feature = "native-calls")]
type CatalogResult = Result<morpheus_rtc::AudioDeviceCatalog, morpheus_rtc::MediaError>;
#[cfg(feature = "native-calls")]
struct DiscoveryOwner {
    supervisor: Option<std::thread::JoinHandle<()>>,
    result: Arc<std::sync::Mutex<Option<Result<CatalogResult, DiscoveryReleaseError>>>>,
    _request: DiscoveryRequest,
    failure: Option<DiscoveryReleaseError>,
}
#[cfg(feature = "native-calls")]
static DISCOVERY_OWNER: std::sync::Mutex<Option<DiscoveryOwner>> = std::sync::Mutex::new(None);
#[cfg(feature = "native-calls")]
static DISCOVERY_CLOSED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "native-calls")]
fn poll_discovery() -> Option<Result<Option<CatalogResult>, DiscoveryReleaseError>> {
    let mut stored = DISCOVERY_OWNER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(owner) = stored.as_mut() else {
        return Some(Ok(None));
    };
    if let Some(error) = owner.failure {
        return Some(Err(error));
    }
    let Some(supervisor) = owner.supervisor.as_ref() else {
        return Some(Err(DiscoveryReleaseError::SupervisorFailed));
    };
    if !supervisor.is_finished() {
        return None;
    }
    let result = owner.supervisor.take().unwrap().join();
    if result.is_err() {
        owner.failure = Some(DiscoveryReleaseError::SupervisorFailed);
        return Some(Err(DiscoveryReleaseError::SupervisorFailed));
    }
    let observed = owner
        .result
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    match observed {
        Some(Ok(catalog)) => {
            *stored = None;
            Some(Ok(Some(catalog)))
        }
        Some(Err(error)) => {
            owner.failure = Some(error);
            Some(Err(error))
        }
        None => {
            owner.failure = Some(DiscoveryReleaseError::SupervisorFailed);
            Some(Err(DiscoveryReleaseError::SupervisorFailed))
        }
    }
}
#[cfg(feature = "native-calls")]
fn start_discovery(
    request: DiscoveryRequest,
    ready: tokio::sync::oneshot::Sender<()>,
) -> Result<(), DiscoveryReleaseError> {
    start_discovery_with(request, ready, morpheus_rtc::audio_devices)
}
#[cfg(feature = "native-calls")]
fn start_discovery_with(
    request: DiscoveryRequest,
    ready: tokio::sync::oneshot::Sender<()>,
    query: impl FnOnce() -> CatalogResult + Send + 'static,
) -> Result<(), DiscoveryReleaseError> {
    let mut stored = DISCOVERY_OWNER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if stored.is_some() || DISCOVERY_CLOSED.load(Ordering::Acquire) {
        return Err(DiscoveryReleaseError::SupervisorFailed);
    }
    let result = Arc::new(std::sync::Mutex::new(None));
    let completed = result.clone();
    let (start, admitted) = std::sync::mpsc::sync_channel(1);
    let supervisor = std::thread::Builder::new()
        .name("morpheus-device-catalog-join".into())
        .spawn(move || {
            if admitted.recv().is_err() {
                return;
            }
            let actual = std::thread::Builder::new()
                .name("morpheus-audio-devices".into())
                .spawn(query);
            let result = match actual {
                Ok(actual) => actual
                    .join()
                    .map_err(|_| DiscoveryReleaseError::WorkerPanicked),
                Err(_) => Err(DiscoveryReleaseError::SupervisorFailed),
            };
            *completed.lock().unwrap_or_else(|error| error.into_inner()) = Some(result);
            // Only a unit notification crosses here. The actual worker's TLS was
            // joined, and the supervisor handle remains owned until poll joins it.
            let _ = ready.send(());
        })
        .map_err(|_| DiscoveryReleaseError::SupervisorFailed)?;
    *stored = Some(DiscoveryOwner {
        supervisor: Some(supervisor),
        result,
        _request: request,
        failure: None,
    });
    start
        .send(())
        .map_err(|_| DiscoveryReleaseError::SupervisorFailed)
}
#[cfg(feature = "native-calls")]
pub(crate) fn shutdown_discovery() {
    DISCOVERY_CLOSED.store(true, Ordering::Release);
}
#[cfg(feature = "native-calls")]
pub(crate) async fn wait_for_discovery_release() -> Result<(), DiscoveryReleaseError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match poll_discovery() {
            Some(Ok(_)) => return Ok(()),
            Some(Err(error)) => return Err(error),
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DiscoveryReleaseError::TimedOut);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Choice {
    id: Option<String>,
    label: String,
    available: bool,
}

fn id_tag(id: &str) -> String {
    Sha256::digest(id.as_bytes())[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A platform device name, bounded and without control or direction
/// characters, for showing only (never an identity).
pub(crate) fn friendly_name(name: &str) -> String {
    let cleaned = name.chars().filter(|character| {
        !character.is_control()
            && !matches!(*character, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }).collect::<String>();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "Audio device".into();
    }
    let mut label = cleaned.chars().take(48).collect::<String>();
    if cleaned.chars().count() > 48 {
        label.push('…');
    }
    label
}

fn choices(catalog: &[(String, String)], saved: &str, loaded: bool) -> Vec<Choice> {
    let mut result = vec![Choice {
        id: None,
        label: "System default".into(),
        available: true,
    }];
    for (id, name) in catalog {
        result.push(Choice {
            id: Some(id.clone()),
            // Keep a stable tag even for unique names: a stale popup label
            // must not select a replacement device with the same name.
            // Full IDs remain data; native widget labels stay bounded.
            label: format!("{} · {}", friendly_name(name), id_tag(id)),
            available: true,
        });
    }
    if !saved.is_empty() && !catalog.iter().any(|(id, _)| id == saved) {
        result.push(Choice {
            id: Some(saved.into()),
            label: format!(
                "{} · {}",
                if loaded {
                    "Unavailable"
                } else {
                    "Saved device; refresh to check"
                },
                id_tag(saved)
            ),
            available: false,
        });
    }
    result
}

fn chosen_id(rows: &[Choice], label: &str) -> Option<Option<String>> {
    let mut matching = rows.iter().filter(|row| row.label == label);
    let choice = matching.next()?;
    (choice.available && matching.next().is_none()).then(|| choice.id.clone())
}

fn rows(ui: &AppWindow, input: bool) -> Vec<Choice> {
    let (catalog, saved) = if input {
        (ui.get_audio_microphones(), ui.get_audio_microphone_id())
    } else {
        (ui.get_audio_speakers(), ui.get_audio_speaker_id())
    };
    let catalog = catalog
        .iter()
        .map(|entry| (entry.id.to_string(), entry.name.to_string()))
        .collect::<Vec<_>>();
    choices(&catalog, &saved, ui.get_audio_catalog_loaded())
}

fn render(ui: &AppWindow) {
    for input in [true, false] {
        let rows = rows(ui, input);
        let saved = if input {
            ui.get_audio_microphone_id()
        } else {
            ui.get_audio_speaker_id()
        };
        let index = rows
            .iter()
            .position(|row| row.id.as_deref().unwrap_or("") == saved.as_str())
            .unwrap_or(0) as i32;
        let labels = ModelRc::new(VecModel::from(
            rows.into_iter()
                .map(|row| SharedString::from(row.label))
                .collect::<Vec<_>>(),
        ));
        if input {
            ui.set_audio_microphone_choices(labels);
            ui.set_audio_microphone_index(index);
        } else {
            ui.set_audio_speaker_choices(labels);
            ui.set_audio_speaker_index(index);
        }
    }
}

pub fn apply_config(ui: &AppWindow, config: &Config) {
    ui.set_audio_microphone_id(
        config
            .media
            .microphone_device
            .as_deref()
            .unwrap_or("")
            .into(),
    );
    ui.set_audio_speaker_id(config.media.speaker_device.as_deref().unwrap_or("").into());
    render(ui);
}

#[cfg(feature = "native-calls")]
pub fn selection(ui: &AppWindow) -> morpheus_rtc::AudioDeviceSelection {
    let selected = |id: SharedString| (!id.is_empty()).then(|| id.to_string());
    morpheus_rtc::AudioDeviceSelection {
        microphone: selected(ui.get_audio_microphone_id()),
        speaker: selected(ui.get_audio_speaker_id()),
    }
}

fn save_selection(ui: &AppWindow, store: &ConfigStore, input: bool, label: &str) {
    let Some(selected) = chosen_id(&rows(ui, input), label) else {
        render(ui);
        ui.set_audio_device_notice(
            "That device is unavailable. Refresh the list and choose again.".into(),
        );
        return;
    };
    if let Some(config) = crate::save_config_change(ui, store, |config| {
        if input {
            config.media.microphone_device = selected;
        } else {
            config.media.speaker_device = selected;
        }
    }) {
        apply_config(ui, &config);
        ui.set_audio_device_notice("Audio device saved for new calls. During a call, choose Use for this call to switch without leaving.".into());
    } else {
        // ComboBox updates its index before selected fires. Restore the last
        // checked preference after a failed save, including invalid TOML edits.
        render(ui);
        ui.set_audio_device_notice(
            "Audio device was not saved. Fix the configuration error and try again.".into(),
        );
    }
}

pub fn register(ui: &AppWindow, store: ConfigStore, handle: &tokio::runtime::Handle) {
    ui.set_audio_devices_enabled(cfg!(feature = "native-calls"));
    for input in [true, false] {
        let weak = ui.as_weak();
        let store = store.clone();
        let select = move |label: SharedString| {
            if let Some(ui) = weak.upgrade() {
                if ui.get_audio_devices_enabled() && !ui.get_audio_devices_busy() {
                    save_selection(&ui, &store, input, &label);
                }
            }
        };
        if input {
            ui.on_select_audio_microphone(select);
        } else {
            ui.on_select_audio_speaker(select);
        }
    }
    #[cfg(not(feature = "native-calls"))]
    {
        let _ = handle;
        ui.set_audio_device_notice("Audio device selection is unavailable in this build.".into());
    }
    #[cfg(feature = "native-calls")]
    register_discovery(ui, handle);
}

#[cfg(feature = "native-calls")]
fn register_discovery(ui: &AppWindow, handle: &tokio::runtime::Handle) {
    use std::time::Duration;

    // A timed-out query keeps its actual worker and supervisor handles. The
    // single-flight guard releases only after both real joins, including TLS.
    let requests = Arc::new(DiscoveryRequests::default());
    let weak = ui.as_weak();
    let handle = handle.clone();
    ui.on_refresh_audio_devices(move || {
        let Some(ui) = weak.upgrade() else { return };
        if DISCOVERY_CLOSED.load(Ordering::Acquire) { return; }
        if matches!(poll_discovery(), Some(Err(_))) {
            ui.set_audio_device_notice("Audio discovery did not release safely. New discovery is blocked.".into());
            return;
        }
        let Some(guard) = requests.begin() else {
            ui.set_audio_device_notice(
                "The previous device query is still waiting for the operating system.".into(),
            );
            return;
        };
        let request = guard.generation;
        ui.set_audio_devices_busy(true);
        ui.set_audio_device_notice("Looking for microphones and speakers…".into());
        let (tx, rx) = tokio::sync::oneshot::channel();
        if start_discovery(guard, tx).is_err()
        {
            ui.set_audio_devices_busy(false);
            ui.set_audio_device_notice("Could not start device discovery.".into());
            return;
        }
        let weak = weak.clone();
        let requests = requests.clone();
        handle.spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(20), async move {
                rx.await.map_err(|_| DiscoveryReleaseError::SupervisorFailed)?;
                loop {
                    match poll_discovery() {
                        Some(Ok(Some(catalog))) => return Ok(catalog),
                        Some(Ok(None)) => return Err(DiscoveryReleaseError::SupervisorFailed),
                        Some(Err(error)) => return Err(error),
                        None => tokio::time::sleep(Duration::from_millis(10)).await,
                    }
                }
            }).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if DISCOVERY_CLOSED.load(Ordering::Acquire) || !requests.is_current(request) {
                    return;
                }
                ui.set_audio_devices_busy(false);
                match result {
                    Ok(Ok(Ok(catalog))) => {
                        let model = |devices: Vec<morpheus_rtc::AudioDevice>| {
                            ModelRc::new(VecModel::from(
                                devices
                                    .into_iter()
                                    .map(|device| AudioDeviceItem {
                                        id: device.id.into(),
                                        name: device.name.into(),
                                    })
                                    .collect::<Vec<_>>(),
                            ))
                        };
                        ui.set_audio_microphones(model(catalog.microphones));
                        ui.set_audio_speakers(model(catalog.speakers));
                        ui.set_audio_catalog_loaded(true);
                        render(&ui);
                        ui.set_audio_device_notice(
                            "Device list refreshed. Choices apply to new calls; Use for this call switches a call in progress.".into(),
                        );
                    }
                    Err(_) => ui.set_audio_device_notice(
                        "Device discovery timed out. The last saved choices are unchanged.".into(),
                    ),
                    _ => ui.set_audio_device_notice(
                        "Could not list audio devices. The last saved choices are unchanged."
                            .into(),
                    ),
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "native-calls")]
    #[test]
    fn discovery_result_and_single_flight_wait_for_the_real_worker_tls_join() {
        use std::time::{Duration, Instant};
        struct Held(std::sync::mpsc::Receiver<()>);
        impl Drop for Held {
            fn drop(&mut self) {
                let _ = self.0.recv();
            }
        }
        thread_local! { static HELD: std::cell::RefCell<Option<Held>> = const { std::cell::RefCell::new(None) }; }
        let requests = Arc::new(DiscoveryRequests::default());
        let request = requests.begin().expect("reserve synthetic catalogue");
        let (release, held) = std::sync::mpsc::channel();
        let (body, returned) = std::sync::mpsc::sync_channel(1);
        let (ready, mut received) = tokio::sync::oneshot::channel();
        let started = start_discovery_with(request, ready, move || {
            HELD.with(|slot| *slot.borrow_mut() = Some(Held(held)));
            let _ = body.send(());
            Ok(morpheus_rtc::AudioDeviceCatalog::default())
        });
        let observation = (|| -> Result<_, &'static str> {
            if started.is_err() {
                return Err("synthetic discovery did not start");
            }
            returned
                .recv_timeout(Duration::from_secs(3))
                .map_err(|_| "synthetic worker body pending")?;
            Ok((
                matches!(
                    received.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ),
                requests.begin().is_none(),
                poll_discovery().is_none(),
            ))
        })();
        // Actual retained worker/supervisor cleanup precedes every assertion.
        let _ = release.send(());
        let until = Instant::now() + Duration::from_secs(3);
        let retired = loop {
            match poll_discovery() {
                None if Instant::now() < until => {
                    std::thread::park_timeout(Duration::from_millis(5))
                }
                result => break result,
            }
        };
        assert!(matches!(observation, Ok((true, true, true))));
        assert!(matches!(retired, Some(Ok(Some(Ok(_))))));
        assert!(requests.begin().is_some());
    }

    #[test]
    fn duplicate_names_keep_identity_across_enumeration_order() {
        let mut catalog = vec![
            ("wasapi:one".into(), "USB headset".into()),
            ("wasapi:two".into(), "USB headset".into()),
        ];
        let first = choices(&catalog, "wasapi:two", true);
        let label = first[2].label.clone();
        catalog.reverse();
        assert_eq!(
            chosen_id(&choices(&catalog, "wasapi:two", true), &label),
            Some(Some("wasapi:two".into()))
        );
        assert_eq!(chosen_id(&first, "System default"), Some(None));
    }

    #[test]
    fn missing_saved_device_is_visible_but_cannot_silently_select_default() {
        let rows = choices(&[], "coreaudio:missing", true);
        assert_eq!(rows[1].id.as_deref(), Some("coreaudio:missing"));
        assert!(rows[1].label.starts_with("Unavailable"));
        assert_eq!(chosen_id(&rows, &rows[1].label), None);
        assert_eq!(chosen_id(&rows, "old catalog label"), None);
        assert!(
            choices(&[], "coreaudio:missing", false)[1]
                .label
                .starts_with("Saved device")
        );
    }

    #[test]
    fn timeout_does_not_release_driver_slot_and_late_completion_cannot_replace_new_query() {
        let requests = Arc::new(DiscoveryRequests::default());
        let driver = requests.begin().unwrap();
        let first = driver.generation;
        // The UI's bounded wait can end while the driver still owns its guard.
        assert!(requests.begin().is_none());
        drop(driver);
        let next_driver = requests.begin().unwrap();
        assert!(!requests.is_current(first));
        assert!(requests.is_current(next_driver.generation));
        assert!(requests.begin().is_none());
        drop(next_driver);
        assert!(requests.begin().is_some());
    }

    #[test]
    fn ambiguous_display_choice_is_rejected_instead_of_selecting_first() {
        let mut rows = choices(&[("alsa:one".into(), "Headset".into())], "", true);
        let mut duplicate = rows[1].clone();
        duplicate.id = Some("alsa:two".into());
        rows.push(duplicate);
        assert_eq!(chosen_id(&rows, &rows[1].label), None);
    }

    #[test]
    fn picker_labels_are_bounded_and_replacement_names_cannot_retarget_old_labels() {
        let old = choices(&[("wasapi:old".into(), "Studio headset".into())], "", true);
        assert!(old[1].label.starts_with("Studio headset · "));
        let replacement = choices(&[("wasapi:new".into(), "Studio headset".into())], "", true);
        assert_eq!(chosen_id(&replacement, &old[1].label), None);
        let long_id = format!("coreaudio:{}", "x".repeat(4000));
        let long_name = "Headset\n\u{202e}".repeat(100);
        let rows = choices(&[(long_id.clone(), long_name)], "", true);
        assert!(rows[1].label.chars().count() <= 64);
        assert!(!rows[1].label.contains(['\n', '\u{202e}']));
        assert_eq!(
            chosen_id(&rows, &rows[1].label),
            Some(Some(long_id.clone()))
        );
        let unavailable = choices(&[], &long_id, true);
        assert!(unavailable[1].label.chars().count() <= 64);
        assert!(!unavailable[1].label.contains(&long_id));
    }
}
