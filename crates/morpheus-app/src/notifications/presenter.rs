//! Native submission on the fixed retained actual-worker book. Nothing is sent
//! by Drop, Tokio blocking threads, a detached listener, or an OS callback.

pub(crate) use super::native::Withdrawal;
use super::{
    PlannedNotification,
    native::{Admission, NativeBook, Source, WorkerResult},
};
use notify_rust::{
    Notification,
    owned::{Mode, Outcome},
};
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use tokio::{runtime::Handle, sync::mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenterEvent {
    Activated(u64),
    Closed(u64),
    Failed(u64),
}
/// Consumed once at true backend send handoff, after preparation awaits.
/// The closure retains original submission authority, never a same-ID recapture.
pub(crate) type Handover = Arc<dyn Fn() -> bool + Send + Sync>;
pub(crate) enum Guarded {
    Refused,
    Handed(Option<Withdrawal>),
}

pub(super) async fn show(
    book: &NativeBook,
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
    handover: Handover,
) -> Result<Guarded, ()> {
    present(
        book,
        plan,
        token,
        events,
        runtime,
        handover,
        Source::Message,
        false,
    )
    .await
}
#[cfg(feature = "native-calls")]
pub(super) async fn show_guarded(
    book: &NativeBook,
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
    handover: Handover,
) -> Result<Guarded, ()> {
    // Preserve the prior guarded-Mac no-listener refusal. Other platforms'
    // original no-free-listener fallback remains explicit SubmitOnly.
    present(
        book,
        plan,
        token,
        events,
        runtime,
        handover,
        Source::Incoming,
        cfg!(target_os = "macos"),
    )
    .await
}
async fn present(
    book: &NativeBook,
    plan: PlannedNotification,
    token: u64,
    events: mpsc::Sender<PresenterEvent>,
    runtime: Handle,
    handover: Handover,
    source: Source,
    require_listener: bool,
) -> Result<Guarded, ()> {
    let operation = book
        .launch(
            &runtime,
            source,
            require_listener,
            token,
            events,
            move |identity, cancellation, listener, callbacks| {
                if cancellation.is_cancelled() || !callbacks.can_submit() {
                    callbacks.finish(true, false);
                    return WorkerResult::released_without_entry();
                }
                let mut notification = Notification::new();
                notification.summary(&plan.summary).body(&plan.body);
                #[cfg(target_os = "macos")]
                {
                    static REGISTERED: OnceLock<bool> = OnceLock::new();
                    if !*REGISTERED.get_or_init(|| {
                        notify_rust::set_application("io.github.brk275.morpheus").is_ok()
                    }) {
                        callbacks.finish(false, true);
                        return WorkerResult::released_without_entry();
                    }
                    if plan.sound {
                        notification.sound_name("NSUserNotificationDefaultSoundName");
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    notification.app_id("io.github.brk275.morpheus");
                    if plan.sound {
                        notification.sound_name("IM");
                    }
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    notification
                        .appname("Morpheus")
                        .icon("io.github.brk275.morpheus")
                        .action("default", "Open");
                    if plan.sound {
                        notification.sound_name("message-new-instant");
                    } else {
                        notification.hint(notify_rust::Hint::SuppressSound(true));
                    }
                }
                drop(plan);
                // This fresh std worker has never entered Tokio. Handover may use
                // Handle::block_on internally but releases that context before the
                // entered native call resumes. No Handle survives response wait.
                let mut submitted = callbacks.clone();
                let responding = callbacks.clone();
                let admission = callbacks.clone();
                let result = notification.show_owned_with_response(
                    identity,
                    cancellation,
                    if listener {
                        Mode::Response
                    } else {
                        Mode::SubmitOnly
                    },
                    move || admission.handover(move || handover()),
                    move || submitted.submitted(),
                    move |response| responding.response(response),
                );
                let refused =
                    matches!(&result, Ok(report) if matches!(&report.outcome, Outcome::Refused));
                let failed = result.is_err();
                let result = WorkerResult::backend(result);
                // Close later native admission before any failure response can
                // wake another app submission. Already-submitted click gates
                // keep their separate fresh protected activation authority.
                callbacks.record_result(&result);
                callbacks.finish(refused, failed);
                result
            },
        )
        .ok_or(())?;
    match operation.wait().await {
        Admission::Refused => Ok(Guarded::Refused),
        Admission::Handed(withdrawal) => Ok(Guarded::Handed(withdrawal)),
        Admission::Unavailable => Err(()),
    }
}
