//! Ordinary Windows quit coordinator. Native origin apartments stay on the
//! actual Slint thread while EVERY mandatory application cleanup is attempted.
//! Neither a failed close nor a timeout is a successful restart authority.
use crate::AppWindow;
use morpheus_rtc::{WasapiCloseError, WasapiCloseObservation, WasapiUiContext};
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

static REQUESTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    // Actual UI-thread coordinator. Its synchronous closure closes product
    // authorities before this quit callback yields; normal Slint/COM pumping
    // continues afterward. No native pointer or Send fiction crosses threads.
    static START: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}
pub(crate) fn coordinator_available() -> bool {
    START.with(|slot| slot.borrow().is_some())
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
/// Consume the real gate receipt AFTER the controller has dropped its inner
/// mutex, so synchronous UI revocation never runs under that unrelated lock.
pub(crate) fn request_admitted_shutdown(
    _admission: crate::security::ShutdownAdmission<'_>,
) -> bool {
    let Some(start) = START.with(|slot| slot.borrow().clone()) else {
        return false;
    };
    morpheus_rtc::begin_wasapi_shutdown();
    REQUESTED.store(true, Ordering::Release);
    start();
    true
}

/// The startup scope creates this bridge immediately after showing its actual
/// window. No audio operation is created merely by attaching the checked lease.
pub(crate) fn attach() -> Result<WasapiUiContext, WasapiCloseError> {
    WasapiUiContext::attach()
}

pub(crate) fn run<R, F>(
    ui: &AppWindow,
    origin: WasapiUiContext,
    runtime: &tokio::runtime::Runtime,
    cleanup: impl FnOnce() -> F + 'static,
) -> Result<
    (
        Result<(), slint::PlatformError>,
        R,
        Result<(), WasapiCloseError>,
    ),
    Box<dyn std::error::Error>,
>
where
    R: Send + 'static,
    F: Future<Output = R> + Send + 'static,
{
    struct State<R, F, C> {
        cleanup: Option<C>,
        task: Option<tokio::task::JoinHandle<R>>,
        result: Option<Result<R, tokio::task::JoinError>>,
        backend: Option<Result<(), WasapiCloseError>>,
        _future: std::marker::PhantomData<fn() -> F>,
    }
    let origin = Rc::new(RefCell::new(origin));
    let state = Rc::new(RefCell::new(State {
        cleanup: Some(cleanup),
        task: None,
        result: None,
        backend: None,
        _future: std::marker::PhantomData::<fn() -> F>,
    }));
    let start_state = state.clone();
    let handle = runtime.handle().clone();
    let start: Rc<dyn Fn()> = Rc::new(move || {
        let cleanup = {
            let Ok(mut state) = start_state.try_borrow_mut() else {
                return;
            };
            state.cleanup.take()
        };
        if let Some(cleanup) = cleanup {
            // Call the synchronous UI revocation while no State borrow is
            // held. A reentrant close sees the one-shot already consumed.
            let future = cleanup();
            start_state.borrow_mut().task = Some(handle.spawn(future));
        }
    });
    if !START.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            false
        } else {
            *slot = Some(start.clone());
            true
        }
    }) {
        return Err("Windows shutdown coordinator already exists".into());
    }
    struct Registration;
    impl Drop for Registration {
        fn drop(&mut self) {
            START.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    let _registration = Registration;
    let observed = state.clone();
    let service_origin = origin.clone();
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(10), move || {
        if !REQUESTED.load(Ordering::Acquire) {
            let _ = service_origin.borrow_mut().poll();
            return;
        }
        service_origin.borrow_mut().begin_shutdown();
        let observation = service_origin.borrow_mut().poll();
        let Ok(mut state) = observed.try_borrow_mut() else { return; };
        if let Some(task) = state.task.as_mut() {
            let mut context = Context::from_waker(Waker::noop());
            if let Poll::Ready(result) = Pin::new(task).poll(&mut context) {
                state.result = Some(result);
                state.task = None;
            }
        }
        if matches!(state.result.as_ref(), Some(Err(_))) {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_status("Morpheus cleanup failed. Keep it open; restart is blocked.".into());
            }
        } else if state.result.is_none() {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_status("Closing Morpheus. Waiting for admitted work to finish; close any open file dialog.".into());
            }
        } else if matches!(state.result.as_ref(), Some(Ok(_))) {
            // Native dialog/write ownership is independent of COM audio. A
            // bounded wait error does not release its actual handles or allow
            // this ordinary window to disappear beneath a still-open panel.
            let appearance = crate::style_profiles::appearance_observation();
            match appearance {
                crate::style_profiles::AppearanceObservation::Released(_) => {}
                crate::style_profiles::AppearanceObservation::Pending => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_update_status("A file dialog or export is still open. Close the dialog; Morpheus will finish shutting down afterward.".into());
                    }
                    return;
                }
                crate::style_profiles::AppearanceObservation::Retained => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_update_status("Appearance cleanup could not be confirmed. Keep Morpheus open; restart is blocked.".into());
                    }
                    return;
                }
            }
            match observation {
                WasapiCloseObservation::Released(result) => {
                    state.backend = Some(result);
                    if slint::quit_event_loop().is_err() && let Some(ui) = weak.upgrade() {
                        ui.set_update_status("Shutdown is complete. Close Morpheus to finish.".into());
                    }
                }
                WasapiCloseObservation::Retained(_) | WasapiCloseObservation::Pending => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_update_status("Windows audio has not released safely. Keep Morpheus open; restart is blocked.".into());
                        ui.set_audio_device_notice("Audio cleanup is still pending. New audio work is disabled.".into());
                    }
                }
            }
        }
    });
    let ui_result = ui.run();
    timer.stop();
    if state.borrow().result.is_none() {
        origin.borrow_mut().begin_shutdown();
        start();
        let task = state.borrow_mut().task.take();
        if let Some(task) = task {
            state.borrow_mut().result = Some(runtime.block_on(task));
        }
        state.borrow_mut().backend = Some(Err(WasapiCloseError::Apartment));
    }
    let mut state = state.borrow_mut();
    let result = state
        .result
        .take()
        .ok_or("Windows shutdown ownership was not retained")?
        .map_err(|_| "Windows mandatory cleanup task failed")?;
    let backend = state
        .backend
        .take()
        .unwrap_or(Err(WasapiCloseError::Apartment));
    Ok((ui_result, result, backend))
}
