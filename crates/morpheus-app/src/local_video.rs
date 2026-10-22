//! Outgoing native video for one joined call. OS capture never owns an RTC
//! publication: the call worker can mute and unpublish a track even if a
//! camera or screen capture call is stuck inside its platform backend.

use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use morpheus_rtc::{CameraCapture, MediaError, Publication, RtcService, ScreenCapture, VideoFrame};
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    time::{self, MissedTickBehavior, timeout},
};

const CAPTURE_QUEUE: usize = 3;
const START_TIMEOUT: Duration = Duration::from_secs(20);
const SELECT_TIMEOUT: Duration = Duration::from_secs(180);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(12);
const UNPUBLISH_TIMEOUT: Duration = Duration::from_secs(5);
const USER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const CAMERA_INTERVAL: Duration = Duration::from_millis(67);
const SCREEN_INTERVAL: Duration = Duration::from_millis(125);
const SCREEN_WIDTH: u32 = 1280;
const SCREEN_HEIGHT: u32 = 720;
const RECONNECTING_START: &str = "the call is reconnecting; start video once it has rejoined";

#[derive(Default)]
pub(crate) struct CaptureStop {
    requested: AtomicBool,
    notify: Notify,
    // One serialization boundary for permanent stop, temporary pauses and
    // every outgoing frame push. Never held across an await.
    push_gate: StdMutex<TransmissionPause>,
    authorization: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Read at every push under `push_gate`. A false result withholds the
    /// frame only; unlike `authorization`, it never latches a stop.
    transmission: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
}

/// A temporary transmission pause, e.g. while signalling reconnects or the
/// call rejoins. Every pause bumps the epoch, so only the newest pause's
/// token can resume; a stale resume never re-enables sending.
#[derive(Default)]
struct TransmissionPause {
    epoch: u64,
    paused: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransmissionPauseToken(u64);

impl CaptureStop {
    pub(crate) fn request(&self) {
        let _gate = self
            .push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// Bind once, after an incoming Answer commits and before any media starts.
    /// A failed/repeated binding is terminal, never a replacement authorization.
    pub(crate) fn bind_authorization(
        &self,
        allowed: impl Fn() -> bool + Send + Sync + 'static,
    ) -> bool {
        let _gate = self
            .push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.requested.load(Ordering::SeqCst)
            || self.authorization.set(Box::new(allowed)).is_err()
        {
            self.requested.store(true, Ordering::SeqCst);
            self.notify.notify_one();
            return false;
        }
        !self.requested()
    }

    pub(crate) fn requested(&self) -> bool {
        if self.requested.load(Ordering::SeqCst) {
            return true;
        }
        if self.authorization.get().is_some_and(|allowed| !allowed()) {
            // This check is also called under push_gate: latch directly rather
            // than recursively locking request(). No later false result can
            // revive this call, even if a supplied predicate changes again.
            self.requested.store(true, Ordering::SeqCst);
            self.notify.notify_one();
            return true;
        }
        false
    }

    async fn cancelled(&self) {
        if self.requested() {
            return;
        }
        self.notify.notified().await;
    }

    /// Bind the call's temporary transmission gate once, before media starts.
    /// It must be cheap and perform no I/O; it runs under the push lock.
    pub(crate) fn bind_transmission_gate(
        &self,
        allowed: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        let _ = self.transmission.set(Box::new(allowed));
    }

    fn transmission_allowed(&self) -> bool {
        self.transmission.get().is_none_or(|allowed| allowed())
    }

    /// Withhold frames until the returned token resumes. `mute` runs under
    /// the push lock, so no frame is pushed between the pause and the mute.
    pub(crate) fn pause_transmission(&self, mute: impl FnOnce()) -> TransmissionPauseToken {
        let mut gate = self
            .push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        gate.epoch = gate.epoch.wrapping_add(1);
        gate.paused = true;
        mute();
        TransmissionPauseToken(gate.epoch)
    }

    /// End the pause `token` started unless a newer pause or a permanent
    /// stop followed it. `unmute` runs under the push lock only on resume.
    pub(crate) fn resume_transmission(
        &self,
        token: TransmissionPauseToken,
        unmute: impl FnOnce(),
    ) -> bool {
        let mut gate = self
            .push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !gate.paused || gate.epoch != token.0 || self.requested() {
            return false;
        }
        gate.paused = false;
        unmute();
        true
    }

    fn transmission_paused(&self) -> bool {
        self.push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .paused
    }

    /// Mute a track that was published while a pause began, under the same
    /// lock, so it cannot send before the matching resume unmutes it.
    fn mute_if_paused(&self, mute: impl FnOnce()) -> bool {
        let gate = self
            .push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if gate.paused {
            mute();
        }
        gate.paused
    }

    fn push_if_active<T>(&self, push: impl FnOnce() -> T) -> Option<T> {
        let gate = self
            .push_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if gate.paused || self.requested() || !self.transmission_allowed() {
            None
        } else {
            Some(push())
        }
    }
}

struct CaptureWorkerGuard(Arc<AtomicUsize>);

impl Drop for CaptureWorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureKind {
    Camera,
    Screen,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CaptureSource {
    Camera(String),
    Display(String),
    Window(String),
}

impl CaptureSource {
    fn needs_system_selection(&self) -> bool {
        matches!(self, Self::Display(key) | Self::Window(key) if key.starts_with("portal:"))
    }

    pub(crate) fn kind(&self) -> CaptureKind {
        match self {
            Self::Camera(_) => CaptureKind::Camera,
            Self::Display(_) | Self::Window(_) => CaptureKind::Screen,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum CapturePhase {
    #[default]
    Off,
    Selecting,
    Starting,
    Stopping,
    Active,
    Failed(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CaptureState {
    pub(crate) phase: CapturePhase,
    pub(crate) source: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LocalVideoStatus {
    pub(crate) token: u64,
    pub(crate) camera: CaptureState,
    pub(crate) screen: CaptureState,
}

pub(crate) enum CaptureCommand {
    Start {
        source: CaptureSource,
        label: String,
        reply: oneshot::Sender<Result<(), MediaError>>,
    },
    Stop {
        kind: CaptureKind,
        reply: oneshot::Sender<Result<(), MediaError>>,
    },
}

pub(crate) enum Captured {
    SourceReady {
        kind: CaptureKind,
        generation: u64,
    },
    Frame {
        kind: CaptureKind,
        generation: u64,
        frame: VideoFrame,
    },
    Failed {
        kind: CaptureKind,
        generation: u64,
        error: MediaError,
    },
}

pub(crate) enum LocalVideoEvent {
    Command(CaptureCommand),
    Frame(Captured),
    Tick,
}

#[derive(Default)]
struct CaptureSlot {
    generation: u64,
    stop: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
    publication: Option<Publication>,
    started: Option<Instant>,
}

impl CaptureSlot {
    /// The publication of a capture the user has not stopped.
    fn live_publication(&self) -> Option<&Publication> {
        if self.stop.is_some() {
            self.publication.as_ref()
        } else {
            None
        }
    }

    fn stop_capture(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(publication) = &self.publication {
            let _ = publication.set_muted(true);
        }
        self.started = None;
    }

    fn reap_finished_worker(&mut self) -> Result<(), MediaError> {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if self
                .worker
                .take()
                .expect("finished worker exists")
                .join()
                .is_err()
            {
                return Err(MediaError::Device(
                    "capture worker stopped unexpectedly".into(),
                ));
            }
        }
        Ok(())
    }
}

/// A session lives inside `CallWorker::run`. Its channels are polled beside
/// local audio and remote video, and `shutdown` runs before MatrixRTC leave.
pub(crate) struct LocalVideoSession {
    token: u64,
    commands: mpsc::Receiver<CaptureCommand>,
    commands_open: bool,
    frames_tx: mpsc::Sender<Captured>,
    frames: mpsc::Receiver<Captured>,
    ticker: time::Interval,
    camera: CaptureSlot,
    screen: CaptureSlot,
    next_generation: u64,
    status: watch::Sender<LocalVideoStatus>,
    stop: Arc<CaptureStop>,
    active_workers: Arc<AtomicUsize>,
    /// The newest pause this session started. Frames are dropped instead of
    /// sent while the call cannot follow signalling or is rejoining; capture
    /// keeps running, so resuming never reopens a device or asks for a new
    /// screen selection.
    transmission_pause: Option<TransmissionPauseToken>,
    /// Source-ready and failure reports found while discarding stale frames.
    deferred: VecDeque<Captured>,
}

impl LocalVideoSession {
    pub(crate) fn new(
        token: u64,
        commands: mpsc::Receiver<CaptureCommand>,
        status: watch::Sender<LocalVideoStatus>,
        stop: Arc<CaptureStop>,
        active_workers: Arc<AtomicUsize>,
    ) -> Self {
        let (frames_tx, frames) = mpsc::channel(CAPTURE_QUEUE);
        let mut ticker = time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self {
            token,
            commands,
            commands_open: true,
            frames_tx,
            frames,
            ticker,
            camera: CaptureSlot::default(),
            screen: CaptureSlot::default(),
            next_generation: 1,
            status,
            stop,
            active_workers,
            transmission_pause: None,
            deferred: VecDeque::new(),
        }
    }

    /// Move this session to the call's new UI generation after a rejoin,
    /// keeping the capture phases the user already sees.
    pub(crate) fn set_presentation_token(&mut self, token: u64) {
        let previous = self.token;
        self.token = token;
        self.status.send_if_modified(|current| {
            if current.token != previous {
                return false;
            }
            current.token = token;
            true
        });
    }

    /// Stop sending without stopping capture or retracting the publications.
    pub(crate) fn pause_transmission(&mut self) {
        let camera = &self.camera.publication;
        let screen = &self.screen.publication;
        let token = self.stop.pause_transmission(|| {
            for publication in [camera, screen].into_iter().flatten() {
                let _ = publication.set_muted(true);
            }
        });
        self.transmission_pause = Some(token);
    }

    /// Resume sending for captures the user has not stopped, unless Leave
    /// stopped the call meanwhile. A capture that has not published yet
    /// publishes on its next frame.
    pub(crate) fn resume_transmission(&mut self) {
        let Some(token) = self.transmission_pause else {
            return;
        };
        let camera = self.camera.live_publication();
        let screen = self.screen.live_publication();
        let resumed = self.stop.resume_transmission(token, || {
            for publication in [camera, screen].into_iter().flatten() {
                // A failed unmute surfaces through the next frame push.
                let _ = publication.set_muted(false);
            }
        });
        if !resumed {
            return;
        }
        self.transmission_pause = None;
        // Frames queued before the resume are stale; never replay them after
        // a reconnect. The queue is bounded, so this drains at most a few.
        for _ in 0..CAPTURE_QUEUE {
            let Ok(event) = self.frames.try_recv() else {
                break;
            };
            if !matches!(event, Captured::Frame { .. }) {
                self.deferred.push_back(event);
            }
        }
        for slot in [&mut self.camera, &mut self.screen] {
            // The first-frame budget restarts; frames were withheld.
            if slot.started.is_some() {
                slot.started = Some(Instant::now());
            }
        }
    }

    /// Release publications that belong to a join which has ended, keeping
    /// any running capture paused for a rejoin. Dropping a publication mutes
    /// it; the ended join's leave removes it from peers.
    pub(crate) fn detach_publications(&mut self) {
        self.pause_transmission();
        for kind in [CaptureKind::Camera, CaptureKind::Screen] {
            drop(self.slot_mut(kind).publication.take());
        }
    }

    fn transmission_paused(&self) -> bool {
        self.stop.transmission_paused()
    }

    pub(crate) async fn next_event(&mut self) -> LocalVideoEvent {
        if let Some(event) = self.deferred.pop_front() {
            return LocalVideoEvent::Frame(event);
        }
        if self.commands_open {
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => LocalVideoEvent::Command(command),
                    None => {
                        self.commands_open = false;
                        LocalVideoEvent::Tick
                    }
                },
                frame = self.frames.recv() => LocalVideoEvent::Frame(frame.expect("session owns frame sender")),
                _ = self.ticker.tick() => LocalVideoEvent::Tick,
            }
        } else {
            tokio::select! {
                frame = self.frames.recv() => LocalVideoEvent::Frame(frame.expect("session owns frame sender")),
                _ = self.ticker.tick() => LocalVideoEvent::Tick,
            }
        }
    }

    pub(crate) async fn handle_event(
        &mut self,
        rtc: &RtcService,
        room_id: &str,
        event: LocalVideoEvent,
    ) {
        match event {
            LocalVideoEvent::Frame(Captured::SourceReady { kind, generation }) => {
                self.source_ready(kind, generation);
            }
            LocalVideoEvent::Command(CaptureCommand::Start {
                source,
                label,
                reply,
            }) => {
                let result = self.start(source, label).await;
                let _ = reply.send(result);
            }
            LocalVideoEvent::Command(CaptureCommand::Stop { kind, reply }) => {
                let result = self.stop(kind).await;
                let _ = reply.send(result);
            }
            LocalVideoEvent::Frame(Captured::Frame {
                kind,
                generation,
                frame,
            }) => {
                self.handle_frame(rtc, room_id, kind, generation, frame)
                    .await;
            }
            LocalVideoEvent::Frame(Captured::Failed {
                kind,
                generation,
                error,
            }) => {
                if self.slot(kind).generation == generation && self.slot(kind).stop.is_some() {
                    self.fail(kind, error.to_string()).await;
                }
            }
            LocalVideoEvent::Tick if self.transmission_paused() => {}
            LocalVideoEvent::Tick => {
                for kind in [CaptureKind::Camera, CaptureKind::Screen] {
                    let selecting = self.current_state(kind).phase == CapturePhase::Selecting;
                    let budget = if selecting {
                        SELECT_TIMEOUT
                    } else {
                        START_TIMEOUT
                    };
                    if self
                        .slot(kind)
                        .started
                        .is_some_and(|since| since.elapsed() > budget)
                    {
                        self.fail(
                            kind,
                            if selecting {
                                "system screen selection timed out"
                            } else {
                                "native capture did not produce a frame in 20 seconds"
                            }
                            .into(),
                        )
                        .await;
                    }
                }
            }
        }
    }

    /// Handle capture events while the call has no join to publish on, i.e.
    /// while it rejoins. Stop always works; Start waits for the new join.
    /// Frames are dropped because transmission is paused.
    pub(crate) async fn handle_detached_event(&mut self, event: LocalVideoEvent) {
        match event {
            LocalVideoEvent::Command(CaptureCommand::Start { reply, .. }) => {
                let error = MediaError::Device(RECONNECTING_START.into());
                let _ = reply.send(Err(error));
            }
            LocalVideoEvent::Command(CaptureCommand::Stop { kind, reply }) => {
                let result = self.stop(kind).await;
                let _ = reply.send(result);
            }
            LocalVideoEvent::Frame(Captured::SourceReady { kind, generation }) => {
                self.source_ready(kind, generation);
            }
            LocalVideoEvent::Frame(Captured::Failed {
                kind,
                generation,
                error,
            }) => {
                if self.slot(kind).generation == generation && self.slot(kind).stop.is_some() {
                    self.fail(kind, error.to_string()).await;
                }
            }
            LocalVideoEvent::Frame(Captured::Frame { .. }) | LocalVideoEvent::Tick => {}
        }
    }

    async fn start(&mut self, source: CaptureSource, label: String) -> Result<(), MediaError> {
        if self.stop.requested() {
            return Err(MediaError::CallEnded);
        }
        let kind = source.kind();
        let selecting = source.needs_system_selection();
        self.stop(kind).await?;
        // Most capture workers finish within one frame interval after Stop.
        // Wait briefly rather than rejecting an immediate source change.
        for _ in 0..25 {
            if self
                .slot(kind)
                .worker
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
            {
                break;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
        let slot = self.slot_mut(kind);
        slot.reap_finished_worker()?;
        if slot.worker.is_some() {
            let error = MediaError::Device("previous native capture is still stopping".into());
            self.set_phase(kind, CapturePhase::Failed(error.to_string()), label);
            return Err(error);
        }
        if self.stop.requested() {
            return Err(MediaError::CallEnded);
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let stop = Arc::new(AtomicBool::new(false));
        let capture_stop = Arc::clone(&stop);
        let call_stop = Arc::clone(&self.stop);
        self.active_workers.fetch_add(1, Ordering::SeqCst);
        let guard = CaptureWorkerGuard(Arc::clone(&self.active_workers));
        let frames = self.frames_tx.clone();
        let worker = thread::Builder::new()
            .name(
                match kind {
                    CaptureKind::Camera => "morpheus-camera",
                    CaptureKind::Screen => "morpheus-screen",
                }
                .into(),
            )
            .spawn(move || {
                let _guard = guard;
                capture_worker(source, generation, capture_stop, call_stop, frames)
            })
            .map_err(|error| MediaError::Device(error.to_string()))?;
        let slot = self.slot_mut(kind);
        slot.generation = generation;
        slot.stop = Some(stop);
        slot.worker = Some(worker);
        slot.started = Some(Instant::now());
        self.set_phase(
            kind,
            if selecting {
                CapturePhase::Selecting
            } else {
                CapturePhase::Starting
            },
            label,
        );
        Ok(())
    }

    fn source_ready(&mut self, kind: CaptureKind, generation: u64) {
        if self.stop.requested()
            || self.slot(kind).generation != generation
            || self.slot(kind).stop.is_none()
        {
            return;
        }
        if self.current_state(kind).phase == CapturePhase::Selecting {
            self.slot_mut(kind).started = Some(Instant::now());
            self.set_phase(
                kind,
                CapturePhase::Starting,
                "System-selected source".into(),
            );
        }
    }

    async fn stop(&mut self, kind: CaptureKind) -> Result<(), MediaError> {
        self.stop_inner(kind, true).await
    }

    async fn stop_inner(&mut self, kind: CaptureKind, wait_worker: bool) -> Result<(), MediaError> {
        let had_capture = self.slot(kind).worker.is_some() || self.slot(kind).publication.is_some();
        if had_capture {
            let source = self.current_state(kind).source;
            self.set_phase(kind, CapturePhase::Stopping, source);
        }
        let slot = self.slot_mut(kind);
        slot.stop_capture();
        let publication = slot.publication.take();
        let unpublish = if let Some(publication) = publication {
            match timeout(UNPUBLISH_TIMEOUT, publication.unpublish()).await {
                Ok(result) => result,
                Err(_) => Err(MediaError::Device("timed out unpublishing video".into())),
            }
        } else {
            Ok(())
        };
        let worker = if wait_worker {
            self.wait_worker(kind, USER_STOP_TIMEOUT).await
        } else {
            Ok(())
        };
        let result = match (unpublish, worker) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(unpublish), Err(worker)) => {
                Err(MediaError::Device(format!("{unpublish}; {worker}")))
            }
        };
        match &result {
            Ok(()) => self.set_phase(kind, CapturePhase::Off, String::new()),
            Err(error) => {
                self.set_phase(kind, CapturePhase::Failed(error.to_string()), String::new())
            }
        }
        result
    }

    async fn wait_worker(
        &mut self,
        kind: CaptureKind,
        max_wait: Duration,
    ) -> Result<(), MediaError> {
        let deadline = Instant::now() + max_wait;
        loop {
            self.slot_mut(kind).reap_finished_worker()?;
            if self.slot(kind).worker.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(MediaError::Device(
                    "native capture is still stopping".into(),
                ));
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn fail(&mut self, kind: CaptureKind, message: String) {
        let stop_result = self.stop(kind).await;
        let message = match stop_result {
            Ok(()) => message,
            Err(error) => format!("{message}; {error}"),
        };
        self.set_phase(kind, CapturePhase::Failed(message), String::new());
    }

    async fn handle_frame(
        &mut self,
        rtc: &RtcService,
        room_id: &str,
        kind: CaptureKind,
        generation: u64,
        frame: VideoFrame,
    ) {
        if self.stop.requested()
            || self.transmission_paused()
            || self.slot(kind).generation != generation
            || self.slot(kind).stop.is_none()
        {
            return;
        }
        if self.slot(kind).publication.is_none() {
            let published = match kind {
                CaptureKind::Camera => tokio::select! {
                    biased;
                    _ = self.stop.cancelled() => return,
                    result = timeout(
                        PUBLISH_TIMEOUT,
                        rtc.publish_camera(room_id, frame.buffer.width, frame.buffer.height, false),
                    ) => result,
                },
                CaptureKind::Screen => tokio::select! {
                    biased;
                    _ = self.stop.cancelled() => return,
                    result = timeout(
                        PUBLISH_TIMEOUT,
                        rtc.publish_screen(room_id, frame.buffer.width, frame.buffer.height, false),
                    ) => result,
                },
            };
            match published {
                Ok(Ok(publication)) => self.slot_mut(kind).publication = Some(publication),
                Ok(Err(error)) => {
                    self.fail(kind, error.to_string()).await;
                    return;
                }
                Err(_) => {
                    self.fail(kind, "timed out publishing video".into()).await;
                    return;
                }
            }
        }
        if self.stop.requested() || self.slot(kind).stop.is_none() {
            // A leave can be requested while upstream publish is suspended.
            // The track is now owned by the slot so shutdown can unpublish it.
            return;
        }
        // A pause can begin while publish is suspended. Keep the new track
        // muted until the worker resumes transmission.
        let published = self.slot(kind).publication.as_ref();
        if self.stop.mute_if_paused(|| {
            if let Some(publication) = published {
                let _ = publication.set_muted(true);
            }
        }) {
            return;
        }
        let Some(result) = self.stop.push_if_active(|| {
            self.slot(kind)
                .publication
                .as_ref()
                .expect("published above")
                .push_video(frame)
        }) else {
            return;
        };
        match result {
            Ok(()) => {
                if self.slot_mut(kind).started.take().is_some() {
                    let current = self.current_state(kind);
                    self.set_phase(kind, CapturePhase::Active, current.source);
                }
            }
            Err(error) => self.fail(kind, error.to_string()).await,
        }
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), String> {
        self.stop.request();
        // Signal and mute both tracks synchronously before any async wait.
        self.camera.stop_capture();
        self.screen.stop_capture();
        let mut errors = Vec::new();
        for kind in [CaptureKind::Camera, CaptureKind::Screen] {
            if let Err(error) = self.stop_inner(kind, false).await {
                errors.push(error.to_string());
            }
        }
        let deadline = Instant::now() + WORKER_STOP_TIMEOUT;
        loop {
            for kind in [CaptureKind::Camera, CaptureKind::Screen] {
                if let Err(error) = self.slot_mut(kind).reap_finished_worker() {
                    errors.push(error.to_string());
                }
            }
            if self.camera.worker.is_none() && self.screen.worker.is_none() {
                break;
            }
            if Instant::now() >= deadline {
                errors.push("native capture is still stopping".into());
                break;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn slot(&self, kind: CaptureKind) -> &CaptureSlot {
        match kind {
            CaptureKind::Camera => &self.camera,
            CaptureKind::Screen => &self.screen,
        }
    }

    fn slot_mut(&mut self, kind: CaptureKind) -> &mut CaptureSlot {
        match kind {
            CaptureKind::Camera => &mut self.camera,
            CaptureKind::Screen => &mut self.screen,
        }
    }

    fn current_state(&self, kind: CaptureKind) -> CaptureState {
        let status = self.status.borrow();
        match kind {
            CaptureKind::Camera => status.camera.clone(),
            CaptureKind::Screen => status.screen.clone(),
        }
    }

    fn set_phase(&self, kind: CaptureKind, phase: CapturePhase, source: String) {
        self.status.send_if_modified(|current| {
            if current.token != self.token {
                return false;
            }
            let target = match kind {
                CaptureKind::Camera => &mut current.camera,
                CaptureKind::Screen => &mut current.screen,
            };
            let next = CaptureState { phase, source };
            if *target == next {
                false
            } else {
                *target = next;
                true
            }
        });
    }
}

impl Drop for LocalVideoSession {
    fn drop(&mut self) {
        self.camera.stop_capture();
        self.screen.stop_capture();
    }
}

fn capture_worker(
    source: CaptureSource,
    generation: u64,
    stop: Arc<AtomicBool>,
    call_stop: Arc<CaptureStop>,
    sender: mpsc::Sender<Captured>,
) {
    let kind = source.kind();
    let result = catch_unwind(AssertUnwindSafe(|| {
        capture_loop(source, generation, &stop, &call_stop, &sender)
    }));
    if !stop.load(Ordering::Acquire) && !call_stop.requested() {
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error,
            Err(_) => MediaError::Device("native capture backend stopped unexpectedly".into()),
        };
        // The call can stop polling this bounded queue during shutdown. Never
        // leave a native capture thread blocked forever reporting its error.
        let mut pending = Captured::Failed {
            kind,
            generation,
            error,
        };
        while !stop.load(Ordering::Acquire) && !call_stop.requested() {
            match sender.try_send(pending) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => break,
                Err(mpsc::error::TrySendError::Full(event)) => {
                    pending = event;
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

fn capture_loop(
    source: CaptureSource,
    generation: u64,
    stop: &AtomicBool,
    call_stop: &CaptureStop,
    sender: &mpsc::Sender<Captured>,
) -> Result<(), MediaError> {
    enum Device {
        Camera(CameraCapture),
        Screen(ScreenCapture),
    }
    let cancelled = || stop.load(Ordering::Acquire) || call_stop.requested();
    let selecting = source.needs_system_selection();
    if cancelled() {
        return Ok(());
    }
    let (mut device, interval, kind) = match source {
        CaptureSource::Camera(key) => (
            Device::Camera(CameraCapture::open_key_cancellable(&key, &cancelled)?),
            CAMERA_INTERVAL,
            CaptureKind::Camera,
        ),
        CaptureSource::Display(key) => (
            Device::Screen(ScreenCapture::open_key_cancellable(&key, &cancelled)?),
            SCREEN_INTERVAL,
            CaptureKind::Screen,
        ),
        CaptureSource::Window(key) => (
            Device::Screen(ScreenCapture::open_key_cancellable(&key, &cancelled)?),
            SCREEN_INTERVAL,
            CaptureKind::Screen,
        ),
    };
    if selecting {
        let mut ready = Captured::SourceReady { kind, generation };
        while !cancelled() {
            match sender.try_send(ready) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(event)) => {
                    ready = event;
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    while !cancelled() {
        let started = Instant::now();
        let frame = match &mut device {
            Device::Camera(camera) => camera.next_frame()?,
            Device::Screen(screen) => {
                screen.next_frame_sized_cancellable(SCREEN_WIDTH, SCREEN_HEIGHT, &cancelled)?
            }
        };
        if cancelled() {
            break;
        }
        match sender.try_send(Captured::Frame {
            kind,
            generation,
            frame,
        }) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
        if let Some(wait) = interval.checked_sub(started.elapsed()) {
            thread::sleep(wait);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_revocation_stops_capture_without_async_watcher_progress() {
        let current = Arc::new(AtomicBool::new(true));
        let observed = current.clone();
        let stop = CaptureStop::default();
        assert!(stop.bind_authorization(move || observed.load(Ordering::SeqCst)));
        assert_eq!(stop.push_if_active(|| 7), Some(7));
        current.store(false, Ordering::SeqCst);
        assert_eq!(
            stop.push_if_active(|| panic!("revoked frame was pushed")),
            None::<()>
        );
        let (sender, _) = mpsc::channel(1);
        assert!(
            capture_loop(
                CaptureSource::Camera("not-a-camera".into()),
                1,
                &AtomicBool::new(false),
                &stop,
                &sender
            )
            .is_ok()
        );
        current.store(true, Ordering::SeqCst);
        assert!(stop.requested());
    }

    #[test]
    fn signaling_gate_withholds_frames_without_an_owner_or_stop() {
        let open = Arc::new(AtomicBool::new(false));
        let gate = Arc::clone(&open);
        let stop = CaptureStop::default();
        stop.bind_transmission_gate(move || gate.load(Ordering::SeqCst));
        // No session pauses here: the push boundary reads the gate itself.
        assert_eq!(stop.push_if_active(|| ()), None);
        assert!(!stop.requested(), "a transient loss must not stop capture");
        open.store(true, Ordering::SeqCst);
        assert_eq!(stop.push_if_active(|| 7), Some(7));
    }

    #[test]
    fn transmission_pause_serializes_with_pushes_and_stale_resumes() {
        let stop = Arc::new(CaptureStop::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let pusher = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                stop.push_if_active(|| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
            })
        };
        entered_rx.recv().unwrap();
        let pauser = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || stop.pause_transmission(|| {}))
        };
        // The pause waits for the in-flight push instead of slipping between
        // that push's authorization check and the push itself.
        thread::sleep(Duration::from_millis(50));
        assert!(!pauser.is_finished());
        release_tx.send(()).unwrap();
        assert_eq!(pusher.join().unwrap(), Some(()));
        let first = pauser.join().unwrap();
        assert_eq!(stop.push_if_active(|| ()), None);

        // Only the newest pause can resume, and Leave dominates any resume.
        let second = stop.pause_transmission(|| {});
        assert!(!stop.resume_transmission(first, || panic!("stale resume unmuted")));
        assert_eq!(stop.push_if_active(|| ()), None);
        assert!(stop.resume_transmission(second, || {}));
        assert_eq!(stop.push_if_active(|| 7), Some(7));
        let third = stop.pause_transmission(|| {});
        stop.request();
        assert!(!stop.resume_transmission(third, || panic!("resumed after Leave")));
        assert_eq!(stop.push_if_active(|| ()), None);
    }

    #[test]
    fn capture_authorization_cannot_be_replaced_or_clear_an_existing_stop() {
        let stop = CaptureStop::default();
        assert!(stop.bind_authorization(|| true));
        assert!(!stop.bind_authorization(|| true));
        assert!(stop.requested());
        let stopped = CaptureStop::default();
        stopped.request();
        assert!(!stopped.bind_authorization(|| true));
        let denied = CaptureStop::default();
        assert!(!denied.bind_authorization(|| false));
        assert!(denied.requested());
    }

    #[tokio::test]
    async fn cancelled_or_stale_portal_selection_cannot_restart_capture() {
        let (_, commands) = mpsc::channel(1);
        let (status, _) = watch::channel(LocalVideoStatus {
            token: 7,
            ..Default::default()
        });
        let mut session = LocalVideoSession::new(
            7,
            commands,
            status,
            Arc::new(CaptureStop::default()),
            Arc::new(AtomicUsize::new(0)),
        );
        session.screen.generation = 3;
        session.screen.stop = Some(Arc::new(AtomicBool::new(false)));
        session.set_phase(
            CaptureKind::Screen,
            CapturePhase::Selecting,
            "Choose a display".into(),
        );
        session.source_ready(CaptureKind::Screen, 2);
        assert_eq!(
            session.current_state(CaptureKind::Screen).phase,
            CapturePhase::Selecting
        );
        session.source_ready(CaptureKind::Screen, 3);
        assert_eq!(
            session.current_state(CaptureKind::Screen).phase,
            CapturePhase::Starting
        );
        assert!(session.screen.started.is_some());
        session.stop(CaptureKind::Screen).await.unwrap();
        session.source_ready(CaptureKind::Screen, 3);
        assert_eq!(
            session.current_state(CaptureKind::Screen).phase,
            CapturePhase::Off
        );
        assert!(session.screen.started.is_none());
    }

    #[test]
    fn cancelled_screen_selection_skips_native_open() {
        let (sender, _receiver) = mpsc::channel(1);
        assert!(
            capture_loop(
                CaptureSource::Display("portal:monitor".into()),
                1,
                &AtomicBool::new(true),
                &CaptureStop::default(),
                &sender
            )
            .is_ok()
        );
    }

    #[test]
    fn stopped_source_skips_native_capture_open() {
        let (sender, _receiver) = mpsc::channel(1);
        assert!(
            capture_loop(
                CaptureSource::Camera("not-a-camera".into()),
                1,
                &AtomicBool::new(true),
                &CaptureStop::default(),
                &sender,
            )
            .is_ok()
        );
    }

    #[test]
    fn ended_call_skips_native_capture_open() {
        let (sender, _receiver) = mpsc::channel(1);
        let call_stop = CaptureStop::default();
        call_stop.request();
        assert!(
            capture_loop(
                CaptureSource::Camera("not-a-camera".into()),
                1,
                &AtomicBool::new(false),
                &call_stop,
                &sender,
            )
            .is_ok()
        );
    }
}
