//! One explicit, memory-only attachment player. No autoplay or plaintext files.
//! Cancellation permanently retires the original view/account/security/call
//! lease. A native open/drop already entered cannot be interrupted: its worker
//! permit remains held until the actual stream has been destroyed.
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

static WORKER: AtomicBool = AtomicBool::new(false);
struct Permit;
impl Permit {
    fn take() -> Option<Self> {
        WORKER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        WORKER.store(false, Ordering::Release);
    }
}
/// Call setup holds its existing reservation, which permanently revokes the
/// old player, while waiting for its download/decoder/native stream to release.
/// It never starts a call output on top of an unreleased attachment stream.
pub(crate) async fn wait_for_release() -> bool {
    let until = tokio::time::Instant::now() + Duration::from_secs(3);
    while WORKER.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
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
    phase: AtomicU8,
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
                } else if request.playback.ended() {
                    "Finished · Play loads it again"
                } else if !request.playback.active() {
                    "Stopped · Play loads it again"
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
                let Some(permit)=Permit::take() else { ui.set_attachment_audio_status("Previous playback is still being released. Try Play again shortly.".into()); return; };
                let guard=Arc::new(Guard { selection, view, stopped:AtomicBool::new(false), wake:Notify::new(), calls:call_authority });
                let native_guard=guard.clone(); let playback=morpheus_rtc::PcmPlayback::new(Arc::new(move || native_guard.current()));
                let request=Arc::new(Request { guard,room:ui.get_active_room_id().to_string(),root:if thread {ui.get_thread_root_id().to_string()} else {String::new()},event:event.to_string(),thread,focused:ui.get_conversation_focused(),phase:AtomicU8::new(0),playback });
                *player.request.lock().unwrap_or_else(|e|e.into_inner())=Some(request.clone());
                ui.set_attachment_audio_loading(true); ui.set_attachment_audio_status("Loading audio into memory…".into());
                let manager=live.manager.clone(); let encrypted=ui.get_image_previews_encrypted_enabled();
                runtime.spawn(async move {
                    let download=tokio::select! { biased;
                        _=request.guard.cancelled()=>return,
                        result=while_allowed(||request.guard.current(),tokio::time::timeout(Duration::from_secs(45),manager.download_audio_playback_for(&request.guard.selection,&request.room,&request.event,encrypted)))=>match result {Some(value)=>value,None=>return},
                    };
                    let downloaded=match download { Ok(Ok(d))=>d, Ok(Err(morpheus_matrix::MatrixCoreError::MediaTooLarge{..}))=>{request.phase.store(AudioError::TooLarge.code(),Ordering::Release);return;}, _=>{request.phase.store(AudioError::Unavailable.code(),Ordering::Release);return;} };
                    drop(manager);
                    if !request.guard.current() { return; }
                    let failed=request.clone();
                    let spawn=std::thread::Builder::new().name("attachment-audio".into()).spawn(move || {
                        let _permit=permit;
                        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if !request.guard.current() { return Err(AudioError::Cancelled); }
                            let plain=downloaded.into_plaintext().map_err(|_|AudioError::Invalid)?;
                            let deadline=std::time::Instant::now()+Duration::from_secs(30); let guard=request.guard.clone();
                            let allowed:decode::Authority=Arc::new(move || guard.current() && std::time::Instant::now()<deadline);
                            let clip=decode::decode(zeroize::Zeroizing::new(plain.bytes),&plain.metadata.mime_type,allowed)?;
                            if !request.guard.current() { return Err(AudioError::Cancelled); }
                            request.phase.store(1,Ordering::Release);
                            request.playback.play_pcm(clip.samples,clip.rate,clip.channels).map_err(|_|AudioError::Unavailable)
                        })).unwrap_or(Err(AudioError::Invalid));
                        if let Err(error)=result { request.phase.store(error.code(),Ordering::Release); }
                    });
                    if spawn.is_err() { failed.phase.store(AudioError::Unavailable.code(),Ordering::Release); }
                    // JoinHandle is detached; the permit stays on the owner thread,
                    // including native open/drop. No blocking Tokio shutdown job.
                });
            }
        });
        owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
