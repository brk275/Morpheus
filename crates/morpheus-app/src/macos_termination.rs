//! Main-thread bridge to the owned Winit application termination protocol.
//! The one strong application root stays in main through Pending/Failed.

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
};

use slint::{ComponentHandle, winit_030::winit::platform::macos as native};

use crate::{
    AppWindow, macos_quit, security::ShutdownAdmission, update_controller::UpdateController,
};

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
    static APPLICATION: RefCell<Weak<Application>> = const { RefCell::new(Weak::new()) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Ordinary,
    Native,
}

#[derive(Clone, Copy)]
pub(crate) enum Reason {
    Window,
    Restart,
    Native,
}

trait CleanupOwner {
    fn request(&self, admission: ShutdownAdmission<'_>) -> bool;
    fn poll(&self) -> macos_quit::Observation;
    fn runtime_finalized(&self) -> bool;
}

impl<R: Send + 'static> CleanupOwner for macos_quit::Coordinator<R> {
    fn request(&self, admission: ShutdownAdmission<'_>) -> bool {
        self.request_admitted(admission)
    }

    fn poll(&self) -> macos_quit::Observation {
        macos_quit::Coordinator::poll(self)
    }

    fn runtime_finalized(&self) -> bool {
        macos_quit::Coordinator::runtime_finalized(self)
    }
}

struct Armed {
    owner: Rc<dyn CleanupOwner>,
    updates: UpdateController,
    ui: slint::Weak<AppWindow>,
}

/// Bootstrap refuses termination until all actual cleanup captures are armed.
/// No strong AppWindow/LiveUi is stored here; the real coordinator owns them.
pub(crate) struct Application {
    armed: RefCell<Option<Rc<Armed>>>,
    mode: Cell<Mode>,
    ordinary_exit_attempted: Cell<bool>,
    ordinary_exit_posted: Cell<bool>,
    failed: Cell<bool>,
    failure_shown: Cell<bool>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Failure {
    Backend,
    Registration,
    Ownership,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Backend => "The owned desktop quit backend is unavailable.",
            Self::Registration => "Owned desktop quit could not be registered.",
            Self::Ownership => "Desktop shutdown ownership could not be confirmed.",
        })
    }
}

impl std::error::Error for Failure {}

impl Application {
    /// This runs before AppWindow/profile/account initialization. A runtime
    /// environment cannot silently replace Winit with an unguarded backend.
    pub(crate) fn install() -> Result<Rc<Self>, Failure> {
        if INSTALLED.with(|installed| installed.replace(true)) {
            return Err(Failure::Registration);
        }
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .select()
            .map_err(|_| Failure::Backend)?;
        let application = Rc::new(Self {
            armed: RefCell::new(None),
            mode: Cell::new(Mode::Idle),
            ordinary_exit_attempted: Cell::new(false),
            ordinary_exit_posted: Cell::new(false),
            failed: Cell::new(false),
            failure_shown: Cell::new(false),
        });
        let handler: Rc<dyn native::ApplicationTerminationHandler> = application.clone();
        native::install_application_termination_handler(Rc::downgrade(&handler))
            .map_err(|_| Failure::Registration)?;
        APPLICATION.with(|slot| *slot.borrow_mut() = Rc::downgrade(&application));
        Ok(application)
    }

    pub(crate) fn arm<R: Send + 'static>(
        &self,
        owner: Rc<macos_quit::Coordinator<R>>,
        updates: UpdateController,
        ui: &AppWindow,
    ) -> Result<(), Failure> {
        let mut slot = self
            .armed
            .try_borrow_mut()
            .map_err(|_| Failure::Ownership)?;
        if slot.is_some() || self.failed.get() {
            return Err(Failure::Ownership);
        }
        *slot = Some(Rc::new(Armed {
            owner,
            updates,
            ui: ui.as_weak(),
        }));
        Ok(())
    }

    fn armed(&self) -> Option<Rc<Armed>> {
        self.armed.try_borrow().ok()?.clone()
    }

    fn request(&self, admission: ShutdownAdmission<'_>, reason: Reason) -> bool {
        let Some(armed) = self.armed() else {
            return false;
        };
        if self.failed.get() {
            return false;
        }
        // The controller has admitted its actual lease; only now select an
        // exit route. A later OS request can adopt the same ordinary cleanup.
        if matches!(reason, Reason::Native) {
            self.mode.set(Mode::Native);
        } else if self.mode.get() == Mode::Idle {
            self.mode.set(Mode::Ordinary);
        }
        if !armed.owner.request(admission) {
            self.failed.set(true);
            self.show_failure(&armed);
            return false;
        }
        true
    }

    fn show_failure(&self, armed: &Armed) {
        if !self.failure_shown.replace(true)
            && let Some(ui) = armed.ui.upgrade()
        {
            ui.set_update_status(
                "Shutdown could not be confirmed. Morpheus remains closed to new work.".into(),
            );
        }
    }
}

impl native::ApplicationTerminationHandler for Application {
    fn requested(&self) -> native::ApplicationTerminationRequest {
        let Some(armed) = self.armed() else {
            return native::ApplicationTerminationRequest::Cancel;
        };
        if !self.failed.get()
            && self.mode.get() == Mode::Ordinary
            && self.ordinary_exit_posted.get()
            && armed.owner.poll() == macos_quit::Observation::Clean
            && armed.owner.runtime_finalized()
        {
            // The real ordinary quit was already posted. Do not start a Later
            // transaction that races it. Winit re-observes poll plus its actual
            // delegate/backend/PanicInfo before returning typed native Now.
            return native::ApplicationTerminationRequest::Now;
        }
        if self.failed.get() || !armed.updates.request_macos_shutdown(Reason::Native) {
            native::ApplicationTerminationRequest::Cancel
        } else {
            native::ApplicationTerminationRequest::Later
        }
    }

    fn poll(&self) -> native::ApplicationTerminationProgress {
        let Some(armed) = self.armed() else {
            return native::ApplicationTerminationProgress::Pending;
        };
        // No RefCell borrow spans actual coordinator polling or UI work.
        match armed.owner.poll() {
            macos_quit::Observation::Pending => native::ApplicationTerminationProgress::Pending,
            macos_quit::Observation::Failed(_) => {
                self.show_failure(&armed);
                native::ApplicationTerminationProgress::Cancel
            }
            macos_quit::Observation::Clean if armed.owner.runtime_finalized() => {
                if self.failed.get() {
                    return native::ApplicationTerminationProgress::Cancel;
                }
                if self.mode.get() == Mode::Ordinary && !self.ordinary_exit_attempted.replace(true)
                {
                    if slint::quit_event_loop().is_ok() {
                        self.ordinary_exit_posted.set(true);
                    } else {
                        self.failed.set(true);
                        self.show_failure(&armed);
                        return native::ApplicationTerminationProgress::Cancel;
                    }
                }
                native::ApplicationTerminationProgress::Allow
            }
            macos_quit::Observation::Clean => {
                self.failed.set(true);
                self.show_failure(&armed);
                native::ApplicationTerminationProgress::Cancel
            }
        }
    }
}

pub(crate) fn coordinator_available() -> bool {
    let application = APPLICATION.with(|slot| slot.borrow().upgrade());
    application
        .is_some_and(|application| application.armed().is_some() && !application.failed.get())
}

pub(crate) fn request_admitted_shutdown(admission: ShutdownAdmission<'_>, reason: Reason) -> bool {
    let application = APPLICATION.with(|slot| slot.borrow().upgrade());
    application.is_some_and(|application| application.request(admission, reason))
}
