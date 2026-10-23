//! Original quiet output-only ringtone. No microphone, media publication or file
//! is opened. A retained supervisor holds the permit through returned application
//! stream Drop and actual worker/TLS join; backend native disposal remains open.
//! Native open/drop cannot be interrupted. Cancellation
//! prevents later startup steps and makes subsequent callback buffers silent;
//! samples already submitted to the OS cannot be recalled.

use cpal::{
    SampleFormat, Stream, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const RATE: u32 = 48_000;
const MAX_DEVICES: usize = 512;
const MAX_CONFIGS: usize = 512;
const MAX_RING: Duration = Duration::from_secs(30);
static OWNED: AtomicBool = AtomicBool::new(false);
static RING_WORKERS: RingWorkers = RingWorkers::new();
const RING_RELEASE_WAIT: Duration = Duration::from_secs(3);

// One actual output owner for both ringtone and attachment PCM. A ringtone
// reservation retires old PCM before waiting for that owner's native teardown.
struct OutputArbitration {
    ringing: AtomicBool,
    epoch: AtomicU64,
}
impl OutputArbitration {
    const fn new() -> Self {
        Self {
            ringing: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
        }
    }
    fn reserve_ring(&self) -> Option<RingReservation<'_>> {
        self.ringing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        if self
            .epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .is_err()
        {
            self.ringing.store(false, Ordering::Release);
            return None;
        }
        Some(RingReservation(self))
    }
    fn allows_pcm(&self, original: u64) -> bool {
        original != u64::MAX
            && !self.ringing.load(Ordering::Acquire)
            && self.epoch.load(Ordering::Acquire) == original
    }
}
struct RingReservation<'a>(&'a OutputArbitration);
impl Drop for RingReservation<'_> {
    fn drop(&mut self) {
        self.0.ringing.store(false, Ordering::Release);
    }
}
static OUTPUT: OutputArbitration = OutputArbitration::new();

struct Permit;
impl Permit {
    fn take() -> Option<Self> {
        OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        OWNED.store(false, Ordering::Release);
    }
}

// Mac native teardown is fallible after the PCM producer returns or unwinds.
// One process-retained slot owns the exact Permit before the first native step;
// no playback handle, stack unwind or cancelled observer can drop that guard.
#[cfg(target_os = "macos")]
static PCM_NATIVE_OWNERS: PcmNativeOwners = PcmNativeOwners::new();
#[cfg(target_os = "macos")]
const PCM_FRESH: u8 = 0;
#[cfg(target_os = "macos")]
const PCM_PRODUCING: u8 = 1;
#[cfg(target_os = "macos")]
const PCM_TERMINAL: u8 = 2;
#[cfg(target_os = "macos")]
const PCM_CLOSING: u8 = 3;
#[cfg(target_os = "macos")]
const PCM_RELEASED: u8 = 4;
#[cfg(target_os = "macos")]
const PCM_FAILED: u8 = 5;
#[cfg(target_os = "macos")]
struct PcmNativeIdentity(AtomicU8);
#[cfg(target_os = "macos")]
impl PcmNativeIdentity {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU8::new(PCM_FRESH)))
    }
}
#[cfg(target_os = "macos")]
struct PcmNativeOwner {
    identity: Arc<PcmNativeIdentity>,
    _permit: Permit,
}
#[cfg(target_os = "macos")]
struct PcmNativeOwners(Mutex<Option<PcmNativeOwner>>);
#[cfg(target_os = "macos")]
impl PcmNativeOwners {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }
    fn admit(
        &self,
        identity: &Arc<PcmNativeIdentity>,
        run: &Run,
    ) -> Result<PcmProducer, RingAudioError> {
        let mut owner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if owner.is_some() {
            return Err(RingAudioError::Busy);
        }
        if !run.active() {
            return Err(RingAudioError::Cancelled);
        }
        let permit = Permit::take().ok_or(RingAudioError::Busy)?;
        // Each playback can produce only once. An old terminal/close observer
        // can never become the release authority for a later production body.
        identity
            .0
            .compare_exchange(
                PCM_FRESH,
                PCM_PRODUCING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| RingAudioError::Busy)?;
        *owner = Some(PcmNativeOwner {
            identity: identity.clone(),
            _permit: permit,
        });
        Ok(PcmProducer(identity.clone()))
    }
    fn close_with(
        &self,
        identity: &Arc<PcmNativeIdentity>,
        close: impl FnOnce() -> Result<(), ()>,
    ) -> Result<(), ()> {
        match identity.0.compare_exchange(
            PCM_TERMINAL,
            PCM_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            // Fresh means this playback never admitted native PCM. Released
            // means its exact checked close already succeeded. Neither path
            // touches another playback's owner or claims a new native close.
            Err(PCM_FRESH | PCM_RELEASED) => return Ok(()),
            // In particular, stop/cancel cannot release a producing body, and
            // concurrent close, panic or terminal uncertainty never retries it.
            Err(_) => return Err(()),
        }
        // No registry lock is held across native teardown. A panic leaves the
        // exact owner in PCM_CLOSING and keeps its guard retained indefinitely.
        if close().is_err() {
            identity.0.store(PCM_FAILED, Ordering::Release);
            return Err(());
        }
        let mut owner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !owner
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(&owner.identity, identity))
        {
            identity.0.store(PCM_FAILED, Ordering::Release);
            return Err(());
        }
        drop(owner.take());
        identity.0.store(PCM_RELEASED, Ordering::Release);
        Ok(())
    }
}
#[cfg(target_os = "macos")]
struct PcmProducer(Arc<PcmNativeIdentity>);
#[cfg(target_os = "macos")]
impl Drop for PcmProducer {
    fn drop(&mut self) {
        // This marker is created before open_pcm and destroyed only after its
        // frame (including stream/sample Drop) has returned or fully unwound.
        // It grants no cleanup receipt and never releases the actual Permit.
        let _ = self.0.0.compare_exchange(
            PCM_PRODUCING,
            PCM_TERMINAL,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Fixed categories; backend errors and endpoint IDs are never retained/logged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingAudioError {
    Busy,
    Unavailable,
    Cancelled,
}

/// The authority function runs on the native callback as well as the owner.
/// It MUST use only nonblocking atomics/immutable data, with no locks, allocation,
/// logging or I/O. It is not an admission API: the application supplies a lease
/// from its authenticated incoming-offer controller.
pub type RingAudioAuthority = Arc<dyn Fn() -> bool + Send + Sync>;

struct Run {
    stopped: AtomicBool,
    // 0 opening, 1 playing, 2 stopped, 3 unavailable. No private diagnostics.
    status: AtomicU8,
    deadline: Instant,
    allowed: RingAudioAuthority,
}
impl Run {
    fn active(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        if Instant::now() >= self.deadline || !(self.allowed)() {
            self.stopped.store(true, Ordering::Release);
            return false;
        }
        true
    }
}

/// The receipt is for the actual application ringtone worker and its TLS.
/// It does not claim CPAL backend monitor/callback disposal quiescence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingAudioReleaseError {
    TimedOut,
    WorkerPanicked,
    SupervisorFailed,
    OutputOwnerUnconfirmed,
    #[cfg(target_os = "macos")]
    NativeClose,
}
impl std::fmt::Display for RingAudioReleaseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::TimedOut => "incoming sound worker release was not confirmed before the deadline",
            Self::WorkerPanicked => "incoming sound worker panicked during playback or release",
            Self::SupervisorFailed => "incoming sound worker ownership could not be confirmed",
            #[cfg(target_os = "macos")]
            Self::NativeClose => "CoreAudio incoming sound cleanup failed and remains owned",
            Self::OutputOwnerUnconfirmed => {
                "the previous output worker release could not be confirmed"
            }
        })
    }
}
impl std::error::Error for RingAudioReleaseError {}
#[derive(Default)]
struct RingCompletion(Mutex<Option<Result<(), RingAudioReleaseError>>>);
impl RingCompletion {
    fn finish(&self, result: Result<(), RingAudioReleaseError>) {
        let mut stored = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if stored.is_none() {
            *stored = Some(result);
        }
    }
    fn result(&self) -> Option<Result<(), RingAudioReleaseError>> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
// The global owner retains both arbitration guards independently of the actual
// worker. Its body returning, unwinding or dropping TLS cannot release them.
// A failed receipt keeps this whole owner alive, including any output permit.
struct RingResources {
    _reservation: RingReservation<'static>,
    permit: Mutex<Option<Permit>>,
}
impl RingResources {
    fn retain_permit(&self, permit: Permit) {
        // Exactly one actual worker acquires once, before its first native
        // device step; no other path replaces or takes this slot before join.
        *self.permit.lock().unwrap_or_else(|e| e.into_inner()) = Some(permit);
    }
}
enum RingWorkerError {
    Playback(RingAudioError),
    Release(RingAudioReleaseError),
}
impl From<RingAudioError> for RingWorkerError {
    fn from(error: RingAudioError) -> Self {
        Self::Playback(error)
    }
}

struct RingOwner {
    supervisor: Option<thread::JoinHandle<()>>,
    completed: Arc<RingCompletion>,
    run: Arc<Run>,
    _resources: Arc<RingResources>,
}
struct RingWorkers(Mutex<Option<RingOwner>>);
impl RingWorkers {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }
    fn poll_locked(owner: &mut Option<RingOwner>) -> Option<Result<(), RingAudioReleaseError>> {
        let Some(active) = owner.as_mut() else {
            return Some(Ok(()));
        };
        if let Some(Err(error)) = active.completed.result() {
            active.run.stopped.store(true, Ordering::Release);
            active.run.status.store(3, Ordering::Release);
            return Some(Err(error));
        }
        let Some(supervisor) = active.supervisor.as_ref() else {
            active.run.stopped.store(true, Ordering::Release);
            active.run.status.store(3, Ordering::Release);
            return Some(Err(RingAudioReleaseError::SupervisorFailed));
        };
        if !supervisor.is_finished() {
            return None;
        }
        // Actual native work and its TLS have already been joined on this
        // fixed supervisor. std's final join epilogue has no hard OS-time bound.
        let joined = active
            .supervisor
            .take()
            .expect("retained ringtone supervisor")
            .join();
        if joined.is_err() || active.completed.result() != Some(Ok(())) {
            active
                .completed
                .finish(Err(RingAudioReleaseError::SupervisorFailed));
            active.run.stopped.store(true, Ordering::Release);
            active.run.status.store(3, Ordering::Release);
            return Some(Err(RingAudioReleaseError::SupervisorFailed));
        }
        *owner = None;
        Some(Ok(()))
    }
    fn poll_release(&self) -> Option<Result<(), RingAudioReleaseError>> {
        Self::poll_locked(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
    fn start_with(
        &self,
        run: Arc<Run>,
        output: &'static OutputArbitration,
        work: impl FnOnce(&Arc<Run>, &RingResources) -> Result<(), RingWorkerError> + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<thread::JoinHandle<()>>,
    ) -> Result<(), RingAudioError> {
        let mut owner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match Self::poll_locked(&mut owner) {
            None => return Err(RingAudioError::Busy),
            Some(Err(_)) => return Err(RingAudioError::Unavailable),
            Some(Ok(())) => {}
        }
        if !run.active() {
            return Err(RingAudioError::Cancelled);
        }
        let reservation = output.reserve_ring().ok_or(RingAudioError::Busy)?;
        let resources = Arc::new(RingResources {
            _reservation: reservation,
            permit: Mutex::new(None),
        });
        let completed = Arc::new(RingCompletion::default());
        let (start, admitted) = std::sync::mpsc::sync_channel(1);
        let worker_run = run.clone();
        let worker_resources = resources.clone();
        let finished = completed.clone();
        let supervisor = spawn(Box::new(move || {
            // Before publication/start, no actual native worker exists. Failed
            // supervisor creation cannot discard the sole native JoinHandle.
            if admitted.recv().is_err() {
                drop(work);
                finished.finish(Ok(()));
                return;
            }
            let actual_run = worker_run.clone();
            let actual = thread::Builder::new()
                .name("morpheus-ring-output".into())
                .spawn(move || run_ring_worker(actual_run, worker_resources, work));
            let result = match actual {
                Ok(actual) => actual
                    .join()
                    .unwrap_or(Err(RingAudioReleaseError::WorkerPanicked)),
                Err(_) => {
                    // No actual worker was created. Keep the original fixed
                    // unavailable status; there is no native owner left to join.
                    worker_run.status.store(3, Ordering::Release);
                    worker_run.stopped.store(true, Ordering::Release);
                    Ok(())
                }
            };
            finished.finish(result);
        }))
        .map_err(|_| RingAudioError::Unavailable)?;
        *owner = Some(RingOwner {
            supervisor: Some(supervisor),
            completed: completed.clone(),
            run,
            _resources: resources,
        });
        if start.send(()).is_err() {
            completed.finish(Err(RingAudioReleaseError::SupervisorFailed));
            return Err(RingAudioError::Unavailable);
        }
        Ok(())
    }
    async fn wait(&self, bound: Duration) -> Result<(), RingAudioReleaseError> {
        let until = tokio::time::Instant::now() + bound;
        loop {
            if let Some(result) = self.poll_release() {
                return result;
            }
            if tokio::time::Instant::now() >= until {
                return Err(RingAudioReleaseError::TimedOut);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
fn run_ring_worker(
    run: Arc<Run>,
    resources: Arc<RingResources>,
    work: impl FnOnce(&Arc<Run>, &RingResources) -> Result<(), RingWorkerError>,
) -> Result<(), RingAudioReleaseError> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !run.active() {
            return Err(RingWorkerError::Playback(RingAudioError::Cancelled));
        }
        work(&run, &resources)
    }));
    #[cfg(target_os = "macos")]
    let native_close = crate::coreaudio_close::close_retired_coreaudio_on_owned_worker();
    let released = match &result {
        Err(_) => Err(RingAudioReleaseError::WorkerPanicked),
        Ok(Err(RingWorkerError::Release(error))) => Err(*error),
        #[cfg(target_os = "macos")]
        _ if native_close.is_err() => Err(RingAudioReleaseError::NativeClose),
        _ => Ok(()),
    };
    let failed = released.is_err()
        || matches!(
            &result,
            Ok(Err(RingWorkerError::Playback(RingAudioError::Unavailable)))
        )
        || run.status.load(Ordering::Acquire) == 3;
    run.status
        .store(if failed { 3 } else { 2 }, Ordering::Release);
    run.stopped.store(true, Ordering::Release);
    released
}
/// Call/update reservation or incoming shutdown must revoke sound first. This
/// bounded observation then requires the retained actual ringtone worker/TLS
/// join; cancellation/timeout never detach it or release its output guards.
pub async fn wait_for_ring_audio_release() -> Result<(), RingAudioReleaseError> {
    RING_WORKERS.wait(RING_RELEASE_WAIT).await
}

/// Drop only revokes callback authority; it never waits on an OS driver. The
/// retained process owner keeps the real worker handle and arbitration guards
/// through actual join/TLS, even after this presentation handle disappears.
pub struct RingPlayback {
    run: Arc<Run>,
}
impl RingPlayback {
    pub fn start(
        speaker: Option<String>,
        expiry: Instant,
        allowed: RingAudioAuthority,
    ) -> Result<Self, RingAudioError> {
        Self::start_after_output_release(speaker, expiry, allowed, || Ok(()))
    }

    /// The existing application output owner supplies its actual join/TLS
    /// receipt. The ring reservation retires PCM before this runs, on our
    /// retained std worker only. The caller must never wait for this same ring
    /// from that receipt; new PCM admission is rejected while ring-reserved.
    pub fn start_after_output_release(
        speaker: Option<String>,
        expiry: Instant,
        allowed: RingAudioAuthority,
        previous_output: impl FnOnce() -> Result<(), RingAudioReleaseError> + Send + 'static,
    ) -> Result<Self, RingAudioError> {
        if speaker
            .as_ref()
            .is_some_and(|id| id.len() > 4096 || id.chars().any(char::is_control))
        {
            return Err(RingAudioError::Unavailable);
        }
        let now = Instant::now();
        let deadline = expiry.min(
            now.checked_add(MAX_RING)
                .ok_or(RingAudioError::Unavailable)?,
        );
        let run = Arc::new(Run {
            stopped: AtomicBool::new(false),
            status: AtomicU8::new(0),
            deadline,
            allowed,
        });
        RING_WORKERS.start_with(
            run.clone(),
            &OUTPUT,
            move |worker, resources| {
                after_output_release(worker, previous_output, || {
                    let permit = acquire_output(worker)?;
                    resources.retain_permit(permit);
                    play(worker, speaker.as_deref()).map_err(RingWorkerError::Playback)
                })
            },
            |supervise| {
                thread::Builder::new()
                    .name("morpheus-ring-owner".into())
                    .spawn(supervise)
            },
        )?;
        Ok(Self { run })
    }
    pub fn stop(&self) {
        self.run.stopped.store(true, Ordering::Release);
    }
    /// Audible authority only, never a native worker-release receipt.
    pub fn is_active(&self) -> bool {
        self.run.active()
    }
    pub fn unavailable(&self) -> bool {
        self.run.status.load(Ordering::Acquire) == 3
    }
}
impl Drop for RingPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

fn after_output_release(
    run: &Run,
    previous_output: impl FnOnce() -> Result<(), RingAudioReleaseError>,
    native: impl FnOnce() -> Result<(), RingWorkerError>,
) -> Result<(), RingWorkerError> {
    if !run.active() {
        return Err(RingWorkerError::Playback(RingAudioError::Cancelled));
    }
    previous_output().map_err(RingWorkerError::Release)?;
    // An OS/account/Answer/expiry revocation can win while the actual previous
    // output owner is joining. It must prevent both Permit and CPAL admission.
    if !run.active() {
        return Err(RingWorkerError::Playback(RingAudioError::Cancelled));
    }
    native()
}

fn acquire_output(run: &Run) -> Result<Permit, RingAudioError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if !run.active() {
            return Err(RingAudioError::Cancelled);
        }
        if let Some(permit) = Permit::take() {
            return Ok(permit);
        }
        if Instant::now() >= deadline {
            return Err(RingAudioError::Unavailable);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn entered<T>(
    run: &Run,
    work: impl FnOnce() -> Result<T, RingAudioError>,
) -> Result<T, RingAudioError> {
    if !run.active() {
        return Err(RingAudioError::Cancelled);
    }
    let value = work()?;
    if !run.active() {
        return Err(RingAudioError::Cancelled);
    }
    Ok(value)
}
fn speaker(run: &Run, selected: Option<&str>) -> Result<cpal::Device, RingAudioError> {
    let host = entered(run, || Ok(cpal::default_host()))?;
    let Some(selected) = selected else {
        return entered(run, || {
            host.default_output_device()
                .ok_or(RingAudioError::Unavailable)
        });
    };
    let mut devices = entered(run, || {
        host.devices().map_err(|_| RingAudioError::Unavailable)
    })?;
    find_selected(
        run,
        selected,
        || Ok(devices.next()),
        |device| {
            device
                .id()
                .map(|id| id.to_string())
                .map_err(|_| RingAudioError::Unavailable)
        },
    )
}
fn find_selected<T>(
    run: &Run,
    selected: &str,
    mut next: impl FnMut() -> Result<Option<T>, RingAudioError>,
    mut identity: impl FnMut(&T) -> Result<String, RingAudioError>,
) -> Result<T, RingAudioError> {
    let mut found = None;
    for _ in 0..MAX_DEVICES {
        let Some(device) = entered(run, &mut next)? else {
            return found.ok_or(RingAudioError::Unavailable);
        };
        // Identity first: supported_output_configs may itself open native PCM.
        // Capability probing happens later, for the unique selected ID only.
        let id = entered(run, || identity(&device))?;
        if id == selected {
            if found.is_some() {
                return Err(RingAudioError::Unavailable);
            }
            found = Some(device);
        }
    }
    if entered(run, next)?.is_some() {
        return Err(RingAudioError::Unavailable);
    }
    found.ok_or(RingAudioError::Unavailable)
}

fn output_config(
    run: &Run,
    device: &cpal::Device,
) -> Result<SupportedStreamConfig, RingAudioError> {
    let mut ranges = entered(run, || {
        device
            .supported_output_configs()
            .map_err(|_| RingAudioError::Unavailable)
    })?;
    let mut selected = None;
    for _ in 0..MAX_CONFIGS {
        let Some(range) = entered(run, || Ok(ranges.next()))? else {
            return selected.ok_or(RingAudioError::Unavailable);
        };
        if (1..=2).contains(&range.channels())
            && range.min_sample_rate() <= RATE
            && range.max_sample_rate() >= RATE
            && matches!(
                range.sample_format(),
                SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
            )
        {
            // First supported bounded PCM config; no recording API is queried.
            if selected.is_none() {
                selected = Some(range.with_sample_rate(RATE));
            }
        }
    }
    if entered(run, || Ok(ranges.next()))?.is_some() {
        return Err(RingAudioError::Unavailable);
    }
    selected.ok_or(RingAudioError::Unavailable)
}
fn play(run: &Arc<Run>, selected: Option<&str>) -> Result<(), RingAudioError> {
    let device = speaker(run, selected)?;
    let config = output_config(run, &device)?;
    let stream = entered(run, || match config.sample_format() {
        SampleFormat::F32 => stream::<f32>(&device, &config, run.clone()),
        SampleFormat::I16 => stream::<i16>(&device, &config, run.clone()),
        SampleFormat::U16 => stream::<u16>(&device, &config, run.clone()),
        _ => Err(RingAudioError::Unavailable),
    })?;
    entered(run, || {
        stream.play().map_err(|_| RingAudioError::Unavailable)
    })?;
    let _ = run
        .status
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    while run.active() && run.status.load(Ordering::Acquire) != 3 {
        thread::sleep(Duration::from_millis(10));
    }
    let failed = run.status.load(Ordering::Acquire) == 3;
    run.stopped.store(true, Ordering::Release);
    // The stream remains entirely on this owner thread and is dropped before
    // the retained process permit. Callback cancellation never waits for this drop.
    drop(stream);
    if failed {
        Err(RingAudioError::Unavailable)
    } else {
        Ok(())
    }
}
trait ToneSample: cpal::SizedSample + Copy {
    fn tone(value: f32) -> Self;
}
impl ToneSample for f32 {
    fn tone(value: f32) -> Self {
        value
    }
}
impl ToneSample for i16 {
    fn tone(value: f32) -> Self {
        (value * 32767.0) as i16
    }
}
impl ToneSample for u16 {
    fn tone(value: f32) -> Self {
        (value * 32767.0 + 32768.0) as u16
    }
}
fn stream<T: ToneSample>(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    run: Arc<Run>,
) -> Result<Stream, RingAudioError> {
    let channels = config.channels() as usize;
    let failed = run.clone();
    let mut synth = Tone::default();
    device
        .build_output_stream(
            config.config(),
            move |data: &mut [T], _| {
                // Nothing in this callback allocates or locks. Every sample is written,
                // including unsigned midpoint silence. Check authority per audio frame.
                fill(data, channels, &mut synth, || run.active());
            },
            move |_| {
                failed.status.store(3, Ordering::Release);
                failed.stopped.store(true, Ordering::Release);
            },
            Some(Duration::from_secs(2)),
        )
        .map_err(|_| RingAudioError::Unavailable)
}
fn fill<T: ToneSample>(
    data: &mut [T],
    channels: usize,
    synth: &mut Tone,
    mut active: impl FnMut() -> bool,
) {
    for frame in data.chunks_mut(channels) {
        let sample = if active() { synth.next() } else { 0.0 };
        frame.fill(T::tone(sample));
    }
}
#[derive(Default)]
struct Tone {
    frame: u32,
}
impl Tone {
    fn next(&mut self) -> f32 {
        // An original soft two-note figure: 320ms, 120ms rest, 320ms,
        // then silence to a 3s period. 15ms linear edges avoid clicks.
        let frame = self.frame;
        self.frame = (self.frame + 1) % (RATE * 3);
        let (offset, frequency) = if frame < 15_360 {
            (frame, 523.25)
        } else if (21_120..36_480).contains(&frame) {
            (frame - 21_120, 659.25)
        } else {
            return 0.0;
        };
        let envelope = (offset.min(15_359 - offset) as f32 / 720.0).min(1.0);
        (std::f32::consts::TAU * frequency * offset as f32 / RATE as f32).sin() * envelope * 0.045
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    pub(super) static PERMIT_TEST: Mutex<()> = Mutex::new(());
    struct RingTlsHold {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for RingTlsHold {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }
    thread_local! {
        static RING_TLS_HOLD: std::cell::RefCell<Option<RingTlsHold>> = const { std::cell::RefCell::new(None) };
    }
    fn owned_run() -> Arc<Run> {
        Arc::new(Run {
            stopped: AtomicBool::new(false),
            status: AtomicU8::new(0),
            deadline: Instant::now() + MAX_RING,
            allowed: Arc::new(|| true),
        })
    }
    fn synthetic_output() -> &'static OutputArbitration {
        // A bounded fixture with the same process-lifetime as production's
        // static output owner; it never opens a platform device.
        Box::leak(Box::new(OutputArbitration::new()))
    }
    fn spawn_supervisor(work: Box<dyn FnOnce() + Send>) -> std::io::Result<thread::JoinHandle<()>> {
        thread::Builder::new().spawn(work)
    }
    #[tokio::test]
    async fn actual_ring_owner_keeps_output_guards_through_late_drop_tls_and_cancelled_wait() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let _exclusive_permit = PERMIT_TEST.lock().unwrap();
        let workers = RingWorkers::new();
        let output = synthetic_output();
        let run = owned_run();
        let playback = RingPlayback { run: run.clone() };
        let (entered, body_entered) = std::sync::mpsc::sync_channel(1);
        let (release_body, body_release) = std::sync::mpsc::channel();
        let (in_tls, tls_entered) = std::sync::mpsc::sync_channel(1);
        let (release_tls, tls_release) = std::sync::mpsc::channel();
        let (dropped, value_dropped) = std::sync::mpsc::sync_channel(1);
        struct NativeValue(std::sync::mpsc::SyncSender<thread::ThreadId>);
        impl Drop for NativeValue {
            fn drop(&mut self) {
                let _ = self.0.send(thread::current().id());
            }
        }
        workers
            .start_with(
                run,
                output,
                move |_, resources| {
                    resources.retain_permit(Permit::take().unwrap());
                    RING_TLS_HOLD.with(|tls| {
                        *tls.borrow_mut() = Some(RingTlsHold {
                            entered: in_tls,
                            release: tls_release,
                        })
                    });
                    let value = NativeValue(dropped);
                    entered.send(thread::current().id()).unwrap();
                    body_release.recv_timeout(Duration::from_secs(2)).unwrap();
                    drop(value);
                    Ok(())
                },
                spawn_supervisor,
            )
            .unwrap();
        let actual_thread = body_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(playback);
        release_body.send(()).unwrap();
        assert_eq!(
            value_dropped.recv_timeout(Duration::from_secs(2)).unwrap(),
            actual_thread
        );
        assert_ne!(actual_thread, thread::current().id());
        tls_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(output.ringing.load(Ordering::Acquire));
        assert!(
            Permit::take().is_none(),
            "body completion cannot release the actual output permit"
        );
        assert_eq!(
            workers.wait(Duration::from_millis(20)).await,
            Err(RingAudioReleaseError::TimedOut)
        );
        assert_eq!(
            workers.start_with(
                owned_run(),
                output,
                |_, _| panic!("replacement entered"),
                spawn_supervisor
            ),
            Err(RingAudioError::Busy)
        );
        let mut cancelled_wait = Box::pin(workers.wait(Duration::from_secs(2)));
        assert!(
            cancelled_wait
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(cancelled_wait);
        assert!(
            workers
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        assert!(output.ringing.load(Ordering::Acquire));
        assert!(Permit::take().is_none());
        release_tls.send(()).unwrap();
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(!output.ringing.load(Ordering::Acquire));
        assert!(Permit::take().is_some());
        assert!(workers.0.lock().unwrap().is_none());
    }
    #[tokio::test]
    async fn supervisor_failure_and_stop_before_actual_creation_do_not_detach_a_worker() {
        let workers = RingWorkers::new();
        let output = synthetic_output();
        let entered = Arc::new(AtomicBool::new(false));
        let called = entered.clone();
        assert_eq!(
            workers.start_with(
                owned_run(),
                output,
                move |_, _| {
                    called.store(true, Ordering::Release);
                    Ok(())
                },
                |_work| Err(std::io::Error::other(
                    "synthetic supervisor creation failure"
                ))
            ),
            Err(RingAudioError::Unavailable)
        );
        assert!(!entered.load(Ordering::Acquire));
        assert!(workers.0.lock().unwrap().is_none());
        assert!(!output.ringing.load(Ordering::Acquire));
        let (release, released) = std::sync::mpsc::channel();
        let run = owned_run();
        let playback = RingPlayback { run: run.clone() };
        let called = entered.clone();
        workers
            .start_with(
                run,
                output,
                move |_, _| {
                    called.store(true, Ordering::Release);
                    Ok(())
                },
                move |supervise| {
                    Ok(thread::spawn(move || {
                        released.recv_timeout(Duration::from_secs(2)).unwrap();
                        supervise();
                    }))
                },
            )
            .unwrap();
        assert!(
            workers
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        drop(playback);
        release.send(()).unwrap();
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(!entered.load(Ordering::Acquire));
        assert!(!output.ringing.load(Ordering::Acquire));
    }
    #[tokio::test]
    async fn native_panic_retains_failed_ring_owner_and_never_admits_replacement() {
        let workers = RingWorkers::new();
        let output = synthetic_output();
        let run = owned_run();
        workers
            .start_with(
                run.clone(),
                output,
                |_, _| panic!("synthetic native owner panic"),
                spawn_supervisor,
            )
            .unwrap();
        assert_eq!(
            workers.wait(Duration::from_secs(2)).await,
            Err(RingAudioReleaseError::WorkerPanicked)
        );
        assert_eq!(run.status.load(Ordering::Acquire), 3);
        assert!(!run.active());
        assert!(output.ringing.load(Ordering::Acquire));
        assert!(
            workers
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        assert_eq!(
            workers.start_with(
                owned_run(),
                output,
                |_, _| panic!("failed replacement"),
                spawn_supervisor
            ),
            Err(RingAudioError::Unavailable)
        );
        // This fixture has no native resource. Explicitly join its finished
        // supervisor; production's static failed owner is never discarded.
        let retained = workers.0.lock().unwrap().take().unwrap();
        retained.supervisor.unwrap().join().unwrap();
    }
    #[tokio::test]
    async fn ring_retires_pcm_before_its_actual_receipt_and_rechecks_cancellation() {
        let workers = RingWorkers::new();
        let output = synthetic_output();
        let old_pcm = output.epoch.load(Ordering::Acquire);
        assert!(output.allows_pcm(old_pcm));
        let run = owned_run();
        let playback = RingPlayback { run: run.clone() };
        let (waiting, prior_waiting) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        let native = entered.clone();
        workers
            .start_with(
                run,
                output,
                move |run, _resources| {
                    after_output_release(
                        run,
                        || {
                            waiting.send(()).unwrap();
                            released.recv_timeout(Duration::from_secs(2)).unwrap()
                        },
                        || {
                            native.store(true, Ordering::Release);
                            Ok(())
                        },
                    )
                },
                spawn_supervisor,
            )
            .unwrap();
        prior_waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!output.allows_pcm(old_pcm));
        assert!(
            !output.allows_pcm(output.epoch.load(Ordering::Acquire)),
            "new PCM admission must fail, never wait for this ring"
        );
        assert!(!entered.load(Ordering::Acquire));
        drop(playback);
        release.send(Ok(())).unwrap();
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(
            !entered.load(Ordering::Acquire),
            "cancelled while the prior output joined"
        );
        assert!(!output.ringing.load(Ordering::Acquire));
        assert!(
            !output.allows_pcm(old_pcm),
            "an old PCM epoch never revives"
        );
    }
    #[tokio::test]
    async fn failed_prior_output_receipt_never_opens_ring_and_keeps_typed_failed_owner() {
        let workers = RingWorkers::new();
        let output = synthetic_output();
        let run = owned_run();
        workers
            .start_with(
                run.clone(),
                output,
                |run, _| {
                    after_output_release(
                        run,
                        || Err(RingAudioReleaseError::OutputOwnerUnconfirmed),
                        || panic!("unconfirmed previous owner cannot enter native output"),
                    )
                },
                spawn_supervisor,
            )
            .unwrap();
        assert_eq!(
            workers.wait(Duration::from_secs(2)).await,
            Err(RingAudioReleaseError::OutputOwnerUnconfirmed)
        );
        assert!(!run.active());
        assert_eq!(run.status.load(Ordering::Acquire), 3);
        assert!(output.ringing.load(Ordering::Acquire));
        assert!(
            workers
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        assert_eq!(
            workers.start_with(
                owned_run(),
                output,
                |_, _| panic!("failed owner replacement"),
                spawn_supervisor
            ),
            Err(RingAudioError::Unavailable)
        );
        // No native resource exists in this fixture. Production's static
        // uncertain owner and arbitration reservation stay retained.
        let retained = workers.0.lock().unwrap().take().unwrap();
        retained.supervisor.unwrap().join().unwrap();
    }
    #[test]
    fn missing_ring_supervisor_receipt_retains_reservation_instead_of_clean_completion() {
        let output = synthetic_output();
        let resources = Arc::new(RingResources {
            _reservation: output.reserve_ring().unwrap(),
            permit: Mutex::new(None),
        });
        let supervisor = thread::spawn(|| ());
        let until = Instant::now() + Duration::from_secs(2);
        while !supervisor.is_finished() {
            assert!(Instant::now() < until, "synthetic supervisor completion");
            thread::yield_now();
        }
        let mut owner = Some(RingOwner {
            supervisor: Some(supervisor),
            completed: Arc::new(RingCompletion::default()),
            run: owned_run(),
            _resources: resources,
        });
        assert_eq!(
            RingWorkers::poll_locked(&mut owner),
            Some(Err(RingAudioReleaseError::SupervisorFailed))
        );
        assert_eq!(
            RingWorkers::poll_locked(&mut owner),
            Some(Err(RingAudioReleaseError::SupervisorFailed))
        );
        assert!(owner.is_some());
        assert!(output.ringing.load(Ordering::Acquire));
    }

    #[test]
    fn synthesized_output_is_bounded_with_silent_edges_gaps_and_period() {
        let mut tone = Tone::default();
        let samples: Vec<_> = (0..RATE * 3).map(|_| tone.next()).collect();
        assert_eq!(samples[0], 0.0);
        assert!(samples.iter().all(|v| v.is_finite() && v.abs() <= 0.045));
        assert!(samples[15_360..21_120].iter().all(|v| *v == 0.0));
        assert!(samples[36_480..].iter().all(|v| *v == 0.0));
        assert!(samples[..15_360].iter().any(|v| v.abs() > 0.04));
        assert_eq!(tone.next(), samples[0]);
    }
    #[test]
    fn silence_matches_all_supported_pcm_formats() {
        assert_eq!(f32::tone(0.0), 0.0);
        assert_eq!(i16::tone(0.0), 0);
        assert_eq!(u16::tone(0.0), 32768);
    }
    #[test]
    fn cancellation_is_terminal_and_blocks_later_device_steps() {
        let allowed = Arc::new(AtomicBool::new(true));
        let test = allowed.clone();
        let run = Run {
            stopped: AtomicBool::new(false),
            status: AtomicU8::new(0),
            deadline: Instant::now() + MAX_RING,
            allowed: Arc::new(move || test.load(Ordering::Acquire)),
        };
        let result = entered(&run, || {
            allowed.store(false, Ordering::Release);
            Ok(())
        });
        assert_eq!(result, Err(RingAudioError::Cancelled));
        allowed.store(true, Ordering::Release);
        assert!(!run.active());
        assert_eq!(
            entered(&run, || panic!("later native step must not run")),
            Err::<(), _>(RingAudioError::Cancelled)
        );
    }
    #[test]
    fn expired_authority_never_opens_output() {
        let run = Run {
            stopped: AtomicBool::new(false),
            status: AtomicU8::new(0),
            deadline: Instant::now(),
            allowed: Arc::new(|| true),
        };
        assert_eq!(
            entered(&run, || panic!("expired native step")),
            Err::<(), _>(RingAudioError::Cancelled)
        );
    }
    #[test]
    fn callback_overwrites_stereo_and_partial_tail_with_midbuffer_silence() {
        let mut tone = Tone { frame: 720 };
        let mut output = [u16::MAX; 17];
        let mut frames = 0;
        fill(&mut output, 2, &mut tone, || {
            frames += 1;
            frames <= 3
        });
        assert!(output[..6].chunks_exact(2).all(|p| p[0] == p[1]));
        assert!(output[..6].iter().any(|s| *s != 32768));
        assert!(output[6..].iter().all(|s| *s == 32768));
        // A fully cancelled later callback cannot advance or replay tone state.
        let stopped_frame = tone.frame;
        fill(&mut output, 2, &mut tone, || false);
        assert!(output.iter().all(|s| *s == 32768));
        assert_eq!(tone.frame, stopped_frame);
    }
    #[test]
    fn one_opener_permit_survives_until_actual_owner_drop() {
        let _exclusive_permit = PERMIT_TEST.lock().unwrap();
        let permit = Permit::take().unwrap();
        assert!(Permit::take().is_none());
        let owner = std::thread::spawn(move || {
            assert!(Permit::take().is_none());
            drop(permit);
        });
        owner.join().unwrap();
        assert!(Permit::take().is_some());
    }
    #[test]
    fn cancelled_identity_lookup_prevents_next_device_probe() {
        let run = Run {
            stopped: AtomicBool::new(false),
            status: AtomicU8::new(0),
            deadline: Instant::now() + MAX_RING,
            allowed: Arc::new(|| true),
        };
        let mut next_calls = 0;
        let mut id_calls = 0;
        let result = find_selected(
            &run,
            "selected",
            || {
                next_calls += 1;
                Ok(Some(1))
            },
            |_| {
                id_calls += 1;
                run.stopped.store(true, Ordering::Release);
                Ok("other".into())
            },
        );
        assert_eq!(result, Err(RingAudioError::Cancelled));
        assert_eq!(next_calls, 1);
        assert_eq!(id_calls, 1);
    }
    #[test]
    fn output_identity_resolution_rejects_duplicates_and_truncated_inventory() {
        let run = Run {
            stopped: AtomicBool::new(false),
            status: AtomicU8::new(0),
            deadline: Instant::now() + MAX_RING,
            allowed: Arc::new(|| true),
        };
        let mut devices = ["other", "selected", "last"].into_iter();
        assert_eq!(
            find_selected(
                &run,
                "selected",
                || Ok(devices.next()),
                |id| Ok((*id).into())
            ),
            Ok("selected")
        );
        let mut duplicate = ["selected", "selected"].into_iter();
        assert_eq!(
            find_selected(
                &run,
                "selected",
                || Ok(duplicate.next()),
                |id| Ok((*id).into())
            ),
            Err(RingAudioError::Unavailable)
        );
        let mut calls = 0;
        assert_eq!(
            find_selected(
                &run,
                "selected",
                || {
                    calls += 1;
                    Ok(Some(calls))
                },
                |id| Ok(if *id == 1 { "selected" } else { "other" }.into())
            ),
            Err(RingAudioError::Unavailable)
        );
        assert_eq!(calls, MAX_DEVICES + 1);
    }
}

/// One explicit attachment's output state. Mac retains its exact output permit
/// through terminal production and checked native cleanup, including unwind;
/// other platforms retain their existing stream-Drop ownership boundary.
/// These controls never touch microphone capture, a call mixer, route or OS volume.
pub struct PcmPlayback {
    run: Arc<Run>,
    paused: AtomicBool,
    ended: AtomicBool,
    consumed: AtomicU64,
    total: AtomicU64,
    rate: AtomicU32,
    #[cfg(target_os = "macos")]
    native_close_failed: AtomicBool,
    #[cfg(target_os = "macos")]
    native_identity: Arc<PcmNativeIdentity>,
}
impl PcmPlayback {
    /// Original output epoch: a ringtone reservation permanently retires it,
    /// including a reservation which later fails to open a native speaker.
    pub fn output_authority() -> RingAudioAuthority {
        // Explicit admission runs off the callback. Reap only a real completed
        // ring join; the returned authority remains atomics-only on native I/O.
        let _ = RING_WORKERS.poll_release();
        let original = OUTPUT.epoch.load(Ordering::Acquire);
        Arc::new(move || OUTPUT.allows_pcm(original))
    }
    pub fn new(allowed: RingAudioAuthority) -> Arc<Self> {
        let output = Self::output_authority();
        let allowed = Arc::new(move || allowed() && output());
        Arc::new(Self {
            run: Arc::new(Run {
                stopped: AtomicBool::new(false),
                status: AtomicU8::new(0),
                deadline: Instant::now() + Duration::from_secs(600),
                allowed,
            }),
            paused: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            consumed: AtomicU64::new(0),
            total: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            #[cfg(target_os = "macos")]
            native_close_failed: AtomicBool::new(false),
            #[cfg(target_os = "macos")]
            native_identity: PcmNativeIdentity::new(),
        })
    }
    pub fn stop(&self) {
        self.run.stopped.store(true, Ordering::Release);
    }
    pub fn toggle_pause(&self) {
        if self.run.active() {
            self.paused.fetch_xor(true, Ordering::AcqRel);
        }
    }
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
    pub fn unavailable(&self) -> bool {
        self.run.status.load(Ordering::Acquire) == 3
    }
    #[cfg(target_os = "macos")]
    pub fn native_close_failed(&self) -> bool {
        self.native_close_failed.load(Ordering::Acquire)
    }
    /// On the actual application owner thread only, after play_pcm has
    /// returned/unwound. An early/concurrent call fails without native teardown
    /// or releasing the still-producing owner's guard. A terminal native error
    /// remains owned; only this exact terminal producer's checked success can
    /// release it. This is separate from the application's actual worker/TLS join.
    #[cfg(target_os = "macos")]
    pub fn close_native_output_on_owned_worker(&self) -> Result<(), RingAudioError> {
        self.stop();
        PCM_NATIVE_OWNERS
            .close_with(&self.native_identity, || {
                crate::coreaudio_close::close_retired_coreaudio_on_owned_worker().map_err(|_| ())
            })
            .map_err(|()| {
                self.native_close_failed.store(true, Ordering::Release);
                self.run.status.store(3, Ordering::Release);
                RingAudioError::Unavailable
            })
    }
    pub fn active(&self) -> bool {
        self.run.active()
    }
    pub fn playing(&self) -> bool {
        self.run.status.load(Ordering::Acquire) == 1 && !self.paused() && self.active()
    }
    /// Source frames consumed by the output callback, not a playback timer or
    /// the file's advertised duration. OS/device presentation latency is unknown.
    pub fn position_ms(&self) -> u64 {
        let rate = self.rate.load(Ordering::Acquire);
        if rate == 0 {
            0
        } else {
            self.consumed.load(Ordering::Acquire).saturating_mul(1000) / u64::from(rate)
        }
    }
    pub fn duration_ms(&self) -> Option<u64> {
        let rate = self.rate.load(Ordering::Acquire);
        (rate != 0)
            .then(|| self.total.load(Ordering::Acquire).saturating_mul(1000) / u64::from(rate))
    }
    /// On the caller's dedicated owner thread only. PCM stays in memory and
    /// is dropped with the stream; no default-device setting is written.
    pub fn play_pcm(
        self: &Arc<Self>,
        samples: Vec<f32>,
        rate: u32,
        channels: u16,
    ) -> Result<(), RingAudioError> {
        let count = usize::from(channels);
        if !(1..=2).contains(&count)
            || !(8_000..=96_000).contains(&rate)
            || samples.is_empty()
            || samples.len() % count != 0
            || samples.len() > 12 * 1024 * 1024
            || samples.len() / count > rate as usize * 180
            || samples.iter().any(|v| !v.is_finite() || v.abs() > 1.0)
        {
            return Err(RingAudioError::Unavailable);
        }
        if !self.run.active() {
            return Err(RingAudioError::Cancelled);
        }
        // No waiting/retry or extra native opener on an explicit Play.
        #[cfg(target_os = "macos")]
        let producer = PCM_NATIVE_OWNERS.admit(&self.native_identity, &self.run)?;
        #[cfg(not(target_os = "macos"))]
        let _permit = Permit::take().ok_or(RingAudioError::Busy)?;
        #[cfg(not(target_os = "macos"))]
        let result = {
            if !self.run.active() {
                return Err(RingAudioError::Cancelled);
            }
            self.total
                .store((samples.len() / count) as u64, Ordering::Release);
            self.rate.store(rate, Ordering::Release);
            self.open_pcm(samples, rate, channels)
        };
        #[cfg(target_os = "macos")]
        let result = if self.run.active() {
            self.total
                .store((samples.len() / count) as u64, Ordering::Release);
            self.rate.store(rate, Ordering::Release);
            self.open_pcm(samples, rate, channels)
        } else {
            Err(RingAudioError::Cancelled)
        };
        #[cfg(target_os = "macos")]
        drop(producer);
        #[cfg(target_os = "macos")]
        if self.close_native_output_on_owned_worker().is_err() {
            return result.and(Err(RingAudioError::Unavailable));
        }
        if result == Err(RingAudioError::Unavailable) {
            self.run.status.store(3, Ordering::Release);
        } else if self.run.status.load(Ordering::Acquire) != 3 {
            self.run.status.store(2, Ordering::Release);
        }
        self.stop();
        result
    }
    fn open_pcm(
        self: &Arc<Self>,
        samples: Vec<f32>,
        rate: u32,
        channels: u16,
    ) -> Result<(), RingAudioError> {
        let device = speaker(&self.run, None)?;
        let config = output_config(&self.run, &device)?;
        let samples = Arc::new(samples);
        let stream = entered(&self.run, || match config.sample_format() {
            SampleFormat::F32 => pcm_stream::<f32>(
                &device,
                &config,
                self.clone(),
                samples.clone(),
                rate,
                channels,
            ),
            SampleFormat::I16 => pcm_stream::<i16>(
                &device,
                &config,
                self.clone(),
                samples.clone(),
                rate,
                channels,
            ),
            SampleFormat::U16 => pcm_stream::<u16>(
                &device,
                &config,
                self.clone(),
                samples.clone(),
                rate,
                channels,
            ),
            _ => Err(RingAudioError::Unavailable),
        })?;
        entered(&self.run, || {
            stream.play().map_err(|_| RingAudioError::Unavailable)
        })?;
        self.run
            .status
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok();
        while self.run.active() && self.run.status.load(Ordering::Acquire) != 3 {
            thread::sleep(Duration::from_millis(10));
        }
        // Callback buffers go silent on revocation without waiting for a native
        // drop. Already submitted OS audio cannot be recalled. Never release the
        // caller's permit before this stream and its captured PCM are destroyed.
        drop(stream);
        drop(samples);
        if self.unavailable() {
            Err(RingAudioError::Unavailable)
        } else {
            Ok(())
        }
    }
}
struct PcmCursor {
    phase: u64,
}
impl PcmCursor {
    fn frame(
        &mut self,
        samples: &[f32],
        source_channels: usize,
        rate: u32,
        output_channels: usize,
    ) -> Option<[f32; 2]> {
        let frames = samples.len() / source_channels;
        let index = (self.phase / u64::from(RATE)) as usize;
        if index >= frames {
            return None;
        }
        let next = (index + 1).min(frames - 1);
        let fraction = (self.phase % u64::from(RATE)) as f32 / RATE as f32;
        let channel = |n: usize| {
            let n = n.min(source_channels - 1);
            let a = samples[index * source_channels + n];
            let b = samples[next * source_channels + n];
            (a + (b - a) * fraction).clamp(-1.0, 1.0) * 0.8
        };
        let (left, right) = (channel(0), channel(1));
        self.phase += u64::from(rate);
        Some(if output_channels == 1 {
            [(left + right) * 0.5, 0.0]
        } else {
            [left, right]
        })
    }
}
fn pcm_stream<T: ToneSample>(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    state: Arc<PcmPlayback>,
    samples: Arc<Vec<f32>>,
    rate: u32,
    source_channels: u16,
) -> Result<Stream, RingAudioError> {
    let output_channels = usize::from(config.channels());
    let failed = state.clone();
    let mut cursor = PcmCursor { phase: 0 };
    device
        .build_output_stream(
            config.config(),
            move |data: &mut [T], _| {
                fill_pcm(
                    data,
                    output_channels,
                    &samples,
                    source_channels as usize,
                    rate,
                    &mut cursor,
                    &state,
                );
            },
            move |_| {
                failed.run.status.store(3, Ordering::Release);
                failed.stop();
            },
            Some(Duration::from_secs(2)),
        )
        .map_err(|_| RingAudioError::Unavailable)
}
fn fill_pcm<T: ToneSample>(
    data: &mut [T],
    output_channels: usize,
    samples: &[f32],
    source_channels: usize,
    rate: u32,
    cursor: &mut PcmCursor,
    state: &PcmPlayback,
) {
    for frame in data.chunks_mut(output_channels) {
        let value = if !state.run.active() || state.paused() {
            [0.0, 0.0]
        } else if let Some(value) = cursor.frame(samples, source_channels, rate, output_channels) {
            value
        } else {
            state.ended.store(true, Ordering::Release);
            state.stop();
            [0.0, 0.0]
        };
        for (n, out) in frame.iter_mut().enumerate() {
            *out = T::tone(value[n]);
        }
    }
    state.consumed.store(
        (cursor.phase / u64::from(RATE)).min((samples.len() / source_channels) as u64),
        Ordering::Release,
    );
}
#[cfg(test)]
mod pcm_tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn pcm_guard_requires_terminal_producer_and_exact_checked_close_before_release() {
        let _exclusive_permit = super::tests::PERMIT_TEST.lock().unwrap();
        let owners = PcmNativeOwners::new();
        let state = PcmPlayback::new(Arc::new(|| true));
        let producer = owners.admit(&state.native_identity, &state.run).unwrap();
        assert!(Permit::take().is_none());
        state.stop();
        assert_eq!(
            owners.close_with(&state.native_identity, || panic!("producer is still live")),
            Err(())
        );
        assert_eq!(
            state.native_identity.0.load(Ordering::Acquire),
            PCM_PRODUCING
        );
        assert!(Permit::take().is_none());
        drop(producer);
        assert_eq!(
            state.native_identity.0.load(Ordering::Acquire),
            PCM_TERMINAL
        );
        assert!(
            Permit::take().is_none(),
            "terminal is not a cleanup receipt"
        );
        let closed = AtomicBool::new(false);
        assert_eq!(
            owners.close_with(&state.native_identity, || {
                closed.store(true, Ordering::Release);
                Ok(())
            }),
            Ok(())
        );
        assert!(closed.load(Ordering::Acquire));
        assert!(owners.0.lock().unwrap().is_none());
        assert!(Permit::take().is_some());
        // A stale released identity cannot close/release a later generation.
        let next = PcmPlayback::new(Arc::new(|| true));
        let next_producer = owners.admit(&next.native_identity, &next.run).unwrap();
        assert_eq!(
            owners.close_with(&state.native_identity, || panic!("stale close ran")),
            Ok(())
        );
        assert!(Permit::take().is_none());
        assert!(Arc::ptr_eq(
            &owners.0.lock().unwrap().as_ref().unwrap().identity,
            &next.native_identity
        ));
        drop(next_producer);
        assert_eq!(owners.close_with(&next.native_identity, || Ok(())), Ok(()));
        assert!(Permit::take().is_some());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn pcm_producer_panic_keeps_exact_guard_until_authentic_terminal_cleanup() {
        let _exclusive_permit = super::tests::PERMIT_TEST.lock().unwrap();
        let owners = PcmNativeOwners::new();
        let state = PcmPlayback::new(Arc::new(|| true));
        let producer = owners.admit(&state.native_identity, &state.run).unwrap();
        let operation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _producer = producer;
            panic!("synthetic PCM producer failure");
        }));
        assert!(
            operation.is_err(),
            "cleanup must not replace original panic"
        );
        assert_eq!(
            state.native_identity.0.load(Ordering::Acquire),
            PCM_TERMINAL
        );
        assert!(
            Permit::take().is_none(),
            "unwind must not drop the output guard"
        );
        assert!(owners.0.lock().unwrap().is_some());
        assert_eq!(owners.close_with(&state.native_identity, || Ok(())), Ok(()));
        assert!(Permit::take().is_some());
        assert!(operation.is_err());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn pcm_native_close_error_or_panic_retains_single_owner_without_retry() {
        let _exclusive_permit = super::tests::PERMIT_TEST.lock().unwrap();
        for panic_close in [false, true] {
            let owners = PcmNativeOwners::new();
            let state = PcmPlayback::new(Arc::new(|| true));
            let producer = owners.admit(&state.native_identity, &state.run).unwrap();
            drop(producer);
            let closed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                owners.close_with(&state.native_identity, || {
                    if panic_close {
                        panic!("synthetic native close panic");
                    }
                    Err(())
                })
            }));
            if panic_close {
                assert!(closed.is_err());
                assert_eq!(state.native_identity.0.load(Ordering::Acquire), PCM_CLOSING);
            } else {
                assert_eq!(closed.unwrap(), Err(()));
                assert_eq!(state.native_identity.0.load(Ordering::Acquire), PCM_FAILED);
            }
            drop(state);
            assert!(owners.0.lock().unwrap().is_some());
            assert!(Permit::take().is_none());
            let next = PcmPlayback::new(Arc::new(|| true));
            assert!(matches!(
                owners.admit(&next.native_identity, &next.run),
                Err(RingAudioError::Busy)
            ));
            let identity = owners.0.lock().unwrap().as_ref().unwrap().identity.clone();
            assert_eq!(
                owners.close_with(&identity, || panic!("uncertain close must not retry")),
                Err(())
            );
            assert!(Permit::take().is_none());
            // These injected closures created no native resource. Only this
            // local test registry is dropped; production's static owner is not.
            drop(owners);
            assert!(Permit::take().is_some());
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn pcm_cancel_before_admission_has_no_native_guard_or_close_receipt() {
        let _exclusive_permit = super::tests::PERMIT_TEST.lock().unwrap();
        let owners = PcmNativeOwners::new();
        let state = PcmPlayback::new(Arc::new(|| true));
        state.stop();
        assert!(matches!(
            owners.admit(&state.native_identity, &state.run),
            Err(RingAudioError::Cancelled)
        ));
        assert_eq!(state.native_identity.0.load(Ordering::Acquire), PCM_FRESH);
        assert_eq!(
            owners.close_with(&state.native_identity, || panic!(
                "nothing entered native PCM"
            )),
            Ok(())
        );
        assert!(owners.0.lock().unwrap().is_none());
        assert!(Permit::take().is_some());
    }
    #[test]
    fn ringtone_reservation_retires_pcm_before_native_wait_and_never_revives_it() {
        let state = OutputArbitration::new();
        let original = state.epoch.load(Ordering::Acquire);
        assert!(state.allows_pcm(original));
        let ring = state.reserve_ring().unwrap();
        assert!(!state.allows_pcm(original));
        assert!(state.reserve_ring().is_none());
        drop(ring);
        assert!(!state.allows_pcm(original));
        assert!(state.allows_pcm(state.epoch.load(Ordering::Acquire)));
        state.epoch.store(u64::MAX, Ordering::Release);
        assert!(state.reserve_ring().is_none());
        assert!(!state.ringing.load(Ordering::Acquire));
        assert!(!state.allows_pcm(u64::MAX));
    }
    #[test]
    fn pcm_pause_and_revocation_silence_without_advancing_or_reviving() {
        let allowed = Arc::new(AtomicBool::new(true));
        let gate = allowed.clone();
        let state = PcmPlayback::new(Arc::new(move || gate.load(Ordering::Acquire)));
        let samples = [0.5f32; 12];
        let mut cursor = PcmCursor { phase: 0 };
        let mut out = [1.0f32; 4];
        state.toggle_pause();
        fill_pcm(&mut out, 2, &samples, 2, 48_000, &mut cursor, &state);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(cursor.phase, 0);
        state.toggle_pause();
        fill_pcm(&mut out, 2, &samples, 2, 48_000, &mut cursor, &state);
        assert!(out.iter().all(|v| *v > 0.0));
        assert_eq!(state.consumed.load(Ordering::Acquire), 2);
        allowed.store(false, Ordering::Release);
        fill_pcm(&mut out, 2, &samples, 2, 48_000, &mut cursor, &state);
        assert_eq!(out, [0.0; 4]);
        let at = cursor.phase;
        allowed.store(true, Ordering::Release);
        fill_pcm(&mut out, 2, &samples, 2, 48_000, &mut cursor, &state);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(cursor.phase, at);
    }
    #[test]
    fn pcm_resampling_channels_completion_and_consumed_position_are_explicit() {
        let state = PcmPlayback::new(Arc::new(|| true));
        let mut cursor = PcmCursor { phase: 0 };
        let samples: Vec<f32> = (0..240).flat_map(|_| [1.0, -1.0]).collect();
        let mut out = [0.0f32; 480];
        state.rate.store(24_000, Ordering::Release);
        state.total.store(240, Ordering::Release);
        assert_eq!(state.duration_ms(), Some(10));
        assert_eq!(state.position_ms(), 0);
        fill_pcm(&mut out, 2, &samples, 2, 24_000, &mut cursor, &state);
        assert_eq!(state.consumed.load(Ordering::Acquire), 120);
        assert_eq!(state.position_ms(), 5);
        assert!(out.chunks(2).all(|c| c[0] > 0.0 && c[1] < 0.0));
        fill_pcm(&mut out, 2, &samples, 2, 24_000, &mut cursor, &state);
        assert_eq!(state.position_ms(), 10);
        fill_pcm(&mut out, 2, &samples, 2, 24_000, &mut cursor, &state);
        assert!(state.ended());
        assert_eq!(out, [0.0; 480]);
        let mut mono = PcmCursor { phase: 0 };
        assert_eq!(mono.frame(&[1.0, -1.0], 2, 48_000, 1), Some([0.0, 0.0]));
        let mut stereo = PcmCursor { phase: 0 };
        assert_eq!(stereo.frame(&[0.5], 1, 48_000, 2), Some([0.4, 0.4]));
    }
}
