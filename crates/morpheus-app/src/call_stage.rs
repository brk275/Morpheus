//! Native call-stage presentation. The private clear below is compiled into
//! every build, so detaching or replacing an account always empties the stage
//! synchronously; the presenter that fills it exists only with native calls.

use slint::{Image, Model, ModelRc, VecModel};

use crate::{AppWindow, CallParticipantItem, CallTileItem};

#[cfg(feature = "native-calls")]
mod presenter;

#[cfg(feature = "native-calls")]
pub use presenter::bind;

/// Synchronously empty every call-stage tile, featured row, picture, roster
/// row and page, then make the presenter forget its layout and pin targets
/// and fence out updates already queued for the call generations it showed.
/// Call on the UI thread at an account detach or change, after call
/// authority is revoked. Capture and the call controller clean up separately.
pub(crate) fn clear_private_presentation(ui: &AppWindow) {
    clear_stage(ui);
    if ui.get_native_call_participants().row_count() > 0 {
        ui.set_native_call_participants(ModelRc::new(VecModel::from(Vec::<
            CallParticipantItem,
        >::new())));
    }
    // A no-op without native calls, where nothing can fill the stage.
    ui.invoke_reset_native_call_stage();
}

/// Empty the stage's tiles, featured row, paging and every frame slot.
fn clear_stage(ui: &AppWindow) {
    ui.set_native_call_featured_visible(false);
    ui.set_native_call_featured(CallTileItem::default());
    if ui.get_native_call_tiles().row_count() > 0 {
        ui.set_native_call_tiles(ModelRc::new(VecModel::from(Vec::<CallTileItem>::new())));
    }
    ui.set_native_call_page(0);
    ui.set_native_call_pages(1);
    ui.set_native_call_tile_count(0);
    ui.set_native_call_media_degraded(false);
    let frames = ui.get_native_call_frames();
    for slot in 0..frames.row_count() {
        frames.set_row_data(slot, Image::default());
    }
}
