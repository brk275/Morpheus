//! Fills the native call stage. The video task publishes a revisioned layout
//! beside fixed frame slots; this maps a changed layout into Slint tile rows
//! and copies only changed pictures into their slots. At most one stage
//! update waits on the event loop, so a slow frame never accumulates closures
//! that own pixel buffers. The participant roster beside the stage is filled
//! here too, so both obey the same account fence.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use morpheus_rtc::{ParticipantInfo, RtcStreamKind};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use tokio::{runtime::Handle, sync::oneshot};

use super::clear_stage;
use crate::{
    AppWindow, CallParticipantItem, CallTileItem,
    calls::CallController,
    live::{accent, initials},
    video_render::{FRAME_SLOTS, StageLayout, TileMedium, TileState, TileView, VideoUpdate},
};

/// Minimum spacing of stage updates: about thirty per second for all tiles.
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(33);

/// What the window currently shows. Only the UI thread changes it.
#[derive(Default)]
struct Applied {
    token: u64,
    /// The video task instance (one per join) whose stage is shown.
    presentation: u64,
    revision: u64,
    layout: Option<Arc<StageLayout>>,
    /// Epoch and sequence of the picture shown in each slot.
    frames: [(u64, u64); FRAME_SLOTS],
    roster_token: u64,
    /// Stage and roster updates of this call generation or an older one never
    /// render again: the account they belonged to was detached or replaced.
    fence: u64,
}

impl Applied {
    fn fenced(&self, token: u64) -> bool {
        token != 0 && token <= self.fence
    }
}

/// Install the frame-slot model, stage callbacks and the presenter tasks.
pub fn bind(ui: &AppWindow, runtime: &Handle, calls: Arc<CallController>) {
    ui.set_native_call_frames(ModelRc::new(VecModel::from(vec![
        Image::default();
        FRAME_SLOTS
    ])));
    let applied = Arc::new(Mutex::new(Applied::default()));
    {
        let calls = calls.clone();
        let applied = applied.clone();
        ui.on_pin_native_call_tile(move |id| {
            let applied = lock(&applied);
            let Some(tile) = applied
                .layout
                .as_deref()
                .and_then(|layout| find_tile(layout, id))
            else {
                return;
            };
            if tile.is_local && !tile.pinned {
                return;
            }
            // Activating the pinned tile, or a departed placeholder, unpins.
            let pinned = (!tile.pinned).then(|| tile.key.clone());
            calls.choose_stage(applied.token, |request| {
                request.pinned = pinned;
                request.page = 0;
            });
        });
    }
    {
        let calls = calls.clone();
        let applied = applied.clone();
        ui.on_page_native_call_tiles(move |delta| {
            let applied = lock(&applied);
            let Some(layout) = applied.layout.as_deref() else {
                return;
            };
            let last = layout.pages.max(1) - 1;
            let page = layout
                .page
                .saturating_add_signed(isize::try_from(delta).unwrap_or_default())
                .min(last);
            if page != layout.page {
                calls.choose_stage(applied.token, |request| request.page = page);
            }
        });
    }
    {
        let calls = calls.clone();
        ui.on_native_call_stage_presented(move |presented| calls.set_stage_presented(presented));
    }
    {
        // Invoked by `clear_private_presentation` after it emptied the window.
        // Forget the shown layout and pin targets, and fence out every call
        // generation shown so far or still current, so an update that was
        // already queued for the departed account cannot repopulate it.
        let calls = calls.clone();
        let applied = applied.clone();
        ui.on_reset_native_call_stage(move || {
            let mut applied = lock(&applied);
            let fence = applied
                .fence
                .max(applied.token)
                .max(applied.roster_token)
                .max(calls.video_token());
            *applied = Applied {
                fence,
                ..Applied::default()
            };
        });
    }
    calls.set_stage_presented(ui.get_page() == 2);
    runtime.spawn(present_roster(calls.clone(), ui.as_weak(), applied.clone()));
    runtime.spawn(present(calls, ui.as_weak(), applied));
}

async fn present(
    calls: Arc<CallController>,
    window: slint::Weak<AppWindow>,
    applied: Arc<Mutex<Applied>>,
) {
    let mut video = calls.subscribe_video();
    loop {
        let update = video.borrow_and_update().clone();
        let (done, shown) = oneshot::channel();
        let calls_for_ui = calls.clone();
        let applied_for_ui = applied.clone();
        let posted = window.upgrade_in_event_loop(move |ui| {
            apply(&ui, &calls_for_ui, &mut lock(&applied_for_ui), update);
            let _ = done.send(());
        });
        // While the event loop is busy, newer pictures replace older ones in
        // the watch instead of queueing behind this update.
        if posted.is_err() || shown.await.is_err() {
            break;
        }
        tokio::time::sleep(MIN_UPDATE_INTERVAL).await;
        if video.changed().await.is_err() {
            break;
        }
    }
}

async fn present_roster(
    calls: Arc<CallController>,
    window: slint::Weak<AppWindow>,
    applied: Arc<Mutex<Applied>>,
) {
    let mut roster = calls.subscribe_roster();
    loop {
        let update = roster.borrow_and_update().clone();
        let token = update.token;
        let members: Vec<_> = update
            .members
            .into_iter()
            .map(participant_item)
            .collect();
        let calls_for_ui = calls.clone();
        let applied_for_ui = applied.clone();
        let closed = window
            .upgrade_in_event_loop(move |ui| {
                apply_roster(&ui, &calls_for_ui, &mut lock(&applied_for_ui), token, members);
            })
            .is_err();
        if closed || roster.changed().await.is_err() {
            break;
        }
    }
}

fn lock(applied: &Mutex<Applied>) -> MutexGuard<'_, Applied> {
    applied.lock().unwrap_or_else(|error| error.into_inner())
}

fn apply(ui: &AppWindow, calls: &CallController, applied: &mut Applied, update: VideoUpdate) {
    // The private clear already emptied the window for this generation.
    if applied.fenced(update.token()) {
        return;
    }
    let current = calls.video_token();
    match update {
        VideoUpdate::Stage { token, stage } if token != 0 && token == current => {
            // A rejoin keeps the call token but starts a new video task whose
            // revisions and slot epochs begin again; never mix the two.
            if applied.token != token || applied.presentation != stage.presentation {
                clear(ui, applied);
                applied.token = token;
                applied.presentation = stage.presentation;
            }
            if applied.layout.is_none() || applied.revision != stage.revision {
                apply_layout(ui, &stage.layout);
                applied.revision = stage.revision;
                applied.layout = Some(Arc::clone(&stage.layout));
            }
            let frames = ui.get_native_call_frames();
            for (slot, frame) in stage.frames.iter().enumerate() {
                let picture = (frame.epoch, frame.sequence);
                if applied.frames[slot] != picture {
                    applied.frames[slot] = picture;
                    let image = frame
                        .pixels
                        .clone()
                        .map(Image::from_rgb8)
                        .unwrap_or_default();
                    frames.set_row_data(slot, image);
                }
            }
        }
        // A queued update can belong to a call that already ended. Clear
        // once for the current generation, never over a newer call's stage.
        VideoUpdate::Stage { .. } => {
            if applied.token != current {
                clear(ui, applied);
                applied.token = current;
            }
        }
        VideoUpdate::Clear { token } => {
            if token == current || applied.token != current {
                clear(ui, applied);
                applied.token = current;
            }
        }
    }
}

fn apply_roster(
    ui: &AppWindow,
    calls: &CallController,
    applied: &mut Applied,
    token: u64,
    members: Vec<CallParticipantItem>,
) {
    if applied.fenced(token) {
        return;
    }
    let current = calls.video_token();
    if token != current {
        if applied.roster_token != current {
            ui.set_native_call_participants(ModelRc::new(VecModel::from(Vec::<
                CallParticipantItem,
            >::new())));
            applied.roster_token = current;
        }
        return;
    }
    ui.set_native_call_participants(ModelRc::new(VecModel::from(members)));
    applied.roster_token = current;
}

/// Forget the shown stage and empty it, including a previous call's pictures.
fn clear(ui: &AppWindow, applied: &mut Applied) {
    applied.presentation = 0;
    applied.revision = 0;
    applied.layout = None;
    applied.frames = [(0, 0); FRAME_SLOTS];
    clear_stage(ui);
}

fn apply_layout(ui: &AppWindow, layout: &StageLayout) {
    match &layout.featured {
        Some(tile) => {
            ui.set_native_call_featured(tile_item(tile));
            ui.set_native_call_featured_visible(true);
        }
        None => {
            ui.set_native_call_featured_visible(false);
            ui.set_native_call_featured(CallTileItem::default());
        }
    }
    let rows: Vec<_> = layout.tiles.iter().map(tile_item).collect();
    let model = ui.get_native_call_tiles();
    let same_tiles = model.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, row)| {
            model
                .row_data(index)
                .is_some_and(|shown| shown.id == row.id)
        });
    if same_tiles {
        // Keep each tile element, and its keyboard focus, when only its
        // details change.
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
    } else {
        ui.set_native_call_tiles(ModelRc::new(VecModel::from(rows)));
    }
    ui.set_native_call_page(to_int(layout.page));
    ui.set_native_call_pages(to_int(layout.pages.max(1)));
    ui.set_native_call_tile_count(to_int(layout.tile_count));
    ui.set_native_call_media_degraded(layout.media_degraded);
}

fn find_tile(layout: &StageLayout, id: i32) -> Option<&TileView> {
    let id = u64::try_from(id).ok()?;
    layout
        .featured
        .iter()
        .chain(&layout.tiles)
        .find(|tile| tile.id == id)
}

fn to_int(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn tile_item(tile: &TileView) -> CallTileItem {
    CallTileItem {
        // Zero is never a tile ID, so an unrepresentable ID cannot be pinned.
        id: i32::try_from(tile.id).unwrap_or_default(),
        name: if tile.is_local {
            SharedString::from("You")
        } else {
            SharedString::from(tile.user_id.as_str())
        },
        initials: initials(&tile.user_id).into(),
        accent: accent(&tile.user_id),
        status: status(tile).into(),
        frame_slot: tile
            .frame_slot
            .and_then(|slot| i32::try_from(slot).ok())
            .unwrap_or(-1),
        live: tile.state == TileState::Live,
        departed: matches!(tile.state, TileState::Left | TileState::Ended),
        screen: tile.key.medium == TileMedium::Screen,
        pinned: tile.pinned,
        is_local: tile.is_local,
        mic_muted: tile.microphone_muted,
        speaking: tile.speaking,
        hand_raised: tile.hand_raised,
    }
}

/// Placeholder text for a tile without a picture.
fn status(tile: &TileView) -> &'static str {
    let screen = tile.key.medium == TileMedium::Screen;
    match tile.state {
        TileState::Live | TileState::Connecting => "Connecting video…",
        TileState::Muted if screen => "Screen share paused",
        TileState::Muted => "Camera paused",
        TileState::Off => "Camera off",
        TileState::Unavailable => "Media unavailable",
        TileState::Undecryptable => "Video can't be decrypted yet",
        TileState::Left => "Left the call",
        TileState::Ended => "Screen share ended",
        TileState::Local if screen => "Your screen share is paused",
        TileState::Local => "Your camera is off",
        TileState::LocalPublishing if screen => "You are sharing your screen",
        TileState::LocalPublishing => "Your camera is on",
    }
}

fn participant_item(participant: ParticipantInfo) -> CallParticipantItem {
    let has_live = |kind| {
        participant
            .streams
            .iter()
            .any(|stream| stream.kind == kind && !stream.muted)
    };
    let microphone = participant
        .streams
        .iter()
        .find(|stream| stream.kind == RtcStreamKind::Microphone);
    let mut media = Vec::new();
    if has_live(RtcStreamKind::ScreenShare) {
        media.push("Sharing screen");
    } else if has_live(RtcStreamKind::Camera) {
        media.push("Camera on");
    }
    match microphone {
        Some(stream) if stream.muted => media.push("Mic muted"),
        Some(_) => media.push("Mic on"),
        None => {}
    }
    let detail = if !participant.reachable {
        "Media unavailable".to_owned()
    } else if media.is_empty() {
        "In call".to_owned()
    } else {
        media.join(" · ")
    };
    CallParticipantItem {
        name: participant.user_id.into(),
        detail: detail.into(),
        is_local: participant.is_local,
    }
}
