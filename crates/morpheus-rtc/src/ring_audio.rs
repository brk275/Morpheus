//! Original quiet output-only ringtone. No microphone, media publication or file
//! is opened. The owner thread holds the process permit through native stream
//! destruction, even if a platform open/drop cannot be interrupted. Cancellation
//! prevents later startup steps and makes subsequent callback buffers silent;
//! samples already submitted to the OS cannot be recalled.

use cpal::{
    SampleFormat, Stream, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    sync::{
        Arc,
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

/// Drop never waits on an OS driver. Its atomic stop survives the owner handle;
/// the one process permit is released only after the detached thread drops the
/// device/stream. A stuck driver cannot cause an accumulating opener pool.
pub struct RingPlayback {
    run: Arc<Run>,
}
impl RingPlayback {
    pub fn start(
        speaker: Option<String>,
        expiry: Instant,
        allowed: RingAudioAuthority,
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
        if !run.active() {
            return Err(RingAudioError::Cancelled);
        }
        let reservation = OUTPUT.reserve_ring().ok_or(RingAudioError::Busy)?;
        let worker = run.clone();
        thread::Builder::new()
            .name("morpheus-ring-output".into())
            .spawn(move || {
                // This dedicated owner waits; never the UI or Tokio worker. The
                // reservation already silences/retires old PCM, and the one actual
                // output permit cannot release before its native stream is gone.
                let _reservation = reservation;
                let result =
                    acquire_output(&worker).and_then(|_permit| play(&worker, speaker.as_deref()));
                worker.status.store(
                    if result == Err(RingAudioError::Unavailable)
                        || worker.status.load(Ordering::Acquire) == 3
                    {
                        3
                    } else {
                        2
                    },
                    Ordering::Release,
                );
                worker.stopped.store(true, Ordering::Release);
            })
            .map_err(|_| RingAudioError::Unavailable)?;
        Ok(Self { run })
    }
    pub fn stop(&self) {
        self.run.stopped.store(true, Ordering::Release);
    }
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
    // the process permit. Callback cancellation does not wait for this drop.
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

/// One explicit attachment's output state. The caller owns its single worker
/// permit until play_pcm returns, after destruction of the actual native stream.
/// These controls never touch microphone capture, a call mixer, route or OS volume.
pub struct PcmPlayback {
    run: Arc<Run>,
    paused: AtomicBool,
    ended: AtomicBool,
    consumed: AtomicU64,
    total: AtomicU64,
    rate: AtomicU32,
}
impl PcmPlayback {
    /// Original output epoch: a ringtone reservation permanently retires it,
    /// including a reservation which later fails to open a native speaker.
    pub fn output_authority() -> RingAudioAuthority {
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
        let _permit = Permit::take().ok_or(RingAudioError::Busy)?;
        if !self.run.active() {
            return Err(RingAudioError::Cancelled);
        }
        self.total
            .store((samples.len() / count) as u64, Ordering::Release);
        self.rate.store(rate, Ordering::Release);
        let result = self.open_pcm(samples, rate, channels);
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
