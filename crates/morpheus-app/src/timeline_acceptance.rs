//! Real-room timeline performance acceptance for instrumented release builds.
//!
//! Compiled into the app only when build.rs sets
//! `cfg(morpheus_timeline_acceptance)` (`MORPHEUS_TIMELINE_ACCEPTANCE_BUILD=1`
//! with the release profile), and into unit-test builds for its pure checks.
//! Such a binary differs from a shipping build by exactly the tokens in
//! `DELTA_FROM_SHIPPING`. It refuses to start without a valid run file, so it
//! never runs as an ordinary app, and its report always says
//! `shipping_acceptance: false`: this is instrumented-build evidence, never
//! installed-package acceptance.
//!
//! The driver uses the real `AppWindow`, the renderer the default backend
//! selects, and the real `LiveUi` controller. It acts only through production
//! paths: the startup restore of the last active account and the `open-room`,
//! `load-older-messages` and `switch-account` callbacks. Scrolling is
//! dispatched as window input events in process. The fixture is an encrypted
//! room with 10,000 variable-height events on loopback homeservers (see
//! crates/morpheus-matrix/tests/timeline_10k_local.rs), with generated
//! accounts in a generated profile and an ephemeral hosted-runner vault.
//!
//! The report is one closed JSON file: fixed keys, tokens and numbers, never
//! IDs, names, credentials or message text. Enrollment also writes the
//! measured account's own device ID to a separate new owner-only file the run
//! file names; only the runner and the seeder read it, so the seeder can wait
//! for exactly that device's keys. Rendering numbers come from
//! Slint's rendering notifier: `AfterRendering` fires once the scene is
//! rendered but before the back buffer is presented, so they are CPU-side
//! frame-hook timings, never GPU time, presentation or display refresh.
//! Values ending in `_observed` are first observed by an event-loop poll and
//! are upper bounds. Delegate instantiation is not measured. See
//! tests/TIMELINE_10K.md for the procedure and what remains open.
#![cfg_attr(test, allow(dead_code))]

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    error::Error,
    fs,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use morpheus_matrix::{AccountManager, SyncBackend, SyncPolicy, SyncStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use slint::{ComponentHandle, GraphicsAPI, Model, RenderingState, SetRenderingNotifierError};
use tokio::runtime::Handle;

use crate::{AppWindow, live::LiveUi};

const CHECK: &str = "morpheus_timeline_10k";
const RUN_ENV: &str = "MORPHEUS_TIMELINE_ACCEPTANCE";
/// Every fixture body carries this prefix and a five-digit index; shared with
/// crates/morpheus-matrix/tests/timeline_10k_local.rs.
const MARKER: &str = "tl10k-";
const EVENTS: usize = 10_000;
const DELTA_FROM_SHIPPING: [&str; 4] = [
    "acceptance_driver",
    "rendering_notifier",
    "generated_profile_root",
    "requested_sync_policy",
];
const NOT_MEASURED: [&str; 4] = [
    "gpu_time",
    "presentation",
    "display_refresh",
    "delegate_instances",
];
/// Fixture labels from scripts/local_matrix_env.py.
const MEASURED_ACCOUNT: &str = "a_second_device";
const SWITCH_ACCOUNT: &str = "b_primary";
const MEASURED_ROOM: &str = "encrypted_room";
const SWITCH_ROOM: &str = "b_room";
const FILE_LIMIT: u64 = 64 * 1024;
const ENROLL_LIMIT: Duration = Duration::from_secs(10 * 60);
const MEASURE_LIMIT: Duration = Duration::from_secs(45 * 60);
const WAIT_LIMIT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(5);
const SLOW_POLL: Duration = Duration::from_millis(250);
const MAX_PAGES: usize = 1_000;
const SCROLL_STEPS: usize = 120;
/// Logical pixels per dispatched wheel event.
const SCROLL_DELTA: f32 = 480.0;
const SCROLL_RENDER_LIMIT: Duration = Duration::from_secs(1);
const IDLE_WINDOW: Duration = Duration::from_secs(1);
const SAMPLE_LIMIT: usize = 4_096;

type Refusal = &'static str;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Enroll,
    Measure,
}

impl Mode {
    fn token(self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Measure => "measure",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adapter {
    Sliding,
    Classic,
}

impl Adapter {
    fn token(self) -> &'static str {
        match self {
            Self::Sliding => "sliding",
            Self::Classic => "classic",
        }
    }

    fn policy(self) -> SyncPolicy {
        match self {
            Self::Sliding => SyncPolicy::Auto,
            Self::Classic => SyncPolicy::ClassicOnly,
        }
    }

    fn expects(self, backend: SyncBackend) -> bool {
        matches!(
            (self, backend),
            (Self::Sliding, SyncBackend::SlidingSticky) | (Self::Classic, SyncBackend::Classic)
        )
    }
}

fn backend_token(backend: SyncBackend) -> &'static str {
    match backend {
        SyncBackend::SlidingSticky => "sliding_sticky",
        SyncBackend::Classic => "classic",
    }
}

/// The run file the acceptance runner writes outside the checkout.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFile {
    mode: String,
    adapter: String,
    profile: PathBuf,
    fixture: PathBuf,
    #[serde(default)]
    room: Option<PathBuf>,
    report: PathBuf,
    /// Enrollment only: a new private file for the measured account's own
    /// device ID, so the seeder can wait for exactly that device's keys.
    #[serde(default)]
    device: Option<PathBuf>,
}

/// Written by the seeder: the 10,000-event room.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoomFile {
    room_id: String,
    events: usize,
}

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<FixtureAccount>,
    rooms: HashMap<String, String>,
}

#[derive(Clone, Deserialize)]
struct FixtureAccount {
    label: String,
    user_id: String,
    username: String,
    homeserver_url: String,
    password: String,
}

/// One validated run. Credentials stay in memory and are never printed.
struct Run {
    mode: Mode,
    adapter: Adapter,
    profile: PathBuf,
    report: PathBuf,
    device: Option<PathBuf>,
    measured: FixtureAccount,
    switch: FixtureAccount,
    measured_enroll_room: String,
    switch_enroll_room: String,
    room: Option<String>,
}

/// Only a fresh GitHub-hosted runner, whose OS vault is disposable, may hold
/// the generated fixture credentials. Never a developer's own vault.
fn hosted_refusal(get: impl Fn(&str) -> Option<String>) -> Option<Refusal> {
    if get("GITHUB_ACTIONS").as_deref() != Some("true")
        || get("RUNNER_ENVIRONMENT").as_deref() != Some("github-hosted")
    {
        return Some("not_hosted");
    }
    // An installed package is a different acceptance; never mix the two.
    if get("APPIMAGE").is_some() {
        return Some("packaged");
    }
    // The runner provides a session bus for its throwaway Secret Service.
    if cfg!(target_os = "linux") && get("DBUS_SESSION_BUS_ADDRESS").is_none_or(|v| v.is_empty()) {
        return Some("no_session_bus");
    }
    None
}

/// Plain HTTP to a loopback address with an explicit port: the local fixture.
fn loopback_homeserver(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    loopback
        && url.scheme() == "http"
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/")
}

/// A generated fixture user on a reserved `.test` server name.
fn synthetic_user(user_id: &str) -> bool {
    let Some((local, server)) = user_id
        .strip_prefix('@')
        .and_then(|rest| rest.split_once(':'))
    else {
        return false;
    };
    !local.is_empty()
        && local
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._=-/".contains(&b))
        && server.len() > ".test".len()
        && server.ends_with(".test")
        && server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

fn plausible_room_id(room_id: &str) -> bool {
    room_id.len() > 1
        && room_id.len() <= 255
        && room_id.starts_with('!')
        && room_id.bytes().all(|b| b.is_ascii_graphic())
}

/// The checkout this binary was built from. Run data must stay outside it.
fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn outside(path: &Path, checkout: &Path) -> Result<PathBuf, Refusal> {
    if !path.is_absolute() {
        return Err("path_not_absolute");
    }
    let resolved = path.canonicalize().map_err(|_| "path_unavailable")?;
    let checkout = checkout
        .canonicalize()
        .map_err(|_| "checkout_unavailable")?;
    if resolved.starts_with(&checkout) {
        return Err("path_in_checkout");
    }
    Ok(resolved)
}

/// The path of a file still to be created, in a folder outside the checkout.
fn new_file_path(path: &Path, checkout: &Path, invalid: Refusal) -> Result<PathBuf, Refusal> {
    if !path.is_absolute() || path.exists() {
        return Err(invalid);
    }
    let parent = path.parent().ok_or(invalid)?;
    let parent = outside(parent, checkout)?;
    let name = path.file_name().ok_or(invalid)?;
    Ok(parent.join(name))
}

/// The same opaque-ID shape the runner and the seeder accept.
fn plausible_device_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.=+/".contains(&byte))
}

fn read_small(path: &Path, checkout: &Path) -> Result<Vec<u8>, Refusal> {
    let path = outside(path, checkout)?;
    let metadata = fs::metadata(&path).map_err(|_| "file_unavailable")?;
    if !metadata.is_file() || metadata.len() > FILE_LIMIT {
        return Err("file_invalid");
    }
    fs::read(&path).map_err(|_| "file_unavailable")
}

#[cfg(unix)]
fn owner_only(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn owner_only(_metadata: &fs::Metadata) -> bool {
    true
}

fn fixture_account(fixture: &Fixture, label: &str) -> Result<FixtureAccount, Refusal> {
    let account = fixture
        .accounts
        .iter()
        .find(|account| account.label == label)
        .ok_or("fixture_account_missing")?;
    if !loopback_homeserver(&account.homeserver_url) {
        return Err("homeserver_not_loopback");
    }
    if !synthetic_user(&account.user_id) || account.username.is_empty() {
        return Err("fixture_user_not_synthetic");
    }
    if account.password.len() < 16 {
        return Err("fixture_password_not_generated");
    }
    Ok(account.clone())
}

fn fixture_room(fixture: &Fixture, key: &str) -> Result<String, Refusal> {
    fixture
        .rooms
        .get(key)
        .filter(|room| plausible_room_id(room))
        .cloned()
        .ok_or("fixture_room_missing")
}

impl Run {
    fn from_environment() -> Result<Self, Refusal> {
        if let Some(refusal) = hosted_refusal(|name| std::env::var(name).ok()) {
            return Err(refusal);
        }
        let path = std::env::var_os(RUN_ENV)
            .map(PathBuf::from)
            .ok_or("run_file_missing")?;
        let checkout = checkout();
        let file = serde_json::from_slice(&read_small(&path, &checkout)?)
            .map_err(|_| "run_file_invalid")?;
        Self::from_file(file, &checkout)
    }

    fn from_file(file: RunFile, checkout: &Path) -> Result<Self, Refusal> {
        let mode = match file.mode.as_str() {
            "enroll" => Mode::Enroll,
            "measure" => Mode::Measure,
            _ => return Err("mode_invalid"),
        };
        let adapter = match file.adapter.as_str() {
            "sliding" => Adapter::Sliding,
            "classic" => Adapter::Classic,
            _ => return Err("adapter_invalid"),
        };
        let profile = outside(&file.profile, checkout)?;
        let metadata = fs::metadata(&profile).map_err(|_| "profile_unavailable")?;
        if !metadata.is_dir() || !owner_only(&metadata) {
            return Err("profile_not_private");
        }
        let mut entries = fs::read_dir(&profile).map_err(|_| "profile_unavailable")?;
        match mode {
            // Enrollment starts from a generated, empty directory: never an
            // existing profile or account.
            Mode::Enroll if entries.next().is_some() => return Err("profile_not_fresh"),
            Mode::Measure if !profile.join("data").is_dir() => {
                return Err("profile_not_enrolled");
            }
            _ => {}
        }
        let fixture: Fixture = serde_json::from_slice(&read_small(&file.fixture, checkout)?)
            .map_err(|_| "fixture_invalid")?;
        let measured = fixture_account(&fixture, MEASURED_ACCOUNT)?;
        let switch = fixture_account(&fixture, SWITCH_ACCOUNT)?;
        let room = match (mode, &file.room) {
            (Mode::Measure, Some(path)) => {
                let room: RoomFile = serde_json::from_slice(&read_small(path, checkout)?)
                    .map_err(|_| "room_file_invalid")?;
                if room.events != EVENTS || !plausible_room_id(&room.room_id) {
                    return Err("room_file_invalid");
                }
                Some(room.room_id)
            }
            (Mode::Enroll, None) => None,
            _ => return Err("room_file_mode"),
        };
        let report = new_file_path(&file.report, checkout, "report_path_invalid")?;
        let device = match (mode, &file.device) {
            (Mode::Enroll, Some(path)) => {
                let path = new_file_path(path, checkout, "device_path_invalid")?;
                if path == report {
                    return Err("device_path_invalid");
                }
                Some(path)
            }
            (Mode::Measure, None) => None,
            _ => return Err("device_file_mode"),
        };
        Ok(Self {
            mode,
            adapter,
            profile,
            report,
            device,
            measured_enroll_room: fixture_room(&fixture, MEASURED_ROOM)?,
            switch_enroll_room: fixture_room(&fixture, SWITCH_ROOM)?,
            measured,
            switch,
            room,
        })
    }
}

/// Nearest-rank percentiles over bounded samples.
#[derive(Default)]
struct Samples {
    values: Vec<u64>,
    dropped: u64,
}

impl Samples {
    fn push(&mut self, value: u64) {
        if self.values.len() < SAMPLE_LIMIT {
            self.values.push(value);
        } else {
            self.dropped += 1;
        }
    }

    fn summary(&self) -> Value {
        if self.values.is_empty() {
            return Value::Null;
        }
        let mut sorted = self.values.clone();
        sorted.sort_unstable();
        let rank = |percent: usize| sorted[(sorted.len() * percent).div_ceil(100).max(1) - 1];
        json!({
            "count": sorted.len(),
            "dropped": self.dropped,
            "p50": rank(50),
            "p95": rank(95),
            "max": sorted[sorted.len() - 1],
        })
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn api_token(api: &GraphicsAPI<'_>) -> &'static str {
    match api {
        GraphicsAPI::NativeOpenGL { .. } => "native_opengl",
        _ => "other",
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Lifecycle {
    #[default]
    NotStarted,
    Active,
    TornDown,
}

/// Bounded records from the window's rendering notifier. Only the first
/// failure is kept.
#[derive(Default)]
struct Frames {
    lifecycle: Lifecycle,
    api: Option<&'static str>,
    failure: Option<Refusal>,
    pending: Option<Instant>,
    completed: u64,
    first_after: Option<Instant>,
    last_after: Option<Instant>,
    /// The first `AfterRendering` since the driver last cleared it.
    next_after: Option<Instant>,
    recording: bool,
    spans_us: Samples,
    intervals_us: Samples,
}

impl Frames {
    fn fail(&mut self, reason: Refusal) {
        self.failure.get_or_insert(reason);
    }

    fn observe(&mut self, state: RenderingState, api: &GraphicsAPI<'_>) {
        if self.failure.is_some() {
            return;
        }
        let now = Instant::now();
        let token = api_token(api);
        if *self.api.get_or_insert(token) != token {
            self.fail("renderer_api_changed");
            return;
        }
        match state {
            RenderingState::RenderingSetup if self.lifecycle == Lifecycle::NotStarted => {
                self.lifecycle = Lifecycle::Active;
            }
            RenderingState::RenderingTeardown if self.lifecycle == Lifecycle::Active => {
                if self.pending.is_some() {
                    self.fail("unpaired_frame");
                }
                self.lifecycle = Lifecycle::TornDown;
            }
            RenderingState::RenderingSetup | RenderingState::RenderingTeardown => {
                self.fail("renderer_lifecycle");
            }
            RenderingState::BeforeRendering => {
                if self.lifecycle != Lifecycle::Active {
                    self.fail("frame_outside_lifecycle");
                } else if self.pending.replace(now).is_some() {
                    self.fail("unpaired_frame");
                }
            }
            RenderingState::AfterRendering => match self.pending.take() {
                Some(start) => {
                    self.completed += 1;
                    self.first_after.get_or_insert(now);
                    self.next_after.get_or_insert(now);
                    if self.recording {
                        self.spans_us.push(micros(now - start));
                        if let Some(previous) = self.last_after {
                            self.intervals_us.push(micros(now - previous));
                        }
                    }
                    self.last_after = Some(now);
                }
                None => self.fail("unpaired_frame"),
            },
            _ => self.fail("renderer_state_unknown"),
        }
    }

    /// Intervals count only between two frames inside the recorded window.
    fn record(&mut self, recording: bool) {
        self.recording = recording;
        self.last_after = None;
    }
}

/// The marker's index in a fixture body, if it carries one.
fn marker_index(body: &str) -> Option<usize> {
    let start = body.find(MARKER)? + MARKER.len();
    let digits = body.get(start..start + 5)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: usize = digits.parse().ok()?;
    (1..=EVENTS).contains(&index).then_some(index)
}

/// Direct counts over the window's message model: rows carrying a fixture
/// marker and distinct fixture indices. Undecryptable events reach the model
/// only as fixed notices without a marker, so the distinct count is also the
/// number of decrypted events.
fn fixture_rows<'a>(bodies: impl Iterator<Item = &'a str>) -> (usize, usize) {
    let mut seen = vec![false; EVENTS + 1];
    let (mut marked, mut distinct) = (0, 0);
    for index in bodies.filter_map(marker_index) {
        marked += 1;
        if !std::mem::replace(&mut seen[index], true) {
            distinct += 1;
        }
    }
    (marked, distinct)
}

/// Resident and peak resident set from /proc, in KiB.
fn proc_rss_kib(status: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        status.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.trim().strip_suffix("kB")?;
            value.trim().parse::<u64>().ok()
        })
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

/// In-process RSS on Linux only; other platforms report null and a runner
/// must sample them externally.
fn rss_kib() -> Option<(u64, u64)> {
    if cfg!(target_os = "linux") {
        proc_rss_kib(&fs::read_to_string("/proc/self/status").ok()?)
    } else {
        None
    }
}

#[derive(Default)]
struct SyncObservation {
    first_running: Option<(Instant, SyncBackend)>,
    last: Option<SyncBackend>,
    backend_changes: u32,
}

/// Wait for a Slint timer: the UI thread has no Tokio context.
async fn pause(duration: Duration) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    slint::Timer::single_shot(duration, move || {
        let _ = sender.send(());
    });
    let _ = receiver.await;
}

/// Poll a cheap UI condition from the event loop; returns when it was seen.
async fn until(
    limit: Duration,
    every: Duration,
    reason: Refusal,
    mut ready: impl FnMut() -> bool,
) -> Result<Instant, Refusal> {
    let deadline = Instant::now() + limit;
    loop {
        if ready() {
            return Ok(Instant::now());
        }
        if Instant::now() >= deadline {
            return Err(reason);
        }
        pause(every).await;
    }
}

async fn on_runtime<T, F>(runtime: &Handle, work: F) -> Result<T, Refusal>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    runtime.spawn(work).await.map_err(|_| "runtime_task")
}

pub(crate) struct Acceptance {
    started: Instant,
    shown: Option<Instant>,
    run: Run,
    sync: Arc<Mutex<SyncObservation>>,
}

impl Acceptance {
    pub(crate) fn from_environment() -> Result<Self, Box<dyn Error>> {
        let started = Instant::now();
        let run = Run::from_environment().map_err(|reason| {
            eprintln!("MORPHEUS_TIMELINE_10K_REFUSED {reason}");
            Box::<dyn Error>::from("This instrumented build only runs the timeline acceptance")
        })?;
        Ok(Self {
            started,
            shown: None,
            run,
            sync: Arc::default(),
        })
    }

    pub(crate) fn profile_root(&self) -> &Path {
        &self.run.profile
    }

    pub(crate) fn sync_policy(&self) -> SyncPolicy {
        self.run.adapter.policy()
    }

    pub(crate) fn mark_window_shown(&mut self) {
        self.shown.get_or_insert_with(Instant::now);
    }

    /// Record the first running adapter and any change of adapter.
    pub(crate) fn observe_sync(&self, manager: &Arc<AccountManager>, runtime: &Handle) {
        let observation = self.sync.clone();
        let mut status = manager.subscribe_sync_status();
        runtime.spawn(async move {
            loop {
                if let SyncStatus::Running { backend } = *status.borrow_and_update() {
                    let mut seen = observation.lock().unwrap_or_else(|e| e.into_inner());
                    seen.first_running.get_or_insert((Instant::now(), backend));
                    if seen.last.is_some_and(|last| last != backend) {
                        seen.backend_changes += 1;
                    }
                    seen.last = Some(backend);
                }
                if status.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// Install the rendering notifier, a watchdog and the driver; the driver
    /// writes the report and quits the event loop.
    pub(crate) fn start(
        self,
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: Handle,
    ) -> Result<Running, Box<dyn Error>> {
        let frames = Rc::new(RefCell::new(Frames::default()));
        let recorder = frames.clone();
        let installed = ui
            .window()
            .set_rendering_notifier(move |state, api| recorder.borrow_mut().observe(state, api));
        if let Err(error) = installed {
            let reason = match error {
                SetRenderingNotifierError::Unsupported => "rendering_notifier_unsupported",
                _ => "rendering_notifier_unavailable",
            };
            let _ = write_report(&self.run.report, &failure(&self.run, "start", reason));
            return Err(format!("Timeline acceptance failed: {reason}").into());
        }
        let limit = match self.run.mode {
            Mode::Enroll => ENROLL_LIMIT,
            Mode::Measure => MEASURE_LIMIT,
        };
        let (done, finished) = mpsc::channel::<()>();
        let report = self.run.report.clone();
        let failed = failure(&self.run, "watchdog", "watchdog");
        thread::Builder::new()
            .name("timeline-acceptance-watchdog".into())
            .spawn(move || {
                if matches!(
                    finished.recv_timeout(limit),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let _ = write_report(&report, &failed);
                    eprintln!("MORPHEUS_TIMELINE_10K_STATUS failed watchdog");
                    std::process::exit(3);
                }
            })?;
        let outcome = Rc::new(Cell::new(None));
        let context = Context {
            ui: ui.clone_strong(),
            live,
            runtime,
            frames,
            stage: Cell::new("start"),
            acceptance: self,
        };
        let result = outcome.clone();
        slint::spawn_local(async move {
            let report = match context.acceptance.run.mode {
                Mode::Enroll => context.enroll().await,
                Mode::Measure => context.measure().await,
            };
            let run = &context.acceptance.run;
            let (value, passed) = match report {
                Ok(value) => (value, true),
                Err(reason) => (failure(run, context.stage.get(), reason), false),
            };
            let written = write_report(&run.report, &value).is_ok();
            let passed = passed && written;
            eprintln!(
                "MORPHEUS_TIMELINE_10K_STATUS {}",
                if passed { "passed" } else { "failed" }
            );
            result.set(Some(passed));
            let _ = slint::quit_event_loop();
        })?;
        Ok(Running { outcome, done })
    }
}

pub(crate) struct Running {
    outcome: Rc<Cell<Option<bool>>>,
    done: mpsc::Sender<()>,
}

impl Running {
    pub(crate) fn finish(self) -> Result<(), Box<dyn Error>> {
        drop(self.done);
        match self.outcome.get() {
            Some(true) => Ok(()),
            _ => Err("Timeline acceptance failed; see its report".into()),
        }
    }
}

fn base(run: &Run, status: &str) -> serde_json::Map<String, Value> {
    let value = json!({
        "check": CHECK,
        "status": status,
        "mode": run.mode.token(),
        "adapter": run.adapter.token(),
        "build": {
            "variant": "instrumented_release",
            "shipping_acceptance": false,
            "delta_from_shipping": DELTA_FROM_SHIPPING,
        },
    });
    match value {
        Value::Object(map) => map,
        _ => unreachable!("report base is an object"),
    }
}

fn failure(run: &Run, stage: Refusal, reason: Refusal) -> Value {
    let mut value = base(run, "failed");
    value.insert("stage".into(), stage.into());
    value.insert("failure".into(), reason.into());
    Value::Object(value)
}

/// A new owner-only file; an existing path is never replaced.
fn write_report(path: &Path, value: &Value) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()
}

struct Context {
    ui: AppWindow,
    live: Arc<LiveUi>,
    runtime: Handle,
    frames: Rc<RefCell<Frames>>,
    stage: Cell<Refusal>,
    acceptance: Acceptance,
}

impl Context {
    fn enter(&self, stage: Refusal) {
        self.stage.set(stage);
    }

    fn since_start(&self, at: Instant) -> u64 {
        millis(at.saturating_duration_since(self.acceptance.started))
    }

    fn rendering(&self) -> Result<(), Refusal> {
        let frames = self.frames.borrow();
        match frames.failure {
            Some(reason) => Err(reason),
            None if frames.lifecycle != Lifecycle::Active => Err("renderer_lifecycle"),
            None => Ok(()),
        }
    }

    async fn first_render(&self) -> Result<Instant, Refusal> {
        self.enter("first_render");
        until(WAIT_LIMIT, POLL, "first_render", || {
            self.frames.borrow().first_after.is_some()
        })
        .await?;
        self.rendering()?;
        self.frames.borrow().first_after.ok_or("first_render")
    }

    async fn rooms_include(&self, room: &str) -> Result<Instant, Refusal> {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            let manager = self.live.manager.clone();
            let rooms = on_runtime(&self.runtime, async move { manager.rooms_snapshot().await })
                .await?
                .map_err(|_| "rooms_unavailable")?;
            if rooms.iter().any(|known| known.room_id == room) {
                return Ok(Instant::now());
            }
            if Instant::now() >= deadline {
                return Err("room_not_synced");
            }
            pause(SLOW_POLL).await;
        }
    }

    /// Sign in generated fixture accounts into the fresh profile: the switch
    /// target first, the measured account last, so startup restores it.
    async fn enroll(&self) -> Result<Value, Refusal> {
        let run = &self.acceptance.run;
        let first = self.first_render().await?;
        self.enter("enroll_profile");
        let manager = self.live.manager.clone();
        let existing = on_runtime(&self.runtime, async move { manager.accounts().await }).await?;
        if !existing.is_empty() {
            return Err("profile_not_fresh");
        }
        let mut measured_device = None;
        for (account, room, stage, measured) in [
            (
                &run.switch,
                &run.switch_enroll_room,
                "enroll_switch_account",
                false,
            ),
            (
                &run.measured,
                &run.measured_enroll_room,
                "enroll_measured_account",
                true,
            ),
        ] {
            self.enter(stage);
            let manager = self.live.manager.clone();
            let (url, user, password) = (
                account.homeserver_url.clone(),
                account.username.clone(),
                account.password.clone(),
            );
            let added = on_runtime(&self.runtime, async move {
                manager.add_password_account(&url, &user, &password).await
            })
            .await?
            .map_err(|_| "login")?;
            if added.user_id != account.user_id {
                return Err("account_mismatch");
            }
            if measured {
                measured_device = Some(added.device_id);
            }
            // Its own room synced, so the client ran a sync. Whether it also
            // published its device keys is checked by the seeder, for exactly
            // this device.
            self.rooms_include(room).await?;
        }
        let backend = self.observed_backend()?;
        self.rendering()?;
        // Only the runner and the seeder read this private file; the report
        // stays closed.
        self.enter("enroll_device_record");
        let device = measured_device
            .filter(|id| plausible_device_id(id))
            .ok_or("device_record")?;
        let path = run.device.as_deref().ok_or("device_record")?;
        write_report(path, &json!({ "device_id": device })).map_err(|_| "device_record")?;
        let mut value = base(run, "passed");
        value.insert("accounts".into(), 2.into());
        value.insert("observed_backend".into(), backend_token(backend).into());
        value.insert(
            "first_render_hook_ms".into(),
            self.since_start(first).into(),
        );
        Ok(Value::Object(value))
    }

    fn observed_backend(&self) -> Result<SyncBackend, Refusal> {
        let seen = self
            .acceptance
            .sync
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_, backend) = seen.first_running.ok_or("sync_not_running")?;
        if !self.acceptance.run.adapter.expects(backend) || seen.backend_changes != 0 {
            return Err("adapter_mismatch");
        }
        if seen.last != Some(backend) {
            return Err("adapter_mismatch");
        }
        Ok(backend)
    }

    fn account_index(&self, local_id: &str) -> Result<i32, Refusal> {
        let options = self.ui.get_account_options();
        (0..options.row_count())
            .filter_map(|row| options.row_data(row))
            .find(|option| option.local_id.as_str() == local_id)
            .map(|option| option.index)
            .ok_or("account_option_missing")
    }

    fn active(&self, local_id: &str) -> bool {
        // Observe the actual enabled switch control, not only cached profile
        // publication while the previous activation still owns its guards.
        self.ui.get_live_session()
            && self.ui.get_active_account_local_id().as_str() == local_id
            && !self.ui.get_account_switch_pending()
    }

    fn showing(&self, room: &str) -> bool {
        self.ui.get_active_room_id().as_str() == room && self.ui.get_messages().row_count() > 0
    }

    fn rss_checkpoint(&self) -> Value {
        match rss_kib() {
            Some((rss, _)) => rss.into(),
            None => Value::Null,
        }
    }

    async fn open_room(&self, room: &str) -> Result<(u64, usize), Refusal> {
        let sent = Instant::now();
        self.ui.invoke_open_room(room.into());
        let shown = until(WAIT_LIMIT, POLL, "room_not_shown", || self.showing(room)).await?;
        Ok((millis(shown - sent), self.ui.get_messages().row_count()))
    }

    async fn measure(&self) -> Result<Value, Refusal> {
        let run = &self.acceptance.run;
        let room = run.room.clone().ok_or("room_file_mode")?;
        let first = self.first_render().await?;

        // Startup: the production restore of the last active account.
        self.enter("restore");
        let manager = self.live.manager.clone();
        let accounts = on_runtime(&self.runtime, async move { manager.accounts().await }).await?;
        let local_id = |user_id: &str| {
            accounts
                .iter()
                .find(|account| account.user_id == user_id)
                .map(|account| account.id.clone())
                .ok_or("profile_not_enrolled")
        };
        let (measured, switch) = (
            local_id(&run.measured.user_id)?,
            local_id(&run.switch.user_id)?,
        );
        let restored = until(WAIT_LIMIT, POLL, "restore", || self.active(&measured)).await?;
        self.enter("sync");
        until(WAIT_LIMIT, POLL, "sync_not_running", || {
            self.acceptance
                .sync
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_running
                .is_some()
        })
        .await?;
        let backend = self.observed_backend()?;
        let sync_running = self
            .acceptance
            .sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_running
            .map(|(at, _)| at)
            .ok_or("sync_not_running")?;
        self.enter("room_listed");
        let listed = self.rooms_include(&room).await?;
        let after_startup = self.rss_checkpoint();

        // The production open-room callback, then the production
        // load-older-messages callback until the start of history.
        self.enter("open_room");
        let (open_ms, initial_rows) = self.open_room(&room).await?;
        self.enter("pagination");
        let frames_before = self.frames.borrow().completed;
        let history_started = Instant::now();
        let mut requests = Samples::default();
        let mut pages = 0;
        while !self.ui.get_history_at_start() {
            if pages == MAX_PAGES {
                return Err("pagination_bound");
            }
            pages += 1;
            let operation = self.ui.get_history_operation();
            let sent = Instant::now();
            self.ui.invoke_load_older_messages();
            if self.ui.get_history_operation() == operation {
                return Err("pagination_not_started");
            }
            let done = until(WAIT_LIMIT, POLL, "pagination_timeout", || {
                !self.ui.get_history_loading()
            })
            .await?;
            if !self.ui.get_history_error().is_empty() {
                return Err("pagination_error");
            }
            requests.push(millis(done - sent));
        }
        // The last page's rows may publish after its loading flag clears, and
        // late room keys can still decrypt events: count until complete.
        self.enter("history_complete");
        let mut counts = (0, 0);
        let complete = until(WAIT_LIMIT, SLOW_POLL, "history_incomplete", || {
            let messages = self.ui.get_messages();
            let bodies: Vec<_> = (0..messages.row_count())
                .filter_map(|row| messages.row_data(row))
                .map(|item| item.body)
                .collect();
            counts = fixture_rows(bodies.iter().map(|body| body.as_str()));
            counts.1 == EVENTS
        })
        .await?;
        let model_rows = self.ui.get_messages().row_count();
        let pagination_frames = self.frames.borrow().completed - frames_before;
        let after_history = self.rss_checkpoint();
        self.rendering()?;

        // In-process wheel events over the timeline at the window's real size.
        self.enter("scroll");
        if self.ui.get_page() != 0 {
            return Err("timeline_not_visible");
        }
        let window = self.ui.window();
        let size = window.size();
        let scale = window.scale_factor();
        let position = slint::LogicalPosition::new(
            size.width as f32 / scale * 0.62,
            size.height as f32 / scale * 0.55,
        );
        window
            .dispatch_event_with_result(slint::platform::WindowEvent::PointerMoved { position })
            .map_err(|_| "input_dispatch")?;
        let idle_before = self.frames.borrow().completed;
        pause(IDLE_WINDOW).await;
        let idle_frames = self.frames.borrow().completed - idle_before;
        self.frames.borrow_mut().record(true);
        let mut dispatch_to_hook = Samples::default();
        let (mut accepted_steps, mut rendered_steps) = (0, 0);
        for delta in [SCROLL_DELTA, -SCROLL_DELTA] {
            for _ in 0..SCROLL_STEPS {
                self.frames.borrow_mut().next_after = None;
                let sent = Instant::now();
                let dispatched = window
                    .dispatch_event_with_result(slint::platform::WindowEvent::PointerScrolled {
                        position,
                        delta_x: 0.0,
                        delta_y: delta,
                    })
                    .map_err(|_| "input_dispatch")?;
                // Accepted: an element, such as the timeline list, consumed it.
                if matches!(
                    dispatched,
                    slint::platform::WindowEventDispatchResult::Accepted
                ) {
                    accepted_steps += 1;
                }
                let seen = until(SCROLL_RENDER_LIMIT, Duration::from_millis(1), "", || {
                    self.frames.borrow().next_after.is_some()
                })
                .await;
                if seen.is_ok()
                    && let Some(after) = self.frames.borrow().next_after
                {
                    rendered_steps += 1;
                    dispatch_to_hook.push(micros(after.saturating_duration_since(sent)));
                }
            }
        }
        self.frames.borrow_mut().record(false);
        if accepted_steps == 0 || rendered_steps == 0 {
            return Err("scroll_not_rendered");
        }
        let after_scroll = self.rss_checkpoint();
        self.rendering()?;

        // The production switch-account callback, away and back, then reopen.
        self.enter("switch_away");
        let sent = Instant::now();
        self.ui.invoke_switch_account(self.account_index(&switch)?);
        let away = until(WAIT_LIMIT, POLL, "switch_away", || self.active(&switch)).await?;
        self.enter("switch_back");
        let back_sent = Instant::now();
        self.ui
            .invoke_switch_account(self.account_index(&measured)?);
        let back = until(WAIT_LIMIT, POLL, "switch_back", || self.active(&measured)).await?;
        self.enter("reopen_room");
        let (reopen_ms, reopen_rows) = self.open_room(&room).await?;
        let after_switch = self.rss_checkpoint();
        self.enter("complete");
        self.observed_backend()?;
        self.rendering()?;

        let frames = self.frames.borrow();
        let peak = rss_kib().map(|(_, peak)| peak);
        let rss_source = if peak.is_some() {
            "proc_self_status"
        } else {
            "unavailable"
        };
        let mut value = base(run, "passed");
        let shown = self.acceptance.shown.map(|at| self.since_start(at));
        let fields = json!({
            "renderer": {
                "graphics_api": frames.api,
                "hook": "slint_rendering_notifier",
                "frames_completed": frames.completed,
            },
            "window": {
                "physical_width": size.width,
                "physical_height": size.height,
                "scale_factor_milli": (scale * 1000.0).round() as u64,
            },
            "sync": {"observed_backend": backend_token(backend)},
            "startup_ms": {
                "window_shown": shown,
                "first_render_hook": self.since_start(first),
                "sync_running": self.since_start(sync_running),
                "restored_observed": self.since_start(restored),
                "room_listed_observed": self.since_start(listed),
            },
            "room_open": {"first_rows_observed_ms": open_ms, "initial_rows": initial_rows},
            "pagination": {
                "requests": pages,
                "request_until_loading_cleared_ms": requests.summary(),
                "full_history_observed_ms": millis(complete - history_started),
                "render_hook_frames": pagination_frames,
            },
            "history": {
                "expected_events": EVENTS,
                "model_rows": model_rows,
                "fixture_rows": counts.0,
                "fixture_distinct_decrypted": counts.1,
                "count_method": "direct_window_model",
            },
            "scroll": {
                "steps": 2 * SCROLL_STEPS,
                "delta_logical_px": SCROLL_DELTA,
                "steps_accepted": accepted_steps,
                "steps_with_render_hook": rendered_steps,
                "idle_render_hook_frames_before": idle_frames,
                "dispatch_to_render_hook_us": dispatch_to_hook.summary(),
                "render_hook_span_us": frames.spans_us.summary(),
                "render_hook_interval_us": frames.intervals_us.summary(),
            },
            "account_switch": {
                "away_observed_ms": millis(away - sent),
                "back_observed_ms": millis(back - back_sent),
                "reopen_first_rows_observed_ms": reopen_ms,
                "reopen_initial_rows": reopen_rows,
            },
            "rss_kib": {
                "source": rss_source,
                "after_startup": after_startup,
                "after_history": after_history,
                "after_scroll": after_scroll,
                "after_switch": after_switch,
                "peak": peak,
            },
            "delegates": {"instances": Value::Null, "method": "unmeasured"},
            "poll_ms": millis(POLL),
            "not_measured": NOT_MEASURED,
        });
        if let Value::Object(fields) = fields {
            value.extend(fields);
        }
        Ok(Value::Object(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosted(extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mut values: HashMap<String, String> = [
            ("GITHUB_ACTIONS", "true"),
            ("RUNNER_ENVIRONMENT", "github-hosted"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1001/bus"),
        ]
        .iter()
        .chain(extra)
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        values.retain(|_, value| !value.is_empty());
        move |name: &str| values.get(name).cloned()
    }

    #[test]
    fn only_a_fresh_hosted_runner_may_run_it() {
        assert_eq!(hosted_refusal(hosted(&[])), None);
        assert_eq!(
            hosted_refusal(hosted(&[("GITHUB_ACTIONS", "")])),
            Some("not_hosted")
        );
        assert_eq!(
            hosted_refusal(hosted(&[("RUNNER_ENVIRONMENT", "self-hosted")])),
            Some("not_hosted")
        );
        assert_eq!(
            hosted_refusal(hosted(&[("APPIMAGE", "/tmp/Morpheus.AppImage")])),
            Some("packaged")
        );
        let no_bus = hosted_refusal(hosted(&[("DBUS_SESSION_BUS_ADDRESS", "")]));
        if cfg!(target_os = "linux") {
            assert_eq!(no_bus, Some("no_session_bus"));
        } else {
            assert_eq!(no_bus, None);
        }
    }

    #[test]
    fn only_loopback_fixture_homeservers_are_accepted() {
        for accepted in [
            "http://127.0.0.1:18008",
            "http://127.0.0.1:18009/",
            "http://localhost:18008",
            "http://[::1]:18008",
        ] {
            assert!(loopback_homeserver(accepted), "{accepted}");
        }
        for refused in [
            "https://127.0.0.1:18008",
            "http://127.0.0.1",
            "http://10.0.0.5:18008",
            "http://matrix.example.org:8008",
            "http://user:pass@127.0.0.1:18008",
            "http://127.0.0.1:18008/path",
            "http://127.0.0.1:18008/?q=1",
            "not a url",
        ] {
            assert!(!loopback_homeserver(refused), "{refused}");
        }
    }

    #[test]
    fn only_generated_test_users_are_accepted() {
        assert!(synthetic_user("@bob:morpheus-a.test"));
        assert!(synthetic_user("@carol:morpheus-b.test"));
        for refused in [
            "@bob:matrix.org",
            "@bob:test",
            "bob:morpheus-a.test",
            "@:morpheus-a.test",
            "@Bob:morpheus-a.test",
            "@bob:morpheus a.test",
        ] {
            assert!(!synthetic_user(refused), "{refused}");
        }
    }

    #[test]
    fn markers_are_counted_directly_and_distinctly() {
        assert_eq!(marker_index("tl10k-00001 short update."), Some(1));
        assert_eq!(marker_index("> tl10k-10000 quoted line"), Some(10_000));
        assert_eq!(marker_index("tl10k-00000"), None);
        assert_eq!(marker_index("tl10k-10001"), None);
        assert_eq!(marker_index("tl10k-12ab3"), None);
        assert_eq!(marker_index("tl10k-123"), None);
        assert_eq!(marker_index("ordinary message"), None);
        let bodies = [
            "tl10k-00001 a",
            "tl10k-00002 b",
            "tl10k-00001 again",
            "other",
        ];
        assert_eq!(fixture_rows(bodies.into_iter()), (3, 2));
    }

    #[test]
    fn proc_status_rss_is_parsed_or_absent() {
        let status = "Name:\tmorpheus-app\nVmHWM:\t  204800 kB\nVmRSS:\t  153600 kB\n";
        assert_eq!(proc_rss_kib(status), Some((153_600, 204_800)));
        assert_eq!(proc_rss_kib("VmRSS:\t1 kB\n"), None);
        assert_eq!(proc_rss_kib("VmRSS:\tlots kB\nVmHWM:\t2 kB\n"), None);
    }

    #[test]
    fn percentiles_use_nearest_rank_within_a_bound() {
        let mut samples = Samples::default();
        assert_eq!(samples.summary(), Value::Null);
        for value in (1..=100).rev() {
            samples.push(value);
        }
        let summary = samples.summary();
        assert_eq!(summary["count"], 100);
        assert_eq!(summary["p50"], 50);
        assert_eq!(summary["p95"], 95);
        assert_eq!(summary["max"], 100);
        for _ in 0..SAMPLE_LIMIT {
            samples.push(1);
        }
        assert_eq!(samples.values.len(), SAMPLE_LIMIT);
        assert_eq!(samples.dropped, 100);
    }

    #[test]
    fn rendering_records_must_pair_inside_one_lifecycle() {
        let opengl = GraphicsAPI::NativeOpenGL {
            get_proc_address: &|_: &std::ffi::CStr| std::ptr::null(),
        };
        let mut frames = Frames::default();
        frames.observe(RenderingState::BeforeRendering, &opengl);
        assert_eq!(frames.failure, Some("frame_outside_lifecycle"));

        let mut frames = Frames::default();
        frames.observe(RenderingState::RenderingSetup, &opengl);
        frames.record(true);
        for _ in 0..3 {
            frames.observe(RenderingState::BeforeRendering, &opengl);
            frames.observe(RenderingState::AfterRendering, &opengl);
        }
        assert_eq!(frames.failure, None);
        assert_eq!(frames.completed, 3);
        assert_eq!(frames.spans_us.values.len(), 3);
        assert_eq!(frames.intervals_us.values.len(), 2);
        assert_eq!(frames.api, Some("native_opengl"));
        frames.observe(RenderingState::AfterRendering, &opengl);
        assert_eq!(frames.failure, Some("unpaired_frame"));

        let mut frames = Frames::default();
        frames.observe(RenderingState::RenderingSetup, &opengl);
        frames.observe(RenderingState::RenderingTeardown, &opengl);
        frames.observe(RenderingState::RenderingSetup, &opengl);
        assert_eq!(frames.failure, Some("renderer_lifecycle"));
    }

    struct Files {
        directory: tempfile::TempDir,
    }

    impl Files {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            for name in ["profile", "out"] {
                let path = directory.path().join(name);
                fs::create_dir(&path).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
            let files = Self { directory };
            files.write("fixture.json", &fixture_json("http://127.0.0.1:18008"));
            files.write(
                "room.json",
                r#"{"room_id": "!fixture:morpheus-a.test", "events": 10000}"#,
            );
            files
        }

        fn path(&self, name: &str) -> PathBuf {
            self.directory.path().join(name)
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.path(name), text).unwrap();
        }

        fn run(&self, mode: &str, room: Option<&str>) -> RunFile {
            RunFile {
                mode: mode.into(),
                adapter: "sliding".into(),
                profile: self.path("profile"),
                fixture: self.path("fixture.json"),
                room: room.map(|name| self.path(name)),
                report: self.path("out").join("report.json"),
                device: (mode == "enroll").then(|| self.path("out").join("device.json")),
            }
        }
    }

    fn fixture_json(homeserver: &str) -> String {
        json!({
            "accounts": [
                {"label": "a_second_device", "user_id": "@bob:morpheus-a.test", "username": "bob",
                 "homeserver_url": homeserver, "password": "generated-password-0123456789"},
                {"label": "b_primary", "user_id": "@carol:morpheus-b.test", "username": "carol",
                 "homeserver_url": "http://127.0.0.1:18009", "password": "generated-password-9876543210"},
            ],
            "rooms": {"encrypted_room": "!enc:morpheus-a.test", "b_room": "!b:morpheus-b.test"},
        })
        .to_string()
    }

    // The test binary's checkout stands in for the one a release build checks.
    fn validate(file: RunFile) -> Result<Run, Refusal> {
        Run::from_file(file, &checkout())
    }

    #[test]
    fn enrollment_needs_a_fresh_private_profile_and_no_room() {
        let files = Files::new();
        let run = validate(files.run("enroll", None)).unwrap();
        assert_eq!((run.mode, run.adapter), (Mode::Enroll, Adapter::Sliding));
        assert_eq!(run.measured.user_id, "@bob:morpheus-a.test");
        assert_eq!(run.switch.user_id, "@carol:morpheus-b.test");
        assert_eq!(
            validate(files.run("enroll", Some("room.json"))).err(),
            Some("room_file_mode")
        );
        files.write("profile/leftover", "x");
        assert_eq!(
            validate(files.run("enroll", None)).err(),
            Some("profile_not_fresh")
        );
    }

    #[test]
    fn measurement_needs_an_enrolled_profile_and_the_full_room() {
        let files = Files::new();
        assert_eq!(
            validate(files.run("measure", Some("room.json"))).err(),
            Some("profile_not_enrolled")
        );
        fs::create_dir(files.path("profile/data")).unwrap();
        let run = validate(files.run("measure", Some("room.json"))).unwrap();
        assert_eq!(run.room.as_deref(), Some("!fixture:morpheus-a.test"));
        assert_eq!(
            validate(files.run("measure", None)).err(),
            Some("room_file_mode")
        );
        files.write(
            "room.json",
            r#"{"room_id": "!fixture:morpheus-a.test", "events": 9999}"#,
        );
        assert_eq!(
            validate(files.run("measure", Some("room.json"))).err(),
            Some("room_file_invalid")
        );
    }

    #[test]
    fn run_data_must_be_private_external_and_new() {
        let files = Files::new();
        let mut run = files.run("enroll", None);
        run.fixture = checkout().join("Cargo.toml");
        assert_eq!(validate(run).err(), Some("path_in_checkout"));
        let mut run = files.run("enroll", None);
        run.profile = PathBuf::from("relative/profile");
        assert_eq!(validate(run).err(), Some("path_not_absolute"));
        files.write("out/report.json", "{}");
        assert_eq!(
            validate(files.run("enroll", None)).err(),
            Some("report_path_invalid")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let files = Files::new();
            fs::set_permissions(files.path("profile"), fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                validate(files.run("enroll", None)).err(),
                Some("profile_not_private")
            );
        }
        let files = Files::new();
        files.write(
            "fixture.json",
            &fixture_json("http://matrix.example.org:8008"),
        );
        assert_eq!(
            validate(files.run("enroll", None)).err(),
            Some("homeserver_not_loopback")
        );
        let mut run = files.run("enroll", None);
        run.adapter = "auto".into();
        assert_eq!(validate(run).err(), Some("adapter_invalid"));
    }

    #[test]
    fn only_enrollment_records_its_device_in_a_new_private_file() {
        let files = Files::new();
        let run = validate(files.run("enroll", None)).unwrap();
        assert_eq!(
            run.device,
            Some(
                files
                    .path("out")
                    .canonicalize()
                    .unwrap()
                    .join("device.json")
            )
        );
        let mut without = files.run("enroll", None);
        without.device = None;
        assert_eq!(validate(without).err(), Some("device_file_mode"));
        let mut same = files.run("enroll", None);
        same.device = Some(same.report.clone());
        assert_eq!(validate(same).err(), Some("device_path_invalid"));
        files.write("out/device.json", "{}");
        assert_eq!(
            validate(files.run("enroll", None)).err(),
            Some("device_path_invalid")
        );
        fs::create_dir(files.path("profile/data")).unwrap();
        let mut measure = files.run("measure", Some("room.json"));
        measure.device = Some(files.path("out").join("other.json"));
        assert_eq!(validate(measure).err(), Some("device_file_mode"));
        assert!(plausible_device_id("ABCDEFGHIJ"));
        let long = "A".repeat(65);
        for id in ["", "two words", "quote\"d", long.as_str()] {
            assert!(!plausible_device_id(id), "{id}");
        }
    }

    #[test]
    fn unknown_run_file_fields_are_refused() {
        let parsed: Result<RunFile, _> = serde_json::from_str(
            r#"{"mode": "enroll", "adapter": "sliding", "profile": "/p", "fixture": "/f",
                "report": "/r", "renderer": "software"}"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn adapters_map_to_the_shipping_policy_and_its_classic_fallback() {
        assert_eq!(Adapter::Sliding.policy(), SyncPolicy::Auto);
        assert_eq!(Adapter::Classic.policy(), SyncPolicy::ClassicOnly);
        assert!(Adapter::Sliding.expects(SyncBackend::SlidingSticky));
        assert!(!Adapter::Sliding.expects(SyncBackend::Classic));
        assert!(Adapter::Classic.expects(SyncBackend::Classic));
        assert!(!Adapter::Classic.expects(SyncBackend::SlidingSticky));
    }

    #[test]
    fn reports_are_new_owner_only_files_without_private_values() {
        let files = Files::new();
        let run = validate(files.run("enroll", None)).unwrap();
        let value = failure(&run, "start", "rendering_notifier_unsupported");
        write_report(&run.report, &value).unwrap();
        assert!(write_report(&run.report, &value).is_err());
        let text = fs::read_to_string(&run.report).unwrap();
        for private in [
            "bob",
            "carol",
            "generated-password",
            "morpheus-a.test",
            "!enc",
        ] {
            assert!(!text.contains(private), "{private}");
        }
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["build"]["shipping_acceptance"], false);
        assert_eq!(parsed["status"], "failed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&run.report).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
