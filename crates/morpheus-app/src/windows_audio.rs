//! Ordinary Windows quit retains its original origin apartment and cleanup
//! task while Slint pumps. Only actual app/native receipts permit loop exit.
use crate::{
    AppWindow,
    app_shutdown::{Failure, Report},
};
use morpheus_rtc::{WasapiCloseError, WasapiCloseObservation, WasapiUiContext};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

type CleanupFuture<R = Report> = Pin<Box<dyn Future<Output = R> + Send>>;
type Transfer<R = Report> = Arc<Mutex<Option<CleanupFuture<R>>>>;
static REQUESTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    // The SAME original slot now retains the origin from attach, including
    // early startup/unexpected-loop failures. It is not another task registry.
    static START: RefCell<Option<Rc<Registration>>> = const { RefCell::new(None) };
}
struct Registration {
    origin: RefCell<WasapiUiContext>,
    start: RefCell<Option<Rc<dyn Fn()>>>,
    cleanup: RefCell<Option<Rc<RefCell<State>>>>,
    observation: tokio::sync::watch::Sender<Result<WasapiCloseObservation, Failure>>,
    failed: Cell<Option<Failure>>,
    native_unproved: Cell<bool>,
    quit_attempted: Cell<bool>,
    quit_posted: Cell<bool>,
}
pub(crate) struct Origin(Rc<Registration>);

/// A fixed typed observation from this exact original UI origin. It carries
/// no COM pointer, opens no native operation, and cannot create a release fact.
#[derive(Clone)]
pub(crate) struct OriginReceipt {
    actual: tokio::sync::watch::Receiver<Result<WasapiCloseObservation, Failure>>,
}
impl Origin {
    pub(crate) fn receipt(&self) -> OriginReceipt {
        OriginReceipt {
            actual: self.0.observation.subscribe(),
        }
    }
}
impl OriginReceipt {
    pub(crate) fn observation(&self) -> Result<WasapiCloseObservation, Failure> {
        if self.actual.has_changed().is_err() {
            return Err(Failure::Ownership);
        }
        *self.actual.borrow()
    }
}

fn relay_native_observation(
    actual: &tokio::sync::watch::Sender<Result<WasapiCloseObservation, Failure>>,
    requested: bool,
    observation: Result<WasapiCloseObservation, Failure>,
) {
    if requested {
        // The only production native value comes directly from original
        // begin_shutdown -> poll on the original UI apartment below.
        actual.send_replace(observation);
    }
}

pub(crate) fn coordinator_available() -> bool {
    registration().is_some_and(|actual| {
        actual.failed.get().is_none()
            && actual.start.try_borrow().is_ok_and(|start| start.is_some())
    })
}
fn registration() -> Option<Rc<Registration>> {
    START.with(|slot| slot.try_borrow().ok()?.clone())
}
pub(crate) fn request_shutdown(
    gate: &crate::security::SecurityGate,
    restart: Option<&crate::security::RestartLease>,
) -> bool {
    if !coordinator_available() {
        return false;
    }
    let Some(admission) = gate.admit_shutdown(restart) else {
        return false;
    };
    request_admitted_shutdown(admission)
}
/// The real controller releases its mutex before this synchronous UI call.
pub(crate) fn request_admitted_shutdown(
    _admission: crate::security::ShutdownAdmission<'_>,
) -> bool {
    let Some(actual) = registration() else {
        return false;
    };
    let start = actual
        .start
        .try_borrow()
        .ok()
        .and_then(|start| start.clone());
    let Some(start) = start else {
        return false;
    };
    if actual.failed.get().is_some() {
        return false;
    }
    morpheus_rtc::begin_wasapi_shutdown();
    REQUESTED.store(true, Ordering::Release);
    start();
    true
}

/// Attach publishes the actual origin before any audio/app worker can use it.
/// Native admission remains refused until run arms the one cleanup closure.
pub(crate) fn attach() -> Result<Origin, WasapiCloseError> {
    if registration().is_some() {
        return Err(WasapiCloseError::Apartment);
    }
    let (observation, _) = tokio::sync::watch::channel(Ok(WasapiCloseObservation::Pending));
    let actual = Rc::new(Registration {
        origin: RefCell::new(WasapiUiContext::attach()?),
        start: RefCell::new(None),
        cleanup: RefCell::new(None),
        observation,
        failed: Cell::new(None),
        native_unproved: Cell::new(false),
        quit_attempted: Cell::new(false),
        quit_posted: Cell::new(false),
    });
    let published = START.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return false;
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(actual.clone());
        true
    });
    if !published {
        return Err(WasapiCloseError::Apartment);
    }
    Ok(Origin(actual))
}

struct State {
    cleanup: Option<Box<dyn FnOnce() -> CleanupFuture>>,
    transfer: Option<Transfer>,
    task: Option<tokio::task::JoinHandle<Result<Report, Failure>>>,
    result: Option<Result<Report, Failure>>,
    backend: Option<Result<(), WasapiCloseError>>,
}

fn spawn_gated<R: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    transfer: Transfer<R>,
) -> Result<
    (
        tokio::task::JoinHandle<Result<R, Failure>>,
        tokio::sync::oneshot::Sender<()>,
    ),
    Failure,
> {
    let (start, admitted) = tokio::sync::oneshot::channel();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.spawn(async move {
            admitted.await.map_err(|_| Failure::Ownership)?;
            let future = transfer
                .lock()
                .map_err(|_| Failure::Ownership)?
                .take()
                .ok_or(Failure::Ownership)?;
            Ok(future.await)
        })
    })) {
        Ok(task) => Ok((task, start)),
        Err(payload) => {
            std::mem::forget(payload);
            Err(Failure::Ownership)
        }
    }
}

fn poll_task(state: &Rc<RefCell<State>>, actual: &Registration) {
    let ready = {
        let Ok(mut state) = state.try_borrow_mut() else {
            actual.failed.set(Some(Failure::Ownership));
            return;
        };
        let Some(task) = state.task.as_mut() else {
            return;
        };
        let mut context = Context::from_waker(Waker::noop());
        match Pin::new(task).poll(&mut context) {
            Poll::Pending => None,
            Poll::Ready(result) => Some((result, state.task.take())),
        }
    };
    let Some((result, task)) = ready else {
        return;
    };
    // Classify/retire arbitrary panic data with no State borrow held.
    let result = match result {
        Ok(result) => result,
        Err(error) if error.is_panic() => {
            std::mem::forget(error.into_panic());
            Err(Failure::CleanupPanicked)
        }
        Err(_) => Err(Failure::CleanupCancelled),
    };
    drop(task);
    match state.try_borrow_mut() {
        Ok(mut state) if state.result.is_none() => state.result = Some(result),
        _ => actual.failed.set(Some(Failure::Ownership)),
    }
}

pub(crate) struct Outcome {
    pub(crate) ui: Result<(), slint::PlatformError>,
    pub(crate) cleanup: Option<Result<Report, Failure>>,
    pub(crate) backend: Option<Result<(), WasapiCloseError>>,
    pub(crate) retain: bool,
    pub(crate) failure: Option<Failure>,
}

pub(crate) fn run<F>(
    ui: &AppWindow,
    origin: Origin,
    runtime: &tokio::runtime::Runtime,
    cleanup: impl FnOnce() -> F + 'static,
) -> Result<Outcome, Failure>
where
    F: Future<Output = Report> + Send + 'static,
{
    let actual = origin.0;
    if !registration().is_some_and(|registered| Rc::ptr_eq(&registered, &actual)) {
        return Err(Failure::Ownership);
    }
    let state = Rc::new(RefCell::new(State {
        cleanup: Some(Box::new(move || Box::pin(cleanup()))),
        transfer: None,
        task: None,
        result: None,
        backend: None,
    }));
    {
        let mut retained = actual
            .cleanup
            .try_borrow_mut()
            .map_err(|_| Failure::Ownership)?;
        if retained.is_some() {
            return Err(Failure::Ownership);
        }
        *retained = Some(state.clone());
    }
    // The original registration retains the actual State directly, rather
    // than hiding its handles inside an inaccessible retained closure.
    let start_state = Rc::downgrade(&state);
    let handle = runtime.handle().clone();
    let weak_actual = Rc::downgrade(&actual);
    let start: Rc<dyn Fn()> = Rc::new(move || {
        let Some(actual) = weak_actual.upgrade() else {
            return;
        };
        let Some(start_state) = start_state.upgrade() else {
            actual.failed.set(Some(Failure::Ownership));
            return;
        };
        let cleanup = match start_state.try_borrow_mut() {
            Ok(mut state) => state.cleanup.take(),
            Err(_) => {
                actual.failed.set(Some(Failure::Ownership));
                return;
            }
        };
        let Some(cleanup) = cleanup else {
            return;
        };
        let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
            Ok(future) => future,
            Err(payload) => {
                std::mem::forget(payload);
                actual.failed.set(Some(Failure::CleanupPanicked));
                return;
            }
        };
        let transfer = Arc::new(Mutex::new(Some(future)));
        start_state.borrow_mut().transfer = Some(transfer.clone());
        match spawn_gated(&handle, transfer) {
            Ok((task, release)) => {
                // The actual handle is retained before the task can take work.
                start_state.borrow_mut().task = Some(task);
                if release.send(()).is_err() {
                    actual.failed.set(Some(Failure::Ownership));
                }
            }
            Err(failure) => actual.failed.set(Some(failure)),
        }
    });
    {
        let mut slot = actual
            .start
            .try_borrow_mut()
            .map_err(|_| Failure::Ownership)?;
        if slot.is_some() || actual.failed.get().is_some() {
            return Err(Failure::Ownership);
        }
        *slot = Some(start.clone());
    }
    let observed = state.clone();
    let service = actual.clone();
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(10), move || {
        let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // A prior uncertain native unwind/borrow is terminal. Retain the
            // actual context without replaying any possibly-entered API.
            if service.native_unproved.get() { return Err(Failure::Ownership); }
            let mut origin = service.origin.try_borrow_mut().map_err(|_| Failure::Ownership)?;
            let requested_before_poll = REQUESTED.load(Ordering::Acquire);
            if requested_before_poll { origin.begin_shutdown(); }
            Ok::<_, Failure>((requested_before_poll, origin.poll()))
        }));
        let native = match native {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                Err(Failure::Ownership)
            }
        };
        let (requested_before_poll, native) = match native {
            Ok(observation) => observation,
            Err(failure) => {
                service.native_unproved.set(true);
                service.failed.set(Some(failure));
                relay_native_observation(&service.observation, REQUESTED.load(Ordering::Acquire), Err(failure));
                // Still observe the original app handle; this does not release
                // the actual uncertain native context or permit loop exit.
                poll_task(&observed, &service);
                if let Some(ui) = weak.upgrade() {
                    ui.set_update_status("Windows audio cleanup is unproved. Morpheus remains open; restart is blocked.".into());
                }
                return;
            }
        };
        if !requested_before_poll { return; }
        // Initial/non-terminal empty audio observations never enter the
        // receipt. Shutdown is already terminal, and this exact poll followed
        // begin_shutdown above while the origin STA still pumps.
        let observation = service.failed.get().map_or(Ok(native), Err);
        relay_native_observation(&service.observation, requested_before_poll, observation);
        poll_task(&observed, &service);
        let result = match observed.try_borrow() {
            Ok(state) => state.result,
            Err(_) => { service.failed.set(Some(Failure::Ownership)); return; }
        };
        if service.failed.get().is_some() || matches!(result, Some(Err(_)))
            || matches!(result, Some(Ok(report)) if report.retain)
        {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_status("Shutdown ownership is unproved. Morpheus remains open; restart is blocked.".into());
            }
            return;
        }
        if result.is_none() {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_status("Closing Morpheus. Waiting for admitted work to finish; close any open file dialog.".into());
            }
            return;
        }
        match crate::style_profiles::appearance_observation() {
            crate::style_profiles::AppearanceObservation::Released(_) => {}
            crate::style_profiles::AppearanceObservation::Pending
            | crate::style_profiles::AppearanceObservation::Retained => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_update_status("A file dialog or export is still owned. Close the dialog; restart remains blocked.".into());
                }
                return;
            }
        }
        match native {
            WasapiCloseObservation::Released(result) => {
                observed.borrow_mut().backend = Some(result);
                if !service.quit_attempted.replace(true) {
                    if slint::quit_event_loop().is_ok() { service.quit_posted.set(true); }
                    else { service.failed.set(Some(Failure::Ownership)); }
                }
            }
            WasapiCloseObservation::Retained(_) | WasapiCloseObservation::Pending => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_update_status("Windows audio is still owned. Morpheus remains open; restart is blocked.".into());
                    ui.set_audio_device_notice("Audio cleanup is still pending. New audio work is disabled.".into());
                }
            }
        }
    });
    let ui_result = ui.run();
    timer.stop();
    if !actual.quit_posted.get() {
        actual.failed.set(Some(Failure::EventLoopEnded));
        actual
            .observation
            .send_replace(Err(Failure::EventLoopEnded));
        // Attempt the same one-shot UI revocation, but never block here on a
        // task whose native origin needs the event loop that just returned.
        morpheus_rtc::begin_wasapi_shutdown();
        start();
        poll_task(&state, &actual);
    }
    let (cleanup, backend) = match state.try_borrow() {
        Ok(state) => (state.result, state.backend),
        Err(_) => (None, None),
    };
    let retain = actual.failed.get().is_some()
        || !matches!(cleanup, Some(Ok(report)) if !report.retain)
        || backend.is_none();
    if !retain {
        // Retire only the exact original registration, outside all app State
        // borrows and only after actual app and native origin receipts.
        let retired = START.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return None;
            };
            if slot
                .as_ref()
                .is_some_and(|registered| Rc::ptr_eq(registered, &actual))
            {
                slot.take()
            } else {
                None
            }
        });
        if retired.is_none() {
            actual.failed.set(Some(Failure::Ownership));
        }
        drop(retired);
    }
    Ok(Outcome {
        ui: ui_result,
        cleanup,
        backend,
        retain: retain || actual.failed.get().is_some(),
        failure: actual.failed.get(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_receipt_requires_terminal_same_origin_observation_and_live_sender() {
        let (origin, actual) = tokio::sync::watch::channel(Ok(WasapiCloseObservation::Pending));
        let receipt = OriginReceipt { actual };
        let released = Ok(WasapiCloseObservation::Released(Ok(())));
        relay_native_observation(&origin, false, released);
        let before_shutdown = receipt.observation();
        relay_native_observation(&origin, true, released);
        let after_actual_observation = receipt.observation();
        relay_native_observation(&origin, true, Err(Failure::Ownership));
        let failed = receipt.observation();
        drop(origin);
        let origin_gone = receipt.observation();
        assert_eq!(before_shutdown, Ok(WasapiCloseObservation::Pending));
        assert_eq!(after_actual_observation, released);
        assert_eq!(failed, Err(Failure::Ownership));
        assert_eq!(origin_gone, Err(Failure::Ownership));
    }

    #[tokio::test]
    async fn gated_cleanup_cannot_take_work_before_its_actual_handle_is_retained() {
        let entered = Arc::new(AtomicBool::new(false));
        let actual = entered.clone();
        let transfer: Transfer<usize> = Arc::new(Mutex::new(Some(Box::pin(async move {
            actual.store(true, Ordering::Release);
            7
        }))));
        let spawned = spawn_gated(&tokio::runtime::Handle::current(), transfer.clone());
        let (retained, joined, before, still_owned, released) = match spawned {
            Ok((task, release)) => {
                let mut actual = Some(task);
                tokio::task::yield_now().await;
                let first = std::future::poll_fn(|cx| {
                    Poll::Ready(Pin::new(actual.as_mut().expect("retained actual task")).poll(cx))
                })
                .await;
                let retained = actual.is_some();
                let before = first.is_pending() && !entered.load(Ordering::Acquire);
                let still_owned = transfer.lock().map(|slot| slot.is_some()).unwrap_or(false);
                let released = release.send(()).is_ok();
                let joined = match actual.as_mut() {
                    Some(task) => task.await.ok().and_then(Result::ok),
                    None => None,
                };
                drop(actual);
                (retained, joined, before, still_owned, released)
            }
            Err(_) => (false, None, false, false, false),
        };
        // Every actual handle has returned before any assertion.
        assert!(retained && before && still_owned && released);
        assert_eq!(joined, Some(7));
        assert!(entered.load(Ordering::Acquire));
        assert!(transfer.lock().unwrap().is_none());
    }
}
