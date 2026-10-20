//! Native notification controls and navigation from desktop banners.
//!
//! - `rules`: Settings › Notifications privacy switch, the account's default
//!   levels and searchable per-room Matrix push rules.
//! - `activation`: opening a clicked banner through the window's
//!   `open-conversation` navigation seam (exact account, room, event and
//!   thread behind the protected account switch).
//! - `read_markers`: withdrawal of banners for rooms read here or elsewhere;
//!   it sends no receipts (the opt-in room activity path is the only
//!   automatic receipt sender).
//! - `room_panel`: the room header's bell and panel for the open room: its
//!   Matrix push-rule level and this device's sound, preview and highlight
//!   switches for it.
//!
//! Delivery itself stays in `crate::notifications`. Nothing here keeps
//! message text, persists notification history or runs after Morpheus quits.

mod activation;
mod read_markers;
mod room_panel;
mod rules;
mod uncertainty;

use std::sync::Arc;

use morpheus_config::{Config, ConfigStore};
use slint::ComponentHandle;
use tokio::runtime::Handle;

use crate::{AppWindow, live::LiveUi, notifications::DesktopNotifications};

use activation::Activator;

/// Keeps the notification controllers alive for the window's lifetime.
pub(crate) struct NotificationCenter {
    _activator: Arc<Activator>,
    _rules: Arc<rules::Rules>,
    _room_panel: Arc<room_panel::RoomPanel>,
}

pub(crate) fn register(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    store: ConfigStore,
    runtime: &Handle,
) -> NotificationCenter {
    let rules = rules::Rules::register(
        ui,
        live.clone(),
        notifications.clone(),
        store.clone(),
        runtime.clone(),
    );
    let room_panel = room_panel::RoomPanel::register(
        ui,
        live.clone(),
        notifications.clone(),
        rules.clone(),
        store,
        runtime.clone(),
    );
    let activator = Activator::register(ui, runtime.clone(), notifications.take_activations());
    runtime.spawn(read_markers::run(live, notifications));
    NotificationCenter {
        _activator: activator,
        _rules: rules,
        _room_panel: room_panel,
    }
}

/// Apply an accepted configuration to the notification controls.
pub(crate) fn apply_config(ui: &AppWindow, config: &Config) {
    rules::apply_config(ui, config);
    room_panel::apply_config(ui, config);
}

/// Whether the window is the active one, holding the keyboard focus or
/// showing one of its popups. Public Slint 1.18 has no API for this, so the
/// window state the pinned version keeps from the windowing system is read.
/// UI thread only.
pub(crate) fn window_active(ui: &AppWindow) -> bool {
    slint::private_unstable_api::re_exports::WindowInner::from_pub(ui.window()).active()
}
