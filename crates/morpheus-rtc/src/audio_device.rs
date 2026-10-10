use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use cpal::{
    SampleFormat, Stream, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::StreamExt;
use matrix_rtc_call_sdk::CallEvent;
use matrix_rtc_transport::AudioFrame;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Duration, MissedTickBehavior},
};

use crate::{CallInfo, MediaError, Publication, RtcService, native::CallLease};

const SAMPLE_RATE: u32 = 48_000;
const CHUNK_SAMPLES_PER_CHANNEL: usize = 480; // 10 ms
const MAX_OUTPUT_BUFFER_SAMPLES: usize = SAMPLE_RATE as usize; // 500 ms stereo
const MAX_REMOTE_STREAMS: usize = 64;
const REMOTE_FRAME_CHANNEL_CAPACITY: usize = 128;
// A hung platform device open must not accumulate one blocked thread per
// retry, or keep Tokio's blocking pool alive when the app quits.
static DEVICE_OPEN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

struct DeviceOpenGuard;

impl Drop for DeviceOpenGuard {
    fn drop(&mut self) {
        DEVICE_OPEN_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

/// Native microphone PCM capture. Frames are 10 ms at 48 kHz, with one or two
/// channels matching the selected input device. Keep this value alive while
/// pumping `next_frame()` into a microphone `Publication`.
pub struct MicrophoneCapture {
    _stream: Stream,
    rx: mpsc::Receiver<AudioFrame>,
    channels: u32,
    stream_error: Arc<Mutex<Option<String>>>,
}

impl MicrophoneCapture {
    /// List input device names for a device picker. This proof currently opens
    /// the default device; names do not serve as stable device identifiers.
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
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| MediaError::Device("no default microphone".into()))?;
        let config = select_48k_config(&device, true)?;
        let channels = u32::from(config.channels());
        let sample_format = config.sample_format();
        let chunk_len = CHUNK_SAMPLES_PER_CHANNEL * channels as usize;
        let (tx, rx) = mpsc::channel(32);
        let stream_error = Arc::new(Mutex::new(None));
        let error_for_callback = Arc::clone(&stream_error);
        let mut pending = VecDeque::<i16>::with_capacity(chunk_len * 2);
        let stream = device
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
                move |error| {
                    if let Ok(mut last) = error_for_callback.lock() {
                        *last = Some(error.to_string());
                    }
                },
                None,
            )
            .map_err(|error| MediaError::Device(error.to_string()))?;
        stream
            .play()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            rx,
            channels,
            stream_error,
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

    pub async fn next_frame(&mut self) -> Result<AudioFrame, MediaError> {
        if let Some(error) = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut last| last.take())
        {
            return Err(MediaError::Device(error));
        }
        self.rx
            .recv()
            .await
            .ok_or_else(|| MediaError::Device("microphone stream stopped".into()))
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
    stream_error: Arc<Mutex<Option<String>>>,
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
    /// List output device names for a device picker. This proof currently
    /// opens the default device.
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
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| MediaError::Device("no default speaker".into()))?;
        let config = select_48k_config(&device, false)?;
        let channels = u32::from(config.channels());
        let sample_format = config.sample_format();
        let mixer = Arc::new(Mutex::new(AudioMixer::new(channels)));
        let mixer_for_callback = Arc::clone(&mixer);
        let stream_error = Arc::new(Mutex::new(None));
        let error_for_callback = Arc::clone(&stream_error);
        let stream = device
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
                                    *sample = mixer.as_mut().map(|m| m.next_sample()).unwrap_or(0);
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
                                    ) + 32768) as u16;
                                }
                            }
                        }
                        _ => {}
                    }
                },
                move |error| {
                    if let Ok(mut last) = error_for_callback.lock() {
                        *last = Some(error.to_string());
                    }
                },
                None,
            )
            .map_err(|error| MediaError::Device(error.to_string()))?;
        stream
            .play()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            mixer,
            stream_error,
        })
    }

    /// Compatibility path for callers with a single remote stream.
    pub fn push_frame(&self, frame: &AudioFrame) -> Result<(), MediaError> {
        self.push_frame_for("_single_stream", frame)
    }

    pub fn push_frame_for(&self, member_id: &str, frame: &AudioFrame) -> Result<(), MediaError> {
        if let Some(error) = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut last| last.take())
        {
            return Err(MediaError::Device(error));
        }
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
    microphone: Option<Arc<Publication>>,
    speaker_mixer: Option<Arc<Mutex<AudioMixer>>>,
}

/// Commands for one account, room, and MatrixRTC membership. Old controls
/// cannot operate a subsequent join, even if it is in the same room.
#[derive(Clone)]
pub struct AudioControls {
    lease: CallLease,
    state: Arc<Mutex<AudioControlState>>,
    requested: watch::Sender<AudioState>,
    applied: watch::Sender<AudioState>,
    running: Arc<AtomicBool>,
}

impl AudioControls {
    fn new(lease: CallLease) -> Self {
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
            state: Arc::new(Mutex::new(AudioControlState {
                desired,
                applied,
                microphone: None,
                speaker_mixer: None,
            })),
            requested,
            applied: applied_updates,
            running: Arc::new(AtomicBool::new(false)),
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

    pub fn is_running(&self) -> bool {
        self.lease.is_active() && self.running.load(Ordering::Acquire)
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
        if !self.lease.is_active() {
            return Err(MediaError::CallEnded);
        }
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
        speaker: &AudioOutput,
        microphone: &mut MicrophoneCapture,
    ) -> Result<(), MediaError> {
        // The transport publishes muted, including while the OS devices open.
        microphone.discard_buffered_frames();
        let mut state = self.lock_state()?;
        speaker.set_deafened(state.desired.deafened)?;
        if !state.desired.microphone_muted {
            publication.set_muted(false)?;
        }
        state.applied = state.desired;
        state.microphone = Some(publication);
        state.speaker_mixer = Some(Arc::clone(&speaker.mixer));
        self.applied.send_replace(state.applied);
        Ok(())
    }

    fn apply_pending_unmute(&self, microphone: &mut MicrophoneCapture) -> Result<(), MediaError> {
        let needs_unmute = {
            let state = self.lock_state()?;
            !state.desired.microphone_muted && state.applied.microphone_muted
        };
        if needs_unmute {
            microphone.discard_buffered_frames();
            let mut state = self.lock_state()?;
            if !state.desired.microphone_muted && state.applied.microphone_muted {
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
        let state = self.lock_state()?;
        Ok(!state.applied.microphone_muted)
    }
}

struct AudioRunGuard<'a>(&'a AudioControls);

impl AudioRunGuard<'_> {
    fn start(controls: &AudioControls) -> Result<AudioRunGuard<'_>, MediaError> {
        controls
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MediaError::ControlAlreadyRunning)?;
        Ok(AudioRunGuard(controls))
    }
}

impl Drop for AudioRunGuard<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
        if let Ok(mut state) = self.0.state.lock() {
            if let Some(publication) = state.microphone.take() {
                let _ = publication.set_muted(true);
            }
            state.speaker_mixer = None;
            state.applied.microphone_muted = true;
            self.0.applied.send_replace(state.applied);
        }
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

    /// Run the default microphone and speaker for a joined call.
    /// Call this only after an explicit user action that authorizes microphone
    /// capture. It returns when the call ends or the media path fails; dropping
    /// the future mutes the publication and aborts remote frame pumps. Each
    /// remote microphone gets a bounded playback queue; device switching and
    /// network jitter control are still outside this adapter.
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
        // CoreAudio can block inside AudioUnitSetProperty while opening the
        // microphone (for example while macOS resolves device permissions).
        // Keeping that synchronous call on the Tokio worker also starves the
        // call's roster and remote video futures. Open both devices on a
        // blocking thread and bound the wait so the call can leave cleanly.
        if DEVICE_OPEN_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(MediaError::Device(
                "audio device setup is still in progress".into(),
            ));
        }
        let opening_lease = lease.clone();
        let (device_tx, device_rx) = tokio::sync::oneshot::channel();
        let opened = std::thread::Builder::new()
            .name("morpheus-audio-open".into())
            .spawn(move || {
                let _opening = DeviceOpenGuard;
                let result = (|| {
                    if !opening_lease.is_active() {
                        return Err(MediaError::CallEnded);
                    }
                    let microphone = MicrophoneCapture::open_default()?;
                    if !opening_lease.is_active() {
                        return Err(MediaError::CallEnded);
                    }
                    let speaker = AudioOutput::open_default()?;
                    if !opening_lease.is_active() {
                        return Err(MediaError::CallEnded);
                    }
                    Ok((microphone, speaker))
                })();
                let _ = device_tx.send(result);
            });
        if opened.is_err() {
            DEVICE_OPEN_IN_PROGRESS.store(false, Ordering::SeqCst);
            return Err(MediaError::Device(
                "could not start audio device setup".into(),
            ));
        }
        // The OS thread is deliberately detached. If CoreAudio never returns,
        // Tokio shutdown and the rest of the call UI still make progress.
        let (mut microphone, speaker) = tokio::time::timeout(Duration::from_secs(30), device_rx)
            .await
            .map_err(|_| MediaError::Device("audio device initialization timed out".into()))?
            .map_err(|_| MediaError::Device("audio device setup task failed".into()))??;
        let publication = Arc::new(
            self.publish_microphone(
                room_id,
                microphone.sample_rate(),
                microphone.channels(),
                true,
            )
            .await?,
        );
        let _mute_on_drop = MuteOnDrop(&publication);
        let mut events = call.subscribe_call_events();
        let (remote_tx, mut remote_rx) = mpsc::channel(REMOTE_FRAME_CHANNEL_CAPACITY);
        let mut remotes = RemoteAudioStreams::new(remote_tx);
        let mut roster_refresh = tokio::time::interval(Duration::from_secs(2));
        roster_refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let result = async {
            controls.attach(Arc::clone(&publication), &speaker, &mut microphone)?;
            let _ = *requests.borrow_and_update();
            controls.apply_pending_unmute(&mut microphone)?;
            remotes.reconcile(call, &speaker)?;
            loop {
                tokio::select! {
                    biased;
                    changed = requests.changed() => {
                        if changed.is_err() {
                            break Ok(());
                        }
                        let _ = *requests.borrow_and_update();
                        controls.apply_pending_unmute(&mut microphone)?;
                    }
                    _ = roster_refresh.tick() => {
                        // A frame stream can close without a matching call
                        // event. Retry subscribed tracks at a bounded rate.
                        remotes.reconcile(call, &speaker)?;
                    }
                    frame = microphone.next_frame() => {
                        let frame = frame?;
                        if controls.should_send_microphone()? {
                            publication.push_audio(frame).await?;
                        }
                    }
                    event = events.recv() => {
                        match event {
                            Ok(CallEvent::StreamStarted { member_id, kind })
                                if kind == matrix_rtc_transport::MediaStreamKind::Microphone
                                    && member_id != call.membership_id() => {
                                remotes.start(call, &speaker, &member_id)?;
                            }
                            Ok(CallEvent::StreamStopped { member_id, kind })
                                if kind == matrix_rtc_transport::MediaStreamKind::Microphone => {
                                remotes.stop(&speaker, &member_id)?;
                            }
                            Ok(CallEvent::ParticipantLeft { member_id }) => {
                                remotes.stop(&speaker, &member_id)?;
                            }
                            Ok(CallEvent::Ended { .. }) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break Ok(());
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                remotes.reconcile(call, &speaker)?;
                            }
                            Ok(_) => {}
                        }
                    }
                    remote = remote_rx.recv() => {
                        match remote {
                            Some(RemoteAudioEvent::Frame { member_id, generation, frame })
                                if remotes.is_current(&member_id, generation) => {
                                speaker.push_frame_for(&member_id, &frame)?;
                            }
                            Some(RemoteAudioEvent::Ended { member_id, generation })
                                if remotes.is_current(&member_id, generation) => {
                                remotes.stop(&speaker, &member_id)?;
                            }
                            Some(_) => {}, // An old stream was replaced or stopped.
                            None => break Ok(()),
                        }
                    }
                }
            }
        }
        .await;
        drop(remotes);
        let _ = publication.set_muted(true);
        let _ = publication.unpublish().await;
        result
    }

    /// Backward-compatible name for the original two-person proof. The audio
    /// path now receives and mixes every subscribed remote microphone.
    pub async fn run_two_person_audio(&self, room_id: &str) -> Result<(), MediaError> {
        self.run_audio(room_id).await
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
        speaker: &AudioOutput,
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

    fn stop(&mut self, speaker: &AudioOutput, member_id: &str) -> Result<(), MediaError> {
        if let Some(task) = self.tasks.remove(member_id) {
            task.task.abort();
        }
        speaker.remove_stream(member_id)
    }

    fn reconcile(
        &mut self,
        call: &matrix_rtc_call_sdk::LiveKitCall,
        speaker: &AudioOutput,
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
}
