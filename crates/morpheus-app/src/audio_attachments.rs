//! One explicit, memory-only attachment player. No autoplay or plaintext files.
//! Cancellation permanently retires the original view/account/security/call
//! lease. A native open/drop already entered cannot be interrupted: its worker
//! handle remains owned until the actual thread, including its TLS, is joined.
#[cfg(feature = "native-calls")]
mod decode;
use crate::{AppWindow, live::LiveUi, security::SecurityGate};
use morpheus_matrix::AccountSelection;
use slint::{ComponentHandle, Model};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

/// One admitted playback owns both its supervisor and actual worker. The start
/// channel remains closed until the supervisor handle is retained. Only the
/// supervisor creates/joins the native worker; no join can block UI or Tokio
/// while native open, stream destruction or worker TLS destruction is pending.
static WORKERS: Workers = Workers::new();
const RELEASE_WAIT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseError {
    TimedOut,
    WorkerPanicked,
    SupervisorFailed,
    #[cfg(all(feature = "native-calls", target_os = "macos"))]
    NativeClose,
}
impl std::fmt::Display for ReleaseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::TimedOut => {
                "attachment audio worker release was not confirmed before the deadline"
            }
            Self::WorkerPanicked => "attachment audio worker panicked during playback or release",
            Self::SupervisorFailed => "attachment audio worker ownership could not be confirmed",
            #[cfg(all(feature = "native-calls", target_os = "macos"))]
            Self::NativeClose => "CoreAudio attachment cleanup failed and remains owned",
        })
    }
}
impl std::error::Error for ReleaseError {}

#[derive(Default)]
struct Completion {
    // Only the supervisor publishes success, after actual worker.join(). A
    // body-done flag, output permit, cancelled request or is_finished is not a
    // substitute for that join, which also observes native worker TLS teardown.
    result: Mutex<Option<Result<(), ReleaseError>>>,
}
impl Completion {
    fn finish(&self, result: Result<(), ReleaseError>) {
        let mut completed = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if completed.is_none() {
            *completed = Some(result);
        }
    }
    fn result(&self) -> Option<Result<(), ReleaseError>> {
        *self.result.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn released(&self) -> bool {
        self.result() == Some(Ok(()))
    }
}
struct Worker {
    supervisor: Option<std::thread::JoinHandle<()>>,
    completion: Arc<Completion>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionError {
    Busy,
    Failed,
    Unavailable,
}
struct StartWorker {
    start: std::sync::mpsc::SyncSender<()>,
    completion: Arc<Completion>,
}
impl StartWorker {
    fn start(self) -> Result<(), ReleaseError> {
        self.start.send(()).map_err(|_| {
            self.completion.finish(Err(ReleaseError::SupervisorFailed));
            ReleaseError::SupervisorFailed
        })
    }
}
struct Workers {
    worker: Mutex<Option<Worker>>,
}
impl Workers {
    const fn new() -> Self {
        Self {
            worker: Mutex::new(None),
        }
    }
    /// Pending/failed ownership is kept in place. No timeout, cancelled waiter,
    /// replacement request or generation change may take/drop these handles.
    fn poll_locked(worker: &mut Option<Worker>) -> Option<Result<(), ReleaseError>> {
        let Some(active) = worker.as_mut() else {
            return Some(Ok(()));
        };
        if let Some(Err(error)) = active.completion.result() {
            return Some(Err(error));
        }
        let Some(handle) = active.supervisor.as_ref() else {
            return Some(Err(ReleaseError::SupervisorFailed));
        };
        if !handle.is_finished() {
            return None;
        }
        // This supervisor runs only the fixed channel/spawn/join protocol. All
        // application/native work and its TLS were on the joined worker. This
        // finished-supervisor join never waits on an audio open/drop. Rust does
        // not promise a hard OS scheduling bound for the final join epilogue.
        let joined = active
            .supervisor
            .take()
            .expect("retained supervisor")
            .join();
        if joined.is_err() || active.completion.result().is_none() {
            active
                .completion
                .finish(Err(ReleaseError::SupervisorFailed));
            return Some(Err(ReleaseError::SupervisorFailed));
        }
        match active.completion.result() {
            Some(Ok(())) => {
                *worker = None;
                Some(Ok(()))
            }
            Some(Err(error)) => Some(Err(error)),
            None => Some(Err(ReleaseError::SupervisorFailed)),
        }
    }
    fn poll_release(&self) -> Option<Result<(), ReleaseError>> {
        Self::poll_locked(&mut self.worker.lock().unwrap_or_else(|e| e.into_inner()))
    }
    fn reserve(
        &self,
        completion: Arc<Completion>,
        phase: Arc<AtomicU8>,
        work: impl FnOnce() -> Result<(), ReleaseError> + Send + 'static,
    ) -> Result<StartWorker, AdmissionError> {
        self.reserve_with(completion, phase, work, |supervise| {
            std::thread::Builder::new()
                .name("attachment-audio-owner".into())
                .spawn(supervise)
        })
    }
    fn reserve_with(
        &self,
        completion: Arc<Completion>,
        phase: Arc<AtomicU8>,
        work: impl FnOnce() -> Result<(), ReleaseError> + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<StartWorker, AdmissionError> {
        let mut retained = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        match Self::poll_locked(&mut retained) {
            None => return Err(AdmissionError::Busy),
            Some(Err(_)) => return Err(AdmissionError::Failed),
            Some(Ok(())) => {}
        }
        let (start, admitted) = std::sync::mpsc::sync_channel(1);
        let completed = completion.clone();
        let supervise = Box::new(move || {
            // Dropping an unpublished start ticket cancels before any native
            // worker exists. The closure's captured request/download inputs are
            // destroyed before success is published, including this path.
            if admitted.recv().is_err() {
                drop(work);
                completed.finish(Ok(()));
                return;
            }
            let result = join_native(
                std::thread::Builder::new()
                    .name("attachment-audio".into())
                    .spawn(work),
                &phase,
            );
            completed.finish(result);
        });
        // If this spawn fails, no actual worker was ever created. Never put an
        // existing native JoinHandle in a closure passed to a fallible spawn.
        let supervisor = spawn(supervise).map_err(|_| AdmissionError::Unavailable)?;
        *retained = Some(Worker {
            supervisor: Some(supervisor),
            completion: completion.clone(),
        });
        Ok(StartWorker { start, completion })
    }
    async fn wait(&self, bound: Duration) -> Result<(), ReleaseError> {
        let until = tokio::time::Instant::now() + bound;
        loop {
            if let Some(result) = self.poll_release() {
                return result;
            }
            if tokio::time::Instant::now() >= until {
                return Err(ReleaseError::TimedOut);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
fn join_native(
    native: std::io::Result<std::thread::JoinHandle<Result<(), ReleaseError>>>,
    phase: &AtomicU8,
) -> Result<(), ReleaseError> {
    match native {
        Ok(native) => native.join().unwrap_or(Err(ReleaseError::WorkerPanicked)),
        Err(_) => {
            // No native worker exists after a failed spawn. Preserve the
            // playback error while proving there is no worker left to join.
            phase.store(AudioError::Unavailable.code(), Ordering::Release);
            Ok(())
        }
    }
}
/// Call setup already holds the reservation that permanently revokes the old
/// request. Shutdown cancels it first. Neither may report release until the
/// actual download/decoder/native worker has been joined. Native open/drop
/// already entered is not interruptible: a bounded wait fails and retains its
/// owner instead of permitting another stream or silently detaching the worker.
pub(crate) async fn wait_for_release() -> Result<(), ReleaseError> {
    WORKERS.wait(RELEASE_WAIT).await
}
pub(crate) struct ViewLease {
    pub(crate) live: Arc<AtomicBool>,
    pub(crate) navigation: Arc<AtomicU64>,
    pub(crate) navigation_at: u64,
    pub(crate) generation: Arc<AtomicU64>,
    pub(crate) generation_at: u64,
    pub(crate) gate: Arc<SecurityGate>,
    pub(crate) security_at: u64,
}
impl ViewLease {
    fn current(&self) -> bool {
        self.live.load(Ordering::Acquire)
            && self.navigation.load(Ordering::Acquire) == self.navigation_at
            && self.generation.load(Ordering::Acquire) == self.generation_at
            && self.gate.ringing_allowed(self.security_at)
    }
}
struct Guard {
    selection: AccountSelection,
    view: ViewLease,
    stopped: AtomicBool,
    wake: Notify,
    calls: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl Guard {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }
    fn current(&self) -> bool {
        let current = !self.stopped.load(Ordering::Acquire)
            && self.selection.is_current()
            && self.view.current()
            && (self.calls)();
        // This also runs on CPAL: only atomics here, never Notify's waiter lock.
        // The async watcher supplies a bounded idle-I/O wake every 20 ms.
        if !current {
            self.stopped.store(true, Ordering::Release);
        }
        current
    }
    async fn cancelled(&self) {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if !self.current() {
                return;
            }
            tokio::select! { _=wake=>{}, _=self.selection.revoked()=>return, _=tokio::time::sleep(Duration::from_millis(20))=>{} }
        }
    }
}
/// Recheck the original immutable authority before EVERY delegated future poll.
/// The cancellation watcher supplies wakeups while I/O is idle. A request
/// already handed to the network, or work inside one delegated poll, cannot be
/// recalled; no later poll can resume it after an observed lease loss.
#[cfg(feature = "native-calls")]
async fn while_allowed<F: std::future::Future>(
    allowed: impl Fn() -> bool,
    future: F,
) -> Option<F::Output> {
    tokio::pin!(future);
    std::future::poll_fn(|cx| {
        if !allowed() {
            return std::task::Poll::Ready(None);
        }
        match future.as_mut().poll(cx) {
            std::task::Poll::Ready(value) => std::task::Poll::Ready(allowed().then_some(value)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioError {
    Unavailable,
    Unsupported,
    TooLarge,
    Invalid,
    Cancelled,
}
impl AudioError {
    fn code(self) -> u8 {
        match self {
            Self::Unavailable => 3,
            Self::Unsupported => 4,
            Self::TooLarge => 5,
            Self::Invalid => 6,
            Self::Cancelled => 7,
        }
    }
    fn text(code: u8) -> &'static str {
        match code {
            4 => "Playback supports MP3 and PCM/float WAV only. Save is still available.",
            5 => {
                "Playback limit: 16 MiB input, 48 MiB decoded audio and 3 minutes. Save is still available."
            }
            6 => "This audio could not be decoded safely. Save is still available.",
            _ => {
                "Audio unavailable. Check the preview privacy setting, room and speaker. Save is still available."
            }
        }
    }
}
struct Request {
    guard: Arc<Guard>,
    room: String,
    root: String,
    event: String,
    thread: bool,
    focused: bool,
    phase: Arc<AtomicU8>,
    completion: Arc<Completion>,
    #[cfg(feature = "native-calls")]
    playback: Arc<morpheus_rtc::PcmPlayback>,
}
impl Request {
    fn stop(&self) {
        self.guard.stop();
        #[cfg(feature = "native-calls")]
        self.playback.stop();
    }
    fn matches(&self, ui: &AppWindow) -> bool {
        self.guard.current()
            && ui.get_live_session()
            && ui.get_page() == 0
            && ui.get_active_account_local_id().as_str() == self.guard.selection.account_id()
            && ui.get_active_room_id().as_str() == self.room
            && if self.thread {
                ui.get_thread_root_id().as_str() == self.root
                    && ui.get_show_side_panel()
                    && ui.get_side_panel_tab() == 1
            } else {
                ui.get_conversation_focused() == self.focused
            }
    }
}
#[derive(Default)]
pub(crate) struct AudioAttachments {
    request: Mutex<Option<Arc<Request>>>,
}
impl Drop for AudioAttachments {
    fn drop(&mut self) {
        self.cancel();
    }
}
fn clear(ui: &AppWindow) {
    ui.set_attachment_audio_event("".into());
    ui.set_attachment_audio_thread(false);
    ui.set_attachment_audio_loading(false);
    ui.set_attachment_audio_playing(false);
    ui.set_attachment_audio_position("0:00".into());
    ui.set_attachment_audio_duration("—".into());
    ui.set_attachment_audio_status("".into());
}
fn time(ms: u64) -> String {
    let sec = ms / 1000;
    format!("{}:{:02}", sec / 60, sec % 60)
}
impl AudioAttachments {
    fn cancel(&self) {
        if let Some(request) = self
            .request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            request.stop();
        }
    }
    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.cancel();
        clear(ui);
    }
    fn tick(&self, ui: &AppWindow) {
        // Reap only a completed, joined owner; pending/failed ownership remains
        // retained even after the presentation request has been cancelled.
        let release = WORKERS.poll_release();
        let request = self
            .request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(request) = request else {
            return;
        };
        if !request.matches(ui)
            || !ui.get_viewport_seek_allowed()
            || ui.get_settings_reveal_protected()
        {
            self.shutdown(ui);
            return;
        }
        let phase = request.phase.load(Ordering::Acquire);
        ui.set_attachment_audio_loading(phase == 0);
        if phase >= 3 {
            ui.set_attachment_audio_playing(false);
            ui.set_attachment_audio_status(AudioError::text(phase).into());
            return;
        }
        if matches!(release, Some(Err(_))) {
            ui.set_attachment_audio_loading(false);
            ui.set_attachment_audio_playing(false);
            ui.set_attachment_audio_status(
                "Audio worker did not release safely. Restart Morpheus before playing audio."
                    .into(),
            );
            return;
        }
        #[cfg(feature = "native-calls")]
        {
            ui.set_attachment_audio_playing(request.playback.playing());
            ui.set_attachment_audio_position(time(request.playback.position_ms()).into());
            if let Some(duration) = request.playback.duration_ms() {
                ui.set_attachment_audio_duration(time(duration).into());
            }
            ui.set_attachment_audio_status(
                if request.playback.unavailable() {
                    AudioError::text(3)
                } else if phase == 0 {
                    "Loading audio into memory…"
                } else if request.playback.ended() && request.completion.released() {
                    "Finished · Play loads it again"
                } else if !request.playback.active() && request.completion.released() {
                    "Stopped · Play loads it again"
                } else if !request.playback.active() {
                    "Releasing speaker…"
                } else if request.playback.paused() {
                    "Paused · stays in memory"
                } else if request.playback.playing() {
                    "Playing · stays in memory"
                } else {
                    "Preparing speaker…"
                }
                .into(),
            );
        }
    }
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: tokio::runtime::Handle,
        #[cfg(feature = "native-calls")] calls: Arc<crate::calls::CallController>,
    ) -> Arc<Self> {
        let owner = Arc::new(Self::default());
        let stop = owner.clone();
        let weak = ui.as_weak();
        ui.on_stop_attachment_audio(move || {
            stop.cancel();
            if let Some(ui) = weak.upgrade() {
                clear(&ui);
            }
        });
        let tick = owner.clone();
        let weak = ui.as_weak();
        ui.on_attachment_audio_tick(move || {
            if let Some(ui) = weak.upgrade() {
                tick.tick(&ui);
            }
        });
        let stop = owner.clone();
        let weak = ui.as_weak();
        ui.on_attachment_audio_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                stop.tick(&ui);
            }
        });
        let player = owner.clone();
        let weak = ui.as_weak();
        ui.on_play_attachment_audio(move |event,thread| {
            let Some(ui)=weak.upgrade() else { return; };
            // Original click authority is captured before admission checks.
            let view=live.attachment_audio_lease(thread);
            #[cfg(feature="native-calls")]
            let call_authority:Arc<dyn Fn()->bool+Send+Sync> = {
                let call=calls.attachment_audio_authority();
                let output=morpheus_rtc::PcmPlayback::output_authority();
                Arc::new(move || call() && output())
            };
            if !ui.get_live_session() || ui.get_page()!=0 || !ui.get_viewport_seek_allowed() || ui.get_settings_reveal_protected() { return; }
            let model=if thread { ui.get_thread_messages() } else { ui.get_messages() };
            if !model.iter().any(|m| m.event_id==event && m.attachment_is_audio) || event.is_empty() { return; }
            if thread && (ui.get_thread_root_id().is_empty() || !ui.get_show_side_panel() || ui.get_side_panel_tab()!=1) { return; }
            #[cfg(not(feature="native-calls"))]
            { let _=(&view,&runtime); ui.set_attachment_audio_event(event); ui.set_attachment_audio_thread(thread); ui.set_attachment_audio_status("Audio playback is unavailable in this build. Save is still available.".into()); return; }
            #[cfg(feature="native-calls")]
            {
                let previous=player.request.lock().unwrap_or_else(|e|e.into_inner()).clone();
                if let Some(previous)=previous && previous.event==event.as_str() && previous.thread==thread && previous.matches(&ui)
                    && previous.phase.load(Ordering::Acquire)<3 && previous.playback.active() {
                    previous.playback.toggle_pause(); player.tick(&ui); return;
                }
                let Some(selection)=live.manager.account_selection(ui.get_active_account_local_id().as_str()) else { return; };
                if !view.current() || !call_authority() { return; }
                player.shutdown(&ui);
                ui.set_attachment_audio_event(event.clone()); ui.set_attachment_audio_thread(thread);
                let guard=Arc::new(Guard { selection, view, stopped:AtomicBool::new(false), wake:Notify::new(), calls:call_authority });
                let native_guard=guard.clone(); let playback=morpheus_rtc::PcmPlayback::new(Arc::new(move || native_guard.current()));
                let request=Arc::new(Request { guard,room:ui.get_active_room_id().to_string(),root:if thread {ui.get_thread_root_id().to_string()} else {String::new()},event:event.to_string(),thread,focused:ui.get_conversation_focused(),phase:Arc::new(AtomicU8::new(0)),completion:Arc::new(Completion::default()),playback });
                let manager=live.manager.clone(); let encrypted=ui.get_image_previews_encrypted_enabled();
                let worker_request=request.clone(); let worker_runtime=runtime.clone();
                let start=WORKERS.reserve(request.completion.clone(),request.phase.clone(),move || {
                    let request=worker_request;
                    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // Drive the same revocable bounded download on this one
                        // owned thread. No Tokio task handle is discarded and no
                        // blocking join job can delay runtime teardown forever.
                        let downloaded=worker_runtime.block_on(async {
                            tokio::select! { biased;
                                _=request.guard.cancelled()=>Err(AudioError::Cancelled),
                                result=while_allowed(||request.guard.current(),tokio::time::timeout(Duration::from_secs(45),manager.download_audio_playback_for(&request.guard.selection,&request.room,&request.event,encrypted)))=>match result {
                                    Some(Ok(Ok(downloaded)))=>Ok(downloaded),
                                    Some(Ok(Err(morpheus_matrix::MatrixCoreError::MediaTooLarge{..})))=>Err(AudioError::TooLarge),
                                    Some(_)=>Err(AudioError::Unavailable),
                                    None=>Err(AudioError::Cancelled),
                                },
                            }
                        })?;
                        drop(manager);
                        if !request.guard.current() { return Err(AudioError::Cancelled); }
                        let plain=downloaded.into_plaintext().map_err(|_|AudioError::Invalid)?;
                        let deadline=std::time::Instant::now()+Duration::from_secs(30); let guard=request.guard.clone();
                        let allowed:decode::Authority=Arc::new(move || guard.current() && std::time::Instant::now()<deadline);
                        let clip=decode::decode(zeroize::Zeroizing::new(plain.bytes),&plain.metadata.mime_type,allowed)?;
                        if !request.guard.current() { return Err(AudioError::Cancelled); }
                        request.phase.store(1,Ordering::Release);
                        request.playback.play_pcm(clip.samples,clip.rate,clip.channels).map_err(|_|AudioError::Unavailable)
                    }));
                    #[cfg(target_os = "macos")]
                    let native_close=request.playback.close_native_output_on_owned_worker();
                    #[cfg(target_os = "macos")]
                    if result.is_ok() && (native_close.is_err() || request.playback.native_close_failed()) {
                        request.phase.store(AudioError::Unavailable.code(),Ordering::Release);
                        return Err(ReleaseError::NativeClose);
                    }
                    match result {
                        Ok(Ok(()))=>Ok(()),
                        Ok(Err(error))=>{request.phase.store(error.code(),Ordering::Release);Ok(())},
                        Err(_)=>{request.phase.store(AudioError::Invalid.code(),Ordering::Release);Err(ReleaseError::WorkerPanicked)},
                    }
                });
                let start=match start {
                    Ok(start)=>start,
                    Err(error)=>{
                        request.stop();
                        ui.set_attachment_audio_status(match error {
                            AdmissionError::Busy=>"Previous playback is still being released. Try Play again shortly.",
                            AdmissionError::Failed=>"Previous audio worker did not release safely. Restart Morpheus before playing audio.",
                            AdmissionError::Unavailable=>AudioError::text(AudioError::Unavailable.code()),
                        }.into());
                        return;
                    }
                };
                *player.request.lock().unwrap_or_else(|e|e.into_inner())=Some(request.clone());
                ui.set_attachment_audio_loading(true); ui.set_attachment_audio_status("Loading audio into memory…".into());
                // Publish the retained owner and request before the actual
                // worker can enter download, decode or a native device call.
                if start.start().is_err() { request.phase.store(AudioError::Unavailable.code(),Ordering::Release); }

            }
        });
        owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn phase() -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(0))
    }
    struct HeldTls {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for HeldTls {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }
    std::thread_local! {
        static HELD_TLS: std::cell::RefCell<Option<HeldTls>> = const { std::cell::RefCell::new(None) };
    }
    #[tokio::test]
    async fn worker_body_return_and_cancelled_wait_do_not_release_held_native_tls() {
        let workers = Workers::new();
        let completed = Arc::new(Completion::default());
        let (entered, in_tls) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::channel();
        workers
            .reserve(completed.clone(), phase(), move || {
                HELD_TLS.with(|slot| {
                    *slot.borrow_mut() = Some(HeldTls {
                        entered,
                        release: released,
                    })
                });
                Ok(())
            })
            .unwrap()
            .start()
            .unwrap();
        // The body has returned but a real worker TLS destructor is held. The
        // supervisor must still own/join that exact thread, not trust a flag.
        in_tls.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!completed.released());
        assert_eq!(workers.poll_release(), None);
        let mut waiting = Box::pin(workers.wait(Duration::from_secs(2)));
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(waiting);
        assert_eq!(
            workers.wait(Duration::from_millis(20)).await,
            Err(ReleaseError::TimedOut)
        );
        assert!(matches!(
            workers.reserve(Arc::new(Completion::default()), phase(), || Ok(())),
            Err(AdmissionError::Busy)
        ));
        assert!(!completed.released());
        release.send(()).unwrap();
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(completed.released());
        assert!(workers.worker.lock().unwrap().is_none());
        let replacement = Arc::new(Completion::default());
        workers
            .reserve(replacement.clone(), phase(), || Ok(()))
            .unwrap()
            .start()
            .unwrap();
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(replacement.released());
    }
    #[tokio::test]
    async fn cancellation_before_publication_keeps_handle_owned_and_never_enters_retired_work() {
        let workers = Workers::new();
        let completed = Arc::new(Completion::default());
        let allowed = Arc::new(AtomicBool::new(true));
        let used = Arc::new(AtomicBool::new(false));
        let gate = allowed.clone();
        let entered = used.clone();
        let start = workers
            .reserve(completed.clone(), phase(), move || {
                if gate.load(Ordering::Acquire) {
                    entered.store(true, Ordering::Release);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(workers.poll_release(), None);
        assert!(
            workers
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supervisor
                .is_some()
        );
        assert!(!used.load(Ordering::Acquire));
        allowed.store(false, Ordering::Release);
        start.start().unwrap();
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(!used.load(Ordering::Acquire));
        assert!(completed.released());
    }
    #[tokio::test]
    async fn dropping_unpublished_ticket_joins_no_native_worker_and_all_captures_are_dropped() {
        struct Capture(Arc<AtomicBool>);
        impl Drop for Capture {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let workers = Workers::new();
        let completed = Arc::new(Completion::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let capture = Capture(dropped.clone());
        let start = workers
            .reserve(completed.clone(), phase(), move || {
                drop(capture);
                panic!("unpublished work must never start")
            })
            .unwrap();
        drop(start);
        assert_eq!(workers.wait(Duration::from_secs(2)).await, Ok(()));
        assert!(dropped.load(Ordering::Acquire));
        assert!(completed.released());
    }
    #[test]
    fn failed_supervisor_spawn_never_creates_a_native_worker_or_loses_a_handle() {
        let workers = Workers::new();
        let completed = Arc::new(Completion::default());
        let entered = Arc::new(AtomicBool::new(false));
        let body = entered.clone();
        let result = workers.reserve_with(
            completed.clone(),
            phase(),
            move || {
                body.store(true, Ordering::Release);
                Ok(())
            },
            |_supervise| Err(std::io::Error::other("synthetic spawn failure")),
        );
        assert!(matches!(result, Err(AdmissionError::Unavailable)));
        assert!(!entered.load(Ordering::Acquire));
        assert!(!completed.released());
        assert_eq!(workers.poll_release(), Some(Ok(())));
        assert!(workers.worker.lock().unwrap().is_none());
    }
    #[test]
    fn failed_native_spawn_preserves_playback_error_without_claiming_a_worker_started() {
        let phase = AtomicU8::new(0);
        assert_eq!(
            join_native(
                Err(std::io::Error::other("synthetic native spawn failure")),
                &phase
            ),
            Ok(())
        );
        assert_eq!(
            phase.load(Ordering::Acquire),
            AudioError::Unavailable.code()
        );
    }
    #[tokio::test]
    async fn native_panic_is_joined_but_latches_failed_release_and_blocks_replacement() {
        let workers = Workers::new();
        let completed = Arc::new(Completion::default());
        workers
            .reserve(completed.clone(), phase(), || {
                panic!("synthetic worker panic")
            })
            .unwrap()
            .start()
            .unwrap();
        assert_eq!(
            workers.wait(Duration::from_secs(2)).await,
            Err(ReleaseError::WorkerPanicked)
        );
        assert!(!completed.released());
        assert!(matches!(
            workers.reserve(Arc::new(Completion::default()), phase(), || Ok(())),
            Err(AdmissionError::Failed)
        ));
        assert_eq!(
            workers.poll_release(),
            Some(Err(ReleaseError::WorkerPanicked))
        );
    }
    #[tokio::test]
    async fn supervisor_protocol_failure_never_turns_missing_completion_into_release() {
        let workers = Workers::new();
        let completed = Arc::new(Completion::default());
        let start = workers
            .reserve_with(
                completed.clone(),
                phase(),
                || Ok(()),
                |supervise| {
                    std::thread::Builder::new().spawn(move || {
                        drop(supervise);
                        panic!("synthetic supervisor panic");
                    })
                },
            )
            .unwrap();
        let _ = start.start();
        assert_eq!(
            workers.wait(Duration::from_secs(2)).await,
            Err(ReleaseError::SupervisorFailed)
        );
        assert!(!completed.released());
        assert!(matches!(
            workers.reserve(Arc::new(Completion::default()), phase(), || Ok(())),
            Err(AdmissionError::Failed)
        ));
    }
    #[test]
    fn view_lease_never_adopts_navigation_generation_or_security_aba() {
        let gate = Arc::new(SecurityGate::default());
        let navigation = Arc::new(AtomicU64::new(5));
        let generation = Arc::new(AtomicU64::new(7));
        let lease = ViewLease {
            live: Arc::new(AtomicBool::new(true)),
            navigation: navigation.clone(),
            navigation_at: 5,
            generation: generation.clone(),
            generation_at: 7,
            gate: gate.clone(),
            security_at: gate.ringing_generation(),
        };
        assert!(lease.current());
        navigation.store(6, Ordering::Release);
        assert!(!lease.current());
        navigation.store(5, Ordering::Release);
        generation.store(8, Ordering::Release);
        assert!(!lease.current());
        generation.store(7, Ordering::Release);
        let step = gate.try_account_access().unwrap();
        drop(step);
        assert!(!gate.blocked());
        assert!(!lease.current());
    }
    #[cfg(feature = "native-calls")]
    #[test]
    fn a_held_download_is_not_polled_again_after_real_security_generation_aba() {
        use std::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        let gate = Arc::new(SecurityGate::default());
        let original = gate.ringing_generation();
        let ready = AtomicBool::new(false);
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let work = std::future::poll_fn(|_| {
            polls.fetch_add(1, Ordering::SeqCst);
            if ready.load(Ordering::Acquire) {
                Poll::Ready(7)
            } else {
                Poll::Pending
            }
        });
        let mut guarded = Box::pin(while_allowed(|| gate.ringing_allowed(original), work));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(guarded.as_mut().poll(&mut cx).is_pending());
        let access = gate.try_account_access().unwrap();
        drop(access);
        assert!(!gate.blocked());
        ready.store(true, Ordering::Release);
        assert_eq!(guarded.as_mut().poll(&mut cx), Poll::Ready(None));
        assert_eq!(polls.load(Ordering::Acquire), 1);
        let current = gate.ringing_generation();
        let mut positive = Box::pin(while_allowed(
            || gate.ringing_allowed(current),
            std::future::ready(9),
        ));
        assert_eq!(positive.as_mut().poll(&mut cx), Poll::Ready(Some(9)));
    }
    #[test]
    fn progress_labels_are_consumed_frame_times_not_wall_clock() {
        assert_eq!(time(0), "0:00");
        assert_eq!(time(61_999), "1:01");
    }
}
