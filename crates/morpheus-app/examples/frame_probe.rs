//! Linux/Xvfb capability probe for real FemtoVG OpenGL rendering callbacks.
//!
//! Opens one small animated Slint window through the winit backend with the
//! FemtoVG renderer, requires Slint's `NativeOpenGL` graphics API and matched
//! `BeforeRendering`/`AfterRendering` pairs, then closes the window. The
//! software renderer has no rendering notifier and is never accepted. See
//! docs/FRAME_TIMING_PROBE.md: the numbers are CPU render-callback timings
//! before presentation, not display FPS, GPU time or timeline performance.
//!
//! Reads only environment variables: no files, network, accounts, profiles or
//! credential stores. Prints one JSON line of fixed tokens and numbers, never
//! backend error text.

use std::{
    cell::RefCell,
    process::ExitCode,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use slint::{ComponentHandle, GraphicsAPI, RenderingState, SetRenderingNotifierError};

const PROBE: &str = "morpheus_frame_timing";
const BACKEND: &str = "winit-femtovg";
/// Completed frames before measuring; the first ones include context setup.
const WARMUP_FRAMES: usize = 10;
/// Stored samples never exceed this bound.
const SAMPLES: usize = 120;
const CHECK_INTERVAL: Duration = Duration::from_millis(50);
/// The event loop closes the window itself if samples are incomplete by then.
const SOFT_DEADLINE: Duration = Duration::from_secs(10);
/// Independent of the event loop: ends the process if rendering or close hangs.
const WATCHDOG: Duration = Duration::from_secs(20);

slint::slint! {
    export component ProbeWindow inherits Window {
        title: "Morpheus frame timing probe";
        width: 320px;
        height: 200px;
        background: darkslategray;

        // animation-tick() keeps requesting frames while the window is shown.
        Rectangle {
            width: 64px;
            height: 64px;
            x: (parent.width - self.width) / 2 + 96px * Math.sin(animation-tick() / 1s * 360deg);
            y: (parent.height - self.height) / 2;
            border-radius: 12px;
            background: lightskyblue;
        }
    }
}

/// Bounded frame records. Only the first failure is kept.
#[derive(Default)]
struct Frames {
    setup: bool,
    pending: Option<Instant>,
    last_start: Option<Instant>,
    completed: usize,
    render_us: Vec<u64>,
    interval_us: Vec<u64>,
    teardown: bool,
    failure: Option<&'static str>,
}

impl Frames {
    fn fail(&mut self, reason: &'static str) {
        self.failure.get_or_insert(reason);
    }

    fn complete(&self) -> bool {
        self.render_us.len() == SAMPLES && self.pending.is_none()
    }

    fn observe(&mut self, state: RenderingState, api: &GraphicsAPI) {
        if self.failure.is_some() {
            return;
        }
        let now = Instant::now();
        match state {
            RenderingState::RenderingSetup if self.setup => self.fail("setup_repeated"),
            RenderingState::RenderingSetup if native_opengl(api) => self.setup = true,
            RenderingState::RenderingSetup => self.fail("graphics_api_not_native_opengl"),
            RenderingState::BeforeRendering => {
                if !self.setup || !matches!(api, GraphicsAPI::NativeOpenGL { .. }) {
                    self.fail("graphics_api_not_native_opengl");
                } else if self.pending.replace(now).is_some() {
                    self.fail("unpaired_frame");
                } else {
                    // Only between two measured frames, so at most SAMPLES - 1.
                    if self.completed > WARMUP_FRAMES
                        && self.render_us.len() < SAMPLES
                        && let Some(previous) = self.last_start
                    {
                        self.interval_us.push(micros(now - previous));
                    }
                    self.last_start = Some(now);
                }
            }
            RenderingState::AfterRendering => match self.pending.take() {
                Some(start) => {
                    self.completed += 1;
                    if self.completed > WARMUP_FRAMES && self.render_us.len() < SAMPLES {
                        self.render_us.push(micros(now - start));
                    }
                }
                None => self.fail("unpaired_frame"),
            },
            RenderingState::RenderingTeardown => self.teardown = true,
            _ => self.fail("unknown_rendering_state"),
        }
    }

    fn report(&self) -> Result<serde_json::Value, &'static str> {
        if let Some(reason) = self.failure {
            return Err(reason);
        }
        if !self.complete() {
            return Err("insufficient_frames");
        }
        Ok(serde_json::json!({
            "probe": PROBE,
            "result": "pass",
            "backend": BACKEND,
            "graphics_api": "native_opengl",
            "software_gl_requested": true,
            "measure": "cpu_render_callback_before_presentation",
            "warmup_frames": WARMUP_FRAMES,
            "samples": SAMPLES,
            "render_callback_us": summary(&self.render_us),
            "frame_start_interval_us": summary(&self.interval_us),
            "teardown_observed": self.teardown,
        }))
    }
}

/// Slint reports an OpenGL context that resolves a core entry point. The
/// returned pointer is only checked for null, never called.
fn native_opengl(api: &GraphicsAPI) -> bool {
    match api {
        GraphicsAPI::NativeOpenGL { get_proc_address } => {
            !get_proc_address(c"glGetString").is_null()
        }
        _ => false,
    }
}

fn micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

/// Nearest-rank percentiles of a complete, therefore non-empty, sample.
fn summary(values: &[u64]) -> serde_json::Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = |percent: usize| sorted[(sorted.len() * percent).div_ceil(100) - 1];
    serde_json::json!({ "p50": rank(50), "p95": rank(95), "max": rank(100) })
}

fn check_environment() -> Result<(), &'static str> {
    if !cfg!(target_os = "linux") {
        return Err("unsupported_platform");
    }
    if std::env::var("SLINT_BACKEND").as_deref() != Ok(BACKEND) {
        return Err("backend_not_winit_femtovg");
    }
    // Mesa's documented switch; this records intent, it cannot prove the driver.
    if std::env::var("LIBGL_ALWAYS_SOFTWARE").as_deref() != Ok("1") {
        return Err("software_gl_not_requested");
    }
    if std::env::var_os("DISPLAY").is_none() {
        return Err("x11_display_missing");
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Err("wayland_display_present");
    }
    Ok(())
}

fn probe() -> Result<serde_json::Value, &'static str> {
    // Naming both backend and renderer means SLINT_BACKEND is not re-read.
    slint::BackendSelector::new()
        .backend_name(BACKEND.into())
        .select()
        .map_err(|_| "backend_unavailable")?;
    let window = ProbeWindow::new().map_err(|_| "window_unavailable")?;
    let frames = Rc::new(RefCell::new(Frames::default()));
    let recorder = frames.clone();
    window
        .window()
        .set_rendering_notifier(move |state, api| recorder.borrow_mut().observe(state, api))
        .map_err(|error| match error {
            // A fallback to the software renderer ends here, before any frame.
            SetRenderingNotifierError::Unsupported => "renderer_not_opengl",
            _ => "notifier_unavailable",
        })?;

    let monitor = slint::Timer::default();
    let watched = frames.clone();
    let weak = window.as_weak();
    let started = Instant::now();
    let mut closing = false;
    monitor.start(slint::TimerMode::Repeated, CHECK_INTERVAL, move || {
        let ready = {
            let frames = watched.borrow();
            frames.failure.is_some() || frames.complete()
        };
        if closing || (!ready && started.elapsed() < SOFT_DEADLINE) {
            return;
        }
        closing = true;
        if !ready {
            watched.borrow_mut().fail("insufficient_frames");
        }
        // No frame borrow is held here: closing may notify RenderingTeardown.
        if weak.upgrade().is_none_or(|window| window.hide().is_err()) {
            watched.borrow_mut().fail("close_failed");
        }
        let _ = slint::quit_event_loop();
    });
    window.show().map_err(|_| "window_show_failed")?;
    slint::run_event_loop().map_err(|_| "event_loop_failed")?;
    drop(monitor);
    drop(window);
    frames.borrow().report()
}

fn print_error(reason: &'static str) {
    println!("{}", serde_json::json!({ "probe": PROBE, "result": "error", "reason": reason }));
}

fn fail(reason: &'static str) -> ExitCode {
    print_error(reason);
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    // Panic text can carry backend details; report one fixed reason instead.
    std::panic::set_hook(Box::new(|_| {}));
    if let Err(reason) = check_environment() {
        return fail(reason);
    }
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if matches!(finished.recv_timeout(WATCHDOG), Err(mpsc::RecvTimeoutError::Timeout)) {
            print_error("watchdog_expired");
            std::process::exit(3);
        }
    });
    let outcome = std::panic::catch_unwind(probe).unwrap_or(Err("probe_panicked"));
    // Disconnecting ends the watchdog's wait at once.
    drop(done);
    let _ = watchdog.join();
    match outcome {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(reason) => fail(reason),
    }
}
