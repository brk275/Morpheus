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
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const RATE: u32 = 48_000;
const MAX_DEVICES: usize = 512;
const MAX_CONFIGS: usize = 512;
const MAX_RING: Duration = Duration::from_secs(30);
static OWNED: AtomicBool = AtomicBool::new(false);

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
        let permit = Permit::take().ok_or(RingAudioError::Busy)?;
        let worker = run.clone();
        thread::Builder::new()
            .name("morpheus-ring-output".into())
            .spawn(move || {
                // Move permit first so stack unwinding releases it after local device
                // objects. No JoinHandle is retained by the UI/Tokio shutdown pool.
                let _permit = permit;
                let result = play(&worker, speaker.as_deref());
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
