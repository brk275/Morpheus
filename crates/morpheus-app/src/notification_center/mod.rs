//! Native notification controls and navigation from desktop banners.
//!
//! - `rules`: Settings › Notifications privacy switch, the account's default
//!   levels and searchable per-room Matrix push rules.
//! - `activation`: opening a clicked banner through the window's
//!   `open-conversation` navigation seam (exact account, room, event and
//!   thread behind the protected account switch).
//! - `read_markers`: private read receipts for the room on screen and
//!   withdrawal of banners for rooms that were read.
//!
//! Delivery itself stays in `crate::notifications`. Nothing here keeps
//! message text, persists notification history or runs after Morpheus quits.

mod activation;
mod read_markers;
mod rules;

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
        store,
        runtime.clone(),
    );
    let activator = Activator::register(ui, runtime.clone(), notifications.take_activations());
    runtime.spawn(read_markers::run(live, notifications, ui.as_weak()));
    NotificationCenter {
        _activator: activator,
        _rules: rules,
    }
}

/// Apply an accepted configuration to the notification controls.
pub(crate) fn apply_config(ui: &AppWindow, config: &Config) {
    rules::apply_config(ui, config);
}

/// Whether the window is the active one, holding the keyboard focus or
/// showing one of its popups. Public Slint 1.18 has no API for this, so the
/// window state the pinned version keeps from the windowing system is read.
/// UI thread only.
pub(crate) fn window_active(ui: &AppWindow) -> bool {
    slint::private_unstable_api::re_exports::WindowInner::from_pub(ui.window()).active()
}
