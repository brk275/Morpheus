//! Native banner presentation with click activation and, where the pinned
//! `notify-rust` backend supports it, withdrawal.
//!
//! A shown banner is identified to the rest of Morpheus only by an opaque
//! token. Listener threads report `Activated(token)` or `Closed(token)` and
//! hold no message text after the banner is handed to the OS. Listeners are
//! detached threads, bounded by `MAX_LISTENERS`, so a banner the user never
//! touches cannot hold back process exit; beyond the bound a banner is shown
//! without click handling.
//!
//! - Linux (desktop notification service): click via the `default` action,
//!   withdrawal via `CloseNotification`.
//! - macOS (Notification Center): click activation; no withdrawal API.
//! - Windows (toast): click activation while the toast exists; no withdrawal API.

use std::sync::atomic::{AtomicUsize, Ordering};

use notify_rust::{Notification, NotificationResponse};
use tokio::{runtime::Handle, sync::mpsc};

use super::PlannedNotification;

/// Banners whose click is listened for at the same time.
const MAX_LISTENERS: usize = 8;

static LISTENERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PresenterEvent {
    Activated(u64),
    Closed(u64),
}

/// Closes one shown banner. Dropping it leaves the banner in place.
#[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
pub(super) struct Withdrawal(tokio::sync::oneshot::Sender<()>);

impl Withdrawal {
    pub(super) fn withdraw(self) {
        let _ = self.0.send(());
    }
}

/// One reserved listener slot, released when its thread ends.
struct Listener;

impl Listener {
    fn reserve() -> Option<Self> {
        LISTENERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_LISTENERS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        LISTENERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn activated(response: &NotificationResponse) -> bool {
    matches!(
        response,
        NotificationResponse::Default | NotificationResponse::Action(_)
    )
}

fn report(events: &mpsc::Sender<PresenterEvent>, token: u64, was_activated: bool) {
    // Bounded: a full queue drops the report; an unreported click does nothing.
    let _ = events.try_send(if was_activated {
        PresenterEvent::Activated(token)
    } else {
        PresenterEvent::Closed(token)
    });
}

/// Hand one banner to the OS. Blocking; call from a blocking worker. Returns
/// a withdrawal handle where the platform supports closing it.
pub(super) fn show(
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
) -> Result<Option<Withdrawal>, String> {
    let mut notification = Notification::new();
    notification.summary(&plan.summary).body(&plan.body);

    #[cfg(target_os = "macos")]
    {
        let _ = &runtime;
        // The legacy macOS backend falls back to Finder when this ID is not
        // registered. Refuse to present under the wrong application identity.
        static APPLICATION_REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*APPLICATION_REGISTERED
            .get_or_init(|| notify_rust::set_application("io.github.brk275.morpheus").is_ok())
        {
            return Err("Morpheus app bundle is not registered with macOS".into());
        }
        if plan.sound {
            // The macOS backend reserves this exact name for the system sound.
            notification.sound_name("NSUserNotificationDefaultSoundName");
        }
        // This backend delivers when the handle is waited on or dropped.
        let handle = notification.show().map_err(|error| error.to_string())?;
        if let Some(listener) = Listener::reserve() {
            // On spawn failure the closure, and with it the handle, is
            // dropped, which still delivers the banner without a listener.
            let _ = std::thread::Builder::new()
                .name("morpheus-notification".into())
                .spawn(move || {
                    let _listener = listener;
                    let mut was_activated = false;
                    let _ = handle.wait_for_response(|response: &NotificationResponse| {
                        was_activated = activated(response);
                    });
                    report(&events, token, was_activated);
                });
        }
        Ok(None)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = &runtime;
        notification.app_id("io.github.brk275.morpheus");
        if plan.sound {
            notification.sound_name("IM");
        }
        let handle = notification.show().map_err(|error| error.to_string())?;
        if let Some(listener) = Listener::reserve() {
            let _ = std::thread::Builder::new()
                .name("morpheus-notification".into())
                .spawn(move || {
                    let _listener = listener;
                    let mut was_activated = false;
                    let _ = handle.wait_for_response(|response: &NotificationResponse| {
                        was_activated = activated(response);
                    });
                    report(&events, token, was_activated);
                });
        }
        Ok(None)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        notification
            .appname("Morpheus")
            .icon("io.github.brk275.morpheus")
            // Servers invoke "default" for a click on the banner body.
            .action("default", "Open");
        if plan.sound {
            notification.sound_name("message-new-instant");
        } else {
            notification.hint(notify_rust::Hint::SuppressSound(true));
        }
        let handle = notification.show().map_err(|error| error.to_string())?;
        let Some(listener) = Listener::reserve() else {
            return Ok(None);
        };
        let (withdraw, withdrawn) = tokio::sync::oneshot::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("morpheus-notification".into())
            .spawn(move || {
                let _listener = listener;
                let mut was_activated = false;
                // The D-Bus futures run on zbus's own executor; this thread
                // only parks. A dropped withdrawal sender leaves the banner.
                runtime.block_on(async {
                    tokio::select! {
                        _ = handle.wait_for_action_async(|response: &NotificationResponse| {
                            was_activated = activated(response);
                        }) => {}
                        Ok(()) = withdrawn => handle.close_async().await,
                    }
                });
                report(&events, token, was_activated);
            });
        Ok(spawned.ok().map(|_| Withdrawal(withdraw)))
    }
}
