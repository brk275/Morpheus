use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(target_os = "linux")]
use crate::alsa_close::{
    AlsaBackendCloseError as NativeAudioCloseError,
    close_retired_alsa_on_owned_worker as close_retired_native_audio,
    wait_for_retired_alsa_release as wait_for_retired_native_audio,
};
#[cfg(target_os = "macos")]
use crate::coreaudio_close::{
    CoreAudioCloseError as NativeAudioCloseError,
    close_retired_coreaudio_on_owned_worker as close_retired_native_audio,
    wait_for_retired_coreaudio_release as wait_for_retired_native_audio,
};

use cpal::{
    SampleFormat, Stream, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::StreamExt;
use matrix_rtc_call_sdk::CallEvent;
use matrix_rtc_transport::AudioFrame;
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time::{Duration, MissedTickBehavior},
};

use crate::{CallInfo, MediaError, Publication, RtcService, native::CallLease};

#[cfg(target_os = "macos")]
const NATIVE_CLOSE_MESSAGE: &str = "CoreAudio native cleanup is not confirmed";
#[cfg(target_os = "linux")]
const NATIVE_CLOSE_MESSAGE: &str = "ALSA native cleanup is not confirmed";
#[cfg(target_os = "macos")]
const NATIVE_ROUTE_CLOSE_MESSAGE: &str = "CoreAudio route cleanup is not confirmed";
#[cfg(target_os = "linux")]
const NATIVE_ROUTE_CLOSE_MESSAGE: &str = "ALSA route cleanup is not confirmed";

const SAMPLE_RATE: u32 = 48_000;
const CHUNK_SAMPLES_PER_CHANNEL: usize = 480; // 10 ms
const MAX_OUTPUT_BUFFER_SAMPLES: usize = SAMPLE_RATE as usize; // 500 ms stereo
const MAX_REMOTE_STREAMS: usize = 64;
const REMOTE_FRAME_CHANNEL_CAPACITY: usize = 128;
// One actual device opener (including unclaimed-result destruction and TLS)
// remains fenced until its retained supervisor has joined it. No native result
// is put in a Tokio channel: cancellation there must never drop a stream on a
// runtime worker. CPAL's own backend disposal remains a separate contract.
static DEVICE_OPEN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static DEVICE_OPEN_OWNER: Mutex<Option<DeviceOpenOwner>> = Mutex::new(None);
const DEVICE_RELEASE_TIMEOUT: Duration = Duration::from_secs(3);

struct DeviceOpenGuard {
    release_allowed: bool,
}
impl Drop for DeviceOpenGuard {
    fn drop(&mut self) {
        if self.release_allowed {
            DEVICE_OPEN_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioDeviceReleaseError {
    TimedOut,
    WorkerPanicked,
    SupervisorFailed,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    NativeClose,
}
impl std::fmt::Display for AudioDeviceReleaseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::TimedOut => "audio device opener release was not confirmed before the deadline",
            Self::WorkerPanicked => "audio device opener panicked during startup or release",
            Self::SupervisorFailed => "audio device opener ownership could not be confirmed",
            #[cfg(target_os = "macos")]
            Self::NativeClose => "CoreAudio native cleanup failed and remains owned",
            #[cfg(target_os = "linux")]
            Self::NativeClose => "ALSA native cleanup failed and remains owned",
        })
    }
}
impl std::error::Error for AudioDeviceReleaseError {}
#[derive(Default)]
struct OpenCompletion {
    result: Mutex<Option<Result<(), AudioDeviceReleaseError>>>,
    handed_off: AtomicBool,
}
impl OpenCompletion {
    fn finish(&self, result: Result<(), AudioDeviceReleaseError>) {
        let mut stored = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if stored.is_none() {
            *stored = Some(result);
        }
    }
    fn result(&self) -> Option<Result<(), AudioDeviceReleaseError>> {
        *self.result.lock().unwrap_or_else(|e| e.into_inner())
    }
}
struct DeviceOpenOwner {
    supervisor: Option<std::thread::JoinHandle<()>>,
    completed: Arc<OpenCompletion>,
}
fn poll_device_open_owner(
    owner: &mut Option<DeviceOpenOwner>,
) -> Option<Result<(), AudioDeviceReleaseError>> {
    let Some(active) = owner.as_mut() else {
        return Some(Ok(()));
    };
    if let Some(Err(error)) = active.completed.result() {
        return Some(Err(error));
    }
    let Some(supervisor) = active.supervisor.as_ref() else {
        return Some(Err(AudioDeviceReleaseError::SupervisorFailed));
    };
    if !supervisor.is_finished() {
        return None;
    }
    // Only the fixed join protocol ran on a successful supervisor. The actual
    // opener and any unclaimed native value were already joined through TLS.
    // The final std supervisor epilogue has no hard OS scheduling guarantee.
    let joined = active
        .supervisor
        .take()
        .expect("retained device supervisor")
        .join();
    if joined.is_err() || active.completed.result() != Some(Ok(())) {
        active
            .completed
            .finish(Err(AudioDeviceReleaseError::SupervisorFailed));
        return Some(Err(AudioDeviceReleaseError::SupervisorFailed));
    }
    *owner = None;
    Some(Ok(()))
}
/// Observe the actual retained opener, including cancelled/late result cleanup.
/// Caller cancellation cannot detach it. Call cleanup/rejoin and shutdown must
/// consume this result before reporting release or releasing the call lease.
pub async fn wait_for_audio_device_release() -> Result<(), AudioDeviceReleaseError> {
    wait_for_device_release(DEVICE_RELEASE_TIMEOUT).await
}
async fn wait_for_device_release(bound: Duration) -> Result<(), AudioDeviceReleaseError> {
    let until = tokio::time::Instant::now() + bound;
    loop {
        let released = poll_device_open_owner(
            &mut DEVICE_OPEN_OWNER.lock().unwrap_or_else(|e| e.into_inner()),
        );
        if let Some(result) = released {
            result?;
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            wait_for_retired_native_audio()
                .await
                .map_err(|error| match error {
                    NativeAudioCloseError::TimedOut => AudioDeviceReleaseError::TimedOut,
                    _ => AudioDeviceReleaseError::NativeClose,
                })?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= until {
            return Err(AudioDeviceReleaseError::TimedOut);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
struct OpenValue<T> {
    value: Option<Result<T, MediaError>>,
    retired: bool,
    taken: bool,
}
struct OpenHandoff<T> {
    value: Mutex<OpenValue<T>>,
    changed: Condvar,
    ready: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    waiting: AtomicBool,
    completed: Arc<OpenCompletion>,
}
impl<T> OpenHandoff<T> {
    fn new(completed: Arc<OpenCompletion>, ready: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            value: Mutex::new(OpenValue {
                value: None,
                retired: false,
                taken: false,
            }),
            changed: Condvar::new(),
            ready: Mutex::new(Some(ready)),
            waiting: AtomicBool::new(true),
            completed,
        }
    }
    fn signal_ready(&self) -> bool {
        self.ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some_and(|ready| ready.send(()).is_ok())
    }
    fn failed_start(&self) {
        let mut state = self.value.lock().unwrap_or_else(|e| e.into_inner());
        // A failed actual-thread creation has no native T. Preserve the
        // original spawn error while the retained unit sender prevents a
        // receiver-close race before the supervisor can publish that error.
        state.value = Some(Err(MediaError::Device(
            "could not start audio device setup".into(),
        )));
        if !self.signal_ready() {
            state.retired = true;
            self.waiting.store(false, Ordering::Release);
        }
    }
    fn retire(&self) {
        self.waiting.store(false, Ordering::Release);
        self.value.lock().unwrap_or_else(|e| e.into_inner()).retired = true;
        self.changed.notify_all();
    }
    fn take(&self, controls: &AudioControls) -> Result<T, MediaError> {
        let mut handoff = self.value.lock().unwrap_or_else(|e| e.into_inner());
        if handoff.retired || !self.waiting.load(Ordering::Acquire) || !controls.is_authorized() {
            handoff.retired = true;
            self.waiting.store(false, Ordering::Release);
            self.changed.notify_all();
            return Err(MediaError::CallEnded);
        }
        let Some(value) = handoff.value.take() else {
            return Err(MediaError::Device("audio device setup task failed".into()));
        };
        // No await or fallible operation follows transfer to the caller. Until
        // this exact take, only the actual native opener may destroy the value.
        handoff.taken = true;
        self.completed.handed_off.store(true, Ordering::Release);
        self.changed.notify_all();
        value
    }
}
struct OpenObserver<T> {
    handoff: Arc<OpenHandoff<T>>,
    ready: tokio::sync::oneshot::Receiver<()>,
}
impl<T> Drop for OpenObserver<T> {
    fn drop(&mut self) {
        self.handoff.retire();
    }
}

/// A routing choice: the devices the user chose for a join. IDs are CPAL
/// host-qualified IDs, never display names or enumeration offsets. None alone
/// requests the system default. A running join changes it only through
/// `AudioControls::request_route`, never from configuration on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDeviceSelection {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
}

/// Which device one direction of call audio uses now. Names are platform
/// display names, for showing only: they are never stored or used to find a
/// device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceRoute {
    /// Not open yet.
    #[default]
    Opening,
    /// The chosen route is open. With `system_default`, the system default
    /// was chosen and `name` is the device it resolved to when it opened, when
    /// known (an output that follows the system default may have moved on
    /// since; then it is None).
    Chosen {
        name: Option<String>,
        system_default: bool,
    },
    /// A requested switch could not be opened: the device in use before it
    /// stays in use.
    Previous { name: Option<String> },
    /// The chosen speaker is unavailable, so the system default output plays
    /// instead until the chosen one returns. Never used for the microphone.
    DefaultInstead { name: Option<String> },
    /// Nothing is in use on this side: the device was lost or could not be
    /// opened. For the microphone nothing is captured or sent, and the user's
    /// mute choice is unchanged.
    Lost,
}

/// A route change the user should be told about. The route itself says what
/// holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteNotice {
    /// A requested route change was applied.
    Switched,
    /// The requested microphone could not be opened.
    MicrophoneSwitchFailed,
    /// The requested speaker could not be opened.
    SpeakerSwitchFailed,
    /// The microphone stopped (unplugged or lost).
    MicrophoneLost,
    /// The operating system withdrew microphone access.
    MicrophoneDenied,
    /// The lost microphone came back and is in use again.
    MicrophoneRestored,
    /// The chosen speaker stopped; the system default output plays instead.
    SpeakerFellBack,
    /// The speaker stopped and no output could be opened.
    SpeakerLost,
    /// The chosen speaker is in use again.
    SpeakerRestored,
    /// The system default speaker the user chose stopped; the default output
    /// opened again, possibly on another device.
    DefaultSpeakerReopened,
    /// Call audio resumed (after a rejoin, or a restart after a failure), but
    /// the microphone it had in use has no exact identity to reopen it by, so
    /// it was not reopened: nothing is captured until the chosen exact device
    /// returns or the user chooses again.
    MicrophoneNotResumed,
    /// Reconnecting stopped after its time budget; choosing a device again
    /// tries again.
    ReconnectStopped,
}

/// A route request as the audio loop receives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RouteChoice {
    selection: AudioDeviceSelection,
    /// The user's own repeat of their previous request (`request_route`).
    retry: bool,
}

/// Where one join's audio loop left off (`AudioControls::resume_point`), so
/// the rejoin that replaces a lost join resumes it under the rules for a
/// device lost inside a join instead of opening the route afresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioResume {
    /// The route the devices answered, side by side.
    route: AudioDeviceSelection,
    /// The last route request the loop handled, so a choice made while the
    /// call reconnected still counts as a new one.
    last_request: AudioDeviceSelection,
    /// The microphone's ID: the open one (only when exact), or the lost one
    /// waited for. `resume_microphone` uses it only when `exact_identity`
    /// validates it; otherwise no microphone is reopened on the rejoin.
    microphone: Option<String>,
    microphone_lost: bool,
    microphone_loss: RouteNotice,
    /// The system default output stood in for the lost chosen speaker.
    speaker_fallback: bool,
    /// No speaker was playing (a lost stand-in is not replaced by another).
    speaker_lost: bool,
    /// The channel count the microphone track was published with.
    channels: u32,
}

impl AudioResume {
    /// For a rejoin that replaces a join whose audio never recorded where it
    /// was (lost before its devices opened): resume `chosen` conservatively.
    /// The microphone is reopened only when `chosen` names an exact device;
    /// System default or an alias stays closed, unresumed, until the user
    /// chooses again, because it may name a different microphone now. The
    /// speaker follows its route.
    pub fn unestablished(chosen: &AudioDeviceSelection) -> Self {
        Self {
            route: chosen.clone(),
            last_request: chosen.clone(),
            microphone: chosen.microphone.clone().filter(|id| exact_identity(id)),
            microphone_lost: false,
            microphone_loss: RouteNotice::MicrophoneLost,
            speaker_fallback: false,
            speaker_lost: false,
            channels: 1,
        }
    }

    /// The only microphone a rejoin from this point may capture from without
    /// a new choice: the retained exact ID (opened at once, or waited for if
    /// it was lost). None when the identity is unknown: that microphone
    /// stays closed until the user chooses.
    pub fn retained_microphone(&self) -> Option<String> {
        match resume_microphone(self) {
            ResumeMicrophone::Open(id) => Some(id),
            ResumeMicrophone::Wait { lost, .. } => lost,
        }
    }
}

/// The local audio route of one join, as its audio loop last applied it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioRoute {
    /// Increases with every report, so two equal notices can be told apart.
    pub generation: u64,
    pub microphone: DeviceRoute,
    pub speaker: DeviceRoute,
    pub notice: Option<RouteNotice>,
}

/// What a running stream's error callback reported. Glitches and reroutes
/// keep a stream running; only these end it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamFault {
    /// The device is gone, or its stream must be rebuilt.
    Lost,
    /// The operating system withdrew access.
    Denied,
}

impl StreamFault {
    /// None for errors that leave the stream running: an underrun or
    /// overrun, a refused real-time priority, or an output that followed the
    /// system default to another device.
    fn classify(kind: cpal::ErrorKind) -> Option<Self> {
        match kind {
            cpal::ErrorKind::Xrun
            | cpal::ErrorKind::RealtimeDenied
            | cpal::ErrorKind::DeviceChanged => None,
            cpal::ErrorKind::PermissionDenied => Some(Self::Denied),
            _ => Some(Self::Lost),
        }
    }

    fn error(self, input: bool) -> MediaError {
        MediaError::Device(
            match (self, input) {
                (Self::Lost, true) => "the microphone was disconnected or stopped",
                (Self::Lost, false) => "the speaker was disconnected or stopped",
                (Self::Denied, true) => "microphone access was denied",
                (Self::Denied, false) => "speaker access was denied",
            }
            .into(),
        )
    }
}

/// A stream's health, written by the platform's error callback (on a backend
/// thread, so it only stores and wakes) and read by the audio loop, which it
/// wakes at once instead of at its next frame or roster tick.
#[derive(Default)]
struct StreamHealth {
    fault: Mutex<Option<StreamFault>>,
    rerouted: AtomicBool,
    wake: Notify,
}

impl StreamHealth {
    fn report(&self, kind: cpal::ErrorKind) {
        match StreamFault::classify(kind) {
            Some(fault) => {
                if let Ok(mut slot) = self.fault.lock() {
                    slot.get_or_insert(fault);
                }
            }
            None if kind == cpal::ErrorKind::DeviceChanged => {
                self.rerouted.store(true, Ordering::Release);
            }
            None => return,
        }
        self.wake.notify_one();
    }

    fn take_fault(&self) -> Option<StreamFault> {
        self.fault.lock().ok()?.take()
    }

    fn take_rerouted(&self) -> bool {
        self.rerouted.swap(false, Ordering::AcqRel)
    }
}

/// Whether an ID names one physical endpoint well enough to reconnect to it
/// without the user choosing again. CoreAudio UIDs and WASAPI endpoint IDs
/// do. ALSA routing aliases (`default`, `pulse`, `sysdefault`) and numeric
/// cards (`CARD=0`), which can name another card after a replug, do not.
fn exact_identity(id: &str) -> bool {
    let Some((host, device)) = id.split_once(':') else {
        return false;
    };
    if host != "alsa" {
        return !device.trim().is_empty();
    }
    let Some(card) = device.split("CARD=").nth(1) else {
        return false;
    };
    let card = card.split(',').next().unwrap_or("");
    !card.is_empty() && !card.chars().all(|character| character.is_ascii_digit())
}

/// The microphone to reopen after it was lost, without the user choosing
/// again: the chosen exact device, or, when the system default was chosen,
/// the same physical device that was in use. Never a different microphone,
/// so a lost one is never silently replaced by another that can hear the
/// room. None when no ID is exact enough to wait for.
fn microphone_reopen_target(chosen: Option<&str>, lost: Option<&str>) -> Option<String> {
    let target = chosen.or(lost)?;
    exact_identity(target).then(|| target.to_owned())
}

/// What a route request does on the microphone side.
#[derive(Debug, PartialEq, Eq)]
enum MicrophoneStep {
    /// Leave that side as it is.
    Keep,
    /// Open the choice, and report it if it cannot be opened: it changed
    /// since the previous request, it is a choice that could not be opened
    /// before (while a microphone is in use), or the user repeated the whole
    /// request unchanged while the microphone is lost (`retry`).
    Open(Option<String>),
    /// The choice did not change and the microphone is lost: reopen only its
    /// exact device, as reconnecting would, and keep waiting quietly if that
    /// fails.
    Reopen(String),
}

/// Requests carry the whole selection, so most leave the microphone choice
/// as it was (they change the speaker). Such a request never opens the
/// current system default or an alias in place of a lost microphone: those
/// may now name a different microphone that can hear the room. Only an
/// explicit microphone choice opens as given: one that differs from the
/// previous request, or `retry`, the user's own request repeated with
/// nothing changed (so not a speaker-only change).
fn microphone_step(
    wanted: Option<&str>,
    previous_request: Option<&str>,
    current: Option<&str>,
    in_use: bool,
    lost: Option<&str>,
    retry: bool,
) -> MicrophoneStep {
    if wanted != previous_request || (in_use && wanted != current) || (retry && !in_use) {
        return MicrophoneStep::Open(wanted.map(str::to_owned));
    }
    if in_use {
        return MicrophoneStep::Keep;
    }
    match microphone_reopen_target(wanted, lost) {
        Some(target) => MicrophoneStep::Reopen(target),
        None => MicrophoneStep::Keep,
    }
}

/// The speaker to reopen while the chosen one is lost or replaced by the
/// default: the chosen exact device, or the system default when that is the
/// choice. Output only, so following the choice never opens a microphone.
fn speaker_reopen_target(chosen: Option<&str>) -> Option<Option<String>> {
    match chosen {
        None => Some(None),
        Some(id) => exact_identity(id).then(|| Some(id.to_owned())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDeviceCatalog {
    pub microphones: Vec<AudioDevice>,
    pub speakers: Vec<AudioDevice>,
}

/// Enumerate the platform's default backend without opening an audio stream.
/// Platform enumeration can block; callers must keep it off the UI thread.
pub fn audio_devices() -> Result<AudioDeviceCatalog, MediaError> {
    let host = cpal::default_host();
    let collect = |input: bool| -> Result<Vec<AudioDevice>, MediaError> {
        let devices = host
            .devices()
            .map_err(|_| MediaError::Device("could not enumerate audio devices".into()))?;
        let mut entries = Vec::new();
        for device in devices.take(512) {
            if !(if input {
                device.supports_input()
            } else {
                device.supports_output()
            }) {
                continue;
            }
            let (Ok(id), Ok(description)) = (device.id(), device.description()) else {
                continue;
            };
            entries.push(AudioDevice {
                id: id.to_string(),
                name: description.name().to_owned(),
            });
        }
        // Duplicate IDs cannot identify one endpoint. Do not offer either.
        let mut counts = HashMap::new();
        for entry in &entries {
            *counts.entry(entry.id.clone()).or_insert(0) += 1;
        }
        entries.retain(|entry| counts[&entry.id] == 1);
        entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(entries)
    };
    Ok(AudioDeviceCatalog {
        microphones: collect(true)?,
        speakers: collect(false)?,
    })
}

fn unique_device_index<'a>(ids: impl Iterator<Item = &'a str>, selected: &str) -> Option<usize> {
    let mut matches = ids
        .enumerate()
        .filter_map(|(index, id)| (id == selected).then_some(index));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn resolve_device(selected: Option<&str>, input: bool) -> Result<cpal::Device, MediaError> {
    let host = cpal::default_host();
    let unavailable = || {
        if input {
            MediaError::MicrophoneUnavailable
        } else {
            MediaError::SpeakerUnavailable
        }
    };
    let Some(selected) = selected else {
        return (if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        })
        .ok_or_else(unavailable);
    };
    // Re-enumerate at open time. An old picker row, unplugged endpoint, foreign
    // host, or duplicated ID must never fall back to a microphone with the same
    // name or to the default microphone.
    let devices = host
        .devices()
        .map_err(|_| unavailable())?
        .filter(|device| {
            if input {
                device.supports_input()
            } else {
                device.supports_output()
            }
        })
        .filter_map(|device| device.id().ok().map(|id| (id.to_string(), device)))
        .collect::<Vec<_>>();
    let index = unique_device_index(devices.iter().map(|(id, _)| id.as_str()), selected)
        .ok_or_else(unavailable)?;
    Ok(devices
        .into_iter()
        .nth(index)
        .expect("resolved enumeration index")
        .1)
}

/// The opened device's host-qualified ID and display name, when the platform
/// reports them. Reading them opens nothing.
fn identify(device: &cpal::Device) -> (Option<String>, Option<String>) {
    let id = device.id().ok().map(|id| id.to_string());
    let name = device
        .description()
        .ok()
        .map(|description| description.name().to_owned())
        .filter(|name| !name.trim().is_empty());
    (id, name)
}

fn while_open_active<T>(
    active: &impl Fn() -> bool,
    action: impl FnOnce() -> Result<T, MediaError>,
) -> Result<T, MediaError> {
    if !active() {
        return Err(MediaError::CallEnded);
    }
    let result = action()?;
    if !active() {
        return Err(MediaError::CallEnded);
    }
    Ok(result)
}

// Keep the startup order testable without opening a real device. Cancellation
// cannot interrupt an OS call already entered, but its result is discarded and
// no subsequent prepare/start step is authorized after that call returns.
fn prepare_audio_pair<M, S>(
    active: &impl Fn() -> bool,
    prepare_microphone: impl FnOnce() -> Result<M, MediaError>,
    prepare_speaker: impl FnOnce() -> Result<S, MediaError>,
    start_speaker: impl FnOnce(&S) -> Result<(), MediaError>,
    start_microphone: impl FnOnce(&M) -> Result<(), MediaError>,
) -> Result<(M, S), MediaError> {
    let microphone = while_open_active(active, prepare_microphone)?;
    let speaker = while_open_active(active, prepare_speaker)?;
    while_open_active(active, || start_speaker(&speaker))?;
    while_open_active(active, || start_microphone(&microphone))?;
    Ok((microphone, speaker))
}

/// Native microphone PCM capture. Frames are 10 ms at 48 kHz, with one or two
/// channels matching the selected input device. Keep this value alive while
/// pumping `next_frame()` into a microphone `Publication`.
pub struct MicrophoneCapture {
    _stream: Stream,
    rx: mpsc::Receiver<AudioFrame>,
    channels: u32,
    health: Arc<StreamHealth>,
    /// The opened device's host-qualified ID and display name, when the
    /// platform reports them. The ID is the one to wait for after a loss; the
    /// name is for showing only.
    id: Option<String>,
    name: Option<String>,
}

impl MicrophoneCapture {
    /// Legacy display-only enumeration. Use `audio_devices` for selectable IDs.
    pub fn devices() -> Result<Vec<String>, MediaError> {
        cpal::default_host()
            .input_devices()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .map(|device| {
                device
                    .description()
                    .map(|description| description.name().to_owned())
                    .map_err(|error| MediaError::Device(error.to_string()))
            })
            .collect()
    }

    pub fn open_default() -> Result<Self, MediaError> {
        let stream = Self::prepare_selected(None, &|| true)?;
        stream.start(&|| true)?;
        Ok(stream)
    }

    fn prepare_selected(
        selected: Option<&str>,
        active: &impl Fn() -> bool,
    ) -> Result<Self, MediaError> {
        let (device, id, name) = while_open_active(active, || {
            let device = resolve_device(selected, true)?;
            let (id, name) = identify(&device);
            Ok((device, id, name))
        })?;
        let config = while_open_active(active, || select_48k_config(&device, true))?;
        let channels = u32::from(config.channels());
        let sample_format = config.sample_format();
        let chunk_len = CHUNK_SAMPLES_PER_CHANNEL * channels as usize;
        let (tx, rx) = mpsc::channel(32);
        let health = Arc::new(StreamHealth::default());
        let health_for_callback = Arc::clone(&health);
        let mut pending = VecDeque::<i16>::with_capacity(chunk_len * 2);
        let stream = while_open_active(active, || {
            device
                .build_input_stream_raw(
                    config.config(),
                    sample_format,
                    move |data, _| {
                        match sample_format {
                            SampleFormat::I16 => {
                                if let Some(samples) = data.as_slice::<i16>() {
                                    pending.extend(samples.iter().copied());
                                }
                            }
                            SampleFormat::F32 => {
                                if let Some(samples) = data.as_slice::<f32>() {
                                    pending.extend(samples.iter().map(|sample| {
                                        (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
                                    }));
                                }
                            }
                            SampleFormat::U16 => {
                                if let Some(samples) = data.as_slice::<u16>() {
                                    pending.extend(
                                        samples
                                            .iter()
                                            .map(|sample| (i32::from(*sample) - 32768) as i16),
                                    );
                                }
                            }
                            _ => return,
                        }
                        while pending.len() >= chunk_len {
                            let frame = AudioFrame {
                                data: pending.drain(..chunk_len).collect(),
                                sample_rate: SAMPLE_RATE,
                                num_channels: channels,
                                samples_per_channel: CHUNK_SAMPLES_PER_CHANNEL as u32,
                            };
                            // The device callback must never wait for the network.
                            let _ = tx.try_send(frame);
                        }
                    },
                    move |error| health_for_callback.report(error.kind()),
                    None,
                )
                .map_err(|error| MediaError::Device(error.to_string()))
        })?;
        Ok(Self {
            _stream: stream,
            rx,
            channels,
            health,
            id,
            name,
        })
    }

    fn start(&self, active: &impl Fn() -> bool) -> Result<(), MediaError> {
        while_open_active(active, || {
            self._stream
                .play()
                .map_err(|_| MediaError::Device("could not start microphone".into()))
        })
    }

    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    fn discard_buffered_frames(&mut self) {
        // Publishing can take longer than the capture queue. Never start a
        // call by transmitting audio recorded before the track was unmuted.
        while self.rx.try_recv().is_ok() {}
    }

    /// The next captured frame. A device loss or withdrawn access ends it with
    /// an error, also while no frame arrives; glitches do not.
    pub async fn next_frame(&mut self) -> Result<AudioFrame, MediaError> {
        loop {
            if let Some(fault) = self.health.take_fault() {
                return Err(fault.error(true));
            }
            tokio::select! {
                frame = self.rx.recv() => {
                    return frame
                        .ok_or_else(|| MediaError::Device("microphone stream stopped".into()));
                }
                () = self.health.wake.notified() => {}
            }
        }
    }

    /// Forward microphone frames until the publication closes or this future
    /// is cancelled. Capture errors and transport errors remain distinct.
    pub async fn pump_into(&mut self, publication: &Publication) -> Result<(), MediaError> {
        loop {
            publication.push_audio(self.next_frame().await?).await?;
        }
    }
}

/// Native speaker playback with one bounded queue per remote participant.
/// The device callback sums available PCM from all participants and clips at
/// the output format's limit. Streams that underflow contribute silence.
pub struct AudioOutput {
    _stream: Stream,
    mixer: Arc<Mutex<AudioMixer>>,
    health: Arc<StreamHealth>,
    /// The opened device's display name, for showing only.
    name: Option<String>,
}

struct AudioMixer {
    channels: u32,
    streams: HashMap<String, VecDeque<i16>>,
    deafened: bool,
}

impl AudioMixer {
    fn new(channels: u32) -> Self {
        Self {
            channels,
            streams: HashMap::new(),
            deafened: false,
        }
    }

    fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
        // Never play audio buffered before the user un-deafens.
        self.streams.clear();
    }

    fn push_frame(&mut self, member_id: &str, frame: &AudioFrame) -> Result<(), MediaError> {
        if frame.sample_rate != SAMPLE_RATE
            || !(1..=2).contains(&frame.num_channels)
            || frame.data.len() != frame.samples_per_channel as usize * frame.num_channels as usize
        {
            return Err(MediaError::FrameFormatMismatch);
        }
        let output_samples = frame.samples_per_channel as usize * self.channels as usize;
        if output_samples > MAX_OUTPUT_BUFFER_SAMPLES {
            return Err(MediaError::FrameFormatMismatch);
        }
        if self.deafened {
            return Ok(());
        }
        let queue = self.streams.entry(member_id.to_owned()).or_default();
        if queue.len() + output_samples > MAX_OUTPUT_BUFFER_SAMPLES {
            queue.clear(); // Preserve low latency after a stalled output device.
        }
        remix_channels(&frame.data, frame.num_channels, self.channels, queue);
        Ok(())
    }

    fn remove_stream(&mut self, member_id: &str) {
        self.streams.remove(member_id);
    }

    fn next_sample(&mut self) -> i16 {
        if self.deafened {
            return 0;
        }
        self.streams
            .values_mut()
            .map(|queue| i64::from(queue.pop_front().unwrap_or(0)))
            .sum::<i64>()
            .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
    }
}

impl AudioOutput {
    /// Legacy display-only enumeration. Use `audio_devices` for selectable IDs.
    pub fn devices() -> Result<Vec<String>, MediaError> {
        cpal::default_host()
            .output_devices()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .map(|device| {
                device
                    .description()
                    .map(|description| description.name().to_owned())
                    .map_err(|error| MediaError::Device(error.to_string()))
            })
            .collect()
    }

    pub fn open_default() -> Result<Self, MediaError> {
        let stream = Self::prepare_selected(None, &|| true)?;
        stream.start(&|| true)?;
        Ok(stream)
    }

    fn prepare_selected(
        selected: Option<&str>,
        active: &impl Fn() -> bool,
    ) -> Result<Self, MediaError> {
        let (device, name) = while_open_active(active, || {
            let device = resolve_device(selected, false)?;
            let (_, name) = identify(&device);
            Ok((device, name))
        })?;
        let config = while_open_active(active, || select_48k_config(&device, false))?;
        let channels = u32::from(config.channels());
        let sample_format = config.sample_format();
        let mixer = Arc::new(Mutex::new(AudioMixer::new(channels)));
        let mixer_for_callback = Arc::clone(&mixer);
        let health = Arc::new(StreamHealth::default());
        let health_for_callback = Arc::clone(&health);
        let stream = while_open_active(active, || {
            device
                .build_output_stream_raw(
                    config.config(),
                    sample_format,
                    move |data, _| {
                        // A contended mutex must produce silence: output buffers
                        // are not guaranteed to be pre-filled by the backend.
                        let mut mixer = mixer_for_callback.try_lock().ok();
                        match sample_format {
                            SampleFormat::I16 => {
                                if let Some(output) = data.as_slice_mut::<i16>() {
                                    for sample in output {
                                        *sample =
                                            mixer.as_mut().map(|m| m.next_sample()).unwrap_or(0);
                                    }
                                }
                            }
                            SampleFormat::F32 => {
                                if let Some(output) = data.as_slice_mut::<f32>() {
                                    for sample in output {
                                        *sample = f32::from(
                                            mixer.as_mut().map(|m| m.next_sample()).unwrap_or(0),
                                        ) / f32::from(i16::MAX);
                                    }
                                }
                            }
                            SampleFormat::U16 => {
                                if let Some(output) = data.as_slice_mut::<u16>() {
                                    for sample in output {
                                        *sample = (i32::from(
                                            mixer.as_mut().map(|m| m.next_sample()).unwrap_or(0),
                                        ) + 32768)
                                            as u16;
                                    }
                                }
                            }
                            _ => {}
                        }
                    },
                    move |error| health_for_callback.report(error.kind()),
                    None,
                )
                .map_err(|error| MediaError::Device(error.to_string()))
        })?;
        Ok(Self {
            _stream: stream,
            mixer,
            health,
            name,
        })
    }

    /// Compatibility path for callers with a single remote stream.
    pub fn push_frame(&self, frame: &AudioFrame) -> Result<(), MediaError> {
        self.push_frame_for("_single_stream", frame)
    }

    fn start(&self, active: &impl Fn() -> bool) -> Result<(), MediaError> {
        while_open_active(active, || {
            self._stream
                .play()
                .map_err(|_| MediaError::Device("could not start speaker".into()))
        })
    }

    pub fn push_frame_for(&self, member_id: &str, frame: &AudioFrame) -> Result<(), MediaError> {
        if let Some(fault) = self.health.take_fault() {
            return Err(fault.error(false));
        }
        self.mix(member_id, frame)
    }

    /// Queue a remote frame without reading the stream's health: the call
    /// audio loop watches that itself and replaces a lost speaker.
    fn mix(&self, member_id: &str, frame: &AudioFrame) -> Result<(), MediaError> {
        self.mixer
            .lock()
            .map_err(|_| MediaError::Device("speaker mixer poisoned".into()))?
            .push_frame(member_id, frame)
    }

    pub fn remove_stream(&self, member_id: &str) -> Result<(), MediaError> {
        self.mixer
            .lock()
            .map_err(|_| MediaError::Device("speaker mixer poisoned".into()))?
            .remove_stream(member_id);
        Ok(())
    }

    /// Silence local playback immediately and discard buffered remote audio.
    /// This is local only; it does not signal a MatrixRTC stream mute to peers.
    pub fn set_deafened(&self, deafened: bool) -> Result<(), MediaError> {
        self.mixer
            .lock()
            .map_err(|_| MediaError::Device("speaker mixer poisoned".into()))?
            .set_deafened(deafened);
        Ok(())
    }
}

/// Applied local audio state for one call join. Microphone mute is published
/// to peers; deafen only silences this device's speaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioState {
    pub microphone_muted: bool,
    pub deafened: bool,
}

struct AudioControlState {
    desired: AudioState,
    applied: AudioState,
    /// Outgoing audio is withheld while the host cannot follow signalling.
    /// This never changes `desired`, so the user's mute choice survives.
    suspended: bool,
    microphone: Option<Arc<Publication>>,
    speaker_mixer: Option<Arc<Mutex<AudioMixer>>>,
}

struct AudioAuthorization {
    cancelled: AtomicBool,
    changed: Notify,
    allowed: Box<dyn Fn() -> bool + Send + Sync>,
    /// Read at every microphone push. Unlike `allowed`, a false result only
    /// withholds audio; it never revokes these controls or closes a device.
    may_transmit: Box<dyn Fn() -> bool + Send + Sync>,
}

impl AudioAuthorization {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}

/// Commands for one account, room, and MatrixRTC membership. Old controls
/// cannot operate a subsequent join, even if it is in the same room.
#[derive(Clone)]
pub struct AudioControls {
    lease: CallLease,
    authorization: Arc<AudioAuthorization>,
    state: Arc<Mutex<AudioControlState>>,
    requested: watch::Sender<AudioState>,
    applied: watch::Sender<AudioState>,
    running: Arc<AtomicBool>,
    /// The route the user chose for this join (`request_route`).
    route_request: watch::Sender<RouteChoice>,
    /// The route the audio loop last applied.
    route: watch::Sender<AudioRoute>,
    /// Where the audio loop left off; None until it opened any device.
    resume: watch::Sender<Option<AudioResume>>,
}

impl AudioControls {
    fn new(lease: CallLease) -> Self {
        Self::new_cancellable(lease, || true)
    }

    fn new_cancellable(
        lease: CallLease,
        allowed: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self::new_gated(lease, allowed, || true)
    }

    fn new_gated(
        lease: CallLease,
        allowed: impl Fn() -> bool + Send + Sync + 'static,
        may_transmit: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        let desired = AudioState {
            microphone_muted: false,
            deafened: false,
        };
        let applied = AudioState {
            microphone_muted: true,
            deafened: false,
        };
        let (requested, _) = watch::channel(desired);
        let (applied_updates, _) = watch::channel(applied);
        Self {
            lease,
            authorization: Arc::new(AudioAuthorization {
                cancelled: AtomicBool::new(false),
                changed: Notify::new(),
                allowed: Box::new(allowed),
                may_transmit: Box::new(may_transmit),
            }),
            state: Arc::new(Mutex::new(AudioControlState {
                desired,
                applied,
                suspended: false,
                microphone: None,
                speaker_mixer: None,
            })),
            requested,
            applied: applied_updates,
            running: Arc::new(AtomicBool::new(false)),
            route_request: watch::channel(RouteChoice::default()).0,
            route: watch::channel(AudioRoute::default()).0,
            resume: watch::channel(None).0,
        }
    }

    /// Choose the devices for this join. Starting the audio loop sets the
    /// route it was given. While it runs, the loop opens the new device beside
    /// the one in use and swaps only once it is open, keeping the published
    /// microphone track, its encryption and the mute and deafen state; a
    /// device that cannot be opened leaves the one in use and is reported in
    /// `route`. Pass only IDs from `audio_devices`, or None for the system
    /// default. Nothing here is persisted.
    ///
    /// `retry`: the user's request is unchanged from the route the call runs
    /// with (decided where requests are admitted one by one, so two coalesced
    /// changes never look like one). Only then may a microphone that is lost
    /// or was not resumed be opened again by its route; a request that
    /// changes only the speaker never opens one.
    pub fn request_route(
        &self,
        selection: AudioDeviceSelection,
        retry: bool,
    ) -> Result<(), MediaError> {
        self.ensure_authorized()?;
        self.route_request
            .send_replace(RouteChoice { selection, retry });
        Ok(())
    }

    /// The route the user chose for this join. Cancellation does not change
    /// it, so a rejoin can carry it to new controls.
    pub fn requested_route(&self) -> AudioDeviceSelection {
        self.route_request.borrow().selection.clone()
    }

    /// The route the audio loop last applied: which device each direction
    /// uses, and the last change worth telling the user about.
    pub fn route(&self) -> AudioRoute {
        self.route.borrow().clone()
    }

    pub fn subscribe_route(&self) -> watch::Receiver<AudioRoute> {
        self.route.subscribe()
    }

    /// Where this join's audio loop left off, for `RtcService::run_audio_resuming`
    /// on the rejoin that replaces it: the route, the microphone's exact ID, a
    /// lost microphone still waited for, a speaker still standing in or lost,
    /// and the last microphone request. Recorded synchronously once the
    /// devices are open, before the first publication await
    /// (`prepare_publication`), then on each report. None only while the
    /// loop has not got that far; keep the previous point then.
    pub fn resume_point(&self) -> Option<AudioResume> {
        self.resume.borrow().clone()
    }

    fn publish_route(
        &self,
        microphone: DeviceRoute,
        speaker: DeviceRoute,
        notice: Option<RouteNotice>,
    ) {
        self.route.send_modify(|route| {
            route.generation = route.generation.wrapping_add(1);
            route.microphone = microphone;
            route.speaker = speaker;
            route.notice = notice;
        });
    }

    /// Point deafen and playback at a newly opened speaker. A new mixer starts
    /// undeafened, so the user's deafen choice is applied before it plays.
    fn install_speaker(&self, speaker: &AudioOutput) -> Result<(), MediaError> {
        let mut state = self.lock_state()?;
        self.ensure_authorized()?;
        speaker.set_deafened(state.desired.deafened)?;
        state.speaker_mixer = Some(Arc::clone(&speaker.mixer));
        state.applied.deafened = state.desired.deafened;
        self.applied.send_replace(state.applied);
        Ok(())
    }

    /// The speaker is gone: deafen changes are kept as requested and applied
    /// to the next one.
    fn release_speaker(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.speaker_mixer = None;
        }
    }

    pub fn call_info(&self) -> &CallInfo {
        &self.lease.info
    }

    pub fn account_user_id(&self) -> &str {
        &self.lease.account_user_id
    }

    pub fn account_device_id(&self) -> &str {
        &self.lease.account_device_id
    }

    pub fn applied_state(&self) -> AudioState {
        *self.applied.borrow()
    }

    pub fn subscribe_applied_state(&self) -> watch::Receiver<AudioState> {
        self.applied.subscribe()
    }

    /// The user's requested mute and deafen state. Unlike `applied_state`,
    /// cancellation and transmission pauses never change it.
    pub fn requested_state(&self) -> AudioState {
        *self.requested.borrow()
    }

    /// Seed new controls for a rejoined membership with the previous join's
    /// requested state before its audio loop starts. Devices are untouched;
    /// the loop applies the state when it attaches the new publication.
    pub fn preset_requested_state(&self, requested: AudioState) -> Result<(), MediaError> {
        self.ensure_authorized()?;
        if self.running.load(Ordering::Acquire) {
            return Err(MediaError::ControlAlreadyRunning);
        }
        let mut state = self.lock_state()?;
        self.ensure_authorized()?;
        state.desired = requested;
        self.requested.send_replace(requested);
        Ok(())
    }

    /// Withhold or resume outgoing microphone audio without changing the
    /// requested mute state or reopening a device. Pausing mutes the live
    /// publication at once. Resuming only wakes the capture loop, which
    /// discards buffered frames before applying a still-requested unmute.
    pub fn set_transmission_suspended(&self, suspended: bool) -> Result<(), MediaError> {
        self.ensure_authorized()?;
        let mut state = self.lock_state()?;
        self.ensure_authorized()?;
        state.suspended = suspended;
        if suspended {
            // Record the pause before touching the transport so a failing
            // mute still stops the capture loop from sending frames.
            state.applied.microphone_muted = true;
            self.applied.send_replace(state.applied);
            if let Some(publication) = &state.microphone {
                publication.set_muted(true)?;
            }
        }
        self.requested.send_replace(state.desired);
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.is_authorized() && self.running.load(Ordering::Acquire)
    }

    /// Permanently stop this join's audio permission, even while a device-open
    /// result receiver is retained or the caller is awaiting other media work.
    /// This does not interrupt an OS operation that has already been entered.
    pub fn cancel(&self) {
        self.authorization.cancel();
        if let Ok(mut state) = self.state.lock() {
            if let Some(publication) = state.microphone.take() {
                let _ = publication.set_muted(true);
            }
            if let Some(mixer) = state.speaker_mixer.take()
                && let Ok(mut mixer) = mixer.lock()
            {
                mixer.set_deafened(true);
            }
            state.applied.microphone_muted = true;
            self.applied.send_replace(state.applied);
        }
    }

    fn is_authorized(&self) -> bool {
        if self.authorization.cancelled.load(Ordering::Acquire) {
            return false;
        }
        if !self.lease.is_active() || !(self.authorization.allowed)() {
            self.authorization.cancel();
            return false;
        }
        !self.authorization.cancelled.load(Ordering::Acquire)
    }

    fn ensure_authorized(&self) -> Result<(), MediaError> {
        self.is_authorized()
            .then_some(())
            .ok_or(MediaError::CallEnded)
    }

    async fn cancelled(&self) {
        loop {
            let changed = self.authorization.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.is_authorized() {
                return;
            }
            changed.await;
        }
    }

    fn is_for(&self, lease: &CallLease) -> bool {
        lease.same_join(&self.lease)
    }

    /// Mute reaches the published track synchronously. Unmute is queued for
    /// the capture loop, which discards old microphone frames first. The
    /// `applied_state` watch reports when that unmute actually takes effect.
    pub fn set_microphone_muted(&self, muted: bool) -> Result<(), MediaError> {
        self.ensure_running()?;
        let mut state = self.lock_state()?;
        self.ensure_authorized()?;
        if muted {
            if let Some(publication) = &state.microphone {
                publication.set_muted(true)?;
                state.applied.microphone_muted = true;
                self.applied.send_replace(state.applied);
            }
        }
        state.desired.microphone_muted = muted;
        self.requested.send_replace(state.desired);
        Ok(())
    }

    /// Stop local speaker playback and clear buffered PCM immediately. Peers
    /// still send their microphone tracks and receive our microphone normally.
    pub fn set_deafened(&self, deafened: bool) -> Result<(), MediaError> {
        self.ensure_running()?;
        let mut state = self.lock_state()?;
        self.ensure_authorized()?;
        if let Some(mixer) = &state.speaker_mixer {
            mixer
                .lock()
                .map_err(|_| MediaError::Device("speaker mixer poisoned".into()))?
                .set_deafened(deafened);
            state.applied.deafened = deafened;
            self.applied.send_replace(state.applied);
        }
        state.desired.deafened = deafened;
        self.requested.send_replace(state.desired);
        Ok(())
    }

    fn ensure_running(&self) -> Result<(), MediaError> {
        self.ensure_authorized()?;
        if !self.running.load(Ordering::Acquire) {
            return Err(MediaError::ControlNotRunning);
        }
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, AudioControlState>, MediaError> {
        self.state
            .lock()
            .map_err(|_| MediaError::Device("audio controls poisoned".into()))
    }

    fn attach(
        &self,
        publication: Arc<Publication>,
        speaker: Option<&AudioOutput>,
        microphone: Option<&mut MicrophoneCapture>,
    ) -> Result<(), MediaError> {
        // The transport publishes muted, including while the OS devices open.
        if let Some(microphone) = microphone {
            microphone.discard_buffered_frames();
        }
        let mut state = self.lock_state()?;
        self.ensure_authorized()?;
        if let Some(speaker) = speaker {
            speaker.set_deafened(state.desired.deafened)?;
        }
        if !state.desired.microphone_muted && !state.suspended {
            publication.set_muted(false)?;
        }
        state.applied = state.desired;
        if state.suspended {
            state.applied.microphone_muted = true;
        }
        state.microphone = Some(publication);
        // Without a speaker, deafen applies to the next one (install_speaker).
        state.speaker_mixer = speaker.map(|speaker| Arc::clone(&speaker.mixer));
        self.applied.send_replace(state.applied);
        Ok(())
    }

    fn apply_pending_unmute(
        &self,
        microphone: Option<&mut MicrophoneCapture>,
    ) -> Result<(), MediaError> {
        self.ensure_authorized()?;
        let needs_unmute = {
            let state = self.lock_state()?;
            !state.suspended && !state.desired.microphone_muted && state.applied.microphone_muted
        };
        if needs_unmute {
            // A lost microphone captures nothing; the requested unmute still
            // reaches the track, which sends silence until one is back.
            if let Some(microphone) = microphone {
                microphone.discard_buffered_frames();
            }
            let mut state = self.lock_state()?;
            self.ensure_authorized()?;
            if !state.suspended && !state.desired.microphone_muted && state.applied.microphone_muted
            {
                if let Some(publication) = &state.microphone {
                    publication.set_muted(false)?;
                    state.applied.microphone_muted = false;
                    self.applied.send_replace(state.applied);
                }
            }
        }
        Ok(())
    }

    fn should_send_microphone(&self) -> Result<bool, MediaError> {
        self.ensure_authorized()?;
        let state = self.lock_state()?;
        let send = !state.suspended
            && !state.applied.microphone_muted
            && (self.authorization.may_transmit)();
        Ok(send)
    }
}

struct AudioRunGuard<'a>(&'a AudioControls);

impl AudioRunGuard<'_> {
    fn start(controls: &AudioControls) -> Result<AudioRunGuard<'_>, MediaError> {
        controls.ensure_authorized()?;
        controls
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MediaError::ControlAlreadyRunning)?;
        let guard = AudioRunGuard(controls);
        controls.ensure_authorized()?;
        Ok(guard)
    }
}

impl Drop for AudioRunGuard<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
        self.0.cancel();
    }
}

impl RtcService {
    /// Create local audio controls for this exact join. Pass the handle to
    /// `run_audio_controlled`; retain a clone for UI actions. A handle from a
    /// previous leave/rejoin is rejected before any OS device opens.
    pub fn audio_controls(&self, room_id: &str) -> Result<AudioControls, MediaError> {
        let lease = self.call_lease(room_id).ok_or(MediaError::NotJoined)?;
        Ok(AudioControls::new(lease.clone()))
    }

    /// Bind device startup and audio publication to an additional owner policy.
    /// The predicate runs on async and device-open threads: it must be cheap,
    /// perform no I/O and be independent of UI/task polling. The first false result
    /// permanently revokes these controls; use new controls for a new join.
    pub fn audio_controls_cancellable(
        &self,
        room_id: &str,
        allowed: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<AudioControls, MediaError> {
        let lease = self.call_lease(room_id).ok_or(MediaError::NotJoined)?;
        Ok(AudioControls::new_cancellable(lease.clone(), allowed))
    }

    /// Like `audio_controls_cancellable`, plus a temporary transmission gate
    /// read at every microphone push, independent of any owner task. While it
    /// returns false, captured audio is dropped instead of sent; the controls
    /// stay authorized and the devices stay open. Same predicate rules apply.
    pub fn audio_controls_gated(
        &self,
        room_id: &str,
        allowed: impl Fn() -> bool + Send + Sync + 'static,
        may_transmit: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<AudioControls, MediaError> {
        let lease = self.call_lease(room_id).ok_or(MediaError::NotJoined)?;
        Ok(AudioControls::new_gated(
            lease.clone(),
            allowed,
            may_transmit,
        ))
    }

    /// Run the default microphone and speaker for a joined call.
    /// Call this only after an explicit user action that authorizes microphone
    /// capture. It returns when the call ends or the media path fails; dropping
    /// the future mutes the publication and aborts remote frame pumps. Each
    /// remote microphone gets a bounded playback queue; network jitter control
    /// is still outside this adapter. Live device routing needs the controls:
    /// see `run_audio_controlled_with_devices`.
    pub async fn run_audio(&self, room_id: &str) -> Result<(), MediaError> {
        let controls = self.audio_controls(room_id)?;
        self.run_audio_controlled(room_id, &controls).await
    }

    /// Run the same default audio path with live call-bound mute and deafen
    /// controls. Muting the microphone signals peers through the native
    /// publication; deafen only affects local playback. This future owns the
    /// devices and releases them on cancellation, failure, or call end.
    pub async fn run_audio_controlled(
        &self,
        room_id: &str,
        controls: &AudioControls,
    ) -> Result<(), MediaError> {
        self.run_audio_controlled_with_devices(room_id, controls, AudioDeviceSelection::default())
            .await
    }

    /// Run call audio on `selection`, the route chosen when the join was
    /// requested (it replaces any route requested on `controls` before). While
    /// it runs, only `AudioControls::request_route` changes the route:
    /// configuration changes never reroute an active call on their own.
    ///
    /// Live routing keeps the published microphone track: a new microphone is
    /// opened beside the one in use, its frames are remixed to the published
    /// channel count, and only then does it replace the old one, so the
    /// track's encryption, its mute state and peers' view of it never change.
    /// A new speaker gets the user's deafen choice before it plays. The loop
    /// awaits each open (bounded by `DEVICE_OPEN_TIMEOUT`), so call audio
    /// pauses in both directions while a device opens.
    ///
    /// A device lost mid-call (unplugged, withdrawn, or its stream invalidated)
    /// no longer ends call audio. The microphone side stops capturing and waits
    /// for the chosen exact device (for the system default, the exact one that
    /// was lost), never another one; the speaker side plays through the system
    /// default instead, visibly, until the chosen one returns. Reconnecting
    /// polls every `RECONNECT_INTERVAL` until `RECONNECT_WINDOW` after the last
    /// change, and never starts an open that could run past that. Every
    /// change is reported on `AudioControls::route`.
    pub async fn run_audio_controlled_with_devices(
        &self,
        room_id: &str,
        controls: &AudioControls,
        selection: AudioDeviceSelection,
    ) -> Result<(), MediaError> {
        self.run_audio_resuming(room_id, controls, None, selection, false)
            .await
    }

    /// `run_audio_controlled_with_devices` on `chosen`, resuming where a lost
    /// join's audio left off (`AudioControls::resume_point` of its controls)
    /// when `resume` is given. A rejoin then follows the rules for a device
    /// lost inside a join rather than opening `chosen` afresh: the microphone
    /// in use reopens by exact ID where the platform gives one and the speaker
    /// on its route, a lost microphone is waited for and never replaced, a
    /// speaker standing in for a lost one keeps standing in (a lost stand-in
    /// is not replaced by another), and one side that cannot be opened stays
    /// closed (and looked for) without costing the call the other. Once open, the loop switches to `chosen`
    /// where it differs, as `request_route` would. Without `resume`, a choice
    /// made while the first devices open is tried if they cannot be opened.
    pub async fn run_audio_resuming(
        &self,
        room_id: &str,
        controls: &AudioControls,
        resume: Option<AudioResume>,
        chosen: AudioDeviceSelection,
        retry: bool,
    ) -> Result<(), MediaError> {
        let call = self.call(room_id).ok_or(MediaError::NotJoined)?;
        let lease = self.call_lease(room_id).ok_or(MediaError::NotJoined)?;
        if !controls.is_for(lease) {
            return Err(MediaError::WrongCall);
        }
        if !lease.is_active() {
            return Err(MediaError::CallEnded);
        }
        let _run_guard = AudioRunGuard::start(controls)?;
        let mut requests = controls.requested.subscribe();
        // The chosen route of this join; later requests arrive on `routes`.
        controls.route_request.send_replace(RouteChoice {
            selection: chosen.clone(),
            retry,
        });
        let mut routes = controls.route_request.subscribe();
        let (mut local, opened_notice) = match resume {
            None => {
                let (microphone, speaker, opened) =
                    open_first_route(controls, &mut routes, chosen).await?;
                (LocalAudio::new(opened, microphone, speaker), None)
            }
            Some(resume) => resume_local_audio(controls, resume).await?,
        };
        controls.ensure_authorized()?;
        // Records the resume point before the publication await below.
        let (sample_rate, published_channels) = local.prepare_publication(controls);
        let publication = Arc::new(
            self.publish_microphone(room_id, sample_rate, published_channels, true)
                .await?,
        );
        let _mute_on_drop = MuteOnDrop(&publication);
        controls.ensure_authorized()?;
        let mut events = call.subscribe_call_events();
        let (remote_tx, mut remote_rx) = mpsc::channel(REMOTE_FRAME_CHANNEL_CAPACITY);
        let mut remotes = RemoteAudioStreams::new(remote_tx);
        let mut roster_refresh = tokio::time::interval(Duration::from_secs(2));
        roster_refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // The track keeps the format it was published with; any later
        // microphone is remixed to it (`conform_channels`).
        let result = async {
            controls.attach(
                Arc::clone(&publication),
                local.speaker.as_ref(),
                local.microphone.as_mut(),
            )?;
            let _ = *requests.borrow_and_update();
            controls.apply_pending_unmute(local.microphone.as_mut())?;
            remotes.reconcile(call, local.speaker.as_ref())?;
            local.report(controls, opened_notice);
            // The route that opened may not be the one chosen: the devices in
            // use before a rejoin, or a choice made while they opened.
            let wanted = routes.borrow_and_update().clone();
            // Also a microphone choice made while a rejoin reconnected that
            // differs from the lost join's last request, even when it names
            // the route in use, and the user's explicit retry that restarted
            // audio; `switch` applies the microphone_step rules.
            if wanted.retry
                || wanted.selection != local.current
                || wanted.selection.microphone != local.last_request.microphone
            {
                local.switch(controls, wanted.selection, wanted.retry).await?;
            }
            let mut reconnect = tokio::time::interval(RECONNECT_INTERVAL);
            reconnect.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                let microphone_health = local.microphone_health();
                let speaker_health = local.speaker_health();
                tokio::select! {
                    biased;
                    _ = controls.cancelled() => break Err(MediaError::CallEnded),
                    changed = requests.changed() => {
                        if changed.is_err() {
                            break Ok(());
                        }
                        let _ = *requests.borrow_and_update();
                        controls.apply_pending_unmute(local.microphone.as_mut())?;
                    }
                    changed = routes.changed() => {
                        if changed.is_ok() {
                            let wanted = routes.borrow_and_update().clone();
                            match local.switch(controls, wanted.selection, wanted.retry).await {
                                Err(MediaError::CallEnded) => break Err(MediaError::CallEnded),
                                result => result?,
                            }
                        }
                    }
                    () = stream_fault(microphone_health, speaker_health) => {
                        match local.handle_faults(controls).await {
                            Err(MediaError::CallEnded) => break Err(MediaError::CallEnded),
                            result => result?,
                        }
                        // The device just lost is looked for a full interval later.
                        reconnect.reset();
                    }
                    _ = reconnect.tick(), if local.reconnecting(&controls.requested_route()) => {
                        match local.reconnect(controls).await {
                            Err(MediaError::CallEnded) => break Err(MediaError::CallEnded),
                            result => result?,
                        }
                    }
                    _ = roster_refresh.tick() => {
                        // A frame stream can close without a matching call
                        // event. Retry subscribed tracks at a bounded rate.
                        remotes.reconcile(call, local.speaker.as_ref())?;
                    }
                    frame = next_microphone_frame(&mut local.microphone) => {
                        match frame {
                            Some(frame) => {
                                if controls.should_send_microphone()? {
                                    publication
                                        .push_audio(conform_channels(frame, published_channels))
                                        .await?;
                                }
                            }
                            // The capture stream ended without reporting why.
                            None => {
                                let notice = local.lose_microphone(controls, StreamFault::Lost).await?;
                                local.report(controls, Some(notice));
                                reconnect.reset();
                            }
                        }
                    }
                    event = events.recv() => {
                        match event {
                            Ok(CallEvent::StreamStarted { member_id, kind })
                                if kind == matrix_rtc_transport::MediaStreamKind::Microphone
                                    && member_id != call.membership_id() => {
                                remotes.start(call, local.speaker.as_ref(), &member_id)?;
                            }
                            Ok(CallEvent::StreamStopped { member_id, kind })
                                if kind == matrix_rtc_transport::MediaStreamKind::Microphone => {
                                remotes.stop(local.speaker.as_ref(), &member_id)?;
                            }
                            Ok(CallEvent::ParticipantLeft { member_id }) => {
                                remotes.stop(local.speaker.as_ref(), &member_id)?;
                            }
                            Ok(CallEvent::Ended { .. }) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break Ok(());
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                remotes.reconcile(call, local.speaker.as_ref())?;
                            }
                            Ok(_) => {}
                        }
                    }
                    remote = remote_rx.recv() => {
                        match remote {
                            Some(RemoteAudioEvent::Frame { member_id, generation, frame })
                                if remotes.is_current(&member_id, generation) => {
                                // Without a speaker, remote audio is dropped.
                                if let Some(output) = &local.speaker {
                                    output.mix(&member_id, &frame)?;
                                }
                            }
                            Some(RemoteAudioEvent::Ended { member_id, generation })
                                if remotes.is_current(&member_id, generation) => {
                                remotes.stop(local.speaker.as_ref(), &member_id)?;
                            }
                            Some(_) => {}, // An old stream was replaced or stopped.
                            None => break Ok(()),
                        }
                    }
                }
            }
        }
        .await;
        // Close the devices before unpublishing, as before the loop owned them.
        let _ = publication.set_muted(true);
        drop(local);
        drop(remotes);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let native_close = wait_for_retired_native_audio().await;
        let _ = publication.set_muted(true);
        let _ = publication.unpublish().await;
        result?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        native_close.map_err(|_| MediaError::Device(NATIVE_CLOSE_MESSAGE.into()))?;
        Ok(())
    }

    /// Backward-compatible name for the original two-person proof. The audio
    /// path now receives and mixes every subscribed remote microphone.
    pub async fn run_two_person_audio(&self, room_id: &str) -> Result<(), MediaError> {
        self.run_audio(room_id).await
    }
}

/// How often a lost or replaced device is looked for again, and until when:
/// a monotonic deadline `RECONNECT_WINDOW` after the last change. No device
/// open starts that could run past it (`reconnect_open_fits`), so looking
/// stops by then however long each open takes.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);
const RECONNECT_WINDOW: Duration = Duration::from_secs(10 * 60);
const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a device open started `now` ends, at its timeout, by `deadline`.
fn reconnect_open_fits(now: tokio::time::Instant, deadline: tokio::time::Instant) -> bool {
    now + DEVICE_OPEN_TIMEOUT <= deadline
}

/// Original call authority plus the async observer's live request. A dropped or
/// timed-out observer permanently retires later prepare/start steps, even when
/// the native open already entered must finish on its retained owner thread.
struct OpenGate<'a> {
    controls: &'a AudioControls,
    waiting: &'a AtomicBool,
}
impl OpenGate<'_> {
    fn active(&self) -> bool {
        self.controls.is_authorized() && self.waiting.load(Ordering::Acquire)
    }
}
fn run_device_open<T: Send + 'static>(
    controls: AudioControls,
    handoff: Arc<OpenHandoff<T>>,
    open: impl FnOnce(&OpenGate<'_>) -> Result<T, MediaError>,
) -> Result<(), AudioDeviceReleaseError> {
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let gate = OpenGate {
            controls: &controls,
            waiting: &handoff.waiting,
        };
        if !gate.active() {
            return Err(MediaError::CallEnded);
        }
        open(&gate)
    }));
    let panicked = opened.is_err();
    let result =
        opened.unwrap_or_else(|_| Err(MediaError::Device("audio device setup task failed".into())));
    let mut state = handoff.value.lock().unwrap_or_else(|e| e.into_inner());
    state.value = Some(result);
    if !handoff.signal_ready() {
        state.retired = true;
        handoff.waiting.store(false, Ordering::Release);
    }
    loop {
        if state.taken {
            drop(state);
            return finish_device_open(panicked);
        }
        if state.retired || !handoff.waiting.load(Ordering::Acquire) || !controls.is_authorized() {
            state.retired = true;
            let unclaimed = state.value.take();
            drop(state);
            // A stream/pair never crosses the unit-only readiness channel.
            // Late results and receiver cancellation destroy it here, outside
            // every mutex, before the supervisor can join this thread's TLS.
            drop(unclaimed);
            return finish_device_open(panicked);
        }
        state = handoff
            .changed
            .wait_timeout(state, Duration::from_millis(20))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

fn finish_device_open(panicked: bool) -> Result<(), AudioDeviceReleaseError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let native = close_retired_native_audio();
    if panicked {
        return Err(AudioDeviceReleaseError::WorkerPanicked);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    native.map_err(|_| AudioDeviceReleaseError::NativeClose)?;
    // Hosts without a native close implementation retain only the application worker/TLS receipt.
    Ok(())
}

/// Open devices on one retained native owner. Its supervisor is published
/// before the actual worker may exist, and the global fence clears only after
/// real join/TLS completion. Native open/drop already entered is non-preemptible;
/// waits stay bounded and failed ownership remains fenced. On macOS the worker
/// also consumes the explicit retired-backend close result. Other hosts retain
/// the narrower application-worker/TLS receipt and their native disposal limits.
async fn open_on_device_thread<T: Send + 'static>(
    controls: &AudioControls,
    open: impl FnOnce(&OpenGate<'_>) -> Result<T, MediaError> + Send + 'static,
) -> Result<T, MediaError> {
    open_with_supervisor(controls, open, |supervise| {
        std::thread::Builder::new()
            .name("morpheus-audio-open-owner".into())
            .spawn(supervise)
    })
    .await
}
async fn open_with_supervisor<T: Send + 'static>(
    controls: &AudioControls,
    open: impl FnOnce(&OpenGate<'_>) -> Result<T, MediaError> + Send + 'static,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>> + Send,
) -> Result<T, MediaError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    tokio::select! {
        biased;
        _ = controls.cancelled() => return Err(MediaError::CallEnded),
        closed = wait_for_retired_native_audio() => closed.map_err(|_| MediaError::Device(NATIVE_CLOSE_MESSAGE.into()))?,
    }
    // Sequential microphone/speaker opens must not lose a route merely because
    // a successfully handed-off previous result is finishing its joined tail.
    // A genuinely pending/retired opener still fails busy, never starts a pool.
    let finishing = {
        let mut owner = DEVICE_OPEN_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        match poll_device_open_owner(&mut owner) {
            Some(Ok(())) => false,
            Some(Err(error)) => return Err(MediaError::Device(error.to_string())),
            None => owner
                .as_ref()
                .is_some_and(|active| active.completed.handed_off.load(Ordering::Acquire)),
        }
    };
    if finishing {
        tokio::select! {
            biased;
            _ = controls.cancelled() => return Err(MediaError::CallEnded),
            released = wait_for_audio_device_release() => released.map_err(|error| MediaError::Device(error.to_string()))?,
        }
    }
    let completed = Arc::new(OpenCompletion::default());
    let (ready, received) = tokio::sync::oneshot::channel();
    let handoff = Arc::new(OpenHandoff::new(completed.clone(), ready));
    let mut observer = OpenObserver {
        handoff: handoff.clone(),
        ready: received,
    };
    {
        let mut owner = DEVICE_OPEN_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        match poll_device_open_owner(&mut owner) {
            Some(Ok(())) => {}
            Some(Err(error)) => return Err(MediaError::Device(error.to_string())),
            None => {
                return Err(MediaError::Device(
                    "audio device setup is still in progress".into(),
                ));
            }
        }
        if DEVICE_OPEN_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(MediaError::Device(
                "audio device setup is still in progress".into(),
            ));
        }
        let guard = DeviceOpenGuard {
            release_allowed: true,
        };
        controls.ensure_authorized()?;
        let opening_controls = controls.clone();
        let (start, admitted) = std::sync::mpsc::sync_channel(1);
        let finished = completed.clone();
        let supervisor = spawn(Box::new(move || {
            let mut guard = guard;
            // On failure/drop before publication, no actual worker exists.
            if admitted.recv().is_err() {
                finished.finish(Ok(()));
                return;
            }
            guard.release_allowed = false;
            let worker_handoff = handoff.clone();
            let opened = std::thread::Builder::new()
                .name("morpheus-audio-open".into())
                .spawn(move || run_device_open(opening_controls, worker_handoff, open));
            match opened {
                Err(_) => {
                    // No actual worker exists. Publish the original spawn
                    // error with the retained unit sender; no native T exists.
                    handoff.failed_start();
                    guard.release_allowed = true;
                    finished.finish(Ok(()));
                }
                Ok(actual) => match actual.join() {
                    Ok(Ok(())) => {
                        guard.release_allowed = true;
                        finished.finish(Ok(()));
                    }
                    Ok(Err(error)) => {
                        finished.finish(Err(error));
                    }
                    Err(_) => {
                        // Keep a failed fence. If the fixed protocol panicked
                        // with an unclaimed result, only this retained native
                        // supervisor destroys it, never a Tokio observer Drop.
                        finished.finish(Err(AudioDeviceReleaseError::WorkerPanicked));
                        handoff.retire();
                        let orphan = handoff
                            .value
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .value
                            .take();
                        drop(orphan);
                    }
                },
            }
        }))
        .map_err(|_| MediaError::Device("could not start audio device setup".into()))?;
        *owner = Some(DeviceOpenOwner {
            supervisor: Some(supervisor),
            completed: completed.clone(),
        });
        if start.send(()).is_err() {
            completed.finish(Err(AudioDeviceReleaseError::SupervisorFailed));
        }
    }
    tokio::select! {
        biased;
        _ = controls.cancelled() => Err(MediaError::CallEnded),
        received = tokio::time::timeout(DEVICE_OPEN_TIMEOUT, &mut observer.ready) => {
            received.map_err(|_| MediaError::Device("audio device initialization timed out".into()))?
                .map_err(|_| MediaError::Device("audio device setup task failed".into()))?;
            observer.handoff.take(controls)
        },
    }
}

/// A fresh join's first microphone and speaker: `start` (the chosen route),
/// or if that cannot be opened, a newer choice made while it opened, once
/// per distinct route. Returns the route that opened.
async fn open_first_route(
    controls: &AudioControls,
    routes: &mut watch::Receiver<RouteChoice>,
    start: AudioDeviceSelection,
) -> Result<(MicrophoneCapture, AudioOutput, AudioDeviceSelection), MediaError> {
    let mut attempt = start;
    loop {
        let opening = attempt.clone();
        let opened = open_on_device_thread(controls, move |gate| {
            let active = || gate.active();
            prepare_audio_pair(
                &active,
                || MicrophoneCapture::prepare_selected(opening.microphone.as_deref(), &active),
                || AudioOutput::prepare_selected(opening.speaker.as_deref(), &active),
                |speaker| speaker.start(&active),
                |microphone| microphone.start(&active),
            )
        })
        .await;
        match opened {
            Ok((microphone, speaker)) => return Ok((microphone, speaker, attempt)),
            Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
            Err(error) => {
                let wanted = routes.borrow_and_update().selection.clone();
                if wanted == attempt {
                    return Err(error);
                }
                attempt = wanted;
            }
        }
    }
}

/// What a rejoin does with the microphone the lost join had.
#[derive(Debug, PartialEq, Eq)]
enum ResumeMicrophone {
    /// Reopen exactly this device: the one in use, by its validated ID.
    Open(String),
    /// Keep it closed. A lost one waits for its exact device (`lost`); one
    /// without an exact identity waits for none. Either way capture resumes
    /// only on the chosen exact device or on the user's explicit choice
    /// (`microphone_step`), never on the current default or an alias.
    Wait {
        lost: Option<String>,
        notice: RouteNotice,
    },
}

/// The rejoin never reopens a microphone by its route: System default or an
/// alias may name a different microphone now. Only a retained exact ID is
/// reopened; a missing or inexact identity stays explicitly unresumed.
fn resume_microphone(resume: &AudioResume) -> ResumeMicrophone {
    let exact = resume.microphone.clone().filter(|id| exact_identity(id));
    if resume.microphone_lost {
        return ResumeMicrophone::Wait {
            lost: exact,
            notice: resume.microphone_loss,
        };
    }
    match exact {
        Some(id) => ResumeMicrophone::Open(id),
        None => ResumeMicrophone::Wait {
            lost: None,
            notice: RouteNotice::MicrophoneNotResumed,
        },
    }
}

/// Reopen what a lost join's audio had, by the rules for a device lost
/// inside a join: the microphone only by its retained exact ID (a lost one
/// is waited for, never replaced; one without an exact identity is not
/// reopened, see `resume_microphone`), the speaker on its route or, visibly,
/// the system default. A side that cannot be opened stays closed while the
/// loop looks for it, so one missing device never costs the call the other.
/// Mute and deafen are the controls' requested state either way. Returns the
/// notice for what could not be resumed.
async fn resume_local_audio(
    controls: &AudioControls,
    resume: AudioResume,
) -> Result<(LocalAudio, Option<RouteNotice>), MediaError> {
    let chosen = controls.requested_route();
    let mut local = LocalAudio::closed(
        resume.route.clone(),
        resume.last_request.clone(),
        resume.channels,
    );
    let mut notice = None;
    match resume_microphone(&resume) {
        ResumeMicrophone::Open(id) => match open_microphone(controls, Some(id.clone())).await {
            Ok(microphone) => local.microphone = Some(microphone),
            Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
            Err(_) => {
                local.microphone_lost = true;
                local.lost_microphone_id = Some(id);
                notice = Some(RouteNotice::MicrophoneLost);
            }
        },
        ResumeMicrophone::Wait { lost, notice: why } => {
            local.microphone_lost = true;
            local.microphone_loss = why;
            local.lost_microphone_id = lost;
            notice = Some(why);
        }
    }
    if resume.speaker_lost {
        // Nothing was playing, and a lost stand-in is not replaced by another:
        // only the chosen speaker itself, as reconnecting would.
        let mut reopened = false;
        if let Some(target) = speaker_reopen_target(chosen.speaker.as_deref()) {
            match open_speaker(controls, target.clone()).await {
                Ok(speaker) => {
                    local.speaker = Some(speaker);
                    local.current.speaker = target;
                    reopened = true;
                }
                Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
                Err(_) => {}
            }
        }
        if !reopened {
            local.speaker_lost = true;
            notice = notice.or(Some(RouteNotice::SpeakerLost));
        }
        return Ok((local, notice));
    }
    let speaker_target = if resume.speaker_fallback {
        None
    } else {
        resume.route.speaker.clone()
    };
    match open_speaker(controls, speaker_target).await {
        Ok(speaker) => {
            local.speaker = Some(speaker);
            local.speaker_fallback = resume.speaker_fallback;
            if resume.speaker_fallback {
                notice = notice.or(Some(RouteNotice::SpeakerFellBack));
            }
        }
        Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
        // The stand-in itself is gone: wait for the chosen speaker.
        Err(_) if resume.speaker_fallback => {
            local.speaker_lost = true;
            notice = notice.or(Some(RouteNotice::SpeakerLost));
        }
        Err(_) => {
            let lost = local.lose_speaker(controls).await?;
            notice = notice.or(Some(lost));
        }
    }
    Ok((local, notice))
}

async fn open_microphone(
    controls: &AudioControls,
    selected: Option<String>,
) -> Result<MicrophoneCapture, MediaError> {
    open_on_device_thread(controls, move |gate| {
        let active = || gate.active();
        let microphone = MicrophoneCapture::prepare_selected(selected.as_deref(), &active)?;
        microphone.start(&active)?;
        Ok(microphone)
    })
    .await
}

async fn open_speaker(
    controls: &AudioControls,
    selected: Option<String>,
) -> Result<AudioOutput, MediaError> {
    open_on_device_thread(controls, move |gate| {
        let active = || gate.active();
        let speaker = AudioOutput::prepare_selected(selected.as_deref(), &active)?;
        speaker.start(&active)?;
        Ok(speaker)
    })
    .await
}

/// The next captured frame of the microphone in use; pending forever while
/// none is. None when its stream ended.
async fn next_microphone_frame(microphone: &mut Option<MicrophoneCapture>) -> Option<AudioFrame> {
    match microphone {
        Some(microphone) => microphone.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Wake when either open device's error callback reported something.
async fn stream_fault(microphone: Option<Arc<StreamHealth>>, speaker: Option<Arc<StreamHealth>>) {
    async fn wait(health: Option<Arc<StreamHealth>>) {
        match health {
            Some(health) => health.wake.notified().await,
            None => std::future::pending().await,
        }
    }
    tokio::select! {
        () = wait(microphone) => {}
        () = wait(speaker) => {}
    }
}

/// A frame in the track's published channel count. A microphone opened after
/// publication may have the other one of mono and stereo.
fn conform_channels(frame: AudioFrame, channels: u32) -> AudioFrame {
    if frame.num_channels == channels {
        return frame;
    }
    let mut out = VecDeque::with_capacity(frame.samples_per_channel as usize * channels as usize);
    remix_channels(&frame.data, frame.num_channels, channels, &mut out);
    AudioFrame {
        data: Vec::from(out),
        sample_rate: frame.sample_rate,
        num_channels: channels,
        samples_per_channel: frame.samples_per_channel,
    }
}

/// What one side of the route reports: the device open (its display name,
/// when known), against the route that opened it and the route now chosen.
fn route_for(
    open: Option<&Option<String>>,
    current: &Option<String>,
    wanted: &Option<String>,
    instead_of_chosen: bool,
    rerouted: bool,
) -> DeviceRoute {
    let Some(name) = open else {
        return DeviceRoute::Lost;
    };
    // An output that followed the system default no longer plays on the
    // device it opened on, and the platform does not say which one.
    let name = if rerouted { None } else { name.clone() };
    if instead_of_chosen {
        return DeviceRoute::DefaultInstead { name };
    }
    if current != wanted {
        return DeviceRoute::Previous { name };
    }
    DeviceRoute::Chosen {
        name,
        system_default: wanted.is_none(),
    }
}

/// A route replacement may already hold its new stream while retiring the old one.
/// Cancellation drops that new value immediately; its native backend stays owned.
#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn wait_for_route_close(controls: &AudioControls) -> Result<(), MediaError> {
    tokio::select! {
        biased;
        _ = controls.cancelled() => Err(MediaError::CallEnded),
        result = wait_for_retired_native_audio() => {
            result.map_err(|_| MediaError::Device(NATIVE_ROUTE_CLOSE_MESSAGE.into()))?;
            controls.ensure_authorized()
        }
    }
}

/// The devices one join's audio loop uses, against the route the user chose.
struct LocalAudio {
    /// The route the open devices answer, side by side.
    current: AudioDeviceSelection,
    /// The previous route request, so a request can tell a new microphone
    /// choice from one that changes only the speaker, and a repeat of the
    /// same request (an explicit retry) from both.
    last_request: AudioDeviceSelection,
    microphone: Option<MicrophoneCapture>,
    speaker: Option<AudioOutput>,
    microphone_lost: bool,
    /// Why the microphone was lost (`MicrophoneLost` or `MicrophoneDenied`).
    microphone_loss: RouteNotice,
    /// The exact ID of the lost microphone, the only one to wait for.
    lost_microphone_id: Option<String>,
    speaker_lost: bool,
    /// The system default output plays instead of the chosen speaker.
    speaker_fallback: bool,
    /// The default output followed the system to another device.
    speaker_rerouted: bool,
    /// Looking for lost or replaced devices stops here (monotonic).
    reconnect_deadline: tokio::time::Instant,
    reconnect_stopped: bool,
    /// The microphone track's published channel count.
    channels: u32,
}

impl LocalAudio {
    fn new(
        current: AudioDeviceSelection,
        microphone: MicrophoneCapture,
        speaker: AudioOutput,
    ) -> Self {
        let last_request = current.clone();
        let mut local = Self::closed(current, last_request, microphone.channels());
        local.microphone = Some(microphone);
        local.speaker = Some(speaker);
        local
    }

    /// No device open yet: `current` is the route the devices to open answer.
    fn closed(
        current: AudioDeviceSelection,
        last_request: AudioDeviceSelection,
        channels: u32,
    ) -> Self {
        Self {
            current,
            last_request,
            microphone: None,
            speaker: None,
            microphone_lost: false,
            microphone_loss: RouteNotice::MicrophoneLost,
            lost_microphone_id: None,
            speaker_lost: false,
            speaker_fallback: false,
            speaker_rerouted: false,
            reconnect_deadline: tokio::time::Instant::now() + RECONNECT_WINDOW,
            reconnect_stopped: false,
            channels,
        }
    }

    /// Look for lost or replaced devices again, for a fresh window from now.
    fn rearm_reconnect(&mut self) {
        self.reconnect_deadline = tokio::time::Instant::now() + RECONNECT_WINDOW;
        self.reconnect_stopped = false;
    }

    fn reconnect_open_fits(&self) -> bool {
        reconnect_open_fits(tokio::time::Instant::now(), self.reconnect_deadline)
    }

    fn microphone_health(&self) -> Option<Arc<StreamHealth>> {
        self.microphone
            .as_ref()
            .map(|microphone| Arc::clone(&microphone.health))
    }

    fn speaker_health(&self) -> Option<Arc<StreamHealth>> {
        self.speaker
            .as_ref()
            .map(|speaker| Arc::clone(&speaker.health))
    }

    /// Whether there is anything to look for: a lost microphone only when it
    /// has a device to reopen (`microphone_reopen_target` for `wanted`); one
    /// without waits for the user, and no search is claimed for it.
    fn reconnecting(&self, wanted: &AudioDeviceSelection) -> bool {
        let microphone = self.microphone_lost
            && microphone_reopen_target(
                wanted.microphone.as_deref(),
                self.lost_microphone_id.as_deref(),
            )
            .is_some();
        !self.reconnect_stopped && (microphone || self.speaker_lost || self.speaker_fallback)
    }

    fn report(&self, controls: &AudioControls, notice: Option<RouteNotice>) {
        let wanted = controls.requested_route();
        let microphone = route_for(
            self.microphone.as_ref().map(|microphone| &microphone.name),
            &self.current.microphone,
            &wanted.microphone,
            false,
            false,
        );
        let speaker = route_for(
            self.speaker.as_ref().map(|speaker| &speaker.name),
            &self.current.speaker,
            &wanted.speaker,
            self.speaker_fallback,
            self.speaker_rerouted,
        );
        controls.publish_route(microphone, speaker, notice);
        self.record_resume(controls);
    }

    /// Store where this loop is, so the rejoin that replaces a lost join
    /// resumes it (`AudioControls::resume_point`) instead of opening the
    /// route afresh.
    fn record_resume(&self, controls: &AudioControls) {
        let microphone = if self.microphone_lost {
            self.lost_microphone_id.clone()
        } else {
            self.microphone
                .as_ref()
                .and_then(|microphone| microphone.id.clone())
                .filter(|id| exact_identity(id))
        };
        controls.resume.send_replace(Some(AudioResume {
            route: self.current.clone(),
            last_request: self.last_request.clone(),
            microphone,
            microphone_lost: self.microphone_lost,
            microphone_loss: self.microphone_loss,
            speaker_fallback: self.speaker_fallback,
            speaker_lost: self.speaker.is_none(),
            channels: self.channels,
        }));
    }

    /// The microphone track's format for this join, decided once the devices
    /// are open. The resume point is recorded here, synchronously, before the
    /// caller's first fallible publication await: a join lost while its track
    /// is still being published leaves the exact identity it opened (or an
    /// explicitly unknown one), never nothing for the rejoin to fill in from
    /// the route.
    fn prepare_publication(&mut self, controls: &AudioControls) -> (u32, u32) {
        // A resumed join may start without a microphone; its track takes the
        // lost join's channel count, and a later microphone is remixed to it.
        let format = match &self.microphone {
            Some(microphone) => (microphone.sample_rate(), microphone.channels()),
            None => (SAMPLE_RATE, self.channels),
        };
        self.channels = format.1;
        self.record_resume(controls);
        format
    }

    /// Open the chosen route's new devices beside the ones in use, and swap
    /// each only once it is open. The track and the mute and deafen state are
    /// kept; a side that cannot be opened keeps its device and is reported.
    /// `retry`: the user repeated their previous request unchanged, as
    /// decided at admission (`AudioControls::request_route`); never inferred
    /// here, where coalesced requests could look like a repeat.
    async fn switch(
        &mut self,
        controls: &AudioControls,
        wanted: AudioDeviceSelection,
        retry: bool,
    ) -> Result<(), MediaError> {
        let mut failed = None;
        let mut switched = false;
        let mut restored = false;
        let step = microphone_step(
            wanted.microphone.as_deref(),
            self.last_request.microphone.as_deref(),
            self.current.microphone.as_deref(),
            self.microphone.is_some(),
            self.lost_microphone_id.as_deref(),
            retry,
        );
        self.last_request = wanted.clone();
        let (target, chosen_now) = match step {
            MicrophoneStep::Keep => (None, false),
            MicrophoneStep::Open(target) => (Some(target), true),
            MicrophoneStep::Reopen(target) => (Some(Some(target)), false),
        };
        if let Some(target) = target {
            // A lost device reopened by its own ID (for the system default,
            // or standing in for a choice that failed) answers the route it
            // answered before, not the choice.
            let answers_choice = target == wanted.microphone;
            match open_microphone(controls, target).await {
                Ok(mut microphone) => {
                    // Nothing recorded before the swap is sent.
                    microphone.discard_buffered_frames();
                    controls.ensure_authorized()?;
                    let previous = self.microphone.take();
                    drop(previous);
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    wait_for_route_close(controls).await?;
                    controls.ensure_authorized()?;
                    self.microphone = Some(microphone);
                    self.microphone_lost = false;
                    self.lost_microphone_id = None;
                    if answers_choice {
                        self.current.microphone = wanted.microphone.clone();
                    }
                    if chosen_now {
                        switched = true;
                    } else {
                        restored = true;
                    }
                }
                Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
                Err(_) if chosen_now => failed = Some(RouteNotice::MicrophoneSwitchFailed),
                // A quiet reopen that fails keeps waiting, as reconnecting does.
                Err(_) => {}
            }
        }
        if wanted.speaker != self.current.speaker || self.speaker.is_none() || self.speaker_fallback
        {
            match open_speaker(controls, wanted.speaker.clone()).await {
                Ok(speaker) => {
                    controls.install_speaker(&speaker)?;
                    let previous = self.speaker.take();
                    drop(previous);
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    wait_for_route_close(controls).await?;
                    controls.ensure_authorized()?;
                    self.speaker = Some(speaker);
                    self.speaker_lost = false;
                    self.speaker_fallback = false;
                    self.speaker_rerouted = false;
                    self.current.speaker = wanted.speaker.clone();
                    switched = true;
                }
                Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
                Err(_) => failed = failed.or(Some(RouteNotice::SpeakerSwitchFailed)),
            }
        }
        self.rearm_reconnect();
        // A microphone still lost is said again rather than "switched".
        let still_lost = self.microphone_lost.then_some(self.microphone_loss);
        self.report(
            controls,
            failed
                .or(still_lost)
                .or(switched.then_some(RouteNotice::Switched))
                .or(restored.then_some(RouteNotice::MicrophoneRestored)),
        );
        Ok(())
    }

    /// Stop using a microphone whose stream was lost. Its exact device is
    /// the only one waited for; capture resumes only on it, or on a new
    /// choice. The track and the mute state are untouched.
    async fn lose_microphone(
        &mut self,
        controls: &AudioControls,
        fault: StreamFault,
    ) -> Result<RouteNotice, MediaError> {
        self.lost_microphone_id = self
            .microphone
            .take()
            .and_then(|microphone| microphone.id.clone());
        self.microphone_lost = true;
        self.rearm_reconnect();
        self.microphone_loss = match fault {
            StreamFault::Denied => RouteNotice::MicrophoneDenied,
            StreamFault::Lost => RouteNotice::MicrophoneLost,
        };
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        wait_for_route_close(controls).await?;
        Ok(self.microphone_loss)
    }

    /// Replace a lost speaker. Output only: playing through the system default
    /// never opens a microphone. The default is what the user chose, or the
    /// visible stand-in for a lost chosen speaker; a stand-in that is itself
    /// lost is not replaced by yet another device.
    async fn lose_speaker(&mut self, controls: &AudioControls) -> Result<RouteNotice, MediaError> {
        let was_fallback = self.speaker_fallback;
        controls.release_speaker();
        self.speaker = None;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        wait_for_route_close(controls).await?;
        self.speaker_lost = true;
        self.speaker_fallback = false;
        self.speaker_rerouted = false;
        self.rearm_reconnect();
        let chosen = controls.requested_route().speaker;
        if chosen.is_some() && was_fallback {
            return Ok(RouteNotice::SpeakerLost);
        }
        match open_speaker(controls, None).await {
            Ok(speaker) => {
                controls.install_speaker(&speaker)?;
                self.speaker = Some(speaker);
                self.speaker_lost = false;
                if chosen.is_some() {
                    self.speaker_fallback = true;
                    Ok(RouteNotice::SpeakerFellBack)
                } else {
                    self.current.speaker = None;
                    Ok(RouteNotice::DefaultSpeakerReopened)
                }
            }
            Err(MediaError::CallEnded) => Err(MediaError::CallEnded),
            Err(_) => Ok(RouteNotice::SpeakerLost),
        }
    }

    async fn handle_faults(&mut self, controls: &AudioControls) -> Result<(), MediaError> {
        let mut changed = false;
        let mut notice = None;
        if let Some(fault) = self
            .microphone
            .as_ref()
            .and_then(|microphone| microphone.health.take_fault())
        {
            notice = Some(self.lose_microphone(controls, fault).await?);
            changed = true;
        }
        if self
            .speaker
            .as_ref()
            .is_some_and(|speaker| speaker.health.take_rerouted())
        {
            self.speaker_rerouted = true;
            changed = true;
        }
        if self
            .speaker
            .as_ref()
            .and_then(|speaker| speaker.health.take_fault())
            .is_some()
        {
            let lost = self.lose_speaker(controls).await?;
            notice = notice.or(Some(lost));
            changed = true;
        }
        if changed {
            self.report(controls, notice);
        }
        Ok(())
    }

    /// Look for the chosen devices again: a lost microphone only on its exact
    /// device (see `microphone_reopen_target`), a lost or replaced speaker on
    /// the chosen route. Bounded by `reconnect_deadline`: no open starts that
    /// could run past it, and looking stops once none can.
    async fn reconnect(&mut self, controls: &AudioControls) -> Result<(), MediaError> {
        if !self.reconnect_open_fits() {
            self.reconnect_stopped = true;
            self.report(controls, Some(RouteNotice::ReconnectStopped));
            return Ok(());
        }
        let wanted = controls.requested_route();
        let mut notice = None;
        if self.microphone_lost
            && let Some(target) = microphone_reopen_target(
                wanted.microphone.as_deref(),
                self.lost_microphone_id.as_deref(),
            )
        {
            // As in `switch`: the lost device itself keeps its route.
            let answers_choice = wanted.microphone.as_deref() == Some(target.as_str());
            match open_microphone(controls, Some(target)).await {
                Ok(mut microphone) => {
                    microphone.discard_buffered_frames();
                    let previous = self.microphone.take();
                    drop(previous);
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    wait_for_route_close(controls).await?;
                    controls.ensure_authorized()?;
                    self.microphone = Some(microphone);
                    self.microphone_lost = false;
                    self.lost_microphone_id = None;
                    if answers_choice {
                        self.current.microphone = wanted.microphone.clone();
                    }
                    notice = Some(RouteNotice::MicrophoneRestored);
                }
                Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
                Err(_) => {}
            }
        }
        // A stand-in default stays until the chosen speaker itself opens. The
        // microphone's open may have used up the time left for this one.
        if (self.speaker_lost || self.speaker_fallback)
            && self.reconnect_open_fits()
            && let Some(target) = speaker_reopen_target(wanted.speaker.as_deref())
            && !(self.speaker_fallback && target.is_none())
        {
            match open_speaker(controls, target.clone()).await {
                Ok(speaker) => {
                    controls.install_speaker(&speaker)?;
                    let previous = self.speaker.take();
                    drop(previous);
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    wait_for_route_close(controls).await?;
                    controls.ensure_authorized()?;
                    self.speaker = Some(speaker);
                    self.speaker_lost = false;
                    self.speaker_fallback = false;
                    self.speaker_rerouted = false;
                    self.current.speaker = target;
                    notice = notice.or(Some(RouteNotice::SpeakerRestored));
                }
                Err(MediaError::CallEnded) => return Err(MediaError::CallEnded),
                Err(_) => {}
            }
        }
        if notice.is_some() {
            self.rearm_reconnect();
            self.report(controls, notice);
        }
        Ok(())
    }
}

enum RemoteAudioEvent {
    Frame {
        member_id: String,
        generation: u64,
        frame: AudioFrame,
    },
    Ended {
        member_id: String,
        generation: u64,
    },
}

struct RemoteAudioTask {
    generation: u64,
    task: JoinHandle<()>,
}

struct RemoteAudioStreams {
    tasks: HashMap<String, RemoteAudioTask>,
    tx: mpsc::Sender<RemoteAudioEvent>,
    next_generation: u64,
}

impl RemoteAudioStreams {
    fn new(tx: mpsc::Sender<RemoteAudioEvent>) -> Self {
        Self {
            tasks: HashMap::new(),
            tx,
            next_generation: 1,
        }
    }

    fn is_current(&self, member_id: &str, generation: u64) -> bool {
        self.tasks
            .get(member_id)
            .is_some_and(|task| task.generation == generation)
    }

    fn start(
        &mut self,
        call: &matrix_rtc_call_sdk::LiveKitCall,
        speaker: Option<&AudioOutput>,
        member_id: &str,
    ) -> Result<(), MediaError> {
        if self.tasks.len() >= MAX_REMOTE_STREAMS && !self.tasks.contains_key(member_id) {
            return Ok(());
        }
        let Some(mut frames) = call
            .remote_track(member_id, matrix_rtc_transport::MediaStreamKind::Microphone)
            .and_then(|track| track.audio_frames())
        else {
            return Ok(());
        };
        self.stop(speaker, member_id)?;
        let member_id_owned = member_id.to_owned();
        let member_id_for_task = member_id_owned.clone();
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let tx = self.tx.clone();
        let task = tokio::spawn(async move {
            while let Some(frame) = frames.next().await {
                match tx.try_send(RemoteAudioEvent::Frame {
                    member_id: member_id_for_task.clone(),
                    generation,
                    frame,
                }) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
            let _ = tx
                .send(RemoteAudioEvent::Ended {
                    member_id: member_id_for_task,
                    generation,
                })
                .await;
        });
        self.tasks
            .insert(member_id_owned, RemoteAudioTask { generation, task });
        Ok(())
    }

    /// Remote streams keep running without a speaker (while one is lost or
    /// being replaced); their frames are then dropped.
    fn stop(&mut self, speaker: Option<&AudioOutput>, member_id: &str) -> Result<(), MediaError> {
        if let Some(task) = self.tasks.remove(member_id) {
            task.task.abort();
        }
        match speaker {
            Some(speaker) => speaker.remove_stream(member_id),
            None => Ok(()),
        }
    }

    fn reconcile(
        &mut self,
        call: &matrix_rtc_call_sdk::LiveKitCall,
        speaker: Option<&AudioOutput>,
    ) -> Result<(), MediaError> {
        let desired: HashSet<String> = call
            .participants()
            .into_iter()
            .filter(|participant| {
                !participant.is_local
                    && participant.streams.iter().any(|stream| {
                        stream.kind == matrix_rtc_transport::MediaStreamKind::Microphone
                    })
            })
            .map(|participant| participant.member_id)
            .collect();
        let stale: Vec<String> = self
            .tasks
            .keys()
            .filter(|member_id| !desired.contains(*member_id))
            .cloned()
            .collect();
        for member_id in stale {
            self.stop(speaker, &member_id)?;
        }
        for member_id in desired {
            if !self.tasks.contains_key(&member_id) {
                self.start(call, speaker, &member_id)?;
            }
        }
        Ok(())
    }
}

impl Drop for RemoteAudioStreams {
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.task.abort();
        }
    }
}

struct MuteOnDrop<'a>(&'a Publication);

impl Drop for MuteOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.set_muted(true);
    }
}

fn select_48k_config(
    device: &cpal::Device,
    input: bool,
) -> Result<SupportedStreamConfig, MediaError> {
    let configs: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .collect()
    };
    configs
        .into_iter()
        .filter(|range| {
            (1..=2).contains(&range.channels())
                && range.min_sample_rate() <= SAMPLE_RATE
                && range.max_sample_rate() >= SAMPLE_RATE
                && matches!(
                    range.sample_format(),
                    SampleFormat::I16 | SampleFormat::F32 | SampleFormat::U16
                )
        })
        .min_by_key(|range| {
            // Prefer stereo output but mono input; f32 and i16 are common.
            let channel_preference = if input {
                range.channels() - 1
            } else {
                2 - range.channels()
            };
            (
                channel_preference,
                !matches!(range.sample_format(), SampleFormat::F32),
            )
        })
        .map(|range| range.with_sample_rate(SAMPLE_RATE))
        .ok_or_else(|| {
            MediaError::Device("device has no supported 48 kHz mono/stereo PCM mode".into())
        })
}

fn remix_channels(
    source: &[i16],
    source_channels: u32,
    target_channels: u32,
    out: &mut VecDeque<i16>,
) {
    match (source_channels, target_channels) {
        (1, 2) => {
            for sample in source {
                out.push_back(*sample);
                out.push_back(*sample);
            }
        }
        (2, 1) => {
            for stereo in source.chunks_exact(2) {
                out.push_back(((i32::from(stereo[0]) + i32::from(stereo[1])) / 2) as i16);
            }
        }
        _ => out.extend(source.iter().copied()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guarded_controls(allowed: impl Fn() -> bool + Send + Sync + 'static) -> AudioControls {
        AudioControls::new_cancellable(
            CallLease::new(
                CallInfo {
                    room_id: "!room:example.test".into(),
                    slot_id: "m.call#room".into(),
                    membership_id: "membership".into(),
                    local_identity: "identity".into(),
                },
                "@alice:example.test".into(),
                "DEVICE".into(),
            ),
            allowed,
        )
    }

    struct OpenTlsHold {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for OpenTlsHold {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }
    std::thread_local! {
        static OPEN_TLS_HOLD: std::cell::RefCell<Option<OpenTlsHold>> = const { std::cell::RefCell::new(None) };
    }
    struct OpenDropRecord(std::sync::mpsc::SyncSender<std::thread::ThreadId>);
    impl Drop for OpenDropRecord {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }
    #[tokio::test]
    async fn actual_opener_stays_owned_through_late_result_tls_and_sequential_handoff() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let controls = guarded_controls(|| true);
        let entered = Arc::new(AtomicBool::new(false));
        let body = entered.clone();
        let failed = open_with_supervisor(
            &controls,
            move |_| {
                body.store(true, Ordering::Release);
                Ok(())
            },
            |_supervisor| {
                Err(std::io::Error::other(
                    "synthetic supervisor creation failure",
                ))
            },
        )
        .await;
        assert!(
            matches!(failed, Err(MediaError::Device(message)) if message == "could not start audio device setup")
        );
        assert!(!entered.load(Ordering::Acquire));
        assert!(!DEVICE_OPEN_IN_PROGRESS.load(Ordering::SeqCst));
        assert!(DEVICE_OPEN_OWNER.lock().unwrap().is_none());

        // Retain the supervisor before its start gate is consumed, cancel the
        // whole async owner, then admit it. No native constructor may run.
        let (admit_supervisor, supervisor_admitted) = std::sync::mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        let body = entered.clone();
        let mut cancelled_before_native = Box::pin(open_with_supervisor(
            &controls,
            move |_| {
                body.store(true, Ordering::Release);
                Ok(())
            },
            move |supervise| {
                Ok(std::thread::spawn(move || {
                    supervisor_admitted
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap();
                    supervise();
                }))
            },
        ));
        // Pending can still be the mandatory native-backend retirement wait.
        // Drive this same future until its actual retained supervisor exists;
        // only then exercise cancellation or wait on the native-thread channel.
        let supervisor_published = tokio::time::timeout(
            Duration::from_secs(2),
            std::future::poll_fn(|cx| match cancelled_before_native.as_mut().poll(cx) {
                std::task::Poll::Ready(_) => std::task::Poll::Ready(false),
                std::task::Poll::Pending => {
                    if DEVICE_OPEN_OWNER
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|owner| owner.supervisor.is_some())
                    {
                        std::task::Poll::Ready(true)
                    } else {
                        std::task::Poll::Pending
                    }
                }
            }),
        )
        .await;
        assert!(matches!(supervisor_published, Ok(true)));
        assert!(
            DEVICE_OPEN_OWNER
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        drop(cancelled_before_native);
        admit_supervisor.send(()).unwrap();
        assert_eq!(wait_for_audio_device_release().await, Ok(()));
        assert!(!entered.load(Ordering::Acquire));
        assert!(!DEVICE_OPEN_IN_PROGRESS.load(Ordering::SeqCst));

        let (opened, entered_open) = std::sync::mpsc::sync_channel(1);
        let (allow_open, continue_open) = std::sync::mpsc::channel();
        let (dropped, saw_drop) = std::sync::mpsc::sync_channel(1);
        let (in_tls, tls_entered) = std::sync::mpsc::sync_channel(1);
        let (release_tls, tls_release) = std::sync::mpsc::channel();
        let mut opening = Box::pin(open_on_device_thread(&controls, move |_| {
            OPEN_TLS_HOLD.with(|tls| {
                *tls.borrow_mut() = Some(OpenTlsHold {
                    entered: in_tls,
                    release: tls_release,
                })
            });
            let result = OpenDropRecord(dropped);
            opened.send(std::thread::current().id()).unwrap();
            continue_open.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(result)
        }));
        // Pending can still be the mandatory native-backend retirement wait.
        // Drive this same future until its actual retained supervisor exists;
        // only then exercise cancellation or wait on the native-thread channel.
        let supervisor_published = tokio::time::timeout(
            Duration::from_secs(2),
            std::future::poll_fn(|cx| match opening.as_mut().poll(cx) {
                std::task::Poll::Ready(_) => std::task::Poll::Ready(false),
                std::task::Poll::Pending => {
                    if DEVICE_OPEN_OWNER
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|owner| owner.supervisor.is_some())
                    {
                        std::task::Poll::Ready(true)
                    } else {
                        std::task::Poll::Pending
                    }
                }
            }),
        )
        .await;
        assert!(matches!(supervisor_published, Ok(true)));
        let actual_thread = entered_open.recv_timeout(Duration::from_secs(2)).unwrap();
        // Drop the whole suspended future, not just the cancellation signal.
        // The actual result is late and its destructor must stay off Tokio.
        drop(opening);
        allow_open.send(()).unwrap();
        assert_eq!(
            saw_drop.recv_timeout(Duration::from_secs(2)).unwrap(),
            actual_thread
        );
        assert_ne!(actual_thread, std::thread::current().id());
        tls_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(DEVICE_OPEN_IN_PROGRESS.load(Ordering::SeqCst));
        assert_eq!(
            wait_for_device_release(Duration::from_millis(20)).await,
            Err(AudioDeviceReleaseError::TimedOut)
        );
        assert!(
            matches!(open_on_device_thread(&controls, |_| Ok(())).await, Err(MediaError::Device(message)) if message == "audio device setup is still in progress")
        );
        assert!(
            DEVICE_OPEN_OWNER
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        release_tls.send(()).unwrap();
        assert_eq!(wait_for_audio_device_release().await, Ok(()));
        assert!(!DEVICE_OPEN_IN_PROGRESS.load(Ordering::SeqCst));

        let (in_tls, tls_entered) = std::sync::mpsc::sync_channel(1);
        let (release_tls, tls_release) = std::sync::mpsc::channel();
        assert_eq!(
            open_on_device_thread(&controls, move |_| {
                OPEN_TLS_HOLD.with(|tls| {
                    *tls.borrow_mut() = Some(OpenTlsHold {
                        entered: in_tls,
                        release: tls_release,
                    })
                });
                Ok(1)
            })
            .await
            .unwrap(),
            1
        );
        tls_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let second_entered = Arc::new(AtomicBool::new(false));
        let second = second_entered.clone();
        let mut next = Box::pin(open_on_device_thread(&controls, move |_| {
            second.store(true, Ordering::Release);
            Ok(2)
        }));
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(!second_entered.load(Ordering::Acquire));
        release_tls.send(()).unwrap();
        assert_eq!(next.await.unwrap(), 2);
        assert_eq!(wait_for_audio_device_release().await, Ok(()));
        assert!(DEVICE_OPEN_OWNER.lock().unwrap().is_none());
        assert!(!DEVICE_OPEN_IN_PROGRESS.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn a_ready_unit_does_not_move_unclaimed_native_results_into_tokio() {
        let controls = guarded_controls(|| true);
        let completion = Arc::new(OpenCompletion::default());
        let (ready, received) = tokio::sync::oneshot::channel();
        let handoff = Arc::new(OpenHandoff::new(completion, ready));
        let mut observer = OpenObserver {
            handoff: handoff.clone(),
            ready: received,
        };
        let (dropped, saw_drop) = std::sync::mpsc::sync_channel(1);
        let native = std::thread::spawn(move || {
            run_device_open(controls, handoff, |_| Ok(OpenDropRecord(dropped)))
        });
        tokio::time::timeout(Duration::from_secs(2), &mut observer.ready)
            .await
            .unwrap()
            .unwrap();
        // Even after readiness is delivered, receiver Drop cannot destroy T.
        drop(observer);
        let drop_thread = saw_drop.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(drop_thread, native.thread().id());
        assert_ne!(drop_thread, std::thread::current().id());
        assert_eq!(native.join().unwrap(), Ok(()));
    }
    #[tokio::test]
    async fn failed_actual_spawn_keeps_original_error_without_native_value_channel() {
        let controls = guarded_controls(|| true);
        let (ready, received) = tokio::sync::oneshot::channel();
        let completion = Arc::new(OpenCompletion::default());
        let handoff = Arc::new(OpenHandoff::<()>::new(completion, ready));
        let mut observer = OpenObserver {
            handoff: handoff.clone(),
            ready: received,
        };
        let supervisor = std::thread::spawn(move || handoff.failed_start());
        tokio::time::timeout(Duration::from_secs(2), &mut observer.ready)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(observer.handoff.take(&controls), Err(MediaError::Device(message)) if message == "could not start audio device setup")
        );
        supervisor.join().unwrap();
    }

    #[tokio::test]
    async fn caught_opener_panic_preserves_startup_error_and_does_not_prove_release() {
        let controls = guarded_controls(|| true);
        let completion = Arc::new(OpenCompletion::default());
        let (ready, received) = tokio::sync::oneshot::channel();
        let handoff = Arc::new(OpenHandoff::<()>::new(completion.clone(), ready));
        let mut observer = OpenObserver {
            handoff: handoff.clone(),
            ready: received,
        };
        let worker_controls = controls.clone();
        let native = std::thread::spawn(move || {
            run_device_open(worker_controls, handoff, |_| {
                panic!("synthetic native opener panic")
            })
        });
        tokio::time::timeout(Duration::from_secs(2), &mut observer.ready)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(observer.handoff.take(&controls), Err(MediaError::Device(message)) if message == "audio device setup task failed")
        );
        assert_eq!(
            native.join().unwrap(),
            Err(AudioDeviceReleaseError::WorkerPanicked)
        );
        completion.finish(Err(AudioDeviceReleaseError::WorkerPanicked));
        let supervisor = std::thread::spawn(|| ());
        let mut retained = Some(DeviceOpenOwner {
            supervisor: Some(supervisor),
            completed: completion,
        });
        assert_eq!(
            poll_device_open_owner(&mut retained),
            Some(Err(AudioDeviceReleaseError::WorkerPanicked))
        );
        assert!(retained.as_ref().unwrap().supervisor.is_some());
        retained.take().unwrap().supervisor.unwrap().join().unwrap();
    }
    #[test]
    fn missing_supervisor_completion_stays_failed_instead_of_releasing_the_owner() {
        let completed = Arc::new(OpenCompletion::default());
        let supervisor = std::thread::spawn(|| ());
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while !supervisor.is_finished() {
            assert!(
                std::time::Instant::now() < until,
                "synthetic supervisor did not finish"
            );
            std::thread::yield_now();
        }
        let mut retained = Some(DeviceOpenOwner {
            supervisor: Some(supervisor),
            completed,
        });
        assert_eq!(
            poll_device_open_owner(&mut retained),
            Some(Err(AudioDeviceReleaseError::SupervisorFailed))
        );
        assert_eq!(
            poll_device_open_owner(&mut retained),
            Some(Err(AudioDeviceReleaseError::SupervisorFailed))
        );
        assert!(retained.is_some());
    }

    #[tokio::test]
    async fn cancellation_before_first_audio_poll_never_enters_device_startup() {
        use std::cell::Cell;
        let controls = guarded_controls(|| true);
        let entered = Cell::new(false);
        let audio = async {
            let _run = AudioRunGuard::start(&controls)?;
            entered.set(true);
            Ok::<_, MediaError>(())
        };
        controls.cancel();
        assert!(controls.lease.is_active());
        assert!(matches!(audio.await, Err(MediaError::CallEnded)));
        assert!(!entered.get());
        assert!(!controls.running.load(Ordering::Acquire));
    }

    // An OS step is deliberately blocked while the async owner keeps the
    // receiver and lease alive, as when it awaits another media branch. No
    // cancellation task is polled; the opener must read the owner's policy.
    fn receiver_retained_startup_cancellation(cancel_explicitly: bool) {
        use std::sync::atomic::AtomicUsize;
        let allowed = Arc::new(AtomicBool::new(true));
        let predicate = allowed.clone();
        let controls = guarded_controls(move || predicate.load(Ordering::Acquire));
        let (device_tx, mut device_rx) = tokio::sync::oneshot::channel();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let later_steps = Arc::new(AtomicUsize::new(0));
        let steps = later_steps.clone();
        let discarded = Arc::new(AtomicBool::new(false));
        let dropped = discarded.clone();
        struct Prepared(Arc<AtomicBool>);
        impl Drop for Prepared {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let opening_controls = controls.clone();
        let opener = std::thread::spawn(move || {
            let active = || opening_controls.is_authorized() && !device_tx.is_closed();
            let result = prepare_audio_pair(
                &active,
                || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(Prepared(dropped))
                },
                || {
                    steps.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_| {
                    steps.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_| {
                    steps.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            );
            let _ = device_tx.send(result);
            let _ = finished_tx.send(());
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(controls.lease.is_active());
        assert!(matches!(
            device_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        if cancel_explicitly {
            controls.cancel();
        } else {
            allowed.store(false, Ordering::Release);
        }
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        opener.join().unwrap();
        assert!(matches!(
            device_rx.try_recv(),
            Ok(Err(MediaError::CallEnded))
        ));
        assert_eq!(later_steps.load(Ordering::SeqCst), 0);
        assert!(discarded.load(Ordering::Acquire));
        allowed.store(true, Ordering::Release);
        assert!(!controls.is_authorized());
    }

    #[test]
    fn explicit_cancel_stops_opening_while_owner_and_receiver_remain_suspended() {
        receiver_retained_startup_cancellation(true);
    }

    #[test]
    fn revoked_owner_stops_opening_without_polling_a_cancellation_watcher() {
        receiver_retained_startup_cancellation(false);
    }

    #[test]
    fn each_prepare_and_play_boundary_discards_cancelled_resources() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct Prepared(Rc<Cell<usize>>);
        impl Drop for Prepared {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        for cancel_at in 0..=4 {
            let controls = guarded_controls(|| true);
            let steps = Cell::new(0);
            let created = Cell::new(0);
            let dropped = Rc::new(Cell::new(0));
            let step = || {
                steps.set(steps.get() + 1);
                if steps.get() == cancel_at {
                    controls.cancel();
                }
            };
            let prepare = || {
                step();
                created.set(created.get() + 1);
                Ok(Prepared(dropped.clone()))
            };
            if cancel_at == 0 {
                controls.cancel();
            }
            let result = prepare_audio_pair(
                &|| controls.is_authorized(),
                &prepare,
                &prepare,
                |_| {
                    step();
                    Ok(())
                },
                |_| {
                    step();
                    Ok(())
                },
            );
            assert!(matches!(result, Err(MediaError::CallEnded)));
            assert_eq!(steps.get(), cancel_at);
            assert_eq!(dropped.get(), created.get());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn cancelled_route_does_not_start_an_observer_or_keep_its_prepared_value_live() {
        let controls = guarded_controls(|| true);
        controls.cancel();
        // The actual route barrier must prefer irrevocable authority cancellation
        // even when a backend-close observation could also complete immediately.
        assert!(matches!(
            wait_for_route_close(&controls).await,
            Err(MediaError::CallEnded)
        ));
        assert!(controls.applied_state().microphone_muted);
    }

    #[test]
    fn cancelled_audio_controls_cannot_unmute_send_or_restart_a_live_lease() {
        let controls = guarded_controls(|| true);
        let running = AudioRunGuard::start(&controls).unwrap();
        {
            let mut state = controls.lock_state().unwrap();
            state.applied.microphone_muted = false;
            controls.applied.send_replace(state.applied);
        }
        assert!(!controls.applied_state().microphone_muted);
        controls.cancel();
        assert!(controls.lease.is_active());
        assert!(controls.applied_state().microphone_muted);
        assert!(matches!(
            controls.set_microphone_muted(false),
            Err(MediaError::CallEnded)
        ));
        assert!(matches!(
            controls.set_deafened(false),
            Err(MediaError::CallEnded)
        ));
        assert!(matches!(
            controls.should_send_microphone(),
            Err(MediaError::CallEnded)
        ));
        drop(running);
        assert!(matches!(
            AudioRunGuard::start(&controls),
            Err(MediaError::CallEnded)
        ));
        let fresh = guarded_controls(|| true);
        assert!(AudioRunGuard::start(&fresh).is_ok());
    }

    #[test]
    fn signaling_gate_withholds_audio_without_an_owner_or_revocation() {
        let open = Arc::new(AtomicBool::new(true));
        let gate = Arc::clone(&open);
        let controls = AudioControls::new_gated(
            CallLease::new(
                CallInfo {
                    room_id: "!room:example.test".into(),
                    slot_id: "m.call#room".into(),
                    membership_id: "membership".into(),
                    local_identity: "identity".into(),
                },
                "@alice:example.test".into(),
                "DEVICE".into(),
            ),
            || true,
            move || gate.load(Ordering::SeqCst),
        );
        let _running = AudioRunGuard::start(&controls).unwrap();
        {
            let mut state = controls.lock_state().unwrap();
            state.applied.microphone_muted = false;
            controls.applied.send_replace(state.applied);
        }
        assert!(controls.should_send_microphone().unwrap());
        // Nothing suspends these controls: the push boundary reads the gate.
        open.store(false, Ordering::SeqCst);
        assert!(!controls.should_send_microphone().unwrap());
        assert!(
            controls.is_running(),
            "a transient loss must not revoke capture"
        );
        open.store(true, Ordering::SeqCst);
        assert!(controls.should_send_microphone().unwrap());
    }

    #[test]
    fn paused_transmission_withholds_audio_and_keeps_the_requested_mute_state() {
        let requested = AudioState {
            microphone_muted: false,
            deafened: true,
        };
        let controls = guarded_controls(|| true);
        controls.preset_requested_state(requested).unwrap();
        let running = AudioRunGuard::start(&controls).unwrap();
        assert!(matches!(
            controls.preset_requested_state(requested),
            Err(MediaError::ControlAlreadyRunning)
        ));
        {
            let mut state = controls.lock_state().unwrap();
            state.applied.microphone_muted = false;
            controls.applied.send_replace(state.applied);
        }
        assert!(controls.should_send_microphone().unwrap());
        controls.set_transmission_suspended(true).unwrap();
        assert!(!controls.should_send_microphone().unwrap());
        assert!(controls.applied_state().microphone_muted);
        // An unmute requested during the pause is remembered, not applied.
        controls.set_microphone_muted(false).unwrap();
        assert!(!controls.should_send_microphone().unwrap());
        assert_eq!(controls.requested_state(), requested);
        // Resuming leaves the actual unmute to the capture loop, which first
        // discards frames buffered during the pause.
        controls.set_transmission_suspended(false).unwrap();
        assert!(!controls.should_send_microphone().unwrap());
        assert_eq!(controls.requested_state(), requested);
        drop(running);
        assert!(matches!(
            controls.set_transmission_suspended(false),
            Err(MediaError::CallEnded)
        ));
    }

    #[tokio::test]
    async fn cancellation_wakes_all_registered_audio_waiters() {
        let controls = guarded_controls(|| true);
        let mut first = Box::pin(controls.cancelled());
        let mut second = Box::pin(controls.cancelled());
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        controls.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(first, second);
            controls.cancelled().await;
        })
        .await
        .unwrap();
    }

    #[test]
    fn selected_ids_are_exact_unique_and_independent_of_catalog_order() {
        let ids = ["wasapi:first", "wasapi:second"];
        assert_eq!(
            unique_device_index(ids.into_iter(), "wasapi:second"),
            Some(1)
        );
        assert_eq!(
            unique_device_index(ids.into_iter().rev(), "wasapi:second"),
            Some(0)
        );
        assert_eq!(
            unique_device_index(ids.into_iter(), "coreaudio:second"),
            None
        );
        assert_eq!(unique_device_index(ids.into_iter(), "Headset"), None);
        assert_eq!(
            unique_device_index(
                ["wasapi:second", "wasapi:second"].into_iter(),
                "wasapi:second"
            ),
            None
        );
        assert_eq!(
            unique_device_index(std::iter::empty(), "wasapi:second"),
            None
        );
    }

    #[test]
    fn cancelled_or_timed_out_open_never_runs_the_next_start_step() {
        use std::cell::Cell;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let started = Cell::new(false);
        drop(rx); // Timeout and dropped call future both close this receiver.
        let result = while_open_active(&|| !tx.is_closed(), || {
            started.set(true);
            Ok(())
        });
        assert!(matches!(result, Err(MediaError::CallEnded)));
        assert!(!started.get());
    }

    #[test]
    fn cancellation_during_an_os_step_drops_result_before_next_step() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct Opened(Rc<Cell<bool>>);
        impl Drop for Opened {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let active = Cell::new(true);
        let dropped = Rc::new(Cell::new(false));
        let result = while_open_active(&|| active.get(), || {
            active.set(false);
            Ok(Opened(dropped.clone()))
        });
        assert!(matches!(result, Err(MediaError::CallEnded)));
        assert!(dropped.get());
    }

    fn frame(samples: &[i16], channels: u32) -> AudioFrame {
        AudioFrame {
            data: samples.to_vec(),
            sample_rate: SAMPLE_RATE,
            num_channels: channels,
            samples_per_channel: (samples.len() / channels as usize) as u32,
        }
    }

    #[test]
    fn stereo_to_mono_mix_does_not_overflow() {
        let mut output = VecDeque::new();
        remix_channels(&[i16::MAX, i16::MAX], 2, 1, &mut output);
        assert_eq!(output.pop_front(), Some(i16::MAX));
    }

    #[test]
    fn mono_to_stereo_duplicates_samples() {
        let mut output = VecDeque::new();
        remix_channels(&[10, -10], 1, 2, &mut output);
        assert_eq!(output.into_iter().collect::<Vec<_>>(), [10, 10, -10, -10]);
    }

    #[test]
    fn mixer_sums_multiple_participants_and_clips() {
        let mut mixer = AudioMixer::new(2);
        mixer
            .push_frame("alice", &frame(&[20_000, -20_000], 1))
            .unwrap();
        mixer
            .push_frame("bob", &frame(&[20_000, -20_000], 1))
            .unwrap();
        assert_eq!(mixer.next_sample(), i16::MAX);
        assert_eq!(mixer.next_sample(), i16::MAX);
        assert_eq!(mixer.next_sample(), i16::MIN);
        assert_eq!(mixer.next_sample(), i16::MIN);
        assert_eq!(mixer.next_sample(), 0);
    }

    #[test]
    fn mixer_removes_departed_stream_without_draining_its_audio() {
        let mut mixer = AudioMixer::new(1);
        mixer.push_frame("alice", &frame(&[10, 20], 1)).unwrap();
        mixer.push_frame("bob", &frame(&[100, 200], 1)).unwrap();
        assert_eq!(mixer.next_sample(), 110);
        mixer.remove_stream("bob");
        assert_eq!(mixer.next_sample(), 20);
        assert_eq!(mixer.next_sample(), 0);
        mixer.push_frame("bob", &frame(&[7], 1)).unwrap();
        assert_eq!(mixer.next_sample(), 7);
    }

    #[test]
    fn mixer_bounds_each_participants_latency_independently() {
        let mut mixer = AudioMixer::new(2);
        let old_frame = frame(&vec![1; 4_800], 1);
        let new_frame = frame(&vec![2; 4_800], 1);
        for _ in 0..5 {
            mixer.push_frame("alice", &old_frame).unwrap();
        }
        mixer.push_frame("bob", &frame(&[9], 1)).unwrap();
        mixer.push_frame("alice", &new_frame).unwrap();
        assert_eq!(mixer.streams["alice"].len(), 9_600);
        assert_eq!(mixer.next_sample(), 11);
        assert_eq!(mixer.next_sample(), 11);
        assert_eq!(mixer.next_sample(), 2);
    }

    #[test]
    fn mixer_rejects_frames_larger_than_playback_budget() {
        let mut mixer = AudioMixer::new(2);
        let oversized = frame(&vec![0; MAX_OUTPUT_BUFFER_SAMPLES], 1);
        assert!(matches!(
            mixer.push_frame("alice", &oversized),
            Err(MediaError::FrameFormatMismatch)
        ));
        assert!(mixer.streams.is_empty());
    }

    #[test]
    fn deafen_discards_queued_audio_and_new_frames_until_reenabled() {
        let mut mixer = AudioMixer::new(1);
        mixer.push_frame("peer", &frame(&[11, 12], 1)).unwrap();
        mixer.set_deafened(true);
        assert_eq!(mixer.next_sample(), 0);
        mixer.push_frame("peer", &frame(&[13, 14], 1)).unwrap();
        assert!(mixer.streams.is_empty());
        mixer.set_deafened(false);
        assert_eq!(mixer.next_sample(), 0);
        mixer.push_frame("peer", &frame(&[15], 1)).unwrap();
        assert_eq!(mixer.next_sample(), 15);
    }

    #[test]
    fn controls_from_previous_join_reject_new_join_even_in_same_room() {
        let info = CallInfo {
            room_id: "!room:example.test".into(),
            slot_id: "m.call#ROOM".into(),
            membership_id: "first-membership".into(),
            local_identity: "first-identity".into(),
        };
        let old_lease = CallLease::new(info.clone(), "@alice:example.test".into(), "DEVICE".into());
        let old = AudioControls::new(old_lease.clone());
        let _running = AudioRunGuard::start(&old).unwrap();
        old.set_microphone_muted(true).unwrap();

        // RtcService::leave revokes this lease before the upstream leave await.
        old_lease.revoke();
        let second = CallLease::new(
            CallInfo {
                membership_id: "second-membership".into(),
                local_identity: "second-identity".into(),
                ..info
            },
            "@alice:example.test".into(),
            "DEVICE".into(),
        );
        assert!(!old.is_for(&second));
        assert!(matches!(
            old.set_microphone_muted(false),
            Err(MediaError::CallEnded)
        ));
        assert!(matches!(old.set_deafened(true), Err(MediaError::CallEnded)));
        let fresh = AudioControls::new(second.clone());
        assert!(fresh.is_for(&second));
        assert!(!fresh.is_running());
        let same_ids_but_new_lease = CallLease::new(
            second.info.clone(),
            "@alice:example.test".into(),
            "DEVICE".into(),
        );
        assert!(!fresh.is_for(&same_ids_but_new_lease));
    }

    #[test]
    fn glitches_and_reroutes_keep_a_stream_running() {
        use cpal::ErrorKind;
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::RealtimeDenied,
            ErrorKind::DeviceChanged,
        ] {
            assert_eq!(StreamFault::classify(kind), None, "{kind:?}");
        }
        for kind in [
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::HostUnavailable,
            ErrorKind::BackendError,
            ErrorKind::Other,
        ] {
            assert_eq!(
                StreamFault::classify(kind),
                Some(StreamFault::Lost),
                "{kind:?}"
            );
        }
        assert_eq!(
            StreamFault::classify(ErrorKind::PermissionDenied),
            Some(StreamFault::Denied)
        );
    }

    #[tokio::test]
    async fn a_lost_device_wakes_the_loop_at_once_and_glitches_do_not() {
        let health = StreamHealth::default();
        health.report(cpal::ErrorKind::Xrun);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), health.wake.notified())
                .await
                .is_err(),
            "a glitch neither wakes nor ends the stream"
        );
        assert_eq!(health.take_fault(), None);
        // Reported from a backend thread before the loop waits: the wake is
        // kept until it does.
        health.report(cpal::ErrorKind::DeviceNotAvailable);
        health.report(cpal::ErrorKind::PermissionDenied);
        tokio::time::timeout(Duration::from_secs(1), health.wake.notified())
            .await
            .expect("a lost device wakes the loop");
        assert_eq!(
            health.take_fault(),
            Some(StreamFault::Lost),
            "the first fault"
        );
        assert_eq!(health.take_fault(), None);
        health.report(cpal::ErrorKind::DeviceChanged);
        assert!(health.take_rerouted());
        assert!(!health.take_rerouted());
        assert_eq!(health.take_fault(), None);
    }

    #[test]
    fn only_exact_device_ids_are_reconnected_without_a_new_choice() {
        for exact in [
            "coreaudio:AppleUSBAudioEngine:Vendor:Headset:1234:1",
            "wasapi:{0.0.1.00000000}.{4b0b3d5e-0000-0000-0000-000000000000}",
            "alsa:hw:CARD=Headset,DEV=0",
            "alsa:sysdefault:CARD=PCH",
        ] {
            assert!(exact_identity(exact), "{exact}");
        }
        for ambiguous in [
            "alsa:default",
            "alsa:pulse",
            "alsa:hw:CARD=0,DEV=0",
            "alsa:plughw:CARD=1,DEV=0",
            "coreaudio:",
            "no host",
        ] {
            assert!(!exact_identity(ambiguous), "{ambiguous}");
        }
    }

    #[test]
    fn a_lost_microphone_reopens_only_on_its_own_exact_device() {
        // The chosen device itself, not the one in use before.
        assert_eq!(
            microphone_reopen_target(Some("coreaudio:usb"), Some("coreaudio:built-in")),
            Some("coreaudio:usb".to_owned())
        );
        // The system default was chosen: the same physical device that was
        // in use, never whatever the default is now.
        assert_eq!(
            microphone_reopen_target(None, Some("coreaudio:headset")),
            Some("coreaudio:headset".to_owned())
        );
        assert_eq!(microphone_reopen_target(None, None), None);
        // Aliases and card numbers can name another device after a replug.
        assert_eq!(microphone_reopen_target(Some("alsa:default"), None), None);
        assert_eq!(
            microphone_reopen_target(None, Some("alsa:hw:CARD=1,DEV=0")),
            None
        );
    }

    #[test]
    fn a_lost_speaker_follows_the_chosen_route() {
        assert_eq!(speaker_reopen_target(None), Some(None));
        assert_eq!(
            speaker_reopen_target(Some("coreaudio:desk")),
            Some(Some("coreaudio:desk".to_owned()))
        );
        assert_eq!(speaker_reopen_target(Some("alsa:default")), None);
    }

    #[test]
    fn a_later_microphone_is_remixed_to_the_published_channels() {
        let stereo = frame(&[100, 300, -200, 200], 2);
        let mono = conform_channels(stereo, 1);
        assert_eq!(mono.num_channels, 1);
        assert_eq!(mono.data, vec![200, 0]);
        assert_eq!(mono.samples_per_channel, 2);
        let back = conform_channels(mono, 2);
        assert_eq!(back.data, vec![200, 200, 0, 0]);
        let same = conform_channels(frame(&[1, 2], 1), 1);
        assert_eq!(same.data, vec![1, 2]);
    }

    #[test]
    fn route_reports_say_what_is_in_use_against_the_choice() {
        let name = Some("Desk mic".to_owned());
        let usb = Some("coreaudio:usb".to_owned());
        assert_eq!(
            route_for(Some(&name), &usb, &usb, false, false),
            DeviceRoute::Chosen {
                name: name.clone(),
                system_default: false
            }
        );
        assert_eq!(
            route_for(Some(&name), &None, &None, false, false),
            DeviceRoute::Chosen {
                name: name.clone(),
                system_default: true
            }
        );
        // A switch that failed keeps the device in use, and says so.
        assert_eq!(
            route_for(Some(&name), &None, &usb, false, false),
            DeviceRoute::Previous { name: name.clone() }
        );
        assert_eq!(
            route_for(Some(&name), &None, &usb, true, false),
            DeviceRoute::DefaultInstead { name: name.clone() }
        );
        assert_eq!(route_for(None, &usb, &usb, false, false), DeviceRoute::Lost);
        // A default output that followed the system: no stale device name,
        // whether it was chosen or stands in for a lost chosen speaker.
        assert_eq!(
            route_for(Some(&name), &None, &None, false, true),
            DeviceRoute::Chosen {
                name: None,
                system_default: true
            }
        );
        assert_eq!(
            route_for(Some(&name), &usb, &usb, true, true),
            DeviceRoute::DefaultInstead { name: None }
        );
    }

    #[test]
    fn a_route_request_needs_live_controls_and_outlives_cancellation() {
        let controls = guarded_controls(|| true);
        let chosen = AudioDeviceSelection {
            microphone: Some("coreaudio:usb".into()),
            speaker: None,
        };
        controls.request_route(chosen.clone(), false).unwrap();
        assert_eq!(controls.requested_route(), chosen);
        let mut reports = controls.subscribe_route();
        controls.publish_route(
            DeviceRoute::Lost,
            DeviceRoute::Opening,
            Some(RouteNotice::MicrophoneLost),
        );
        assert!(reports.has_changed().unwrap());
        let first = reports.borrow_and_update().clone();
        controls.publish_route(
            DeviceRoute::Lost,
            DeviceRoute::Opening,
            Some(RouteNotice::MicrophoneLost),
        );
        assert!(
            controls.route().generation > first.generation,
            "equal notices differ"
        );
        controls.cancel();
        assert!(matches!(
            controls.request_route(AudioDeviceSelection::default(), true),
            Err(MediaError::CallEnded)
        ));
        // A rejoin carries the choice to new controls.
        assert_eq!(controls.requested_route(), chosen);
    }

    #[test]
    fn deafen_chosen_while_no_speaker_is_open_waits_for_the_next_one() {
        let controls = guarded_controls(|| true);
        let _running = AudioRunGuard::start(&controls).unwrap();
        controls.release_speaker();
        controls.set_deafened(true).unwrap();
        assert!(controls.requested_state().deafened);
        assert!(
            !controls.applied_state().deafened,
            "nothing plays, so nothing was deafened yet"
        );
        // The choice is kept for the next speaker; install_speaker applies
        // `desired` to it (that needs a real output stream, so it is covered
        // by device acceptance, not here).
        assert!(controls.lock_state().unwrap().desired.deafened);
    }

    #[test]
    fn a_route_request_never_swaps_a_lost_microphone_for_another() {
        use MicrophoneStep::{Keep, Open, Reopen};
        let usb = Some("coreaudio:usb");
        let built_in = Some("coreaudio:built-in");
        let headset = Some("coreaudio:headset");
        // A new choice opens as chosen, the system default included.
        assert_eq!(
            microphone_step(usb, built_in, built_in, true, None, false),
            Open(Some("coreaudio:usb".into()))
        );
        assert_eq!(
            microphone_step(None, usb, usb, false, usb, false),
            Open(None)
        );
        // The same choice while its microphone is in use: nothing to open,
        // even when the user repeats the request.
        assert_eq!(microphone_step(usb, usb, usb, true, None, false), Keep);
        assert_eq!(microphone_step(usb, usb, usb, true, None, true), Keep);
        // The same choice again after it could not be opened: retried while
        // another microphone is in use.
        assert_eq!(
            microphone_step(usb, usb, built_in, true, None, false),
            Open(Some("coreaudio:usb".into()))
        );
        // A request that changes only the speaker, while the microphone is
        // lost: the default (or an alias) may now be a different microphone,
        // so only the exact lost or chosen device is reopened, quietly.
        assert_eq!(
            microphone_step(None, None, None, false, headset, false),
            Reopen("coreaudio:headset".into())
        );
        assert_eq!(microphone_step(None, None, None, false, None, false), Keep);
        let alias = Some("alsa:default");
        assert_eq!(
            microphone_step(alias, alias, alias, false, None, false),
            Keep
        );
        // The default was chosen but could not be opened, so the call stayed
        // on USB, which was then unplugged. A later speaker-only request
        // waits for USB; it never opens whatever the default is now.
        assert_eq!(
            microphone_step(None, None, usb, false, usb, false),
            Reopen("coreaudio:usb".into())
        );
        // Only the user's own request repeated unchanged is an explicit
        // microphone choice while none is open (a microphone with no exact
        // identity waits for exactly that).
        assert_eq!(
            microphone_step(None, None, None, false, None, true),
            Open(None)
        );
        assert_eq!(
            microphone_step(alias, alias, alias, false, None, true),
            Open(Some("alsa:default".into()))
        );
    }

    fn resume_point(microphone: Option<&str>, lost: bool) -> AudioResume {
        AudioResume {
            route: AudioDeviceSelection::default(),
            last_request: AudioDeviceSelection::default(),
            microphone: microphone.map(str::to_owned),
            microphone_lost: lost,
            microphone_loss: RouteNotice::MicrophoneDenied,
            speaker_fallback: false,
            speaker_lost: false,
            channels: 1,
        }
    }

    #[test]
    fn a_rejoin_reopens_a_microphone_only_by_its_retained_exact_id() {
        // The device in use, by its exact ID.
        assert_eq!(
            resume_microphone(&resume_point(Some("coreaudio:usb"), false)),
            ResumeMicrophone::Open("coreaudio:usb".into())
        );
        // No exact identity: never the route's System default or an alias,
        // which may name a different microphone now. It stays unresumed.
        for identity in [None, Some("alsa:default"), Some("alsa:hw:CARD=1,DEV=0")] {
            assert_eq!(
                resume_microphone(&resume_point(identity, false)),
                ResumeMicrophone::Wait {
                    lost: None,
                    notice: RouteNotice::MicrophoneNotResumed
                },
                "{identity:?}"
            );
        }
        // A lost one keeps waiting for its own exact device, with its reason.
        assert_eq!(
            resume_microphone(&resume_point(Some("coreaudio:usb"), true)),
            ResumeMicrophone::Wait {
                lost: Some("coreaudio:usb".into()),
                notice: RouteNotice::MicrophoneDenied
            }
        );
        assert_eq!(
            resume_microphone(&resume_point(Some("alsa:default"), true)),
            ResumeMicrophone::Wait {
                lost: None,
                notice: RouteNotice::MicrophoneDenied
            }
        );
    }

    #[test]
    fn reconnecting_never_starts_an_open_that_could_outlast_its_window() {
        let start = tokio::time::Instant::now();
        let deadline = start + RECONNECT_WINDOW;
        assert!(reconnect_open_fits(start, deadline));
        assert!(reconnect_open_fits(
            deadline - DEVICE_OPEN_TIMEOUT,
            deadline
        ));
        assert!(!reconnect_open_fits(
            deadline - DEVICE_OPEN_TIMEOUT + Duration::from_millis(1),
            deadline
        ));
        assert!(!reconnect_open_fits(deadline, deadline));
    }

    #[test]
    fn a_join_lost_while_its_track_publishes_already_left_its_resume_point() {
        // The production step between the device opens and the publication
        // await (`prepare_publication`): a join lost while publishing has
        // already recorded where it was, so the rejoin hand-off finds it.
        let controls = guarded_controls(|| true);
        let usb = AudioDeviceSelection {
            microphone: Some("coreaudio:usb".into()),
            speaker: None,
        };
        let mut local = LocalAudio::closed(usb.clone(), usb.clone(), 2);
        local.microphone_lost = true;
        local.lost_microphone_id = Some("coreaudio:usb".into());
        assert_eq!(controls.resume_point(), None, "nothing recorded yet");
        assert_eq!(local.prepare_publication(&controls), (SAMPLE_RATE, 2));
        let point = controls.resume_point().expect("recorded before publishing");
        assert_eq!(point.retained_microphone(), Some("coreaudio:usb".into()));
        // An open microphone with no exact identity is recorded as unknown:
        // the rejoin keeps it closed rather than reopening the route.
        let default = AudioDeviceSelection::default();
        let mut local = LocalAudio::closed(default.clone(), default, 1);
        local.prepare_publication(&controls);
        assert_eq!(controls.resume_point().unwrap().retained_microphone(), None);
    }

    #[test]
    fn a_rejoin_with_no_recorded_point_reopens_only_an_exact_chosen_microphone() {
        let exact = AudioDeviceSelection {
            microphone: Some("coreaudio:usb".into()),
            speaker: None,
        };
        assert_eq!(
            AudioResume::unestablished(&exact).retained_microphone(),
            Some("coreaudio:usb".into())
        );
        for chosen in [None, Some("alsa:default"), Some("alsa:hw:CARD=1,DEV=0")] {
            let route = AudioDeviceSelection {
                microphone: chosen.map(str::to_owned),
                speaker: None,
            };
            let point = AudioResume::unestablished(&route);
            assert_eq!(point.retained_microphone(), None, "{chosen:?}");
            assert_eq!(
                resume_microphone(&point),
                ResumeMicrophone::Wait {
                    lost: None,
                    notice: RouteNotice::MicrophoneNotResumed
                }
            );
        }
    }
}
