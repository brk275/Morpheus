//! Explicit, memory-only native image previews. One download/decode is admitted
//! at a time and no decoded cache is retained. A cancelled codec call already
//! entered on its dedicated worker cannot be recalled, but its permit remains
//! held until exit and its result cannot publish to a revoked selection/view.
//! An animated GIF or WebP plays on that same worker, which then keeps the
//! permit until playback stops.
mod animation;
pub(crate) mod avatar;
mod decode;

use crate::{AppWindow, StyleTokens, live::LiveUi};
use animation::{Control, Playback};
use morpheus_config::{Config, ConfigStore};
use morpheus_matrix::{AccountManager, AccountSelection};
use slint::{ComponentHandle, Model, Rgba8Pixel, SharedPixelBuffer, SharedString};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::Duration,
};
use tokio::sync::Notify;
use zeroize::Zeroizing;

pub(super) struct Pixels {
    width: u32,
    height: u32,
    rgba: Zeroizing<Vec<u8>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewError {
    Unavailable,
    TooLarge,
    Unsupported,
    Invalid,
    Busy,
}
impl PreviewError {
    fn text(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "This image could not be loaded. Check the room and preview privacy setting."
            }
            Self::TooLarge => {
                "Preview limit: 8 MiB, 4096 pixels per side, 4 megapixels and 1000 animation frames. You can still save the attachment."
            }
            Self::Unsupported => {
                "Preview supports 8-bit PNG, baseline and progressive JPEG, GIF and WebP, and plays animated PNG, GIF and WebP. Arithmetic-coded JPEG, 16-bit PNG, SVG, audio and video previews are not supported yet."
            }
            Self::Invalid => {
                "This image could not be decoded safely. You can still save the attachment."
            }
            Self::Busy => "The previous image is still being released. Try again shortly.",
        }
    }

    /// Whether loading the same image again can succeed.
    fn retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::Busy)
    }
}

/// The same bounded PNG/JPEG decoder for other memory-only native
/// previews: (width, height, RGBA). Call it only on a dedicated worker.
pub(crate) fn decode_rgba(
    bytes: Zeroizing<Vec<u8>>,
) -> Result<(u32, u32, Zeroizing<Vec<u8>>), PreviewError> {
    decode::decode(bytes).map(|pixels| (pixels.width, pixels.height, pixels.rgba))
}

#[derive(Default)]
struct Cancel {
    stopped: AtomicBool,
    wake: Notify,
}
impl Cancel {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            wake.await;
        }
    }
}
struct Request {
    id: u64,
    selection: AccountSelection,
    room: String,
    cancel: Cancel,
}
impl Request {
    fn current(&self) -> bool {
        self.selection.is_current() && !self.cancel.stopped.load(Ordering::Acquire)
    }
    async fn cancelled(&self) {
        tokio::select! { _ = self.selection.revoked() => {}, _ = self.cancel.wait() => {} }
    }
}

struct Permit(Arc<AtomicBool>);
impl Permit {
    fn acquire(gate: &Arc<AtomicBool>) -> Option<Self> {
        gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(gate.clone()))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Playback commands for the current request's animation; dropping the
/// sender stops the worker at its next wait.
const PLAYER_COMMANDS: usize = 4;
/// How long a new request waits for a stopping worker to release its permit.
const ADMIT_WAIT: Duration = Duration::from_secs(3);

/// The newest animation picture for one request, waiting for the window.
struct Frame {
    request: u64,
    buffer: SharedPixelBuffer<Rgba8Pixel>,
    playback: Playback,
}

#[derive(Default)]
struct State {
    request: Option<Arc<Request>>,
    pending: Option<(u64, Result<Pixels, PreviewError>, Option<&'static str>)>,
    player: Option<SyncSender<Control>>,
    frame: Option<Frame>,
}
#[derive(Default)]
pub struct ImagePreviews {
    state: Mutex<State>,
    worker: Arc<AtomicBool>,
    sequence: AtomicU64,
    queued: AtomicBool,
    frame_queued: AtomicBool,
}

/// What the worker found in the decrypted plaintext.
enum Decoded {
    Still(Pixels, Option<&'static str>),
    /// Frames now arrive through `ImagePreviews::show_frame`.
    Animated,
}

/// What a request's worker needs to play an animation.
struct Player {
    commands: Receiver<Control>,
    autoplay: bool,
    sink: Arc<ImagePreviews>,
    weak: slint::Weak<AppWindow>,
}

fn playback_status(width: u32, height: u32, playback: Playback) -> String {
    let state = if playback.playing {
        "playing"
    } else if playback.ended {
        "finished · Replay starts it again"
    } else {
        "paused · press Play or K"
    };
    let format = playback.format.label();
    match playback.note {
        Some(note) => format!("{width} × {height} · animated {format} · {state} · {note}"),
        None => format!("{width} × {height} · animated {format} · {state}"),
    }
}

fn clear(ui: &AppWindow) {
    ui.set_image_preview_open(false);
    ui.set_image_preview_image(slint::Image::default());
    ui.set_image_preview_loaded(false);
    ui.set_image_preview_loading(false);
    ui.set_image_preview_failed(false);
    ui.set_image_preview_animated(false);
    ui.set_image_preview_playing(false);
    ui.set_image_preview_status("".into());
    ui.set_image_preview_title("".into());
    ui.set_image_preview_event_id("".into());
    ui.set_image_preview_position("".into());
    ui.set_image_preview_has_previous(false);
    ui.set_image_preview_has_next(false);
    ui.set_image_preview_account_id("".into());
    ui.set_image_preview_room_id("".into());
    ui.set_image_preview_token("".into());
}

/// The loaded images of the timeline holding `event`: the room's or the open
/// thread's. Only rows already loaded count; nothing is fetched to extend it.
fn gallery(ui: &AppWindow, event: &str) -> Option<(Vec<(SharedString, SharedString)>, usize)> {
    for timeline in [ui.get_messages(), ui.get_thread_messages()] {
        let images: Vec<_> = timeline
            .iter()
            .filter(|message| message.attachment_is_image && !message.event_id.is_empty())
            .map(|message| (message.event_id, message.attachment))
            .collect();
        if let Some(index) = images.iter().position(|(id, _)| id.as_str() == event) {
            return Some((images, index));
        }
    }
    None
}

fn publish_position(ui: &AppWindow, event: &str) {
    let (position, previous, next) = match gallery(ui, event) {
        Some((images, index)) if images.len() > 1 => (
            format!("{} of {}", index + 1, images.len()),
            index > 0,
            index + 1 < images.len(),
        ),
        _ => (String::new(), false, false),
    };
    ui.set_image_preview_position(position.into());
    ui.set_image_preview_has_previous(previous);
    ui.set_image_preview_has_next(next);
}

pub fn apply_config(ui: &AppWindow, config: &Config) {
    // Read when a viewer opens; an animation already open keeps its state.
    ui.set_image_previews_autoplay(config.media.autoplay_animated_media);
    let enabled = config.media.previews_in_encrypted_rooms;
    if ui.get_image_previews_encrypted_enabled() != enabled {
        // Runs synchronously on the UI thread for each Settings save and each
        // observed valid file reload. Disabling/re-enabling never revives work.
        ui.invoke_close_image_preview();
        if !enabled {
            ui.invoke_cancel_room_settings();
        }
        ui.set_image_previews_encrypted_enabled(enabled);
    }
}

impl ImagePreviews {
    fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(request) = state.request.take() {
            request.cancel.stop();
        }
        state.pending = None;
        // Its worker stops at its next wait; no waiting picture survives.
        state.player = None;
        state.frame = None;
    }

    /// Send a playback command to the current animation, if one plays.
    fn control(&self, control: Control) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .player
            .as_ref()
            .is_some_and(|player| player.try_send(control).is_ok())
    }

    fn take_current_frame(&self) -> Option<(Arc<Request>, Frame)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let frame = state.frame.take()?;
        let request = state.request.as_ref()?;
        if request.id != frame.request || !request.current() {
            return None;
        }
        Some((request.clone(), frame))
    }

    /// Called on the worker for every picture. Only the newest waits, and at
    /// most one window update is queued; a newer view or close discards it.
    fn show_frame(
        self: &Arc<Self>,
        request: &Arc<Request>,
        picture: &animation::Picture,
        playback: Playback,
        weak: &slint::Weak<AppWindow>,
    ) {
        if !request.current() {
            return;
        }
        // Copied here, off the UI thread; a dropped picture frees its copy.
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            picture.rgba.as_slice(),
            picture.width,
            picture.height,
        );
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .request
                .as_ref()
                .is_none_or(|active| active.id != request.id)
            {
                return;
            }
            state.frame = Some(Frame {
                request: request.id,
                buffer,
                playback,
            });
        }
        if self.frame_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let owner = self.clone();
        if weak
            .upgrade_in_event_loop(move |ui| {
                owner.frame_queued.store(false, Ordering::Release);
                let Some((request, frame)) = owner.take_current_frame() else {
                    return;
                };
                if !request.current() || !view_matches(&ui, &request) {
                    return;
                }
                let (width, height) = (frame.buffer.width(), frame.buffer.height());
                ui.set_image_preview_image(slint::Image::from_rgba8(frame.buffer));
                ui.set_image_preview_loaded(true);
                ui.set_image_preview_loading(false);
                ui.set_image_preview_failed(false);
                ui.set_image_preview_animated(true);
                ui.set_image_preview_playing(frame.playback.playing);
                // Announce only state changes, never every picture.
                let status = playback_status(width, height, frame.playback);
                if ui.get_image_preview_status().as_str() != status {
                    ui.set_image_preview_status(status.into());
                }
            })
            .is_err()
        {
            self.frame_queued.store(false, Ordering::Release);
            self.cancel();
        }
    }
    pub fn shutdown(&self, ui: &AppWindow) {
        self.cancel();
        clear(ui);
    }

    fn take_current_result(
        &self,
    ) -> Option<(Arc<Request>, Result<Pixels, PreviewError>, Option<&'static str>)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (id, result, note) = state.pending.take()?;
        let request = state.request.as_ref()?;
        if request.id != id || !request.current() {
            return None;
        }
        Some((request.clone(), result, note))
    }

    fn present(
        self: &Arc<Self>,
        request: &Arc<Request>,
        result: Result<Pixels, PreviewError>,
        note: Option<&'static str>,
        weak: &slint::Weak<AppWindow>,
    ) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !request.current()
                || state
                    .request
                    .as_ref()
                    .is_none_or(|active| active.id != request.id)
            {
                return;
            }
            state.pending = Some((request.id, result, note));
        }
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let owner = self.clone();
        if weak
            .upgrade_in_event_loop(move |ui| {
                owner.queued.store(false, Ordering::Release);
                let Some((request, result, note)) = owner.take_current_result() else {
                    return;
                };
                if !request.current() || !view_matches(&ui, &request) {
                    return;
                }
                ui.set_image_preview_loading(false);
                match result {
                    Ok(pixels) => {
                        let buffer =
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                pixels.rgba.as_slice(),
                                pixels.width,
                                pixels.height,
                            );
                        // Pixels may briefly exist in CPU/GPU allocations; dropping
                        // the image clears ownership, not a promise of GPU zeroing.
                        ui.set_image_preview_image(slint::Image::from_rgba8(buffer));
                        ui.set_image_preview_loaded(true);
                        ui.set_image_preview_failed(false);
                        let status = format!(
                            "{} × {} · preview stays in memory",
                            pixels.width, pixels.height
                        );
                        let status = match note {
                            Some(note) => format!("{status} · {note}"),
                            None => status,
                        };
                        ui.set_image_preview_status(status.into());
                    }
                    Err(error) => {
                        ui.set_image_preview_failed(error.retryable());
                        ui.set_image_preview_status(error.text().into());
                    }
                }
            })
            .is_err()
        {
            self.queued.store(false, Ordering::Release);
            self.cancel();
        }
    }

    pub fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        store: ConfigStore,
        handle: &tokio::runtime::Handle,
    ) -> Arc<Self> {
        let owner = Arc::new(Self::default());
        let close = owner.clone();
        let weak = ui.as_weak();
        ui.on_close_image_preview(move || {
            close.cancel();
            if let Some(ui) = weak.upgrade() {
                clear(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.on_set_encrypted_image_previews(move |enabled| {
            if let Some(ui) = weak.upgrade() {
                let _ = crate::save_config_change(&ui, &store, |config| {
                    config.media.previews_in_encrypted_rooms = enabled
                });
            }
        });
        let context = Opener {
            controller: owner.clone(),
            live,
            runtime: handle.clone(),
            weak: ui.as_weak(),
        };
        {
            let context = context.clone();
            ui.on_preview_image_attachment(move |event, filename| {
                if let Some(ui) = context.weak.upgrade() {
                    context.open(&ui, &event, &filename);
                }
            });
        }
        {
            let context = context.clone();
            ui.on_preview_adjacent_image(move |step| {
                let Some(ui) = context.weak.upgrade() else {
                    return;
                };
                let current = ui.get_image_preview_event_id();
                let Some((images, index)) = gallery(&ui, &current) else {
                    return;
                };
                let target = index.checked_add_signed(isize::try_from(step).unwrap_or_default());
                if let Some((event, filename)) = target.and_then(|target| images.get(target)) {
                    context.open(&ui, event, filename);
                }
            });
        }
        {
            let context = context.clone();
            ui.on_retry_image_preview(move || {
                let Some(ui) = context.weak.upgrade() else {
                    return;
                };
                let event = ui.get_image_preview_event_id();
                if ui.get_image_preview_open() && !event.is_empty() {
                    context.open(&ui, &event, &ui.get_image_preview_title());
                }
            });
        }
        {
            let context = context.clone();
            ui.on_cancel_image_preview_loading(move || {
                let Some(ui) = context.weak.upgrade() else {
                    return;
                };
                if !ui.get_image_preview_loading() {
                    return;
                }
                context.controller.cancel();
                // A token no request carries: the cancelled one can neither
                // publish pixels nor close the viewer when it ends.
                ui.set_image_preview_token("".into());
                ui.set_image_preview_loading(false);
                ui.set_image_preview_failed(true);
                ui.set_image_preview_status("Loading cancelled.".into());
            });
        }
        {
            let context = context.clone();
            ui.on_toggle_image_preview_playback(move || {
                let Some(ui) = context.weak.upgrade() else {
                    return;
                };
                if ui.get_image_preview_animated() {
                    let control = if ui.get_image_preview_playing() {
                        Control::Pause
                    } else {
                        Control::Play
                    };
                    context.controller.control(control);
                }
            });
        }
        {
            let context = context.clone();
            ui.on_replay_image_preview(move || {
                let Some(ui) = context.weak.upgrade() else {
                    return;
                };
                if ui.get_image_preview_animated() {
                    context.controller.control(Control::Replay);
                }
            });
        }
        owner
    }
}

/// What opening an image needs; cloned into each viewer callback.
#[derive(Clone)]
struct Opener {
    controller: Arc<ImagePreviews>,
    live: Arc<LiveUi>,
    runtime: tokio::runtime::Handle,
    weak: slint::Weak<AppWindow>,
}

impl Opener {
    /// Replace whatever the viewer shows with one explicitly requested image.
    fn open(&self, ui: &AppWindow, event: &str, filename: &str) {
        if !ui.get_live_session() || event.is_empty() || ui.get_active_room_id().is_empty() {
            return;
        }
        let Some(selection) = self
            .live
            .manager
            .account_selection(ui.get_active_account_local_id().as_str())
        else {
            return;
        };
        let controller = &self.controller;
        controller.cancel();
        clear(ui);
        let id = controller
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let request = Arc::new(Request {
            id,
            selection,
            room: ui.get_active_room_id().to_string(),
            cancel: Cancel::default(),
        });
        ui.set_image_preview_token(id.to_string().into());
        ui.set_image_preview_account_id(request.selection.account_id().into());
        ui.set_image_preview_room_id(request.room.clone().into());
        ui.set_image_preview_event_id(event.into());
        ui.set_image_preview_title(crate::directory_plain_text(filename, 160).into());
        ui.set_image_preview_status("Loading image into memory…".into());
        ui.set_image_preview_loading(true);
        publish_position(ui, event);
        ui.set_image_preview_open(true);
        let (player, commands) = sync_channel(PLAYER_COMMANDS);
        {
            let mut state = controller.state.lock().unwrap_or_else(|e| e.into_inner());
            state.request = Some(request.clone());
            state.player = Some(player);
        }
        // Animation never starts by itself under reduced motion or while
        // autoplay is off (the default); the first frame waits for Play.
        let autoplay = ui.get_image_previews_autoplay()
            && !ui.global::<StyleTokens>().get_reduced_motion();
        let event = event.to_owned();
        let encrypted = ui.get_image_previews_encrypted_enabled();
        let manager = self.live.manager.clone();
        let controller = controller.clone();
        let weak = self.weak.clone();
        self.runtime.spawn(async move {
            match admit(&controller.worker, &request).await {
                Some(permit) => {
                    let player = Player {
                        commands,
                        autoplay,
                        sink: controller.clone(),
                        weak: weak.clone(),
                    };
                    match fetch_decode(manager, request.clone(), event, encrypted, permit, player)
                        .await
                    {
                        Some(Ok(Decoded::Still(pixels, note))) => {
                            controller.present(&request, Ok(pixels), note, &weak);
                        }
                        Some(Err(error)) => controller.present(&request, Err(error), None, &weak),
                        Some(Ok(Decoded::Animated)) | None => {}
                    }
                }
                None => controller.present(&request, Err(PreviewError::Busy), None, &weak),
            }
            if request.current() {
                request.cancelled().await;
            }
            let owner = controller.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if ui.get_image_preview_token().as_str() == request.id.to_string() {
                    owner.cancel();
                    clear(&ui);
                }
            });
        });
    }
}

fn view_matches(ui: &AppWindow, request: &Request) -> bool {
    publication_context_matches(
        ui.get_image_preview_open(),
        request.current(),
        ui.get_image_preview_token().as_str(),
        &request.id.to_string(),
        ui.get_active_account_local_id().as_str(),
        request.selection.account_id(),
        ui.get_active_room_id().as_str(),
        &request.room,
    )
}

fn publication_context_matches(
    open: bool,
    current: bool,
    actual_token: &str,
    expected_token: &str,
    actual_account: &str,
    expected_account: &str,
    actual_room: &str,
    expected_room: &str,
) -> bool {
    open && current
        && actual_token == expected_token
        && actual_account == expected_account
        && actual_room == expected_room
}

/// The single decode permit, waiting briefly for a stopping worker (such as
/// an animation that was just closed) to release it.
async fn admit(gate: &Arc<AtomicBool>, request: &Request) -> Option<Permit> {
    let deadline = tokio::time::Instant::now() + ADMIT_WAIT;
    loop {
        if let Some(permit) = Permit::acquire(gate) {
            return Some(permit);
        }
        if !request.current() || tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn fetch_decode(
    manager: Arc<AccountManager>,
    request: Arc<Request>,
    event: String,
    encrypted: bool,
    permit: Permit,
    player: Player,
) -> Option<Result<Decoded, PreviewError>> {
    let downloaded = tokio::select! {
        biased;
        _ = request.cancelled() => return None,
        value = tokio::time::timeout(Duration::from_secs(45), manager.download_image_preview_for(&request.selection, &request.room, &event, encrypted)) => match value {
            Ok(Ok(download)) => download,
            Ok(Err(morpheus_matrix::MatrixCoreError::MediaTooLarge { .. })) => return Some(Err(PreviewError::TooLarge)),
            _ => return Some(Err(PreviewError::Unavailable)),
        }
    };
    if !request.current() {
        return None;
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let work = request.clone();
    let spawned = std::thread::Builder::new()
        .name("image-preview".into())
        .spawn(move || {
            let _permit = permit;
            if !work.current() {
                return;
            }
            let bytes = match downloaded.into_plaintext() {
                Ok(download) => Zeroizing::new(download.bytes),
                Err(_) => {
                    let _ = sender.send(Err(PreviewError::Invalid));
                    return;
                }
            };
            if !work.current() {
                return;
            }
            let Some(format) = animation::Format::sniff(&bytes) else {
                let result = decode::decode(bytes).map(|pixels| Decoded::Still(pixels, None));
                if work.current() {
                    let _ = sender.send(result);
                }
                return;
            };
            let mut pass = match animation::Pass::open(format, &bytes) {
                Ok(animation::Opened::Animated(pass)) => pass,
                Ok(animation::Opened::Still(pixels)) => {
                    let _ = sender.send(Ok(Decoded::Still(pixels, None)));
                    return;
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            let first = match pass.next_frame() {
                Ok(Some(first)) => first,
                Ok(None) => {
                    let _ = sender.send(Err(PreviewError::Invalid));
                    return;
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            if !pass.playable() {
                let note = "Shown still: animations over 2 megapixels don't play.";
                let _ = sender.send(Ok(Decoded::Still(first.into_pixels(), Some(note))));
                return;
            }
            let second = match pass.next_frame() {
                Ok(Some(second)) => second,
                // A single picture, or a broken second frame: show the first.
                Ok(None) | Err(_) => {
                    let _ = sender.send(Ok(Decoded::Still(first.into_pixels(), None)));
                    return;
                }
            };
            if !work.current() || sender.send(Ok(Decoded::Animated)).is_err() {
                return;
            }
            let Player {
                commands,
                autoplay,
                sink,
                weak,
            } = player;
            animation::play(
                &bytes,
                pass,
                first,
                second,
                &commands,
                autoplay,
                &|| work.current(),
                &|picture, playback| sink.show_frame(&work, picture, playback, &weak),
            );
        });
    if spawned.is_err() {
        return Some(Err(PreviewError::Unavailable));
    }
    // The dedicated worker is detached, not Tokio's shutdown-critical pool.
    // Cancellation drops its receiver; the worker keeps the sole permit until
    // its already-entered bounded codec operation or playback ends.
    tokio::select! {
        biased;
        _ = request.cancelled() => None,
        result = receiver => Some(result.unwrap_or(Err(PreviewError::Invalid))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_never_releases_a_worker_which_has_not_exited() {
        let gate = Arc::new(AtomicBool::new(false));
        let permit = Permit::acquire(&gate).unwrap();
        let cancelled = Cancel::default();
        cancelled.stop();
        assert!(Permit::acquire(&gate).is_none());
        drop(permit);
        assert!(Permit::acquire(&gate).is_some());
    }
    #[tokio::test]
    async fn cancellation_wakes_all_waiters_and_is_permanent() {
        let cancel = Cancel::default();
        let mut first = Box::pin(cancel.wait());
        let mut second = Box::pin(cancel.wait());
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        cancel.stop();
        tokio::time::timeout(Duration::from_secs(1), async {
            first.await;
            second.await;
            cancel.wait().await;
        })
        .await
        .unwrap();
    }
    #[test]
    fn revoked_or_replaced_view_never_admits_old_pixels() {
        assert!(publication_context_matches(
            true, true, "2", "2", "a", "a", "r", "r"
        ));
        // A→B→A has matching account strings but the old selection stays revoked.
        assert!(!publication_context_matches(
            true, false, "2", "2", "a", "a", "r", "r"
        ));
        assert!(!publication_context_matches(
            true, true, "3", "2", "a", "a", "r", "r"
        ));
        assert!(!publication_context_matches(
            true, true, "2", "2", "b", "a", "r", "r"
        ));
        assert!(!publication_context_matches(
            true, true, "2", "2", "a", "a", "s", "r"
        ));
        assert!(!publication_context_matches(
            false, true, "2", "2", "a", "a", "r", "r"
        ));
    }
    #[test]
    fn cancelling_drops_queued_private_pixels() {
        let owner = ImagePreviews::default();
        owner.state.lock().unwrap().pending = Some((
            1,
            Ok(Pixels {
                width: 1,
                height: 1,
                rgba: Zeroizing::new(vec![1, 2, 3, 4]),
            }),
            None,
        ));
        owner.cancel();
        assert!(owner.state.lock().unwrap().pending.is_none());
    }
}
