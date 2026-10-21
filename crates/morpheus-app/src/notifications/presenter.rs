//! Native banner presentation with click activation and, where the pinned
//! `notify-rust` backend supports it, withdrawal.
//!
//! A shown banner is identified to the rest of Morpheus only by an opaque
//! token. Listener threads report `Activated(token)` or `Closed(token)` and
//! hold no message text after the banner is handed to the OS. Listeners are
//! detached threads, bounded by `MAX_LISTENERS`, so a banner the user never
//! touches cannot hold back process exit. With every listener taken, the
//! caller withdraws the oldest banner it can (on Linux), and a new banner
//! waits briefly for that banner's listener; otherwise it is shown without
//! click handling.
//!
//! - Linux (desktop notification service): click via the `default` action,
//!   withdrawal via `CloseNotification`.
//! - macOS (Notification Center): click activation; no withdrawal API.
//! - Windows (toast): click activation while the toast exists; no withdrawal API.
//!
//! A guarded show (`show_guarded`, incoming calls only) also checks its
//! caller's handover right before the backend's actual send; see `Handover`.

use std::sync::atomic::{AtomicUsize, Ordering};

use notify_rust::{Notification, NotificationResponse};
use tokio::{runtime::Handle, sync::mpsc};

use super::PlannedNotification;

/// Banners whose click is listened for at the same time.
const MAX_LISTENERS: usize = 8;
/// How long a new banner waits for the listener a withdrawn banner releases.
#[cfg(all(unix, not(target_os = "macos")))]
const LISTENER_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

static LISTENERS: AtomicUsize = AtomicUsize::new(0);

/// Whether every listener is taken, so a new banner would get no click
/// handling.
pub(super) fn listeners_full() -> bool {
    LISTENERS.load(Ordering::Acquire) >= MAX_LISTENERS
}

/// What a banner's listener reports for its token. Shared with other
/// Morpheus surfaces through `notifications` (incoming calls), each of which
/// gives the presenter its own event channel and token space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenterEvent {
    Activated(u64),
    Closed(u64),
    /// The backend failed the send it was handed: only a guarded macOS
    /// listener, where the send is inside the wait, reports this.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Failed(u64),
}

/// Closes one shown banner. Dropping it leaves the banner in place.
#[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
pub(crate) struct Withdrawal(tokio::sync::oneshot::Sender<()>);

impl Withdrawal {
    pub(crate) fn withdraw(self) {
        let _ = self.0.send(());
    }

    /// A handle, and what its banner's listener would receive.
    #[cfg(test)]
    pub(crate) fn for_test() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (send, receive) = tokio::sync::oneshot::channel();
        (Self(send), receive)
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

    /// A slot, waiting up to `wait` for one a withdrawn banner's listener is
    /// releasing.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn reserve_within(wait: std::time::Duration) -> Option<Self> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Some(listener) = Self::reserve() {
                return Some(listener);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
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

/// A guarded macOS send failed. Bounded like `report`.
#[cfg(target_os = "macos")]
fn report_failure(events: &mpsc::Sender<PresenterEvent>, token: u64) {
    let _ = events.try_send(PresenterEvent::Failed(token));
}

/// Checked on the thread that hands a banner to the OS, right before the
/// backend's actual send: `false` refuses it. It may block for a bounded time
/// (the caller's own sample of the window), so it never runs on the UI thread.
/// Linux and Windows send inside `Notification::show`, so the check precedes
/// that call. The macOS backend sends only when its handle is waited on or
/// dropped, so there the check runs in the listener thread right before the
/// wait, a refused banner never builds a handle that a drop would send, and a
/// banner with no free listener fails instead of being sent unobserved. Only
/// the incoming-call banners (incoming_ring::desktop) pass one; message
/// banners are shown as before.
pub(crate) type Handover = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// What a guarded show did.
pub(crate) enum Guarded {
    /// The handover check failed before anything reached the OS.
    Refused,
    /// Handed to the backend. A withdrawal handle comes back only on Linux,
    /// and only when a listener slot was reserved and its thread started; no
    /// other banner can be withdrawn. On macOS it was handed to its listener,
    /// which sends it only once the handover check passes there, or never.
    Handed(Option<Withdrawal>),
}

/// Hand one banner to the OS. Blocking; call from a blocking worker. Returns
/// a withdrawal handle where the platform supports closing it.
pub(super) fn show(
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
) -> Result<Option<Withdrawal>, String> {
    match present(plan, token, events, runtime, None)? {
        Guarded::Handed(withdrawal) => Ok(withdrawal),
        Guarded::Refused => Ok(None),
    }
}

/// `show`, checking `handover` at the actual send (see `Handover`). Blocking;
/// call from a blocking thread.
#[cfg(feature = "native-calls")]
pub(super) fn show_guarded(
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
    handover: Handover,
) -> Result<Guarded, String> {
    present(plan, token, events, runtime, Some(handover))
}

fn present(
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
    handover: Option<Handover>,
) -> Result<Guarded, String> {
    let mut notification = Notification::new();
    notification.summary(&plan.summary).body(&plan.body);
    // Linux and Windows: the send is inside `show`, right after this.
    #[cfg(not(target_os = "macos"))]
    let refused = || handover.as_ref().is_some_and(|handover| !handover());

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
        if let Some(handover) = handover {
            // A guarded banner is sent only by a listener that can report its
            // result. With none free it fails closed before any handle exists:
            // a handle dropped here would send without a result to observe.
            let Some(listener) = Listener::reserve() else {
                return Err("no free listener".into());
            };
            // The notification moves into the listener unsent. On spawn
            // failure it is dropped as plain data: no handle, no send.
            return std::thread::Builder::new()
                .name("morpheus-notification".into())
                .spawn(move || {
                    let _listener = listener;
                    // The actual send is inside wait_for_response. Refused
                    // here, no handle is built, so no drop sends it either.
                    if !handover() {
                        return;
                    }
                    let Ok(handle) = notification.show() else {
                        report_failure(&events, token);
                        return;
                    };
                    let mut was_activated = false;
                    let sent = handle.wait_for_response(|response: &NotificationResponse| {
                        was_activated = activated(response);
                    });
                    // The send is inside that wait: its error is reported,
                    // never ignored.
                    if sent.is_ok() {
                        report(&events, token, was_activated);
                    } else {
                        report_failure(&events, token);
                    }
                })
                .map(|_| Guarded::Handed(None))
                .map_err(|_| "no listener thread".to_owned());
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
        Ok(Guarded::Handed(None))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = &runtime;
        notification.app_id("io.github.brk275.morpheus");
        if plan.sound {
            notification.sound_name("IM");
        }
        if refused() {
            return Ok(Guarded::Refused);
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
        Ok(Guarded::Handed(None))
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
        if refused() {
            return Ok(Guarded::Refused);
        }
        let handle = notification.show().map_err(|error| error.to_string())?;
        let Some(listener) = Listener::reserve_within(LISTENER_WAIT) else {
            return Ok(Guarded::Handed(None));
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
        Ok(Guarded::Handed(spawned.ok().map(|_| Withdrawal(withdraw))))
    }
}
