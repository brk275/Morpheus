//! Native notification controls and navigation from desktop banners.
//!
//! - `rules`: Settings › Notifications privacy switch, the account's default
//!   levels and searchable per-room Matrix push rules.
//! - `activation`: opening a clicked banner through the window's
//!   `open-conversation` navigation seam (exact account, room, event and
//!   thread behind the protected account switch).
//! - `read_markers`: withdrawal of banners for rooms read on another device.
//!   Receipts for the conversation on screen are `crate::unread_navigation`.
//!
//! Delivery itself stays in `crate::notifications`. Nothing here keeps
//! message text, persists notification history or runs after Morpheus quits.

mod activation;
mod read_markers;
mod rules;

use std::sync::Arc;

use morpheus_config::{Config, ConfigStore};
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
    runtime.spawn(read_markers::run(live, notifications));
    NotificationCenter {
        _activator: activator,
        _rules: rules,
    }
}

/// Apply an accepted configuration to the notification controls.
pub(crate) fn apply_config(ui: &AppWindow, config: &Config) {
    rules::apply_config(ui, config);
}
