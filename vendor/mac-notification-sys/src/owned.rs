//! Explicit native ownership for plain, immediate macOS banners.
//!
//! This operation runs only on its caller-owned worker. It creates no thread or
//! runloop timer, never sends from Drop, and never passes a Rust context or text
//! to a native callback. Returning is not a worker/TLS join or OS-delivery fence.
//! An uncertain Objective-C operation retains its exact fixed native record.

use std::{
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::Duration,
};

// Eight actual response listeners plus two actual single-flight submissions.
// These are bounded opaque callback entries, never SDK/UI/account/text owners.
const CAPACITY: usize = 10;
const OPEN: u8 = 0;
const CLAIMED: u8 = 1;
const RETURNED: u8 = 2;
const CLOSED_UNCLAIMED: u8 = 3;
const CLOSED_CLAIMED: u8 = 4;
const CLOSED_RETURNED: u8 = 5;

/// Only the successful native contents-click is an activation. Unsupported
/// native button/reply events are no-action; arbitrary strings never cross ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Response {
    /// A successfully decoded contents click.
    Click,
    /// Dismissal or an unsupported activation type.
    Dismissed,
}

/// Actual Rust callback-body fact at the atomic admission/retirement boundary.
/// BodyReturned is not a native invocation, thread, or OS queue join.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackBody {
    /// No body could enter after close consumed admission.
    NotEntered,
    /// The only admitted body returned before close.
    BodyReturned,
    /// The same admission claim recorded a body still running at close.
    InFlightClosed,
}

/// Last entered native construction/release stage, without private text/errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stage(u32);

impl Stage {
    fn checked(value: u32) -> Option<Self> {
        (1..=25).contains(&value).then_some(Self(value))
    }

    /// Stable numeric stage from the same-version native ABI, for fixed-category
    /// diagnostics only. It does not assert that an entered method returned.
    pub fn code(self) -> u32 {
        self.0
    }
}

/// The exact native record retained after an exception/unproved ABI result.
/// These nonsecret values identify ownership; they are not account authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedRecord {
    /// Fixed native record slot, when the returned receipt was well formed.
    pub slot: Option<u32>,
    /// Same monotonic cookie; zero denotes an unproved receipt.
    pub cookie: u64,
    /// Last entered stage, when well formed.
    pub stage: Option<Stage>,
}

/// Known local native-release observations, never whole-OS cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRetirement {
    /// Native construction was not entered.
    NotEntered,
    /// All app-owned local native references were released on their owner thread.
    Released,
    /// The same native record/capabilities remain retained and cannot be retried.
    Retained(RetainedRecord),
}

/// Known operation failure, separate from callback/native ownership facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationFailure {
    /// Exact application registration was never proved successful.
    Application,
    /// Fixed native/callback capacity is already occupied.
    Capacity,
    /// Unsupported/over-limit input or known preparation refusal.
    Preparation,
    /// Native submission/cleanup completion is unproved; no retry is permitted.
    NativeUncertain,
    /// The only admitted callback failed or its bounded response could not arrive.
    Callback,
}

/// Per-operation facts returned before the caller's actual worker/TLS join.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retirement {
    /// Local native-reference result, separately from OS-delivery withdrawal.
    pub native: NativeRetirement,
    /// Atomic callback-body observation.
    pub callback_body: CallbackBody,
}

impl Retirement {
    /// Keep this SAME finite identity reservation. No timer/count permits reuse.
    pub fn requires_identity_reservation(self) -> bool {
        matches!(self.native, NativeRetirement::Retained(_))
            || self.callback_body == CallbackBody::InFlightClosed
    }
}

/// Actual operation outcome, not a native worker/TLS/supervisor join receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Original admission refused before native submit entry.
    Refused,
    /// Actual native deliver method returned; no daemon acknowledgement implied.
    Submitted,
    /// Cancelled after submission; exact-object removal was attempted.
    Cancelled,
    /// Actual checked callback response, for later app authorization.
    Responded(Response),
}

/// Success still carries late-body/native identity-retention obligations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    /// Actual operation outcome.
    pub outcome: Outcome,
    /// Separate ownership/retirement observations.
    pub retirement: Retirement,
}

impl Report {
    /// Whether the finite book must retain this exact identity reservation.
    pub fn requires_identity_reservation(self) -> bool {
        self.retirement.requires_identity_reservation()
    }
}

/// Fixed operation error plus actual separate native/callback observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    /// Fixed nonsecret operation category.
    pub operation: OperationFailure,
    /// Separate ownership/retirement observations.
    pub retirement: Retirement,
}

impl Failure {
    /// Retain caught-panic accounting even after the callback body returned.
    pub fn requires_identity_reservation(self) -> bool {
        self.operation == OperationFailure::Callback
            || self.retirement.requires_identity_reservation()
    }
}

#[repr(C)]
struct Bytes {
    bytes: *const u8,
    length: u64,
}

impl From<&str> for Bytes {
    fn from(value: &str) -> Self {
        Self {
            bytes: value.as_ptr(),
            length: value.len() as u64,
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct NativeResult {
    slot: u32,
    status: u32,
    cookie: u64,
    stage: u32,
    reserved: u32,
}

const _: () = {
    assert!(size_of::<Bytes>() == 16);
    assert!(std::mem::offset_of!(Bytes, length) == 8);
    assert!(size_of::<NativeResult>() == 24);
    assert!(align_of::<NativeResult>() == 8);
    assert!(std::mem::offset_of!(NativeResult, cookie) == 8);
    assert!(std::mem::offset_of!(NativeResult, stage) == 16);
};

#[link(name = "notify")]
unsafe extern "C" {
    fn morpheus_owned_prepare(
        identity: *const u8,
        title: Bytes,
        subtitle: Bytes,
        body: Bytes,
        sound: u32,
        output: *mut NativeResult,
    );
    fn morpheus_owned_submit(
        slot: u32,
        cookie: u64,
        identity: *const u8,
        output: *mut NativeResult,
    );
    fn morpheus_owned_retire(
        slot: u32,
        cookie: u64,
        identity: *const u8,
        withdraw: u32,
        output: *mut NativeResult,
    );
    fn morpheus_owned_abandon(slot: u32, cookie: u64, identity: *const u8);
}

impl NativeResult {
    fn retained(&self) -> NativeRetirement {
        NativeRetirement::Retained(RetainedRecord {
            slot: (self.slot < CAPACITY as u32).then_some(self.slot),
            cookie: self.cookie,
            stage: Stage::checked(self.stage),
        })
    }

    fn is_live(&self) -> bool {
        self.slot < CAPACITY as u32
            && self.cookie != 0
            && self.reserved == 0
            && self.status == 0
            && self.stage == 14
    }
}

struct NativeOwner {
    slot: u32,
    cookie: u64,
    identity: [u8; 16],
    live: bool,
    // All operations and local final releases stay on the creating pthread.
    _thread: PhantomData<Rc<()>>,
}

impl NativeOwner {
    fn prepare(
        identity: [u8; 16],
        title: &str,
        subtitle: &str,
        body: &str,
        sound: bool,
    ) -> Result<Self, (OperationFailure, NativeRetirement)> {
        let mut result = NativeResult::default();
        // SAFETY: all borrowed UTF8/identity bytes remain valid for the entire
        // synchronous call; C copies them only inside a previously reserved
        // native record. Output is a fixed repr(C) value, never retained by C.
        unsafe {
            morpheus_owned_prepare(
                identity.as_ptr(),
                title.into(),
                subtitle.into(),
                body.into(),
                u32::from(sound),
                &mut result,
            )
        };
        if result.is_live() {
            return Ok(Self {
                slot: result.slot,
                cookie: result.cookie,
                identity,
                live: true,
                _thread: PhantomData,
            });
        }
        match (result.status, result.cookie, result.stage, result.reserved) {
            (1, 0, 0, 0) => Err((OperationFailure::Capacity, NativeRetirement::NotEntered)),
            (2, 0, 0, 0) => Err((OperationFailure::Preparation, NativeRetirement::NotEntered)),
            (3, cookie, 21, 0) if cookie != 0 && result.slot < CAPACITY as u32 => {
                Err((OperationFailure::Preparation, NativeRetirement::Released))
            }
            _ => Err((OperationFailure::NativeUncertain, result.retained())),
        }
    }

    fn retained(&self, result: &NativeResult) -> NativeRetirement {
        NativeRetirement::Retained(RetainedRecord {
            slot: Some(self.slot),
            cookie: self.cookie,
            stage: (result.slot == self.slot
                && result.cookie == self.cookie
                && result.reserved == 0)
                .then(|| Stage::checked(result.stage))
                .flatten(),
        })
    }

    fn submit(&mut self) -> Result<(), NativeRetirement> {
        let mut result = NativeResult::default();
        // SAFETY: this !Send owner holds the exact cookie+identity on the same
        // original pthread. C validates each field before touching native refs.
        unsafe {
            morpheus_owned_submit(self.slot, self.cookie, self.identity.as_ptr(), &mut result)
        };
        if result.slot == self.slot
            && result.cookie == self.cookie
            && result.reserved == 0
            && result.status == 0
            && result.stage == 16
        {
            Ok(())
        } else {
            self.live = false; // No native retry after an uncertain result.
            Err(self.retained(&result))
        }
    }

    fn retire(mut self, withdraw: bool) -> NativeRetirement {
        let mut result = NativeResult::default();
        // SAFETY: exact same-thread owner and fixed output ABI, as for submit.
        unsafe {
            morpheus_owned_retire(
                self.slot,
                self.cookie,
                self.identity.as_ptr(),
                u32::from(withdraw),
                &mut result,
            )
        };
        self.live = false;
        if result.slot == self.slot
            && result.cookie == self.cookie
            && result.reserved == 0
            && result.status == 0
            && result.stage == 21
        {
            NativeRetirement::Released
        } else {
            self.retained(&result)
        }
    }
}

impl Drop for NativeOwner {
    fn drop(&mut self) {
        if self.live {
            // SAFETY: metadata-only abandonment on the exact owner pthread.
            // Native refs are NOT released or submitted from Drop. The same
            // fixed record becomes Unknown and remains process-lifetime owned.
            unsafe { morpheus_owned_abandon(self.slot, self.cookie, self.identity.as_ptr()) };
        }
    }
}

struct Entry {
    lifecycle: AtomicU8,
    failed: AtomicBool,
    sender: SyncSender<Response>,
}

impl Entry {
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

    fn deliver(&self, produce: impl FnOnce() -> Option<Response>) {
        if self
            .lifecycle
            .compare_exchange(OPEN, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let result = catch_unwind(AssertUnwindSafe(|| match produce() {
            Some(response) => {
                if self.sender.try_send(response).is_err() {
                    self.failed.store(true, Ordering::Release);
                }
            }
            None => self.failed.store(true, Ordering::Release),
        }));
        if let Err(payload) = result {
            self.failed.store(true, Ordering::Release);
            self.close(); // Close the token before retaining at most one payload.
            std::mem::forget(payload);
        }
        let mut current = self.lifecycle.load(Ordering::Acquire);
        loop {
            let next = match current {
                CLAIMED => RETURNED,
                CLOSED_CLAIMED => CLOSED_RETURNED,
                _ => return,
            };
            match self.lifecycle.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

struct Record {
    identity: [u8; 16],
    entry: Arc<Entry>,
}

type Records = [Option<Record>; CAPACITY];
fn records() -> &'static Mutex<Records> {
    static RECORDS: OnceLock<Mutex<Records>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(std::array::from_fn(|_| None)))
}

struct Registration {
    identity: [u8; 16],
    entry: Arc<Entry>,
    finished: bool,
}

impl Registration {
    fn new(identity: [u8; 16]) -> Option<(Self, Receiver<Response>)> {
        let mut records = records().lock().unwrap_or_else(|e| e.into_inner());
        if records
            .iter()
            .flatten()
            .any(|record| record.identity == identity)
        {
            return None;
        }
        let slot = records.iter_mut().find(|slot| slot.is_none())?;
        let (sender, receive) = mpsc::sync_channel(1);
        let entry = Arc::new(Entry {
            lifecycle: AtomicU8::new(OPEN),
            failed: AtomicBool::new(false),
            sender,
        });
        *slot = Some(Record {
            identity,
            entry: Arc::clone(&entry),
        });
        Some((
            Self {
                identity,
                entry,
                finished: false,
            },
            receive,
        ))
    }

    fn finish(
        mut self,
        native: NativeRetirement,
        operation: Option<OperationFailure>,
        first_close: Option<CallbackBody>,
    ) -> Retirement {
        let mut callback_body = self.entry.close();
        // An earlier atomic close already observed an admitted body. Keep that
        // original conservative fact even if the body returned during native
        // removal; never remove/recycle this record before publishing the fact.
        if first_close == Some(CallbackBody::InFlightClosed) {
            callback_body = CallbackBody::InFlightClosed;
        }
        let retirement = Retirement {
            native,
            callback_body,
        };
        let retain = retirement.requires_identity_reservation()
            || operation == Some(OperationFailure::Callback)
            || self.entry.failed.load(Ordering::Acquire);
        self.finished = true;
        if !retain {
            let mut records = records().lock().unwrap_or_else(|e| e.into_inner());
            let retired = records
                .iter_mut()
                .find(|record| {
                    record.as_ref().is_some_and(|record| {
                        record.identity == self.identity && Arc::ptr_eq(&record.entry, &self.entry)
                    })
                })
                .and_then(Option::take);
            drop(records);
            drop(retired);
        }
        retirement
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.finished {
            self.entry.close();
            // Unexpected Rust unwind retains this exact bounded opaque entry.
            // The app worker panic is separately a sticky joined failure.
        }
    }
}

/// Deliver one plain banner on this actual caller-owned worker. The application
/// must have successfully called `set_application` for its exact registered ID.
/// The original admission closure is consumed directly before native submission.
/// Native response callbacks may precede `submitted`; the owning app must reserve
/// its opaque token first and hold at most one early response until publication.
///
/// `cancelled` is observed at most every 100ms between native calls. An in-flight
/// native call is never abandoned or counted complete by that observation bound.
/// Exact-object withdrawal is best effort, not a queued-delivery/callback fence.
/// User callbacks execute only here, outside native callbacks and registry locks.
/// Owned Rust text is dropped immediately after synchronous native preparation;
/// only the exact native notification retains its required native text afterward.
pub fn run_owned(
    identity: [u8; 16],
    title: String,
    subtitle: String,
    body: String,
    sound: bool,
    wait_for_response: bool,
    cancelled: impl Fn() -> bool,
    handover: impl FnOnce() -> bool,
    submitted: impl FnOnce(),
    response: impl FnOnce(Response),
) -> Result<Report, Failure> {
    let empty = Retirement {
        native: NativeRetirement::NotEntered,
        callback_body: CallbackBody::NotEntered,
    };
    if !crate::APPLICATION_READY.load(Ordering::Acquire) {
        return Err(Failure {
            operation: OperationFailure::Application,
            retirement: empty,
        });
    }
    if title.len() > 4096 || subtitle.len() > 4096 || body.len() > 65536 {
        return Err(Failure {
            operation: OperationFailure::Preparation,
            retirement: empty,
        });
    }
    if cancelled() {
        return Ok(Report {
            outcome: Outcome::Refused,
            retirement: empty,
        });
    }
    let Some((registration, receive)) = Registration::new(identity) else {
        return Err(Failure {
            operation: OperationFailure::Capacity,
            retirement: empty,
        });
    };
    let prepared = NativeOwner::prepare(identity, &title, &subtitle, &body, sound);
    // C has consumed all bounded borrowed bytes. Do not retain duplicate Rust
    // plaintext through native response wait, cancellation or late callbacks.
    drop((title, subtitle, body));
    let mut owner = match prepared {
        Ok(owner) => owner,
        Err((operation, native)) => {
            let retirement = registration.finish(native, Some(operation), None);
            return Err(Failure {
                operation,
                retirement,
            });
        }
    };
    if cancelled() || !handover() {
        let retirement = registration.finish(owner.retire(false), None, None);
        return match retirement.native {
            NativeRetirement::Released => Ok(Report {
                outcome: Outcome::Refused,
                retirement,
            }),
            _ => Err(Failure {
                operation: OperationFailure::NativeUncertain,
                retirement,
            }),
        };
    }
    if let Err(native) = owner.submit() {
        let retirement = registration.finish(native, Some(OperationFailure::NativeUncertain), None);
        return Err(Failure {
            operation: OperationFailure::NativeUncertain,
            retirement,
        });
    }
    submitted();
    let mut operation = None;
    let outcome = if !wait_for_response {
        Outcome::Submitted
    } else {
        loop {
            if cancelled() {
                break Outcome::Cancelled;
            }
            if registration.entry.failed.load(Ordering::Acquire) {
                operation = Some(OperationFailure::Callback);
                break Outcome::Cancelled;
            }
            match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(value) => break Outcome::Responded(value),
                Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => {
                    operation = Some(OperationFailure::Callback);
                    break Outcome::Cancelled;
                }
            }
        }
    };
    // Close before native removal/release. A callback already admitted is tracked
    // atomically and must keep the exact reservation even on successful return.
    let body_at_retirement = registration.entry.close();
    let withdraw = matches!(outcome, Outcome::Cancelled | Outcome::Responded(_));
    let native = owner.retire(withdraw);
    if registration.entry.failed.load(Ordering::Acquire) {
        operation = Some(OperationFailure::Callback);
    }
    let retirement = registration.finish(native, operation, Some(body_at_retirement));
    if matches!(native, NativeRetirement::Retained(_)) {
        return Err(Failure {
            operation: OperationFailure::NativeUncertain,
            retirement,
        });
    }
    if let Some(operation) = operation {
        return Err(Failure {
            operation,
            retirement,
        });
    }
    if let Outcome::Responded(value) = outcome {
        response(value);
    }
    Ok(Report {
        outcome,
        retirement,
    })
}

/// Strict synchronous fixed-value bridge. No object pointer or text enters Rust.
///
/// # Safety
///
/// A nonnull `identity` must point to sixteen readable, initialized bytes for
/// this complete synchronous call. The native delegate passes its fixed local
/// identity array. No reference is retained; null is an inert refusal.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn morpheus_owned_response(identity: *const u8, kind: u32) {
    if identity.is_null() {
        return;
    }
    // SAFETY: the sole C caller uses a 16-byte local array for this synchronous
    // call. No reference to it is stored or escapes this function.
    let identity: [u8; 16] = unsafe { std::slice::from_raw_parts(identity, 16) }
        .try_into()
        .unwrap();
    let entry = {
        let records = records().lock().unwrap_or_else(|e| e.into_inner());
        records
            .iter()
            .flatten()
            .find(|record| record.identity == identity)
            .map(|record| Arc::clone(&record.entry))
    };
    if let Some(entry) = entry {
        entry.deliver(|| match kind {
            0 => Some(Response::Click),
            1 => Some(Response::Dismissed),
            _ => None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_abi_receipts_never_classify_unknown_stages_as_release() {
        let good = NativeResult {
            slot: 9,
            status: 0,
            cookie: 23,
            stage: 14,
            reserved: 0,
        };
        assert!(good.is_live());
        let bad = NativeResult { stage: 26, ..good };
        assert!(!bad.is_live());
        assert_eq!(
            bad.retained(),
            NativeRetirement::Retained(RetainedRecord {
                slot: Some(9),
                cookie: 23,
                stage: None,
            })
        );
        assert!(
            Retirement {
                native: bad.retained(),
                callback_body: CallbackBody::NotEntered
            }
            .requires_identity_reservation()
        );
    }

    #[test]
    fn exact_callback_identity_and_unknown_native_kinds_never_authorize_a_click() {
        let id = [0x3a; 16];
        let (registration, receive) = Registration::new(id).unwrap();
        // SAFETY: this fixed local array is readable for the synchronous call.
        unsafe { morpheus_owned_response([0x3b; 16].as_ptr(), 0) };
        let unrelated = receive.try_recv();
        // SAFETY: id is an initialized sixteen-byte local array, still live.
        unsafe { morpheus_owned_response(id.as_ptr(), 2) };
        let failed = registration.entry.failed.load(Ordering::Acquire);
        let received = receive.try_recv();
        // This actual failure must retain its exact bounded identity entry.
        let retirement = registration.finish(
            NativeRetirement::NotEntered,
            Some(OperationFailure::Callback),
            None,
        );
        assert!(unrelated.is_err());
        assert!(received.is_err());
        assert!(failed);
        assert!(
            Failure {
                operation: OperationFailure::Callback,
                retirement
            }
            .requires_identity_reservation()
        );
    }

    #[test]
    fn admitted_callback_body_keeps_the_same_reservation_until_real_release_and_join() {
        let identity = [0x3c; 16];
        let (registration, receive) = Registration::new(identity).unwrap();
        let entry = Arc::clone(&registration.entry);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let worker_entry = Arc::clone(&entry);
        let worker = std::thread::spawn(move || {
            worker_entry.deliver(|| {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
                Some(Response::Click)
            })
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(1));
        let closed = entry.close();
        let fact = Retirement {
            native: NativeRetirement::Released,
            callback_body: closed,
        };
        let _ = release_tx.send(());
        drop(release_tx);
        let joined = worker.join();
        let join_ok = match joined {
            Ok(()) => true,
            Err(payload) => {
                std::mem::forget(payload);
                false
            }
        };
        let response = receive.try_recv();
        entry.deliver(|| Some(Response::Dismissed));
        let duplicate = receive.try_recv();
        // Finish AFTER the held body really joined: the original atomic close
        // still requires retention of this exact registration, not only a flag.
        let retirement = registration.finish(NativeRetirement::Released, None, Some(closed));
        let retained_same_entry = records()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
            .any(|record| record.identity == identity && Arc::ptr_eq(&record.entry, &entry));
        assert!(entered.is_ok());
        assert!(join_ok);
        assert_eq!(closed, CallbackBody::InFlightClosed);
        assert!(fact.requires_identity_reservation());
        assert!(retirement.requires_identity_reservation());
        assert!(retained_same_entry);
        assert_eq!(response.unwrap(), Response::Click);
        assert!(duplicate.is_err());
    }
}
