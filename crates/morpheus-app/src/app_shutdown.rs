//! Actual application resources held before cleanup starts on every host. This is one
//! resource pack, not a task registry. Unknown ownership never unloads stores.

use crate::{
    config_watch::{ConfigWatch, ConfigWatchError},
    live::{DraftDrainError, LiveUi},
    marker_scans::MarkerDrainError,
    notifications::{self, DesktopNotifications, NotificationDrainFailure},
    style_profiles::AppearanceDrainError,
    update_controller::UpdateController,
    verification_work::VerificationShutdownError,
};
use morpheus_config::ConfigStore;
use morpheus_matrix::AccountManager;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{sync::Mutex, task::JoinHandle};

/// The conclusion selects the first failed step in main's existing order.
/// Every value is fixed; no SDK/native error or panic payload is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownStep {
    Draft,
    AttachmentAudio,
    DeviceOpening,
    RingAudio,
    NativeAudio,
    NativeCall,
    UpdateWorker,
    Acceptance,
    DraftWork,
    ConfigWrite,
    Appearance,
    Verification,
    ConfigWatch,
    MarkerScan,
    Discovery,
    NotificationApp,
    NotificationNative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Cleanup(ShutdownStep),
    CleanupPanicked,
    CleanupCancelled,
    Ownership,
    FinalizerStart,
    FinalizerPanicked,
    EventLoopEnded,
}

use self::ShutdownStep as Step;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Complete,
    Failed,
    Pending,
    Unproved,
}
impl Status {
    fn failed(self) -> bool {
        matches!(self, Self::Failed | Self::Unproved)
    }
}

/// Before and after preserve the original acceptance error precedence. The
/// actual main-thread acceptance conclusion consumes its owner exactly once.
#[derive(Clone, Copy)]
pub(crate) struct Report {
    before_acceptance: Option<Step>,
    after_acceptance: Option<Step>,
    notification: Option<Step>,
    pub(crate) retain: bool,
}
impl Report {
    pub(crate) fn outcome(self, acceptance_failed: bool) -> Result<(), Failure> {
        self.before_acceptance
            .or(acceptance_failed.then_some(Step::Acceptance))
            .or(self.after_acceptance)
            .or(self.notification)
            .map_or(
                if self.retain {
                    Err(Failure::Ownership)
                } else {
                    Ok(())
                },
                |step| Err(Failure::Cleanup(step)),
            )
    }
}

pub(crate) struct Resources {
    pub(crate) manager: Arc<AccountManager>,
    pub(crate) live: Arc<LiveUi>,
    pub(crate) store: ConfigStore,
    pub(crate) updates: UpdateController,
    pub(crate) notifications: Arc<DesktopNotifications>,
    pub(crate) config: Mutex<ConfigWatch>,
    pub(crate) update_observer: Mutex<Option<JoinHandle<()>>>,
    #[cfg(feature = "native-calls")]
    pub(crate) calls: Arc<crate::calls::CallController>,
    #[cfg(feature = "native-calls")]
    pub(crate) incoming: Mutex<crate::incoming_ring::ui::IncomingUi>,
    #[cfg(feature = "native-calls")]
    pub(crate) banners: Arc<crate::incoming_ring::desktop::CallBanners>,
    #[cfg(all(windows, feature = "native-calls"))]
    windows_audio: crate::windows_audio::OriginReceipt,
    admission_failed: AtomicBool,
}
impl Resources {
    pub(crate) fn new(
        manager: Arc<AccountManager>,
        live: Arc<LiveUi>,
        store: ConfigStore,
        updates: UpdateController,
        notifications: Arc<DesktopNotifications>,
        config: ConfigWatch,
        update_observer: JoinHandle<()>,
        #[cfg(feature = "native-calls")] calls: Arc<crate::calls::CallController>,
        #[cfg(feature = "native-calls")] incoming: crate::incoming_ring::ui::IncomingUi,
        #[cfg(feature = "native-calls")] banners: Arc<crate::incoming_ring::desktop::CallBanners>,
        #[cfg(all(windows, feature = "native-calls"))]
        windows_audio: crate::windows_audio::OriginReceipt,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager,
            live,
            store,
            updates,
            notifications,
            config: Mutex::new(config),
            update_observer: Mutex::new(Some(update_observer)),
            #[cfg(feature = "native-calls")]
            calls,
            #[cfg(feature = "native-calls")]
            incoming: Mutex::new(incoming),
            #[cfg(feature = "native-calls")]
            banners,
            #[cfg(all(windows, feature = "native-calls"))]
            windows_audio,
            admission_failed: AtomicBool::new(false),
        })
    }

    pub(crate) fn close_config(&self) {
        match self.config.try_lock() {
            Ok(config) => config.shutdown(),
            Err(_) => self.admission_failed.store(true, Ordering::Release),
        }
    }
    pub(crate) fn close_incoming_and_notifications(&self) {
        #[cfg(feature = "native-calls")]
        {
            self.banners.shutdown();
            match self.incoming.try_lock() {
                Ok(incoming) => incoming.shutdown(),
                Err(_) => self.admission_failed.store(true, Ordering::Release),
            }
        }
        self.notifications.shutdown();
        // MANDATORY native-source input: these functions must be supplied by
        // the fully reviewed presenter owner. No app-only or stub Ok fallback.
        notifications::shutdown_native();
        match self.update_observer.try_lock() {
            Ok(task) => {
                if let Some(task) = task.as_ref() {
                    task.abort();
                }
            }
            Err(_) => self.admission_failed.store(true, Ordering::Release),
        }
    }

    async fn observer(&self) -> Status {
        let mut owner = self.update_observer.lock().await;
        let Some(task) = owner.as_mut() else {
            return Status::Unproved;
        };
        let status = match task.await {
            Ok(()) => Status::Complete,
            Err(error) if error.is_cancelled() => Status::Complete,
            Err(error) if error.is_panic() => {
                std::mem::forget(error.into_panic());
                Status::Failed
            }
            Err(_) => Status::Unproved,
        };
        owner.take();
        status
    }

    /// Runs once on the existing Runtime. Every Pending observation below
    /// refers to these SAME original owners, before dependent store teardown.
    pub(crate) async fn finish(self: Arc<Self>) -> Report {
        #[cfg(feature = "native-calls")]
        let (observer, updated, call, incoming) = tokio::join!(
            self.observer(),
            self.updates.shutdown(),
            self.calls.end_call(),
            async { self.incoming.lock().await.finish_checked().await },
        );
        #[cfg(not(feature = "native-calls"))]
        let (observer, updated) = tokio::join!(self.observer(), self.updates.shutdown());
        // WorkerFailed also covers a poisoned owner before worker take. Its
        // generic Err must retain the actual controller, not infer completion.
        let update = if updated.is_ok() {
            observer
        } else {
            Status::Unproved
        };
        #[cfg(feature = "native-calls")]
        let mut call = call_status(call);
        #[cfg(feature = "native-calls")]
        let incoming = match incoming {
            Ok(()) => Status::Complete,
            Err(crate::incoming_ring::ui::IncomingDrainError::Worker) => Status::Failed,
            Err(crate::incoming_ring::ui::IncomingDrainError::Ownership) => Status::Unproved,
        };
        #[cfg(feature = "native-calls")]
        let mut ring = observe_ring().await;
        #[cfg(feature = "native-calls")]
        let mut device = observe_device().await;
        let mut attachment = observe_attachment().await;
        #[cfg(feature = "native-calls")]
        let mut discovery = observe_discovery().await;
        #[cfg(feature = "native-calls")]
        let mut backend = self.observe_backend().await;
        let mut appearance = observe_appearance().await;
        let draft_work = match self.live.drain_draft_work().await {
            Ok(()) => Status::Complete,
            Err(DraftDrainError::Worker) => Status::Failed,
            Err(DraftDrainError::NotClosed | DraftDrainError::Ownership) => Status::Unproved,
        };
        let mut verification = observe_verification(&self.live).await;
        let config = match self.config.lock().await.finish().await {
            Ok(()) => Status::Complete,
            Err(ConfigWatchError::Ownership) => Status::Unproved,
            // These are actual joined receipts/proven no-worker startup cases.
            Err(
                ConfigWatchError::Start
                | ConfigWatchError::Producer
                | ConfigWatchError::Bridge
                | ConfigWatchError::Refresh
                | ConfigWatchError::Admission,
            ) => Status::Failed,
        };
        let markers = match self.live.drain_marker_scans().await {
            Ok(()) => Status::Complete,
            Err(MarkerDrainError::Worker) => Status::Failed,
            Err(MarkerDrainError::Ownership) => Status::Unproved,
        };
        let mut notification_app = self.notification_app().await;
        let mut notification_native = self.notification_native().await;
        let mut draft = None;
        let mut config_write = None;
        loop {
            let all = [
                update,
                attachment,
                appearance,
                draft_work,
                verification,
                config,
                markers,
                notification_app,
                notification_native,
                #[cfg(feature = "native-calls")]
                call,
                #[cfg(feature = "native-calls")]
                incoming,
                #[cfg(feature = "native-calls")]
                ring,
                #[cfg(feature = "native-calls")]
                device,
                #[cfg(feature = "native-calls")]
                discovery,
                #[cfg(feature = "native-calls")]
                backend,
            ];
            let unproved =
                self.admission_failed.load(Ordering::Acquire) || all.contains(&Status::Unproved);
            if unproved {
                // Still-live owners need the original executor and stores.
                // Dependent final writer sweep/manager.stop are not replayed.
                return report(
                    true,
                    draft,
                    config_write,
                    update,
                    attachment,
                    appearance,
                    draft_work,
                    verification,
                    config,
                    markers,
                    notification_app,
                    notification_native,
                    #[cfg(feature = "native-calls")]
                    call,
                    #[cfg(feature = "native-calls")]
                    incoming,
                    #[cfg(feature = "native-calls")]
                    ring,
                    #[cfg(feature = "native-calls")]
                    device,
                    #[cfg(feature = "native-calls")]
                    discovery,
                    #[cfg(feature = "native-calls")]
                    backend,
                );
            }
            if !all.contains(&Status::Pending) {
                break;
            }
            // Re-observe only Pending original owners; do not restart shutdown,
            // downloads, dialog work, native close, or a failed leave attempt.
            #[cfg(feature = "native-calls")]
            {
                if call == Status::Pending {
                    call = call_status(self.calls.end_call().await);
                }
                if ring == Status::Pending {
                    ring = observe_ring().await;
                }
                if device == Status::Pending {
                    device = observe_device().await;
                }
                if discovery == Status::Pending {
                    discovery = observe_discovery().await;
                }
                if backend == Status::Pending {
                    backend = self.observe_backend().await;
                }
            }
            if attachment == Status::Pending {
                attachment = observe_attachment().await;
            }
            if appearance == Status::Pending {
                appearance = observe_appearance().await;
            }
            if verification == Status::Pending {
                verification = observe_verification(&self.live).await;
            }
            if notification_app == Status::Pending {
                notification_app = self.notification_app().await;
            }
            if notification_native == Status::Pending {
                notification_native = self.notification_native().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        config_write = Some(loop {
            match self.store.write_shutdown_confirmed() {
                Ok(true) => break Status::Complete,
                Ok(false) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                // The only Err is acquired-exclusive-guard poison after all
                // synchronous publish scopes returned; retain the failure.
                Err(_) => break Status::Failed,
            }
        });
        draft = Some(if self.live.flush_drafts().await.is_ok() {
            Status::Complete
        } else {
            Status::Failed
        });
        self.manager.stop().await;
        report(
            false,
            draft,
            config_write,
            update,
            attachment,
            appearance,
            draft_work,
            verification,
            config,
            markers,
            notification_app,
            notification_native,
            #[cfg(feature = "native-calls")]
            call,
            #[cfg(feature = "native-calls")]
            incoming,
            #[cfg(feature = "native-calls")]
            ring,
            #[cfg(feature = "native-calls")]
            device,
            #[cfg(feature = "native-calls")]
            discovery,
            #[cfg(feature = "native-calls")]
            backend,
        )
    }

    #[cfg(feature = "native-calls")]
    async fn observe_backend(&self) -> Status {
        #[cfg(windows)]
        {
            // Same original UI origin, after its terminal begin_shutdown/poll.
            // A Result/flag or global empty registry is never substituted.
            match self.windows_audio.observation() {
                Ok(morpheus_rtc::WasapiCloseObservation::Released(Ok(()))) => Status::Complete,
                Ok(morpheus_rtc::WasapiCloseObservation::Released(Err(_))) => Status::Failed,
                Ok(morpheus_rtc::WasapiCloseObservation::Pending) => Status::Pending,
                Ok(morpheus_rtc::WasapiCloseObservation::Retained(_)) | Err(_) => Status::Unproved,
            }
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            observe_backend().await
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            Status::Unproved
        }
    }

    async fn notification_native(&self) -> Status {
        // The exact native owner input also exposes fixture-local books.
        // Observe the captured wrapper too; global None is no local receipt.
        let local = self.notifications.finish_native_work().await;
        let finished = notifications::finish_native_work().await;
        let observed = notifications::observe_retained_native_work();
        let both = notifications::combine_app_work(local, finished);
        notification_status(notifications::combine_app_work(both, observed))
    }

    async fn notification_app(&self) -> Status {
        let desktop = self.notifications.finish_app_work().await;
        #[cfg(feature = "native-calls")]
        let incoming = self.banners.finish_app_work().await;
        #[cfg(not(feature = "native-calls"))]
        let incoming = Ok(());
        // Both actual reads run even when the first failed; whole production
        // ownership is observed independently of wrapper-local cfgtest scope.
        let both = notifications::combine_app_work(desktop, incoming);
        let global = notifications::observe_retained_app_work();
        notification_status(notifications::combine_app_work(both, global))
    }
}

fn notification_status(result: Result<(), NotificationDrainFailure>) -> Status {
    match result {
        Ok(()) => Status::Complete,
        Err(NotificationDrainFailure::Pending) => Status::Pending,
        Err(NotificationDrainFailure::Worker) => Status::Failed,
        Err(NotificationDrainFailure::Ownership) => Status::Unproved,
    }
}
async fn observe_attachment() -> Status {
    match crate::audio_attachments::wait_for_release().await {
        Ok(()) => Status::Complete,
        Err(crate::audio_attachments::ReleaseError::TimedOut) => Status::Pending,
        // Current owner returns errors before its actual supervisor join.
        Err(_) => Status::Unproved,
    }
}
async fn observe_appearance() -> Status {
    match crate::style_profiles::wait_for_appearance_release().await {
        Ok(()) => Status::Complete,
        Err(AppearanceDrainError::Pending) => Status::Pending,
        Err(AppearanceDrainError::Worker) => Status::Failed,
        Err(AppearanceDrainError::Ownership) => Status::Unproved,
    }
}
async fn observe_verification(live: &LiveUi) -> Status {
    match live.finish_verification_shutdown().await {
        Ok(()) => Status::Complete,
        Err(VerificationShutdownError::Pending) => Status::Pending,
        Err(VerificationShutdownError::Worker) => Status::Failed,
        Err(VerificationShutdownError::Unproved) => Status::Unproved,
    }
}
#[cfg(feature = "native-calls")]
fn call_status(result: Result<(), crate::calls::CallControllerError>) -> Status {
    match result {
        Ok(()) => Status::Complete,
        // end_call re-observes the same Opening/CleanupFailed watch on timeout;
        // it cannot replay a failed server leave or abandon its held lease.
        Err(crate::calls::CallControllerError::TimedOut(_)) => Status::Pending,
        Err(_) => Status::Unproved,
    }
}
#[cfg(feature = "native-calls")]
async fn observe_ring() -> Status {
    match morpheus_rtc::wait_for_ring_audio_release().await {
        Ok(()) => Status::Complete,
        Err(morpheus_rtc::RingAudioReleaseError::TimedOut) => Status::Pending,
        Err(_) => Status::Unproved,
    }
}
#[cfg(feature = "native-calls")]
async fn observe_device() -> Status {
    match morpheus_rtc::wait_for_audio_device_release().await {
        Ok(()) => Status::Complete,
        Err(morpheus_rtc::AudioDeviceReleaseError::TimedOut) => Status::Pending,
        Err(_) => Status::Unproved,
    }
}
#[cfg(feature = "native-calls")]
async fn observe_discovery() -> Status {
    match crate::audio_devices::wait_for_discovery_release().await {
        Ok(()) => Status::Complete,
        Err(crate::audio_devices::DiscoveryReleaseError::TimedOut) => Status::Pending,
        Err(crate::audio_devices::DiscoveryReleaseError::WorkerPanicked) => Status::Failed,
        Err(crate::audio_devices::DiscoveryReleaseError::SupervisorFailed) => Status::Unproved,
    }
}
#[cfg(all(feature = "native-calls", target_os = "macos"))]
async fn observe_backend() -> Status {
    match morpheus_rtc::wait_for_all_coreaudio_release().await {
        Ok(()) => Status::Complete,
        Err(
            morpheus_rtc::CoreAudioCloseError::TimedOut
            | morpheus_rtc::CoreAudioCloseError::Pending,
        ) => Status::Pending,
        Err(_) => Status::Unproved,
    }
}

#[cfg(all(feature = "native-calls", target_os = "linux"))]
async fn observe_backend() -> Status {
    match morpheus_rtc::wait_for_all_alsa_release().await {
        Ok(()) => Status::Complete,
        Err(
            morpheus_rtc::AlsaBackendCloseError::TimedOut
            | morpheus_rtc::AlsaBackendCloseError::Pending,
        ) => Status::Pending,
        // Terminal native/API/worker errors retain the exact ALSA owner.
        Err(_) => Status::Unproved,
    }
}

fn report(
    retain: bool,
    draft: Option<Status>,
    config_write: Option<Status>,
    update: Status,
    attachment: Status,
    appearance: Status,
    draft_work: Status,
    verification: Status,
    config: Status,
    markers: Status,
    notification_app: Status,
    notification_native: Status,
    #[cfg(feature = "native-calls")] call: Status,
    #[cfg(feature = "native-calls")] incoming: Status,
    #[cfg(feature = "native-calls")] ring: Status,
    #[cfg(feature = "native-calls")] device: Status,
    #[cfg(feature = "native-calls")] discovery: Status,
    #[cfg(feature = "native-calls")] backend: Status,
) -> Report {
    let before_acceptance = [
        draft.filter(|x| x.failed()).map(|_| Step::Draft),
        attachment.failed().then_some(Step::AttachmentAudio),
        #[cfg(feature = "native-calls")]
        device.failed().then_some(Step::DeviceOpening),
        #[cfg(feature = "native-calls")]
        ring.failed().then_some(Step::RingAudio),
        #[cfg(all(
            feature = "native-calls",
            any(target_os = "macos", target_os = "linux")
        ))]
        backend.failed().then_some(Step::NativeAudio),
        #[cfg(feature = "native-calls")]
        (call.failed() || incoming.failed()).then_some(Step::NativeCall),
        update.failed().then_some(Step::UpdateWorker),
    ]
    .into_iter()
    .flatten()
    .next();
    let after_acceptance = [
        draft_work.failed().then_some(Step::DraftWork),
        config_write
            .filter(|x| x.failed())
            .map(|_| Step::ConfigWrite),
        appearance.failed().then_some(Step::Appearance),
        verification.failed().then_some(Step::Verification),
        config.failed().then_some(Step::ConfigWatch),
        markers.failed().then_some(Step::MarkerScan),
        #[cfg(feature = "native-calls")]
        discovery.failed().then_some(Step::Discovery),
        #[cfg(all(windows, feature = "native-calls"))]
        backend.failed().then_some(Step::NativeAudio),
    ]
    .into_iter()
    .flatten()
    .next();
    let notification = [
        notification_app.failed().then_some(Step::NotificationApp),
        notification_native
            .failed()
            .then_some(Step::NotificationNative),
    ]
    .into_iter()
    .flatten()
    .next();
    Report {
        before_acceptance,
        after_acceptance,
        notification,
        retain,
    }
}

/// Only closed category data reaches Rust main's Debug termination path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShutdownError(pub(crate) Failure);
impl std::fmt::Display for ShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "Desktop shutdown could not be confirmed ({:?}).",
            self.0
        )
    }
}
impl std::error::Error for ShutdownError {}

/// The one ordinary non-Mac application root owns the actual Runtime before
/// any application task starts. This holds resources, not a second task list.
/// A retained root stays reachable after an unexpected UI-loop return.
#[cfg(not(target_os = "macos"))]
static PROCESS: std::sync::OnceLock<RuntimeOwner> = std::sync::OnceLock::new();

#[cfg(not(target_os = "macos"))]
pub(crate) struct RuntimeOwner {
    runtime: std::sync::Mutex<Option<tokio::runtime::Runtime>>,
    resources: std::sync::Mutex<Option<Arc<Resources>>>,
    unproved: AtomicBool,
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct RuntimeBorrow<'a> {
    actual: std::sync::MutexGuard<'a, Option<tokio::runtime::Runtime>>,
}

#[cfg(not(target_os = "macos"))]
impl std::ops::Deref for RuntimeBorrow<'_> {
    type Target = tokio::runtime::Runtime;

    fn deref(&self) -> &Self::Target {
        // Construction verified Some; this exact guard excludes a take.
        self.actual.as_ref().expect("retained application Runtime")
    }
}

#[cfg(not(target_os = "macos"))]
impl RuntimeOwner {
    fn new(runtime: tokio::runtime::Runtime) -> Self {
        Self {
            runtime: std::sync::Mutex::new(Some(runtime)),
            resources: std::sync::Mutex::new(None),
            unproved: AtomicBool::new(false),
        }
    }

    pub(crate) fn install(runtime: tokio::runtime::Runtime) -> Result<&'static Self, Failure> {
        // The only production caller is immediately after constructing this
        // fresh Runtime, before LiveUi, observers or native product workers.
        // A duplicate refuses before any owner can use the unused Runtime.
        PROCESS
            .set(Self::new(runtime))
            .map_err(|_| Failure::Ownership)?;
        PROCESS.get().ok_or(Failure::Ownership)
    }

    pub(crate) fn borrow_runtime(&self) -> Result<RuntimeBorrow<'_>, Failure> {
        let actual = self.runtime.try_lock().map_err(|_| Failure::Ownership)?;
        if actual.is_none() {
            return Err(Failure::Ownership);
        }
        Ok(RuntimeBorrow { actual })
    }

    pub(crate) fn publish(&self, actual: Arc<Resources>) -> Result<(), Failure> {
        let mut slot = match self.resources.lock() {
            Ok(slot) => slot,
            Err(error) => {
                // Recovery is only for retaining the real owner, never a
                // clean receipt. No user Drop runs under this mutex.
                self.unproved.store(true, Ordering::Release);
                error.into_inner()
            }
        };
        match slot.as_ref() {
            None => *slot = Some(actual),
            Some(previous) if Arc::ptr_eq(previous, &actual) => {}
            Some(_) => {
                // The installed Main constructs/publishes this pack once;
                // another production Main is refused before it has workers.
                self.unproved.store(true, Ordering::Release);
            }
        }
        if self.unproved.load(Ordering::Acquire) {
            Err(Failure::Ownership)
        } else {
            Ok(())
        }
    }

    fn take_completed_runtime(&self, report: Report) -> Result<tokio::runtime::Runtime, Failure> {
        if report.retain || self.unproved.load(Ordering::Acquire) {
            return Err(Failure::Ownership);
        }
        let mut actual = self.runtime.try_lock().map_err(|_| Failure::Ownership)?;
        actual.take().ok_or(Failure::Ownership)
    }

    /// The caller also checks its actual Windows origin receipt before this
    /// method. The Report comes only from the original completed Resources
    /// future, not a task count/flag or a successful Drop.
    pub(crate) fn finalize(
        &self,
        expected: &Arc<Resources>,
        report: Report,
    ) -> Result<(), Failure> {
        let runtime = {
            let slot = self.resources.try_lock().map_err(|_| Failure::Ownership)?;
            if !slot
                .as_ref()
                .is_some_and(|actual| Arc::ptr_eq(actual, expected))
            {
                return Err(Failure::Ownership);
            }
            self.take_completed_runtime(report)?
        };
        // Release both owner locks before actual Runtime destruction and its
        // task/TLS payload drops. No active EnterGuard was created or moved.
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)))
        {
            std::mem::forget(payload);
            self.unproved.store(true, Ordering::Release);
            // The actual consumed Runtime cannot be reconstructed/retried.
            // Keep its exact product resources; never fabricate a live one.
            return Err(Failure::FinalizerPanicked);
        }
        let resources = {
            let mut slot = self.resources.try_lock().map_err(|_| Failure::Ownership)?;
            if !slot
                .as_ref()
                .is_some_and(|actual| Arc::ptr_eq(actual, expected))
            {
                self.unproved.store(true, Ordering::Release);
                return Err(Failure::Ownership);
            }
            slot.take()
        };
        drop(resources);
        Ok(())
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod runtime_tests {
    use super::*;

    #[test]
    fn retained_report_keeps_the_original_runtime_and_real_task_observable() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let owner = RuntimeOwner::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let dropped = Arc::new(AtomicBool::new(false));
        let (release, wait) = tokio::sync::oneshot::channel();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let mut task = {
            let runtime = owner.borrow_runtime().unwrap();
            let marker = Dropped(dropped.clone());
            let task = runtime.spawn(async move {
                let _marker = marker;
                let _ = entered.send(());
                let _ = wait.await;
            });
            let _ = runtime.block_on(ready);
            task
        };
        let report = Report {
            before_acceptance: None,
            after_acceptance: None,
            notification: None,
            retain: true,
        };
        let mut taken = owner.take_completed_runtime(report);
        let refused = taken.is_err();
        let still_owned = !dropped.load(Ordering::Acquire);
        let released = release.send(());
        // Even a regressed take is cleaned using that same actual Runtime.
        let joined = match taken.as_mut() {
            Ok(runtime) => runtime.block_on(&mut task),
            Err(_) => owner.borrow_runtime().unwrap().block_on(&mut task),
        };
        drop(task);
        drop(taken);
        drop(owner);
        assert!(refused && still_owned && released.is_ok());
        assert!(joined.is_ok() && dropped.load(Ordering::Acquire));
    }
}
