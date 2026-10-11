//! Explicit, memory-only native image previews. One download/decode is admitted
//! at a time and no decoded cache is retained. A cancelled codec call already
//! entered on its dedicated worker cannot be recalled, but its permit remains
//! held until exit and its result cannot publish to a revoked selection/view.
pub(crate) mod avatar;
mod decode;

use crate::{AppWindow, live::LiveUi};
use morpheus_config::{Config, ConfigStore};
use morpheus_matrix::{AccountManager, AccountSelection};
use slint::ComponentHandle;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
pub(super) enum PreviewError {
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
                "Preview limit: 8 MiB, 4096 pixels per side, and 4 megapixels. You can still save the attachment."
            }
            Self::Unsupported => {
                "Preview supports static 8-bit PNG and baseline JPEG. Animation, progressive JPEG, WebP, audio and video previews are not supported yet."
            }
            Self::Invalid => {
                "This image could not be decoded safely. You can still save the attachment."
            }
            Self::Busy => {
                "The previous image is still being released. Close this viewer and try again shortly."
            }
        }
    }
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

#[derive(Default)]
struct State {
    request: Option<Arc<Request>>,
    pending: Option<(u64, Result<Pixels, PreviewError>)>,
}
#[derive(Default)]
pub struct ImagePreviews {
    state: Mutex<State>,
    worker: Arc<AtomicBool>,
    sequence: AtomicU64,
    queued: AtomicBool,
}

fn clear(ui: &AppWindow) {
    ui.set_image_preview_open(false);
    ui.set_image_preview_image(slint::Image::default());
    ui.set_image_preview_loaded(false);
    ui.set_image_preview_status("".into());
    ui.set_image_preview_title("".into());
    ui.set_image_preview_account_id("".into());
    ui.set_image_preview_room_id("".into());
    ui.set_image_preview_token("".into());
}

pub fn apply_config(ui: &AppWindow, config: &Config) {
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
    }
    pub fn shutdown(&self, ui: &AppWindow) {
        self.cancel();
        clear(ui);
    }

    fn take_current_result(&self) -> Option<(Arc<Request>, Result<Pixels, PreviewError>)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (id, result) = state.pending.take()?;
        let request = state.request.as_ref()?;
        if request.id != id || !request.current() {
            return None;
        }
        Some((request.clone(), result))
    }

    fn present(
        self: &Arc<Self>,
        request: &Arc<Request>,
        result: Result<Pixels, PreviewError>,
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
            state.pending = Some((request.id, result));
        }
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let owner = self.clone();
        if weak
            .upgrade_in_event_loop(move |ui| {
                owner.queued.store(false, Ordering::Release);
                let Some((request, result)) = owner.take_current_result() else {
                    return;
                };
                if !request.current() || !view_matches(&ui, &request) {
                    return;
                }
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
                        ui.set_image_preview_status(
                            format!(
                                "{} × {} · preview stays in memory",
                                pixels.width, pixels.height
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
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
        let weak = ui.as_weak();
        let controller = owner.clone();
        let runtime = handle.clone();
        ui.on_preview_image_attachment(move |event, filename| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if !ui.get_live_session() || event.is_empty() || ui.get_active_room_id().is_empty() {
                return;
            }
            let Some(selection) = live
                .manager
                .account_selection(ui.get_active_account_local_id().as_str())
            else {
                return;
            };
            controller.cancel();
            clear(&ui);
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
            ui.set_image_preview_title(crate::directory_plain_text(&filename, 160).into());
            ui.set_image_preview_status("Loading image into memory…".into());
            ui.set_image_preview_open(true);
            controller
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .request = Some(request.clone());
            let Some(permit) = Permit::acquire(&controller.worker) else {
                controller.present(&request, Err(PreviewError::Busy), &weak);
                return;
            };
            let event = event.to_string();
            let encrypted = ui.get_image_previews_encrypted_enabled();
            let manager = live.manager.clone();
            let controller = controller.clone();
            let weak = weak.clone();
            runtime.spawn(async move {
                let result = fetch_decode(manager, request.clone(), event, encrypted, permit).await;
                if let Some(result) = result {
                    controller.present(&request, result, &weak);
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
        });
        owner
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

async fn fetch_decode(
    manager: Arc<AccountManager>,
    request: Arc<Request>,
    event: String,
    encrypted: bool,
    permit: Permit,
) -> Option<Result<Pixels, PreviewError>> {
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
            let result = downloaded
                .into_plaintext()
                .map_err(|_| PreviewError::Invalid)
                .and_then(|download| {
                    let bytes = Zeroizing::new(download.bytes);
                    if !work.current() {
                        return Err(PreviewError::Unavailable);
                    }
                    decode::decode(bytes)
                });
            if work.current() {
                let _ = sender.send(result);
            }
        });
    if spawned.is_err() {
        return Some(Err(PreviewError::Unavailable));
    }
    // The dedicated worker is detached, not Tokio's shutdown-critical pool.
    // Cancellation drops its receiver; the worker keeps the sole permit until
    // its already-entered bounded codec operation finishes.
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
        ));
        owner.cancel();
        assert!(owner.state.lock().unwrap().pending.is_none());
    }
}
