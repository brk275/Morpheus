//! Explicit signed-update actions, with no automatic polling or installation.
//!
//! Integration contract (this module does not construct or own a window):
//! - Declare `Arc<Mutex<StartupSession>>` before the UI, stores and runtime. Run
//!   early dispatch/startup recovery and acknowledge before opening any store,
//!   exactly as main already does. Keep that root Arc until shutdown finishes.
//! - Pass the same `UpdateActivity` used by CallController, the live security
//!   gate and a runtime Handle to `new`. Bind buttons only to
//!   check/download/restart/discard. Missing
//!   compiled feed/key or unsupported installation means disabled controls.
//! - On a watch notification, fetch `snapshot()` on the UI event loop rather
//!   than applying a queued old snapshot. `is_current` is available for adapters
//!   that have to queue snapshots. Display `message()` and `version` as text.
//! - Restart reserves the security gate before starting work, excluding key
//!   recovery/setup until a safe failure or orderly shutdown. A current
//!   RestartReady snapshot means helper readiness was confirmed. Quit
//!   the UI, then run normal call/draft/account shutdown and `shutdown().await`
//!   before dropping the runtime or root StartupSession. Do not call exit().
//! - A normal window close must also call shutdown. Blocking downloads cannot
//!   be aborted by dropping a Tokio task; cancellation discards their result
//!   after the updater's existing bounded network timeout. A restart already
//!   authorized by the user drains through helper handoff or a safe failure.
//!
//! Neither a feed/key override nor a test-mode install path is exposed. Platform
//! transactions and official signature/transport checks remain in updater.rs.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use slint::ComponentHandle;
use tokio::{runtime::Handle, sync::watch, task::JoinHandle};

use crate::updater::{
    AvailableUpdate, StagedUpdate, StartupSession, UpdateActivity, UpdateError, UpdateService,
};
use crate::{
    AppWindow,
    security::{RestartLease, SecurityGate},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerError {
    Update(UpdateError),
    Busy,
    ActionUnavailable,
    ShuttingDown,
    WorkerFailed,
    RecoveryActive,
}

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // UpdateError contains static, private messages only.
            Self::Update(error) => fmt::Display::fmt(error, f),
            Self::Busy => f.write_str("An update operation is already running."),
            Self::ActionUnavailable => {
                f.write_str("This update action is not currently available.")
            }
            Self::ShuttingDown => f.write_str("Morpheus is shutting down."),
            Self::WorkerFailed => f.write_str("The update operation could not finish safely."),
            Self::RecoveryActive => {
                f.write_str("Finish recovery and save any new recovery key before restarting.")
            }
        }
    }
}

impl std::error::Error for ControllerError {}

impl From<UpdateError> for ControllerError {
    fn from(error: UpdateError) -> Self {
        Self::Update(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdatePhase {
    Disabled,
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Staged,
    PreparingRestart,
    Discarding,
    RestartReady,
    Closing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateStatus {
    /// Changes on every state transition, including cancellation and errors.
    pub revision: u64,
    pub phase: UpdatePhase,
    /// Bounded display-only version. Never use this label to prepare an update.
    pub version: Option<String>,
    pub error: Option<ControllerError>,
}

impl UpdateStatus {
    pub fn can_check(&self) -> bool {
        matches!(
            self.phase,
            UpdatePhase::Idle | UpdatePhase::UpToDate | UpdatePhase::Available
        )
    }

    pub fn can_download(&self) -> bool {
        self.phase == UpdatePhase::Available
    }
    pub fn can_restart(&self) -> bool {
        self.phase == UpdatePhase::Staged
    }
    pub fn can_discard(&self) -> bool {
        matches!(
            self.phase,
            UpdatePhase::Checking
                | UpdatePhase::Available
                | UpdatePhase::Downloading
                | UpdatePhase::Staged
        )
    }

    pub fn message(&self) -> String {
        if let Some(error) = self.error {
            return error.to_string();
        }
        match self.phase {
            UpdatePhase::Disabled => "Signed updates are unavailable for this build.",
            UpdatePhase::Idle => "Check for a signed application update.",
            UpdatePhase::Checking => "Checking for a signed update…",
            UpdatePhase::UpToDate => "No newer application update is available.",
            UpdatePhase::Available => "An application update is available to download.",
            UpdatePhase::Downloading => "Downloading and verifying the signed update…",
            UpdatePhase::Staged => "The signed update is ready. Leave any call before restarting.",
            UpdatePhase::PreparingRestart => "Preparing a safe application restart…",
            UpdatePhase::Discarding => {
                "Discarding the update; any current transfer must finish first…"
            }
            UpdatePhase::RestartReady => "Update preparation is complete. Restarting Morpheus…",
            UpdatePhase::Closing => "Finishing update work before quitting…",
        }
        .into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Check,
    Download,
    Restart,
    Discard,
}

#[derive(Clone, Copy)]
struct Pending {
    generation: u64,
    action: Action,
    cancelled: bool,
}

enum Work<A, S> {
    Check,
    Download(A),
    Restart(S),
    Discard {
        available: Option<A>,
        staged: Option<S>,
    },
}

enum Outcome<A, S> {
    Checked(Result<Option<(A, String)>, ControllerError>),
    Downloaded(Result<(S, String), ControllerError>),
    Restarted {
        result: Result<(), ControllerError>,
        retry: Option<S>,
    },
    Discarded,
}

impl<A, S> Outcome<A, S> {
    fn action(&self) -> Action {
        match self {
            Self::Checked(_) => Action::Check,
            Self::Downloaded(_) => Action::Download,
            Self::Restarted { .. } => Action::Restart,
            Self::Discarded => Action::Discard,
        }
    }
}

/// The transition model owns assets, so a stale/cancelled result cannot leave
/// behind an actionable download or a staged file. Generic assets allow tests
/// to exercise the same ownership/transitions without network or fake crypto.
struct Workflow<A, S> {
    status: UpdateStatus,
    generation: u64,
    pending: Option<Pending>,
    available: Option<A>,
    staged: Option<S>,
    closing: bool,
    handed_off: bool,
}

impl<A, S> Workflow<A, S> {
    fn new(disabled: Option<ControllerError>) -> Self {
        Self {
            status: UpdateStatus {
                revision: 0,
                phase: if disabled.is_some() {
                    UpdatePhase::Disabled
                } else {
                    UpdatePhase::Idle
                },
                version: None,
                error: disabled,
            },
            generation: 0,
            pending: None,
            available: None,
            staged: None,
            closing: false,
            handed_off: false,
        }
    }

    fn changed(&mut self) {
        self.status.revision = self.status.revision.wrapping_add(1);
    }

    fn begin(&mut self, action: Action) -> Result<(u64, Work<A, S>), ControllerError> {
        if self.closing {
            return Err(ControllerError::ShuttingDown);
        }
        if self.pending.is_some() {
            return Err(ControllerError::Busy);
        }
        let work = match action {
            Action::Check if self.status.can_check() => {
                self.available = None;
                self.status.version = None;
                self.status.phase = UpdatePhase::Checking;
                Work::Check
            }
            Action::Download if self.status.can_download() => {
                let available = self
                    .available
                    .take()
                    .ok_or(ControllerError::ActionUnavailable)?;
                self.status.phase = UpdatePhase::Downloading;
                Work::Download(available)
            }
            Action::Restart if self.status.can_restart() => {
                let staged = self
                    .staged
                    .take()
                    .ok_or(ControllerError::ActionUnavailable)?;
                self.status.phase = UpdatePhase::PreparingRestart;
                Work::Restart(staged)
            }
            Action::Discard if self.status.can_discard() => {
                self.status.phase = UpdatePhase::Discarding;
                Work::Discard {
                    available: self.available.take(),
                    staged: self.staged.take(),
                }
            }
            _ => return Err(ControllerError::ActionUnavailable),
        };
        self.generation = self.generation.wrapping_add(1);
        self.pending = Some(Pending {
            generation: self.generation,
            action,
            cancelled: false,
        });
        self.status.error = None;
        self.changed();
        Ok((self.generation, work))
    }

    /// Cancellation never aborts a blocking download or races an install helper.
    fn cancel_transfer(&mut self) -> Result<bool, ControllerError> {
        if self.closing {
            return Err(ControllerError::ShuttingDown);
        }
        let Some(pending) = self.pending.as_mut() else {
            return Ok(false);
        };
        if !matches!(pending.action, Action::Check | Action::Download) {
            return Err(ControllerError::Busy);
        }
        pending.cancelled = true;
        self.status.phase = UpdatePhase::Discarding;
        self.status.error = None;
        self.changed();
        Ok(true)
    }

    fn authorized(&self, generation: u64) -> bool {
        self.pending
            .is_some_and(|pending| pending.generation == generation && !pending.cancelled)
    }

    fn finish(&mut self, generation: u64, outcome: Outcome<A, S>) -> bool {
        let Some(pending) = self
            .pending
            .filter(|pending| pending.generation == generation)
        else {
            return false;
        };
        if pending.cancelled {
            self.pending = None;
            self.reset_after_discard();
            return true;
        }
        if pending.action != outcome.action() {
            return self.worker_failed(generation);
        }
        self.pending = None;
        match outcome {
            Outcome::Checked(Ok(Some((available, version)))) => {
                self.available = Some(available);
                self.status.version = Some(display_version(&version));
                self.status.phase = UpdatePhase::Available;
            }
            Outcome::Checked(Ok(None)) => {
                self.status.version = None;
                self.status.phase = UpdatePhase::UpToDate;
            }
            Outcome::Downloaded(Ok((staged, version))) => {
                self.staged = Some(staged);
                self.status.version = Some(display_version(&version));
                self.status.phase = UpdatePhase::Staged;
            }
            Outcome::Checked(Err(error)) | Outcome::Downloaded(Err(error)) => self.fail(error),
            Outcome::Restarted { result: Ok(()), .. } => {
                self.handed_off = true;
                self.status.phase = UpdatePhase::RestartReady;
            }
            Outcome::Restarted {
                result: Err(error),
                retry,
            } => {
                if let Some(staged) = retry {
                    self.staged = Some(staged);
                    self.status.phase = UpdatePhase::Staged;
                    self.status.error = Some(error);
                } else {
                    self.fail(error);
                }
            }
            Outcome::Discarded => self.reset_after_discard(),
        }
        if self.closing && !self.handed_off {
            self.status.phase = UpdatePhase::Closing;
        }
        self.changed();
        true
    }

    fn reset_after_discard(&mut self) {
        self.available = None;
        self.staged = None;
        self.status.phase = if self.closing {
            UpdatePhase::Closing
        } else {
            UpdatePhase::Idle
        };
        self.status.version = None;
        self.status.error = None;
        self.changed();
    }

    fn fail(&mut self, error: ControllerError) {
        self.status.phase = if matches!(
            error,
            ControllerError::WorkerFailed
                | ControllerError::Update(
                    UpdateError::NotConfigured
                        | UpdateError::PackageManager
                        | UpdateError::UnsupportedPackage
                        | UpdateError::InstallNotValidated
                        | UpdateError::InvalidConfiguration
                        | UpdateError::RecoveryRequired
                )
        ) {
            UpdatePhase::Disabled
        } else {
            UpdatePhase::Idle
        };
        self.status.version = None;
        self.status.error = Some(error);
    }

    fn worker_failed(&mut self, generation: u64) -> bool {
        if !self
            .pending
            .is_some_and(|pending| pending.generation == generation)
        {
            return false;
        }
        self.pending = None;
        // An unwinding restart worker may have reached handoff. Never offer a
        // second install after an unknown outcome; next startup owns recovery.
        self.status.phase = UpdatePhase::Disabled;
        self.status.error = Some(ControllerError::WorkerFailed);
        self.changed();
        true
    }

    fn begin_shutdown(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        if let Some(pending) = self.pending.as_mut()
            && pending.action != Action::Restart
        {
            pending.cancelled = true;
        }
        if !self.handed_off {
            self.status.phase = UpdatePhase::Closing;
        }
        self.changed();
    }
}

fn display_version(version: &str) -> String {
    let mut label: String = version.chars().take(128).collect();
    if version.chars().count() > 128 {
        label.push('…');
    }
    label
}

struct Inner {
    workflow: Workflow<AvailableUpdate, StagedUpdate>,
    workers: Vec<JoinHandle<()>>,
    restart_lease: Option<RestartLease>,
}

struct Shared {
    inner: Mutex<Inner>,
    status: watch::Sender<UpdateStatus>,
    startup: Arc<Mutex<StartupSession>>,
    activity: UpdateActivity,
    security: Arc<SecurityGate>,
    account_access_busy: Arc<dyn Fn() -> bool + Send + Sync>,
    shutdown: tokio::sync::Mutex<()>,
}

impl Shared {
    fn publish(&self, inner: &Inner) {
        self.status.send_replace(inner.workflow.status.clone());
    }

    fn finish(
        &self,
        generation: u64,
        outcome: Result<Outcome<AvailableUpdate, StagedUpdate>, ControllerError>,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let changed = match outcome {
            Ok(outcome) => inner.workflow.finish(generation, outcome),
            Err(_) => inner.workflow.worker_failed(generation),
        };
        if changed {
            // Only a known preparation failure releases this reservation.
            // Success and an unknown worker outcome keep it through shutdown.
            if !inner.workflow.handed_off
                && inner.workflow.pending.is_none()
                && inner.workflow.status.error != Some(ControllerError::WorkerFailed)
            {
                inner.restart_lease.take();
            }
            self.publish(&inner);
        }
    }
}

#[derive(Clone)]
pub struct UpdateController {
    shared: Arc<Shared>,
    runtime: Handle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownSummary {
    pub restart_handed_off: bool,
}

impl UpdateController {
    /// Does no network or filesystem work and does not create updater sessions.
    pub fn new(
        runtime: Handle,
        activity: UpdateActivity,
        startup: Arc<Mutex<StartupSession>>,
        security: Arc<SecurityGate>,
        account_access_busy: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        let configured = option_env!("MORPHEUS_UPDATE_FEED").is_some_and(|value| !value.is_empty())
            && option_env!("MORPHEUS_UPDATE_PUBLIC_KEY").is_some_and(|value| !value.is_empty());
        let disabled = if !configured {
            Some(UpdateError::NotConfigured.into())
        } else if !cfg!(target_os = "linux") {
            Some(UpdateError::InstallNotValidated.into())
        } else {
            #[cfg(target_os = "linux")]
            {
                match startup.lock() {
                    Ok(session) if session.appimage.is_some() => None,
                    Ok(_) => Some(UpdateError::UnsupportedPackage.into()),
                    Err(_) => Some(ControllerError::WorkerFailed),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                Some(UpdateError::InstallNotValidated.into())
            }
        };
        let workflow = Workflow::new(disabled);
        let (status, _) = watch::channel(workflow.status.clone());
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    workflow,
                    workers: Vec::new(),
                    restart_lease: None,
                }),
                status,
                startup,
                activity,
                security,
                account_access_busy,
                shutdown: tokio::sync::Mutex::new(()),
            }),
            runtime,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<UpdateStatus> {
        self.shared.status.subscribe()
    }
    pub fn snapshot(&self) -> UpdateStatus {
        self.shared.status.borrow().clone()
    }
    pub fn is_current(&self, snapshot: &UpdateStatus) -> bool {
        self.snapshot().revision == snapshot.revision
    }

    pub fn check(&self) -> Result<(), ControllerError> {
        self.start(Action::Check)
    }
    pub fn download(&self) -> Result<(), ControllerError> {
        self.start(Action::Download)
    }
    pub fn restart(&self) -> Result<(), ControllerError> {
        self.start(Action::Restart)
    }
    pub fn discard(&self) -> Result<(), ControllerError> {
        self.start(Action::Discard)
    }

    /// Bind explicit UI actions and return the status observer. Abort/await only
    /// this observer after the window closes; controller.shutdown must be drained.
    pub fn bind_ui(&self, ui: &AppWindow) -> JoinHandle<()> {
        apply_snapshot(ui, self);
        let controller = self.clone();
        let weak = ui.as_weak();
        ui.on_check_for_updates(move || invoke_from_ui(&weak, &controller, Action::Check));
        let controller = self.clone();
        let weak = ui.as_weak();
        ui.on_download_update(move || invoke_from_ui(&weak, &controller, Action::Download));
        let controller = self.clone();
        let weak = ui.as_weak();
        ui.on_restart_to_update(move || invoke_from_ui(&weak, &controller, Action::Restart));
        let controller = self.clone();
        let weak = ui.as_weak();
        ui.on_discard_update(move || invoke_from_ui(&weak, &controller, Action::Discard));

        let mut status = self.subscribe();
        let controller = self.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            while status.changed().await.is_ok() {
                // No watch::Ref crosses this boundary. Read the current owned
                // snapshot on the UI thread, never apply an enqueued old one.
                let controller = controller.clone();
                if weak
                    .upgrade_in_event_loop(move |ui| apply_snapshot(&ui, &controller))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn start(&self, action: Action) -> Result<(), ControllerError> {
        let mut inner = self
            .shared
            .inner
            .lock()
            .map_err(|_| ControllerError::WorkerFailed)?;
        if action == Action::Discard && inner.workflow.cancel_transfer()? {
            self.shared.publish(&inner);
            return Ok(());
        }
        let restart_lease = if action == Action::Restart {
            Some(
                self.shared
                    .security
                    .try_restart()
                    .ok_or(ControllerError::RecoveryActive)?,
            )
        } else {
            None
        };
        if action == Action::Restart && (self.shared.account_access_busy)() {
            return Err(ControllerError::RecoveryActive);
        }
        let (generation, work) = inner.workflow.begin(action)?;
        if restart_lease.is_some() {
            inner.restart_lease = restart_lease;
        }
        self.shared.publish(&inner);
        // Register the task under the same mutex as the reservation. Shutdown
        // cannot drain the task list between authorization and registration.
        inner.workers.retain(|worker| !worker.is_finished());
        let shared = self.shared.clone();
        let runtime = self.runtime.clone();
        inner.workers.push(self.runtime.spawn(async move {
            let blocking_shared = shared.clone();
            let result = runtime
                .spawn_blocking(move || {
                    let authorized = blocking_shared
                        .inner
                        .lock()
                        .map(|inner| inner.workflow.authorized(generation))
                        .unwrap_or(false);
                    let outcome = if !authorized {
                        drop(work); // Drop a staged tempfile on the blocking pool.
                        Outcome::Discarded
                    } else {
                        perform(work, &blocking_shared)
                    };
                    // A cancelled/stale success can own a verified tempfile.
                    // Apply or discard it here so its unlink also stays off the
                    // UI and async executor threads.
                    blocking_shared.finish(generation, Ok(outcome));
                })
                .await;
            if result.is_err() {
                shared.finish(generation, Err(ControllerError::WorkerFailed));
            }
        }));
        Ok(())
    }

    /// Prevent new requests immediately, then wait for our background work.
    /// The caller still owns the root StartupSession through app worker shutdown.
    /// This must run before the runtime is dropped; do not abort its task.
    pub async fn shutdown(&self) -> Result<ShutdownSummary, ControllerError> {
        // A second caller must await the first drain, not mistake its empty
        // task list for completed work and release the root lifetime lock.
        let _shutdown = self.shared.shutdown.lock().await;
        let workers = {
            let mut inner = self
                .shared
                .inner
                .lock()
                .map_err(|_| ControllerError::WorkerFailed)?;
            inner.workflow.begin_shutdown();
            self.shared.publish(&inner);
            std::mem::take(&mut inner.workers)
        };
        let mut worker_failed = false;
        for worker in workers {
            // Drain all tasks even when an earlier one panicked or was cancelled.
            worker_failed |= worker.await.is_err();
        }
        let (summary, discarded) = {
            let mut inner = self
                .shared
                .inner
                .lock()
                .map_err(|_| ControllerError::WorkerFailed)?;
            let discarded = (
                inner.workflow.available.take(),
                inner.workflow.staged.take(),
            );
            (
                ShutdownSummary {
                    restart_handed_off: inner.workflow.handed_off,
                },
                discarded,
            )
        };
        self.runtime
            .spawn_blocking(move || drop(discarded))
            .await
            .map_err(|_| ControllerError::WorkerFailed)?;
        if worker_failed {
            return Err(ControllerError::WorkerFailed);
        }
        Ok(summary)
    }
}

fn invoke_from_ui(weak: &slint::Weak<AppWindow>, controller: &UpdateController, action: Action) {
    let Some(ui) = weak.upgrade() else { return };
    let result = match action {
        Action::Check => controller.check(),
        Action::Download => controller.download(),
        Action::Restart => controller.restart(),
        Action::Discard => controller.discard(),
    };
    apply_snapshot(&ui, controller);
    if let Err(error) = result {
        ui.set_update_status(error.to_string().into());
    }
}

fn apply_snapshot(ui: &AppWindow, controller: &UpdateController) {
    let snapshot = controller.snapshot();
    ui.set_update_status(snapshot.message().into());
    ui.set_update_version(snapshot.version.as_deref().unwrap_or("").into());
    ui.set_update_can_check(snapshot.can_check());
    ui.set_update_can_download(snapshot.can_download());
    ui.set_update_can_restart(snapshot.can_restart());
    ui.set_update_can_discard(snapshot.can_discard());
    if snapshot.phase == UpdatePhase::RestartReady && controller.is_current(&snapshot) {
        // This terminal phase is published only after helper readiness. The
        // normal main shutdown path drains workers/stores before releasing the
        // lifetime lock; no process::exit or UI-owned session is permitted.
        if slint::quit_event_loop().is_err() {
            ui.set_update_status(
                "Update preparation is complete. Close Morpheus to finish restarting.".into(),
            );
        }
    }
}

fn perform(
    work: Work<AvailableUpdate, StagedUpdate>,
    shared: &Shared,
) -> Outcome<AvailableUpdate, StagedUpdate> {
    match work {
        Work::Check => Outcome::Checked((|| {
            Ok(UpdateService::embedded()?.check()?.map(|available| {
                let version = available.version().to_owned();
                (available, version)
            }))
        })()),
        Work::Download(available) => Outcome::Downloaded(
            available
                .stage()
                .map(|staged| {
                    let version = staged.version().to_owned();
                    (staged, version)
                })
                .map_err(Into::into),
        ),
        Work::Restart(mut staged) => {
            let result = prepare_and_handoff(&mut staged, shared);
            // A call can win the gate after the Restart click. Preserve the
            // verified stage for an explicit retry after the user leaves it.
            let retry = matches!(
                result,
                Err(ControllerError::Update(UpdateError::CallActive))
            )
            .then_some(staged);
            Outcome::Restarted { result, retry }
        }
        Work::Discard { available, staged } => {
            drop((available, staged));
            Outcome::Discarded
        }
    }
}

fn prepare_and_handoff(staged: &mut StagedUpdate, shared: &Shared) -> Result<(), ControllerError> {
    #[cfg(target_os = "linux")]
    {
        let mut startup = shared
            .startup
            .lock()
            .map_err(|_| ControllerError::WorkerFailed)?;
        let session = startup
            .appimage
            .as_mut()
            .ok_or(UpdateError::UnsupportedPackage)?;
        staged
            .prepare_restart(&shared.activity, session)?
            .launch_supervisor()?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (staged, shared);
        Err(UpdateError::InstallNotValidated.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Asset(Arc<AtomicUsize>);
    impl Drop for Asset {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn asset() -> (Asset, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        (Asset(drops.clone()), drops)
    }
    fn available(workflow: &mut Workflow<Asset, Asset>) {
        let (generation, _) = workflow.begin(Action::Check).unwrap();
        workflow.finish(
            generation,
            Outcome::Checked(Ok(Some((asset().0, "0.2.0".into())))),
        );
    }
    fn staged(workflow: &mut Workflow<Asset, Asset>) {
        available(workflow);
        let (generation, work) = workflow.begin(Action::Download).unwrap();
        drop(work);
        workflow.finish(
            generation,
            Outcome::Downloaded(Ok((asset().0, "0.2.0".into()))),
        );
    }

    #[test]
    fn disabled_build_never_authorizes_any_work() {
        let mut workflow = Workflow::<Asset, Asset>::new(Some(UpdateError::NotConfigured.into()));
        for action in [
            Action::Check,
            Action::Download,
            Action::Restart,
            Action::Discard,
        ] {
            assert!(matches!(
                workflow.begin(action),
                Err(ControllerError::ActionUnavailable)
            ));
        }
        assert!(workflow.pending.is_none());
        assert!(!workflow.status.can_check());
    }

    #[test]
    fn each_step_requires_an_explicit_action_and_handoff_is_terminal() {
        let mut workflow = Workflow::<Asset, Asset>::new(None);
        available(&mut workflow);
        assert_eq!(workflow.status.phase, UpdatePhase::Available);
        assert!(workflow.pending.is_none());
        assert!(matches!(
            workflow.begin(Action::Restart),
            Err(ControllerError::ActionUnavailable)
        ));
        let (generation, work) = workflow.begin(Action::Download).unwrap();
        drop(work);
        workflow.finish(
            generation,
            Outcome::Downloaded(Ok((asset().0, "0.2.0".into()))),
        );
        assert!(workflow.pending.is_none());
        let (generation, work) = workflow.begin(Action::Restart).unwrap();
        drop(work);
        assert!(matches!(
            workflow.begin(Action::Check),
            Err(ControllerError::Busy)
        ));
        assert_eq!(workflow.cancel_transfer(), Err(ControllerError::Busy));
        workflow.finish(
            generation,
            Outcome::Restarted {
                result: Ok(()),
                retry: None,
            },
        );
        assert!(workflow.handed_off);
        for action in [
            Action::Check,
            Action::Download,
            Action::Restart,
            Action::Discard,
        ] {
            assert!(matches!(
                workflow.begin(action),
                Err(ControllerError::ActionUnavailable)
            ));
        }
    }

    #[test]
    fn cancelled_transfer_discards_a_completed_verified_asset_and_rejects_old_completion() {
        let mut workflow = Workflow::<Asset, Asset>::new(None);
        available(&mut workflow);
        let (generation, work) = workflow.begin(Action::Download).unwrap();
        drop(work);
        assert!(workflow.cancel_transfer().unwrap());
        assert!(!workflow.authorized(generation));
        assert!(matches!(
            workflow.begin(Action::Check),
            Err(ControllerError::Busy)
        ));
        let (download, drops) = asset();
        workflow.finish(
            generation,
            Outcome::Downloaded(Ok((download, "0.2.0".into()))),
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(workflow.status.phase, UpdatePhase::Idle);
        let (new_generation, _) = workflow.begin(Action::Check).unwrap();
        assert_ne!(new_generation, generation);
        assert!(!workflow.finish(generation, Outcome::Checked(Ok(None))));
        assert_eq!(workflow.status.phase, UpdatePhase::Checking);
    }

    #[test]
    fn failed_call_gate_retains_stage_for_retry_without_handoff() {
        let mut workflow = Workflow::<Asset, Asset>::new(None);
        staged(&mut workflow);
        let (generation, work) = workflow.begin(Action::Restart).unwrap();
        let Work::Restart(stage) = work else {
            panic!("wrong work");
        };
        workflow.finish(
            generation,
            Outcome::Restarted {
                result: Err(UpdateError::CallActive.into()),
                retry: Some(stage),
            },
        );
        assert!(workflow.status.can_restart());
        assert_eq!(workflow.status.error, Some(UpdateError::CallActive.into()));
        assert!(!workflow.handed_off);
        assert!(matches!(
            workflow.begin(Action::Restart),
            Ok((_, Work::Restart(_)))
        ));
    }

    #[test]
    fn shutdown_cancels_transfer_but_drains_an_authorized_restart() {
        let mut check = Workflow::<Asset, Asset>::new(None);
        let (generation, _) = check.begin(Action::Check).unwrap();
        check.begin_shutdown();
        assert!(!check.authorized(generation));
        check.finish(generation, Outcome::Checked(Ok(None)));
        assert_eq!(check.status.phase, UpdatePhase::Closing);
        assert!(matches!(
            check.begin(Action::Check),
            Err(ControllerError::ShuttingDown)
        ));

        let mut restart = Workflow::<Asset, Asset>::new(None);
        staged(&mut restart);
        let (generation, work) = restart.begin(Action::Restart).unwrap();
        drop(work);
        restart.begin_shutdown();
        assert!(restart.authorized(generation));
        restart.finish(
            generation,
            Outcome::Restarted {
                result: Ok(()),
                retry: None,
            },
        );
        assert!(restart.handed_off);
        assert_eq!(restart.status.phase, UpdatePhase::RestartReady);
    }

    #[test]
    fn worker_failure_blocks_retries_after_unknown_restart_outcome() {
        let mut workflow = Workflow::<Asset, Asset>::new(None);
        staged(&mut workflow);
        let (generation, work) = workflow.begin(Action::Restart).unwrap();
        drop(work);
        assert!(workflow.worker_failed(generation));
        assert_eq!(workflow.status.phase, UpdatePhase::Disabled);
        assert!(!workflow.status.can_restart());
        assert!(matches!(
            workflow.begin(Action::Check),
            Err(ControllerError::ActionUnavailable)
        ));
    }

    #[test]
    fn snapshots_change_for_each_transition_and_do_not_expose_transport_diagnostics() {
        let mut workflow = Workflow::<Asset, Asset>::new(None);
        let before = workflow.status.clone();
        let (generation, _) = workflow.begin(Action::Check).unwrap();
        assert_ne!(before.revision, workflow.status.revision);
        let checking = workflow.status.clone();
        workflow.finish(
            generation,
            Outcome::Checked(Err(UpdateError::CheckFailed.into())),
        );
        assert_ne!(checking.revision, workflow.status.revision);
        assert_eq!(
            workflow.status.message(),
            "The signed update feed could not be checked."
        );
        assert!(display_version(&"x".repeat(1024)).chars().count() <= 129);
    }

    #[test]
    fn restart_security_lease_releases_only_for_current_known_failure() {
        fn preparing() -> (Shared, Arc<SecurityGate>) {
            let security = Arc::new(SecurityGate::default());
            let mut workflow = Workflow::new(None);
            workflow.generation = 1;
            workflow.pending = Some(Pending {
                generation: 1,
                action: Action::Restart,
                cancelled: false,
            });
            workflow.status.phase = UpdatePhase::PreparingRestart;
            let (status, _) = watch::channel(workflow.status.clone());
            let shared = Shared {
                inner: Mutex::new(Inner {
                    workflow,
                    workers: Vec::new(),
                    restart_lease: security.try_restart(),
                }),
                status,
                startup: Arc::new(Mutex::new(StartupSession {
                    #[cfg(target_os = "linux")]
                    appimage: None,
                    restarted: false,
                })),
                activity: UpdateActivity::default(),
                security: security.clone(),
                account_access_busy: Arc::new(|| false),
                shutdown: tokio::sync::Mutex::new(()),
            };
            (shared, security)
        }

        let (shared, security) = preparing();
        shared.finish(
            0,
            Ok(Outcome::Restarted {
                result: Err(UpdateError::CallActive.into()),
                retry: None,
            }),
        );
        assert!(
            security.blocked(),
            "stale failures cannot release a live reservation"
        );
        shared.finish(
            1,
            Ok(Outcome::Restarted {
                result: Err(UpdateError::CallActive.into()),
                retry: None,
            }),
        );
        assert!(
            !security.blocked(),
            "known failure leaves recovery available"
        );

        let (shared, security) = preparing();
        shared.finish(
            1,
            Ok(Outcome::Restarted {
                result: Ok(()),
                retry: None,
            }),
        );
        assert!(
            security.blocked(),
            "successful handoff holds the reservation"
        );
        drop(shared);
        assert!(!security.blocked());

        let (shared, security) = preparing();
        shared.finish(1, Err(ControllerError::WorkerFailed));
        assert!(
            security.blocked(),
            "unknown handoff outcome must fail closed"
        );
        assert_eq!(
            shared.inner.lock().unwrap().workflow.status.phase,
            UpdatePhase::Disabled
        );
        drop(shared);
        assert!(!security.blocked());
    }

    #[tokio::test]
    async fn shutdown_drains_other_workers_after_failure_and_serializes_multiple_callers() {
        let startup = Arc::new(Mutex::new(StartupSession {
            #[cfg(target_os = "linux")]
            appimage: None,
            restarted: false,
        }));
        let controller = UpdateController::new(
            Handle::current(),
            UpdateActivity::default(),
            startup.clone(),
            Arc::new(SecurityGate::default()),
            Arc::new(|| false),
        );
        let mut status = controller.subscribe();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(AtomicUsize::new(0));
        let failed = tokio::spawn(std::future::pending::<()>());
        failed.abort();
        let worker = {
            let started = started.clone();
            let release = release.clone();
            let finished = finished.clone();
            tokio::spawn(async move {
                started.notify_one();
                release.notified().await;
                finished.fetch_add(1, Ordering::SeqCst);
            })
        };
        controller
            .shared
            .inner
            .lock()
            .unwrap()
            .workers
            .extend([failed, worker]);
        started.notified().await;
        let first = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.shutdown().await })
        };
        while status.borrow_and_update().phase != UpdatePhase::Closing {
            status.changed().await.unwrap();
        }
        let second = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.shutdown().await })
        };
        tokio::task::yield_now().await;
        assert!(!first.is_finished());
        assert!(!second.is_finished());
        assert_eq!(finished.load(Ordering::SeqCst), 0);
        release.notify_one();
        assert_eq!(first.await.unwrap(), Err(ControllerError::WorkerFailed));
        assert_eq!(
            second.await.unwrap(),
            Ok(ShutdownSummary {
                restart_handed_off: false
            })
        );
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        // Root ownership outlives both drains, including an error result.
        assert!(Arc::strong_count(&startup) >= 2);
        assert!(matches!(
            controller.check(),
            Err(ControllerError::ShuttingDown)
        ));
    }
}
