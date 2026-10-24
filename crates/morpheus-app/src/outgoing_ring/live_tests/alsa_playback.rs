//! Exact-child, hosted-only ALSA endpoint for the real call audio adapter.
//!
//! No PCM is saved. ALSA's built-in plug/file/null chain borrows our actual
//! pipe descriptors; it never receives a pathname, command, default or hardware
//! endpoint. One retained I/O worker paces silence into the synthetic microphone
//! and drains the production speaker. Only its bounded, fresh spectral windows
//! count as voice evidence. The call's own AudioOutput mixes the remote peers.
//! This module is not compiled into the shipping application.
#![cfg(all(test, target_os = "linux", feature = "native-calls"))]

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

mod loaded;

const RATE: usize = 48_000;
const WINDOW: usize = 4_800;
const CHUNK: usize = 1_920;
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const LIFETIME: Duration = Duration::from_secs(480);
const PERIOD: Duration = Duration::from_millis(10);
const WAIT: Duration = Duration::from_secs(20);
const CONFIG_NAME: &std::ffi::CStr = c"alsa.conf";
const LIBRARY_NAME: &std::ffi::CStr = c"alsa-library.json";
pub(super) const INPUT_ID: &str = "alsa:morpheus_call_input";
pub(super) const OUTPUT_ID: &str = "alsa:morpheus_call_output";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    NotAdmitted,
    Library,
    Io,
    Bound,
    Worker,
    Cleanup,
    NativeRetirement,
    StaleCheckpoint,
    MissingTone,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tone {
    Before,
    After,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Peer {
    Alice,
    Carol,
}
#[derive(Clone)]
pub(super) struct Checkpoint {
    epoch: u64,
    instance: Arc<()>,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Evidence {
    pub frames: u64,
    pub matching_windows: u32,
    pub remote_tones: u8,
}

fn require(value: bool) -> Result<(), Failure> {
    if value {
        Ok(())
    } else {
        Err(Failure::NotAdmitted)
    }
}

/// Deliberately retain on an unproved path. In particular, an integer which
/// ALSA still borrows must never be closed/reused just because a test unwinds.
/// The existing runner retains and bounds the entire child process on failure.
struct Capability {
    file: Option<File>,
    borrowed: bool,
}
impl Capability {
    fn new(file: File) -> Self {
        Self {
            file: Some(file),
            borrowed: false,
        }
    }
    fn file(&self) -> Result<&File, Failure> {
        self.file.as_ref().ok_or(Failure::Cleanup)
    }
    fn close(&mut self) -> Result<(), Failure> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        let fd = file.into_raw_fd();
        // Exactly one close. Linux may already have released the descriptor on
        // an error; do not retry, reconstruct it or use that number again.
        if unsafe { libc::close(fd) } == 0 {
            Ok(())
        } else {
            Err(Failure::Cleanup)
        }
    }
}
impl Drop for Capability {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            if self.borrowed {
                std::mem::forget(file);
            }
            // Before admission no native borrower exists. The failed setup
            // never emits a cleanup/success receipt; the owner waits child exit.
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Stamp {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
    links: u64,
    size: u64,
}
impl Stamp {
    fn of(info: &Metadata) -> Self {
        Self {
            dev: info.dev(),
            ino: info.ino(),
            uid: info.uid(),
            mode: info.mode(),
            links: info.nlink(),
            size: info.len(),
        }
    }
}
fn private_directory(file: &File) -> Result<Stamp, Failure> {
    let info = file.metadata().map_err(|_| Failure::Io)?;
    require(
        info.is_dir() && info.uid() == unsafe { libc::geteuid() } && info.mode() & 0o7777 == 0o700,
    )?;
    Ok(Stamp::of(&info))
}
fn opened(fd: i32) -> Result<Capability, Failure> {
    if fd < 0 {
        return Err(Failure::Io);
    }
    // The fd is returned by this module's successful open/openat/pipe2 only.
    Ok(Capability::new(unsafe { File::from_raw_fd(fd) }))
}
fn open_child(
    parent: &Capability,
    name: &std::ffi::CStr,
    flags: i32,
    mode: u32,
) -> Result<Capability, Failure> {
    opened(unsafe {
        libc::openat(
            parent.file()?.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    })
}
fn pipe(blocking_read: bool) -> Result<(Capability, Capability), Failure> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(Failure::Io);
    }
    let mut read = opened(fds[0])?;
    let mut write = opened(fds[1])?;
    let setup = (|| {
        require(fds[0] >= 3 && fds[1] >= 3 && fds[0] != fds[1])?;
        let size = unsafe { libc::fcntl(fds[0], libc::F_SETPIPE_SZ, 4096) };
        require((4096..=65_536).contains(&size))?;
        let borrowed = if blocking_read { fds[0] } else { fds[1] };
        let flags = unsafe { libc::fcntl(borrowed, libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(borrowed, libc::F_SETFL, flags & !libc::O_NONBLOCK) } != 0
        {
            return Err(Failure::Io);
        }
        Ok(())
    })();
    if let Err(error) = setup {
        let clean = read.close().and(write.close());
        return Err(if clean.is_ok() {
            error
        } else {
            Failure::Cleanup
        });
    }
    Ok((read, write))
}

fn config(output: i32, input: i32, discard: i32) -> Vec<u8> {
    // No includes/hooks/default/card nodes. The file-side formats are fixed
    // even when CPAL selects float PCM on the outer plug's native callback.
    format!(
        r#"defaults.namehint.showall off
pcm.morpheus_call_output {{
    type plug
    slave {{
        pcm {{ type file slave.pcm {{ type null }} file {output} format "raw" }}
        format "S16_LE"
        rate 48000
        channels 2
    }}
    hint {{ show on description "Morpheus synthetic call output" }}
}}
pcm.morpheus_call_input {{
    type plug
    slave {{
        pcm {{ type file slave.pcm {{ type null }} file {discard} infile {input} format "raw" }}
        format "S16_LE"
        rate 48000
        channels 1
    }}
    hint {{ show on description "Morpheus synthetic silent microphone" }}
}}
"#
    )
    .into_bytes()
}

struct State {
    epoch: u64,
    tone: Tone,
    remotes: u8,
    frames: u64,
    matches: u32,
    fault: Option<Failure>,
}
struct Window {
    samples: Vec<f64>,
    tail: Vec<u8>,
    epoch: u64,
    skip_frame: bool,
}
impl Window {
    fn new() -> Self {
        Self {
            samples: Vec::with_capacity(WINDOW),
            tail: Vec::with_capacity(4),
            epoch: 0,
            skip_frame: false,
        }
    }
    fn accept(&mut self, bytes: &[u8], state: &mut State) {
        if self.epoch != state.epoch {
            self.epoch = state.epoch;
            self.samples.clear();
            // Preserve byte alignment; discard only the frame straddling the
            // checkpoint. Clearing a partial stereo frame would misalign every
            // subsequent sample and is not a fresh-data boundary.
            self.skip_frame = !self.tail.is_empty();
        }
        for byte in bytes {
            self.tail.push(*byte);
            if self.tail.len() != 4 {
                continue;
            }
            let left = i16::from_le_bytes([self.tail[0], self.tail[1]]);
            let right = i16::from_le_bytes([self.tail[2], self.tail[3]]);
            self.tail.clear();
            if self.skip_frame {
                self.skip_frame = false;
                continue;
            }
            self.samples
                .push((f64::from(left) + f64::from(right)) * 0.5);
            state.frames += 1;
            if self.samples.len() == WINDOW {
                if matches(&self.samples, state.tone, state.remotes) {
                    state.matches = state.matches.saturating_add(1);
                } else {
                    state.matches = 0;
                }
                self.samples.clear();
            }
        }
    }
}
fn power(samples: &[f64], hz: f64) -> f64 {
    let omega = 2.0 * std::f64::consts::PI * hz / RATE as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, value) in samples.iter().enumerate() {
        let (sin, cos) = (omega * n as f64).sin_cos();
        re += value * cos;
        im += value * sin;
    }
    2.0 * (re * re + im * im) / samples.len() as f64
}
fn matches(samples: &[f64], tone: Tone, remotes: u8) -> bool {
    if samples.len() != WINDOW || !(1..=2).contains(&remotes) {
        return false;
    }
    let energy: f64 = samples.iter().map(|sample| sample * sample).sum();
    let rms = (energy / samples.len() as f64).sqrt();
    if !(400.0..=20_000.0).contains(&rms) {
        return false;
    }
    let (wanted, rejected) = match tone {
        Tone::Before => ([500.0, 700.0], [1100.0, 1700.0]),
        Tone::After => ([1100.0, 1700.0], [500.0, 700.0]),
    };
    let ratios = wanted.map(|hz| power(samples, hz) / energy);
    let old = rejected
        .into_iter()
        .map(|hz| power(samples, hz) / energy)
        .sum::<f64>();
    let desired = ratios[..usize::from(remotes)].iter().sum::<f64>();
    old <= 0.15
        && desired >= 0.65
        && ratios[..usize::from(remotes)]
            .iter()
            .all(|ratio| *ratio >= if remotes == 1 { 0.60 } else { 0.18 })
}

/// The source peers still use real encrypted Publication::push_audio.
/// Each has a distinct signal; the room oracle must recover both simultaneously.
pub(super) fn tone_frame(peer: Peer, tone: Tone, index: u64) -> morpheus_rtc::AudioFrame {
    let hz = match (peer, tone) {
        (Peer::Alice, Tone::Before) => 500.0,
        (Peer::Carol, Tone::Before) => 700.0,
        (Peer::Alice, Tone::After) => 1100.0,
        (Peer::Carol, Tone::After) => 1700.0,
    };
    let start = (index % 100) * 480;
    let data = (0..480)
        .map(|n| {
            (5000.0 * (2.0 * std::f64::consts::PI * hz * (start + n) as f64 / RATE as f64).sin())
                as i16
        })
        .collect::<Vec<_>>();
    morpheus_rtc::AudioFrame {
        data: data.into(),
        sample_rate: RATE as u32,
        num_channels: 1,
        samples_per_channel: 480,
    }
}

fn io_worker(
    mut output: Capability,
    mut microphone: Capability,
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<State>>,
) -> Result<(), Failure> {
    let began = Instant::now();
    let mut next = began;
    let mut count = 0_u64;
    let mut window = Window::new();
    let mut buffer = [0_u8; CHUNK];
    let silence = [0_u8; 960];
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut draining = 0;
        loop {
            if began.elapsed() >= LIFETIME {
                return Err(Failure::Bound);
            }
            let finishing = stop.load(Ordering::Acquire);
            if finishing {
                draining += 1;
                if draining > 128 {
                    return Err(Failure::Bound);
                }
            } else {
                let now = Instant::now();
                if now < next {
                    std::thread::sleep((next - now).min(PERIOD));
                    continue;
                }
                next = now + PERIOD;
                match microphone.file()?.write(&silence) {
                    Ok(960) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    _ => return Err(Failure::Io),
                }
            }
            let read = match output.file()?.read(&mut buffer) {
                Ok(0) if finishing => break Ok(()),
                Ok(0) => return Err(Failure::Io),
                Ok(n) => n,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && finishing => {
                    break Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                _ => return Err(Failure::Io),
            };
            count = count.checked_add(read as u64).ok_or(Failure::Bound)?;
            if count > MAX_BYTES {
                return Err(Failure::Bound);
            }
            let mut state = shared.lock().map_err(|_| Failure::Worker)?;
            window.accept(&buffer[..read], &mut state);
        }
    }))
    .unwrap_or(Err(Failure::Worker));
    let cleanup = output.close().and(microphone.close());
    let result = if cleanup.is_err() {
        Err(Failure::Cleanup)
    } else {
        outcome
    };
    if let Err(error) = result {
        if let Ok(mut state) = shared.lock() {
            state.fault = Some(error);
        }
    }
    result
}

/// Only the supervisor can construct this after the actual native thread join.
struct Joined {
    result: Result<(), Failure>,
}

pub(super) struct NativePlayback {
    root: PathBuf,
    parent: Capability,
    directory: Capability,
    root_stamp: Stamp,
    config_stamp: Stamp,
    library_stamp: Stamp,
    library: loaded::Identity,
    borrowed: Vec<Capability>,
    worker: Arc<Mutex<Option<JoinHandle<Result<(), Failure>>>>>,
    joining: Option<tokio::task::JoinHandle<Result<Joined, Failure>>>,
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<State>>,
    retired: bool,
    terminal: Option<Failure>,
    completion: Option<Result<(), Failure>>,
    instance: Arc<()>,
}
impl NativePlayback {
    /// Must precede every CPAL/ALSA call in this exact, freshly spawned test child.
    /// The wrapper passes these two values only to the explicit outage command.
    pub(super) fn start() -> Result<Self, Failure> {
        for (key, value) in [
            ("GITHUB_ACTIONS", "true"),
            ("RUNNER_ENVIRONMENT", "github-hosted"),
            ("RUNNER_OS", "Linux"),
            ("GITHUB_REPOSITORY", "brk275/Morpheus"),
        ] {
            require(std::env::var(key).as_deref() == Ok(value))?;
        }
        require(
            unsafe { libc::getuid() } == unsafe { libc::geteuid() }
                && unsafe { libc::geteuid() } != 0,
        )?;
        require(
            fs::symlink_metadata("/dev/snd")
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        )?;
        require(std::env::vars_os().all(|(key, _)| {
            !key.as_encoded_bytes().starts_with(b"ALSA_") || key == "ALSA_CONFIG_PATH"
        }))?;
        let remotes = match std::env::var("MORPHEUS_RTC_OUTAGE_TARGET").as_deref() {
            Ok("dm") => 1,
            Ok("room") => 2,
            _ => return Err(Failure::NotAdmitted),
        };
        let home = PathBuf::from(
            std::env::var_os("MORPHEUS_MATRIX_TEST_HOME").ok_or(Failure::NotAdmitted)?,
        );
        require(home.is_absolute() && home.canonicalize().map_err(|_| Failure::Io)? == home)?;
        let suffix = if remotes == 1 {
            "rtc-playback-dm"
        } else {
            "rtc-playback-room"
        };
        let root = home.join(suffix);
        require(
            std::env::var_os("MORPHEUS_CALL_AUDIO_ROOT").as_deref() == Some(root.as_os_str())
                && std::env::var_os("ALSA_CONFIG_PATH").as_deref()
                    == Some(root.join("alsa.conf").as_os_str()),
        )?;
        let parent = Capability::new(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(&home)
                .map_err(|_| Failure::Io)?,
        );
        private_directory(parent.file()?)?;
        let basename = if remotes == 1 {
            c"rtc-playback-dm"
        } else {
            c"rtc-playback-room"
        };
        if unsafe { libc::mkdirat(parent.file()?.as_raw_fd(), basename.as_ptr(), 0o700) } != 0 {
            return Err(Failure::Io);
        }
        let directory = open_child(&parent, basename, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        private_directory(directory.file()?)?;
        // Bind the actual already-loaded PCM/file/null/plug code before any
        // CPAL/PCM operation or config can be consumed by native code.
        let library = loaded::Identity::observe()?;
        let identity = library.record();
        require(identity.len() <= 2048)?;
        let mut record = open_child(
            &directory,
            LIBRARY_NAME,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        record
            .file()?
            .write_all(&identity)
            .map_err(|_| Failure::Io)?;
        if unsafe { libc::fchmod(record.file()?.as_raw_fd(), 0o400) } != 0 {
            return Err(Failure::Io);
        }
        record.file()?.sync_all().map_err(|_| Failure::Io)?;
        let info = record.file()?.metadata().map_err(|_| Failure::Io)?;
        require(
            info.is_file()
                && info.nlink() == 1
                && info.uid() == unsafe { libc::geteuid() }
                && info.mode() & 0o7777 == 0o400
                && info.len() == identity.len() as u64,
        )?;
        let library_stamp = Stamp::of(&info);
        record.close()?;
        let (output_read, output_write) = pipe(false)?;
        let (input_read, input_write) = pipe(true)?;
        let discard = Capability::new(
            OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/dev/null")
                .map_err(|_| Failure::Io)?,
        );
        let info = discard.file()?.metadata().map_err(|_| Failure::Io)?;
        require(info.file_type().is_char_device() && info.rdev() == libc::makedev(1, 3))?;
        let bytes = config(
            output_write.file()?.as_raw_fd(),
            input_read.file()?.as_raw_fd(),
            discard.file()?.as_raw_fd(),
        );
        require(bytes.len() <= 2048)?;
        let mut config_file = open_child(
            &directory,
            CONFIG_NAME,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        config_file
            .file()?
            .write_all(&bytes)
            .map_err(|_| Failure::Io)?;
        if unsafe { libc::fchmod(config_file.file()?.as_raw_fd(), 0o400) } != 0 {
            return Err(Failure::Io);
        }
        config_file.file()?.sync_all().map_err(|_| Failure::Io)?;
        let info = config_file.file()?.metadata().map_err(|_| Failure::Io)?;
        require(
            info.is_file()
                && info.nlink() == 1
                && info.uid() == unsafe { libc::geteuid() }
                && info.mode() & 0o7777 == 0o400
                && info.len() == bytes.len() as u64,
        )?;
        let config_stamp = Stamp::of(&info);
        config_file.close()?;
        let root_stamp = private_directory(directory.file()?)?;
        let shared = Arc::new(Mutex::new(State {
            epoch: 0,
            tone: Tone::Before,
            remotes,
            frames: 0,
            matches: 0,
            fault: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let state = shared.clone();
        let stopped = stop.clone();
        let worker = std::thread::Builder::new()
            .name("owned-call-pcm-fixture".into())
            .spawn(move || io_worker(output_read, input_write, stopped, state))
            .map_err(|_| Failure::Worker)?;
        let mut borrowed = vec![output_write, input_read, discard];
        for cap in &mut borrowed {
            cap.borrowed = true;
        }
        Ok(Self {
            root,
            parent,
            directory,
            root_stamp,
            config_stamp,
            library_stamp,
            library,
            borrowed,
            worker: Arc::new(Mutex::new(Some(worker))),
            joining: None,
            stop,
            shared,
            retired: false,
            terminal: None,
            completion: None,
            instance: Arc::new(()),
        })
    }
    pub(super) fn checkpoint(&self, tone: Tone) -> Result<Checkpoint, Failure> {
        self.library.verify()?;
        let mut state = self.shared.lock().map_err(|_| Failure::Worker)?;
        if let Some(error) = state.fault {
            return Err(error);
        }
        state.epoch = state.epoch.checked_add(1).ok_or(Failure::Bound)?;
        state.tone = tone;
        state.frames = 0;
        state.matches = 0;
        Ok(Checkpoint {
            epoch: state.epoch,
            instance: self.instance.clone(),
        })
    }
    pub(super) async fn wait_for(&self, checkpoint: &Checkpoint) -> Result<Evidence, Failure> {
        if !Arc::ptr_eq(&checkpoint.instance, &self.instance) {
            return Err(Failure::StaleCheckpoint);
        }
        let until = tokio::time::Instant::now() + WAIT;
        loop {
            let evidence = {
                let state = self.shared.lock().map_err(|_| Failure::Worker)?;
                if let Some(error) = state.fault {
                    return Err(error);
                }
                if state.epoch != checkpoint.epoch {
                    return Err(Failure::StaleCheckpoint);
                }
                (state.matches >= 3).then_some(Evidence {
                    frames: state.frames,
                    matching_windows: state.matches,
                    remote_tones: state.remotes,
                })
            };
            if let Some(evidence) = evidence {
                self.library.verify()?;
                let state = self.shared.lock().map_err(|_| Failure::Worker)?;
                if let Some(error) = state.fault {
                    return Err(error);
                }
                if state.epoch != checkpoint.epoch {
                    return Err(Failure::StaleCheckpoint);
                }
                return Ok(evidence);
            }
            if tokio::time::Instant::now() >= until {
                return Err(Failure::MissingTone);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    /// Call only after the real CallController and all remote publications/peers
    /// have finished, on every success/error outcome. Cancellation of this
    /// observation cannot detach its retained native capabilities or join task.
    pub(super) async fn finish(&mut self) -> Result<(), Failure> {
        if let Some(result) = self.completion {
            return result;
        }
        if let Some(error) = self.terminal {
            return Err(error);
        }
        if !self.retired {
            if self.library.verify().is_err() {
                self.terminal = Some(Failure::Library);
                return Err(Failure::Library);
            }
            if morpheus_rtc::wait_for_audio_device_release().await.is_err()
                || morpheus_rtc::wait_for_all_alsa_release().await.is_err()
            {
                self.terminal = Some(Failure::NativeRetirement);
                return Err(Failure::NativeRetirement);
            }
            self.retired = true;
            self.stop.store(true, Ordering::Release);
        }
        if self.joining.is_none() {
            // The actual handle stays in a separately retained slot until the
            // supervisor is running. If spawn_blocking panics or discards its
            // closure, dropping that closure cannot detach the native worker.
            let slot = self.worker.clone();
            self.joining = Some(tokio::task::spawn_blocking(move || {
                let worker = slot
                    .lock()
                    .map_err(|_| Failure::Worker)?
                    .take()
                    .ok_or(Failure::Worker)?;
                let result = worker.join().unwrap_or(Err(Failure::Worker));
                Ok(Joined { result })
            }));
        }
        let joined = self.joining.as_mut().ok_or(Failure::Worker)?.await;
        self.joining.take();
        let result = match joined {
            Ok(Ok(Joined { result })) => result,
            // A supervisor failure is not an actual native-thread join receipt.
            // Keep every borrowed capability and the transfer slot retained.
            Ok(Err(_)) | Err(_) => {
                self.terminal = Some(Failure::Worker);
                return Err(Failure::Worker);
            }
        };
        // Actual worker join (including TLS) has completed. Its result can fail
        // while the now-unborrowed capabilities still require checked cleanup.
        let mut cleanup = Ok(());
        for cap in &mut self.borrowed {
            if cap.close().is_err() {
                cleanup = Err(Failure::Cleanup);
            }
        }
        if self.library.verify().is_err() {
            cleanup = Err(Failure::Library);
        }
        if self.library.close().is_err() {
            cleanup = Err(Failure::Cleanup);
        }
        let owned = private_directory(self.directory.file()?)? == self.root_stamp
            && fs::symlink_metadata(&self.root)
                .map(|info| Stamp::of(&info) == self.root_stamp)
                .unwrap_or(false)
            && fs::symlink_metadata(self.root.join("alsa.conf"))
                .map(|info| Stamp::of(&info) == self.config_stamp)
                .unwrap_or(false)
            && fs::symlink_metadata(self.root.join("alsa-library.json"))
                .map(|info| Stamp::of(&info) == self.library_stamp)
                .unwrap_or(false);
        if owned {
            if unsafe {
                libc::unlinkat(self.directory.file()?.as_raw_fd(), CONFIG_NAME.as_ptr(), 0)
            } != 0
            {
                cleanup = Err(Failure::Cleanup);
            }
            if unsafe {
                libc::unlinkat(self.directory.file()?.as_raw_fd(), LIBRARY_NAME.as_ptr(), 0)
            } != 0
            {
                cleanup = Err(Failure::Cleanup);
            }
            let name = if self
                .root
                .file_name()
                .is_some_and(|name| name == "rtc-playback-dm")
            {
                c"rtc-playback-dm"
            } else {
                c"rtc-playback-room"
            };
            if unsafe {
                libc::unlinkat(
                    self.parent.file()?.as_raw_fd(),
                    name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                cleanup = Err(Failure::Cleanup);
            }
        } else {
            cleanup = Err(Failure::Cleanup);
        }
        if self.directory.close().is_err() {
            cleanup = Err(Failure::Cleanup);
        }
        if self.parent.close().is_err() {
            cleanup = Err(Failure::Cleanup);
        }
        let result = cleanup.and(result);
        self.completion = Some(result);
        result
    }
}
impl Drop for NativePlayback {
    fn drop(&mut self) {
        // Never stop the reader or close a still-borrowed fd on unwind/error.
        // No successful cleanup fact can be emitted from Drop. The existing
        // retained runner child has the independent hard process deadline.
        if self.completion.is_none() {
            std::mem::forget(self.worker.clone());
        }
        if let Some(joining) = self.joining.take() {
            std::mem::forget(joining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wave(tone: Tone, remotes: u8) -> Vec<f64> {
        (0..WINDOW)
            .map(|n| {
                let frequencies = if tone == Tone::Before {
                    [500.0, 700.0]
                } else {
                    [1100.0, 1700.0]
                };
                frequencies[..usize::from(remotes)]
                    .iter()
                    .map(|hz| {
                        5000.0 * (2.0 * std::f64::consts::PI * hz * n as f64 / RATE as f64).sin()
                    })
                    .sum()
            })
            .collect()
    }
    #[test]
    fn group_oracle_requires_both_distinct_new_tones_and_refuses_old_or_silent_audio() {
        assert!(matches(&wave(Tone::After, 2), Tone::After, 2));
        assert!(matches(&wave(Tone::After, 1), Tone::After, 1));
        assert!(!matches(&wave(Tone::After, 1), Tone::After, 2));
        assert!(!matches(&wave(Tone::Before, 2), Tone::After, 2));
        assert!(!matches(&vec![0.0; WINDOW], Tone::After, 2));
        let swapped = (0..WINDOW)
            .map(|n| 5000.0 * (2.0 * std::f64::consts::PI * 1700.0 * n as f64 / RATE as f64).sin())
            .collect::<Vec<_>>();
        assert!(!matches(&swapped, Tone::After, 2));
    }
    #[test]
    fn checkpoint_retires_old_windows_and_reassembles_partial_stereo_frames() {
        let mut window = Window::new();
        let mut state = State {
            epoch: 1,
            tone: Tone::Before,
            remotes: 2,
            frames: 0,
            matches: 0,
            fault: None,
        };
        let encode = |tone| {
            wave(tone, 2)
                .iter()
                .flat_map(|sample| {
                    let bytes = (*sample as i16).to_le_bytes();
                    [bytes[0], bytes[1], bytes[0], bytes[1]]
                })
                .collect::<Vec<_>>()
        };
        let old = encode(Tone::Before);
        window.accept(&old, &mut state);
        assert_eq!(state.matches, 1);
        window.accept(&old[..3], &mut state);
        state.epoch = 2;
        state.tone = Tone::After;
        state.frames = 0;
        state.matches = 0;
        window.accept(&old[3..4], &mut state);
        assert_eq!(state.frames, 0);
        window.accept(&old, &mut state);
        assert_eq!(state.matches, 0);
        let new = encode(Tone::After);
        for part in new.chunks(7) {
            window.accept(part, &mut state);
        }
        assert_eq!(state.matches, 1);
        assert_eq!(state.frames, (WINDOW * 2) as u64);
    }
}
