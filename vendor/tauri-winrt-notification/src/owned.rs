//! The caller owns the thread running this operation and must join that actual
//! thread (including TLS) separately. No worker, response thread, or timer is
//! created here. Removing a registration or history entry is not a callback or
//! operating-system delivery join.

use std::{
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender},
        Arc,
    },
    time::Duration,
};

use windows::{
    core::{IInspectable, Interface, HSTRING},
    Foundation::TypedEventHandler,
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
    UI::Notifications::{
        ToastActivatedEventArgs, ToastDismissalReason, ToastDismissedEventArgs, ToastNotification,
        ToastNotificationManager, ToastNotifier,
    },
};

use crate::Toast;

const MAX_ACTIONS: usize = 8;
const MAX_ACTION_UNITS: usize = 64;
const OBSERVATION_STEP: Duration = Duration::from_millis(100);

/// Exact per-toast native identity, never a credential or account authority.
/// The caller must refuse collisions with its bounded live and retained owners.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    tag: String,
    group: String,
}

impl Identity {
    /// Windows 10 supports these sixteen-character tag and group limits.
    /// No assumption is made about the format of a caller's UUID constructor.
    pub fn new(tag: &str, group: &str) -> Option<Self> {
        fn valid(value: &str) -> bool {
            value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        }
        (valid(tag) && valid(group)).then(|| Self {
            tag: tag.to_owned(),
            group: group.to_owned(),
        })
    }
}

/// A bounded response. An action index refers only to the original builder's
/// checked button order; no native string is forwarded through a callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Response {
    Default,
    Action(usize),
    Dismissed(Option<ToastDismissalReason>),
}

/// Result of a native operation, separate from actual thread completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Refused,
    Submitted,
    Cancelled,
    Responded(Response),
}

/// Fixed operation category. No server, XML, path, or native error is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationFailure {
    Unsupported,
    Preparation,
    Submission,
    Callback,
}

/// Exact event-registration result after this token's gate is closed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RegistrationRetirement {
    /// The registration method was never entered.
    #[default]
    NotEntered,
    /// Removing the exact returned registration token returned success.
    /// Asynchronous events already in flight can still arrive at closed state.
    Revoked,
    /// Registration or token removal returned an error. No successful
    /// unregistration is asserted. A native-held delegate can own only the
    /// closed bounded state; its identity must stay in the caller's bounded
    /// retained-identity ledger, without a retry or token reuse.
    FailedClosed,
}

/// Optional withdrawal of the exact toast, separate from delegate ownership.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HistoryWithdrawal {
    #[default]
    NotRequested,
    /// Exact tag/group/AUMID removal returned success, without a claim that
    /// OS delivery or an in-flight event cannot still complete.
    Removed,
    Failed,
}

/// State of the Rust callback body at the same atomic gate-close operation.
/// BodyReturned is not a native Invoke/thread/callback-queue join.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallbackBody {
    #[default]
    NotEntered,
    BodyReturned,
    /// A body was atomically admitted but had not returned at gate closure.
    /// It may still catch and retain one panic payload. The SAME identity
    /// reservation must remain held even if failed was not yet observable.
    InFlightClosed,
}

/// Facts observed before local interface references are released on their
/// original apartment. No callback or OS-delivery quiescence is asserted.
/// A native call that is still blocked has not returned these facts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Retirement {
    pub activated: RegistrationRetirement,
    pub dismissed: RegistrationRetirement,
    pub history: HistoryWithdrawal,
    pub callback_body: CallbackBody,
}

impl Retirement {
    /// Whether the caller must keep this exact bounded identity reservation.
    /// This is conservative and does not wait for global OS quiescence.
    pub fn requires_identity_reservation(self) -> bool {
        self.callback_body == CallbackBody::InFlightClosed || self.has_failed_call()
    }

    fn has_failed_call(self) -> bool {
        self.activated == RegistrationRetirement::FailedClosed
            || self.dismissed == RegistrationRetirement::FailedClosed
            || self.history == HistoryWithdrawal::Failed
    }
}

/// A known operation error and the separate retirement observations.
/// This report follows local interface Release, not an actual worker/TLS join.
/// It never authorizes identity reuse or successful unregistration where a
/// registration result is FailedClosed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    pub operation: Option<OperationFailure>,
    pub retirement: Retirement,
}

impl Failure {
    /// Keep callback panic retention finite even after all actual app-owned
    /// worker joins. A late admitted body is covered by the retirement fact.
    pub fn requires_identity_reservation(self) -> bool {
        self.operation == Some(OperationFailure::Callback)
            || self.retirement.requires_identity_reservation()
    }
}

/// Every returned native-operation success includes its retirement facts.
/// This is not an actual worker/TLS/supervisor completion receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub outcome: Outcome,
    pub retirement: Retirement,
}

impl Report {
    /// A successful operation can still have admitted an opaque callback body
    /// at retirement; it must not silently recycle that exact reservation.
    pub fn requires_identity_reservation(self) -> bool {
        self.retirement.requires_identity_reservation()
    }
}

struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn enter() -> Result<Self, OperationFailure> {
        // SAFETY: this is the actual caller-owned native thread. S_OK and
        // S_FALSE each acquire one lease; an error acquires none. The !Send
        // guard is released on this same thread after all local interfaces.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|_| OperationFailure::Preparation)?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: one successful RoInitialize belongs to this !Send guard.
        unsafe { RoUninitialize() };
    }
}

const OPEN: u8 = 0;
const CLAIMED: u8 = 1;
const RETURNED: u8 = 2;
const CLOSED_UNCLAIMED: u8 = 3;
const CLOSED_CLAIMED: u8 = 4;
const CLOSED_RETURNED: u8 = 5;

struct CallbackState {
    lifecycle: AtomicU8,
    failed: AtomicBool,
    sender: SyncSender<Response>,
    actions: Vec<Vec<u16>>,
}

impl CallbackState {
    fn close(&self) -> CallbackBody {
        let mut current = self.lifecycle.load(Ordering::Acquire);
        loop {
            let (next, fact) = match current {
                OPEN => (CLOSED_UNCLAIMED, CallbackBody::NotEntered),
                CLAIMED => (CLOSED_CLAIMED, CallbackBody::InFlightClosed),
                RETURNED => (CLOSED_RETURNED, CallbackBody::BodyReturned),
                CLOSED_UNCLAIMED => return CallbackBody::NotEntered,
                CLOSED_CLAIMED => return CallbackBody::InFlightClosed,
                CLOSED_RETURNED => return CallbackBody::BodyReturned,
                _ => {
                    self.failed.store(true, Ordering::Release);
                    (CLOSED_CLAIMED, CallbackBody::InFlightClosed)
                }
            };
            match self.lifecycle.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return fact,
                Err(observed) => current = observed,
            }
        }
    }

    fn deliver(&self, produce: impl FnOnce() -> Response) {
        // The SAME atomic transition both consumes the only admission and
        // records the in-flight body. There is no post-claim counter window.
        if self
            .lifecycle
            .compare_exchange(OPEN, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let response = produce();
            if self.sender.try_send(response).is_err() {
                self.failed.store(true, Ordering::Release);
            }
        }));
        if let Err(payload) = result {
            self.failed.store(true, Ordering::Release);
            std::mem::forget(payload);
        }
        // Mark body return only AFTER fixed failure/payload retention. A close
        // racing the body observes CLAIMED atomically and retains its identity,
        // even if this store happens after the operation already returned.
        match self.lifecycle.compare_exchange(
            CLAIMED,
            RETURNED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(CLOSED_CLAIMED) => self.lifecycle.store(CLOSED_RETURNED, Ordering::Release),
            Err(_) => self.failed.store(true, Ordering::Release),
        }
    }

    fn activated(&self, value: &Option<IInspectable>) -> Response {
        let Some(value) = value else {
            return Response::Dismissed(None);
        };
        let Ok(args) = value.cast::<ToastActivatedEventArgs>() else {
            return Response::Dismissed(None);
        };
        let Ok(arguments) = args.Arguments() else {
            return Response::Dismissed(None);
        };
        let units: &[u16] = &arguments;
        if units.is_empty() {
            return Response::Default;
        }
        if units.len() <= MAX_ACTION_UNITS {
            if let Some(index) = self
                .actions
                .iter()
                .position(|action| action.as_slice() == units)
            {
                return Response::Action(index);
            }
        }
        // An unrecognized action must never become a default/open action.
        Response::Dismissed(None)
    }
}

fn revoke_registration(
    entered: bool,
    token: Option<i64>,
    remove: impl FnOnce(i64) -> windows::core::Result<()>,
) -> RegistrationRetirement {
    if !entered {
        RegistrationRetirement::NotEntered
    } else if token.is_some_and(|token| remove(token).is_ok()) {
        RegistrationRetirement::Revoked
    } else {
        // A failed registration has no returned token; it is not
        // silently converted to "not registered". Both failures
        // require closed bounded late-callback/identity accounting.
        RegistrationRetirement::FailedClosed
    }
}

struct NativeOwner {
    template: Option<ToastNotification>,
    notifier: Option<ToastNotifier>,
    activated: Option<TypedEventHandler<ToastNotification, IInspectable>>,
    dismissed: Option<TypedEventHandler<ToastNotification, ToastDismissedEventArgs>>,
    activated_token: Option<i64>,
    dismissed_token: Option<i64>,
    activated_entered: bool,
    dismissed_entered: bool,
    callbacks: Arc<CallbackState>,
    receiver: Receiver<Response>,
    identity: Identity,
    app_id: HSTRING,
    show_entered: bool,
    retired: Option<Retirement>,
    // Fields are dropped in declaration order: this lease is last, including
    // during unwinding after an application callback panics.
    _apartment: Apartment,
}

impl NativeOwner {
    fn prepare(toast: &Toast, identity: Identity) -> Result<Self, Failure> {
        let fail = |operation| Failure {
            operation: Some(operation),
            retirement: Retirement::default(),
        };
        // Existing callback APIs remain unchanged, but cannot enter this owned
        // route: their arbitrary closures must not become late COM payloads.
        if toast.on_activated.is_some()
            || toast.on_dismissed.is_some()
            || toast.progress.is_some()
            || toast.buttons.len() > MAX_ACTIONS
        {
            return Err(fail(OperationFailure::Unsupported));
        }
        let mut actions = Vec::with_capacity(toast.buttons.len());
        for button in &toast.buttons {
            let units: Vec<u16> = button
                .action
                .encode_utf16()
                .take(MAX_ACTION_UNITS + 1)
                .collect();
            if units.is_empty() || units.len() > MAX_ACTION_UNITS {
                return Err(fail(OperationFailure::Unsupported));
            }
            actions.push(units);
        }
        let apartment = Apartment::enter().map_err(fail)?;
        let (sender, receiver) = sync_channel(1);
        let callbacks = Arc::new(CallbackState {
            lifecycle: AtomicU8::new(OPEN),
            failed: AtomicBool::new(false),
            sender,
            actions,
        });
        let mut owner = Self {
            template: None,
            notifier: None,
            activated: None,
            dismissed: None,
            activated_token: None,
            dismissed_token: None,
            activated_entered: false,
            dismissed_entered: false,
            callbacks,
            receiver,
            identity,
            app_id: HSTRING::from(&toast.app_id),
            show_entered: false,
            retired: None,
            _apartment: apartment,
        };
        let prepared = (|| -> crate::Result<()> {
            owner.template = Some(toast.create_template()?);
            let template = owner.template.as_ref().expect("owned template");
            template.SetTag(&HSTRING::from(&owner.identity.tag))?;
            template.SetGroup(&HSTRING::from(&owner.identity.group))?;
            owner.notifier = Some(ToastNotificationManager::CreateToastNotifierWithId(
                &owner.app_id,
            )?);
            let state = Arc::clone(&owner.callbacks);
            owner.activated = Some(TypedEventHandler::new(move |_, value| {
                state.deliver(|| state.activated(&value));
                Ok(())
            }));
            owner.activated_entered = true;
            owner.activated_token = Some(
                template.Activated(owner.activated.as_ref().expect("owned activation delegate"))?,
            );
            let state = Arc::clone(&owner.callbacks);
            owner.dismissed = Some(TypedEventHandler::<
                ToastNotification,
                ToastDismissedEventArgs,
            >::new(move |_, args| {
                state.deliver(|| {
                    Response::Dismissed(args.as_ref().and_then(|args| args.Reason().ok()))
                });
                Ok(())
            }));
            owner.dismissed_entered = true;
            owner.dismissed_token = Some(
                template.Dismissed(owner.dismissed.as_ref().expect("owned dismissal delegate"))?,
            );
            Ok(())
        })();
        if prepared.is_err() {
            let retirement = owner.retire(false);
            return Err(Failure {
                operation: Some(OperationFailure::Preparation),
                retirement,
            });
        }
        Ok(owner)
    }

    fn withdraw_exact(&self) -> HistoryWithdrawal {
        // Hide has foreground restrictions. Never use it, Clear(), or an
        // unqualified tag removal. Only this exact tag/group/AUMID is used.
        if ToastNotificationManager::History()
            .and_then(|history| {
                history.RemoveGroupedTagWithId(
                    &HSTRING::from(&self.identity.tag),
                    &HSTRING::from(&self.identity.group),
                    &self.app_id,
                )
            })
            .is_ok()
        {
            HistoryWithdrawal::Removed
        } else {
            HistoryWithdrawal::Failed
        }
    }

    fn retire(&mut self, withdraw: bool) -> Retirement {
        if let Some(retirement) = self.retired {
            return retirement;
        }
        let callback_body = self.callbacks.close();
        let mut retirement = Retirement {
            callback_body,
            ..Retirement::default()
        };
        if let Some(template) = &self.template {
            retirement.activated = revoke_registration(
                self.activated_entered,
                self.activated_token.take(),
                |token| template.RemoveActivated(token),
            );
            retirement.dismissed = revoke_registration(
                self.dismissed_entered,
                self.dismissed_token.take(),
                |token| template.RemoveDismissed(token),
            );
        }
        if withdraw && self.show_entered {
            retirement.history = self.withdraw_exact();
        }
        // COM ownership is per interface reference. Releasing our references
        // does not revoke a separately native-held delegate reference, which
        // contains only the closed Arc state. Its later Release is safe and
        // cannot drop an app callback, SDK/client/UI owner, or message body.
        // Record FailedClosed distinctly and never repeat Remove from Drop.
        self.activated.take();
        self.dismissed.take();
        self.template.take();
        self.notifier.take();
        self.retired = Some(retirement);
        retirement
    }
}

impl Drop for NativeOwner {
    fn drop(&mut self) {
        // Unwind cleanup is resource safety, not a successful completion
        // receipt. The actual retained worker's panic remains a failed join.
        if self.retired.is_none() {
            let _ = self.retire(self.show_entered);
        }
    }
}

impl Toast {
    /// Submit and optionally observe one toast on the caller-owned thread.
    ///
    /// `cancelled` must read only the caller's closed cancellation state.
    /// `handover` consumes the original app/account authority immediately before
    /// Show. It is never run from a COM callback. `submitted` runs only after
    /// Show returns success, before a possibly already queued response is read.
    /// The caller must publish its pending token before entering this method.
    ///
    /// This new route requires Windows 10. Existing legacy APIs are unchanged.
    pub fn run_owned(
        self,
        identity: Identity,
        wait_for_response: bool,
        cancelled: impl Fn() -> bool,
        handover: impl FnOnce() -> bool,
        submitted: impl FnOnce(),
        response: impl FnOnce(Response),
    ) -> Result<Report, Failure> {
        let mut owner = NativeOwner::prepare(&self, identity)?;
        // Native callbacks now retain no builder, title, body, images, SDK,
        // application callback, or client/cache owner.
        drop(self);
        let mut withdraw = false;
        let outcome = if cancelled() || !handover() {
            Ok(Outcome::Refused)
        } else {
            owner.show_entered = true;
            // Final consuming handover above is directly followed by Show;
            // there is no policy await, fallback, or retry in between.
            if owner
                .notifier
                .as_ref()
                .expect("owned notifier")
                .Show(owner.template.as_ref().expect("owned template"))
                .is_err()
            {
                withdraw = true;
                Err(OperationFailure::Submission)
            } else {
                submitted();
                if !wait_for_response {
                    Ok(Outcome::Submitted)
                } else {
                    loop {
                        if owner.callbacks.failed.load(Ordering::Acquire) {
                            withdraw = true;
                            break Err(OperationFailure::Callback);
                        }
                        if cancelled() {
                            withdraw = true;
                            break Ok(Outcome::Cancelled);
                        }
                        match owner.receiver.recv_timeout(OBSERVATION_STEP) {
                            Ok(event) => break Ok(Outcome::Responded(event)),
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => {
                                withdraw = true;
                                break Err(OperationFailure::Callback);
                            }
                        }
                    }
                }
            }
        };
        let mut retirement = owner.retire(withdraw);
        // A callback failure already observed during retirement must not be
        // silently turned into a successful cancellation/submission receipt.
        let outcome = if owner.callbacks.failed.load(Ordering::Acquire) && outcome.is_ok() {
            if owner.show_entered && retirement.history == HistoryWithdrawal::NotRequested {
                retirement.history = owner.withdraw_exact();
                owner.retired = Some(retirement);
            }
            Err(OperationFailure::Callback)
        } else {
            outcome
        };
        drop(owner); // all interfaces, then this thread's apartment lease
        if let Ok(Outcome::Responded(event)) = outcome {
            // A valid response remains a separate fact even if exact native
            // revocation failed. Caller authorization still decides whether
            // this explicit action may enter protected navigation.
            response(event);
        }
        if retirement.has_failed_call() || outcome.is_err() {
            return Err(Failure {
                operation: outcome.err(),
                retirement,
            });
        }
        Ok(Report {
            outcome: outcome.expect("checked native outcome"),
            retirement,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn callbacks() -> (Arc<CallbackState>, Receiver<Response>) {
        let (sender, receiver) = sync_channel(1);
        (
            Arc::new(CallbackState {
                lifecycle: AtomicU8::new(OPEN),
                failed: AtomicBool::new(false),
                sender,
                actions: Vec::new(),
            }),
            receiver,
        )
    }

    #[test]
    fn exact_identity_and_closed_callback_never_admit_a_late_response() {
        assert!(Identity::new("0123456789abcdef", "fedcba9876543210").is_some());
        assert!(Identity::new("0123456789abcdeg", "fedcba9876543210").is_none());
        assert!(Identity::new("0123456789abcdef0", "fedcba9876543210").is_none());
        let (invalid, invalid_receiver) = callbacks();
        let no_action = invalid.activated(&None);
        // A genuine unrelated WinRT value exercises failed IInspectable cast
        // without creating or submitting a notification. All refs are released
        // before the owned apartment lease and before any assertion.
        let apartment = Apartment::enter();
        let unrelated = if apartment.is_ok() {
            windows::Foundation::PropertyValue::CreateString(&HSTRING::from("not-activation"))
                .map(|value| invalid.activated(&Some(value)))
                .map_err(|_| ())
        } else {
            Err(())
        };
        drop(apartment);
        drop(invalid_receiver);
        drop(invalid);
        assert_eq!(no_action, Response::Dismissed(None));
        assert_eq!(unrelated, Ok(Response::Dismissed(None)));
        let (state, receiver) = callbacks();
        state.deliver(|| Response::Default);
        state.deliver(|| panic!("late callback body must be inert"));
        let positive = receiver.try_recv();
        let no_extra = receiver.try_recv();
        assert_eq!(positive, Ok(Response::Default));
        assert!(no_extra.is_err());
        assert!(!state.failed.load(Ordering::Acquire));
        let (closed, receiver) = callbacks();
        closed.close();
        closed.deliver(|| panic!("closed callback body must be inert"));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn retirement_facts_preserve_exact_tokens_and_distinguish_unknown_registration() {
        let mut removed = Vec::new();
        let not_entered = revoke_registration(false, None, |token| {
            removed.push(token);
            Ok(())
        });
        let no_returned_token = revoke_registration(true, None, |token| {
            removed.push(token);
            Ok(())
        });
        let revoked = revoke_registration(true, Some(23), |token| {
            removed.push(token);
            Ok(())
        });
        let failed_closed = revoke_registration(true, Some(17), |token| {
            removed.push(token);
            Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                -2147467259,
            )))
        });
        let ordinary = Retirement {
            activated: revoked,
            dismissed: not_entered,
            history: HistoryWithdrawal::NotRequested,
            callback_body: CallbackBody::NotEntered,
        };
        let registration_failed = Retirement {
            activated: failed_closed,
            ..ordinary
        };
        let history_failed = Retirement {
            history: HistoryWithdrawal::Failed,
            ..ordinary
        };
        assert_eq!(not_entered, RegistrationRetirement::NotEntered);
        assert_eq!(no_returned_token, RegistrationRetirement::FailedClosed);
        assert_eq!(revoked, RegistrationRetirement::Revoked);
        assert_eq!(failed_closed, RegistrationRetirement::FailedClosed);
        assert_eq!(removed, [23, 17]);
        assert!(!ordinary.has_failed_call());
        assert!(registration_failed.has_failed_call());
        assert!(history_failed.has_failed_call());
        assert_eq!(history_failed.activated, RegistrationRetirement::Revoked);
        assert_eq!(registration_failed.history, HistoryWithdrawal::NotRequested);
    }

    #[test]
    fn retirement_reserves_the_same_identity_before_an_admitted_body_can_return() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let (state, receiver) = callbacks();
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let body_state = Arc::clone(&state);
        let body_drops = Arc::clone(&drops);
        let worker = std::thread::Builder::new()
            .name("owned-callback-test".into())
            .spawn(move || {
                body_state.deliver(|| {
                    let _ = entered_tx.send(());
                    let _ = release_rx.recv();
                    std::panic::panic_any(Payload(body_drops));
                });
            });
        let entered = entered_rx.recv_timeout(Duration::from_secs(1));
        let snapshot = Retirement {
            callback_body: state.close(),
            ..Retirement::default()
        };
        let report = Report {
            outcome: Outcome::Submitted,
            retirement: snapshot,
        };
        let failed_before_release = state.failed.load(Ordering::Acquire);
        // Defensive release and actual thread/TLS join precede every assertion,
        // including the unexpected spawn/admission paths.
        let _ = release_tx.send(());
        drop(release_tx);
        let joined = match worker {
            Ok(worker) => match worker.join() {
                Ok(()) => Some(true),
                Err(payload) => {
                    std::mem::forget(payload);
                    Some(false)
                }
            },
            Err(_) => None,
        };
        let after_body = state.close();
        let failed_after_join = state.failed.load(Ordering::Acquire);
        state.deliver(|| panic!("retired token cannot admit another body"));
        drop(receiver);
        drop(state);
        assert!(entered.is_ok());
        assert_eq!(joined, Some(true));
        assert!(!failed_before_release);
        assert_eq!(snapshot.callback_body, CallbackBody::InFlightClosed);
        assert!(report.requires_identity_reservation());
        assert_eq!(after_body, CallbackBody::BodyReturned);
        assert!(failed_after_join);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn callback_panic_closes_once_before_payload_retention_and_foreign_return() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let (state, receiver) = callbacks();
        let drops = Arc::new(AtomicUsize::new(0));
        let callback = Arc::clone(&state);
        let callback_drops = Arc::clone(&drops);
        let delegate = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            callback.deliver(|| std::panic::panic_any(Payload(Arc::clone(&callback_drops))));
            Ok(())
        });
        // Invoke the real generated COM ABI without constructing a toast or
        // submitting any notification. Both calls must return through FFI.
        let first = delegate.Invoke(None::<&ToastNotification>, None::<&IInspectable>);
        let late = delegate.Invoke(None::<&ToastNotification>, None::<&IInspectable>);
        let failed = state.failed.load(Ordering::Acquire);
        let closed = state.lifecycle.load(Ordering::Acquire) != OPEN;
        let event = receiver.try_recv();
        drop(delegate);
        drop(receiver);
        drop(state);
        assert!(first.is_ok() && late.is_ok());
        assert!(failed && closed);
        assert!(event.is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(Failure {
            operation: Some(OperationFailure::Callback),
            retirement: Retirement::default()
        }
        .requires_identity_reservation());
    }
}
