//! Pure Rust per-operation cancellation and checked native identity.
//!
//! Closing admission wakes an owned listener; it is not a native withdrawal or
//! worker/TLS join receipt. One owner runs the platform operation. This module
//! does not create a thread, detach a waiter, or submit from Drop.

#[cfg(all(unix, not(target_os = "macos"), feature = "zbus", feature = "async"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(all(unix, not(target_os = "macos"), feature = "zbus", feature = "async"))]
pub use linux::{Failure, OperationFailure, Outcome, ProtocolRetirement, Report, Retirement};

#[cfg(target_os = "macos")]
pub use mac_notification_sys::owned::{
    CallbackBody, Failure, NativeRetirement, OperationFailure, Outcome, Report, Retirement,
};

#[cfg(target_os = "windows")]
pub use winrt_notification::owned::{
    CallbackBody, Failure, HistoryWithdrawal, OperationFailure, Outcome, RegistrationRetirement,
    Report, Retirement,
};

/// Whether a RETURNED failure still owns an unresolved native capability.
/// This does not observe or retire an actual running worker/supervisor. Its
/// separate real joins remain mandatory even when this returns false. Finite
/// closed opaque callback-identity obligations are checked independently on
/// every successful Report and Failure; they are not whole-OS quiescence.
#[cfg(target_os = "macos")]
pub fn retains_native_owner(failure: &Failure) -> bool {
    matches!(failure.retirement.native, NativeRetirement::Retained(_))
}

/// Whether a RETURNED failure still owns an unresolved native capability.
/// Windows has released its local interfaces/apartment before returning;
/// Linux tears down its private same-thread connection/executor before return.
/// A blocked call has not returned. Actual worker/TLS/supervisor joins and
/// separate finite opaque callback-identity obligations remain mandatory.
#[cfg(any(
    target_os = "windows",
    all(unix, not(target_os = "macos"), feature = "zbus", feature = "async")
))]
pub fn retains_native_owner(_failure: &Failure) -> bool {
    false
}

/// Whether this exact operation reserves a response listener or only submits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Preserve click and dismissal handling until cancellation or response.
    Response,
    /// Existing explicit no-listener fallback, chosen only by the app owner.
    SubmitOnly,
}

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(all(unix, not(target_os = "macos")))]
use std::{sync::Mutex, task::Waker};

#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Default)]
struct Waiting {
    active: bool,
    waker: Option<Waker>,
}

#[derive(Default)]
struct CancellationState {
    closed: AtomicBool,
    #[cfg(all(unix, not(target_os = "macos")))]
    waiting: Mutex<Waiting>,
}

/// A terminal per-operation cancellation flag with no native pointer, text,
/// SDK/client, UI or account owner. Linux registered signal waits are woken;
/// other native waits observe the flag using their documented bounded steps.
/// Calling `cancel` does not certify native operation/OS delivery completion.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<CancellationState>);

impl Cancellation {
    /// Create an open cancellation gate for one exact native operation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Permanently close this gate and wake a registered asynchronous wait.
    /// Repeated close is inert; no drop/send/retry operation is performed.
    pub fn cancel(&self) {
        self.0.closed.store(true, Ordering::Release);
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let waker = self
                .0
                .waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .waker
                .take();
            // Runtime wake/drop can release arbitrary task references. Neither
            // is invoked under our mutex or app/native retirement locks.
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// Whether this exact operation's gate has been closed.
    pub fn is_cancelled(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            cancellation: self,
            registered: false,
        }
    }
}

/// One signal wait belongs to the owned Linux operation. Sequential
/// waits can reuse the flag; overlapping waiters fail closed rather than replace
/// another live task's wake. This future is not public application authority.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) struct Cancelled<'a> {
    cancellation: &'a Cancellation,
    registered: bool,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl std::future::Future for Cancelled<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = self.get_mut();
        let candidate = cx.waker().clone();
        let state = &this.cancellation.0;
        let mut waiting = state.waiting.lock().unwrap_or_else(|e| e.into_inner());
        if !this.registered {
            if waiting.active {
                drop(waiting);
                drop(candidate);
                this.cancellation.cancel();
                return std::task::Poll::Ready(());
            }
            waiting.active = true;
            this.registered = true;
        }
        if state.closed.load(Ordering::Acquire) {
            waiting.active = false;
            this.registered = false;
            let old = waiting.waker.take();
            drop(waiting);
            drop(candidate);
            drop(old);
            return std::task::Poll::Ready(());
        }
        let old = waiting.waker.replace(candidate);
        drop(waiting);
        drop(old);
        // cancel sets the terminal flag before taking/waking this registration,
        // so a close racing the check cannot lose its wake.
        std::task::Poll::Pending
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        if self.registered {
            let mut waiting = self
                .cancellation
                .0
                .waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            waiting.active = false;
            let old = waiting.waker.take();
            drop(waiting);
            drop(old);
        }
    }
}

/// A validated 128-bit UUID value supplied by the application. It is an opaque
/// correlation identity, not an account credential or a mathematical nonreuse
/// guarantee. The caller retains and checks its finite active/failed ledger.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Identity([u8; 16]);

impl Identity {
    /// Accept exactly thirty-two ASCII hex digits from the pinned identity
    /// producer. Case variants normalize to the same bytes and same ledger key.
    pub fn from_hex(value: &str) -> Option<Self> {
        fn nibble(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        }
        if value.len() != 32 {
            return None;
        }
        let mut bytes = [0; 16];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(bytes))
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn bytes(self) -> [u8; 16] {
        self.0
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn windows_parts(self) -> (String, String) {
        let mut hex = String::with_capacity(32);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            hex.push(char::from(HEX[usize::from(byte >> 4)]));
            hex.push(char::from(HEX[usize::from(byte & 15)]));
        }
        (hex[..16].to_owned(), hex[16..].to_owned())
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_is_exact_normalized_and_never_parses_a_path_or_token() {
        let a = Identity::from_hex("0123456789abcdefFEDCBA9876543210");
        let b = Identity::from_hex("0123456789ABCDEffedcba9876543210");
        assert_eq!(a, b);
        assert_eq!(
            a.unwrap().bytes(),
            [1, 35, 69, 103, 137, 171, 205, 239, 254, 220, 186, 152, 118, 84, 50, 16]
        );
        assert!(Identity::from_hex("0123456789abcdefFEDCBA98765432100").is_none());
        assert!(Identity::from_hex("0123456789abcdefFEDCBA987654321/").is_none());
        assert!(Identity::from_hex("0123456789abcdefFEDCBA987654321é").is_none());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn cancelled_wait_uses_actual_wake_and_drops_its_registration() {
        use std::{
            future::Future,
            pin::Pin,
            sync::atomic::AtomicUsize,
            task::{Context, Poll, Wake, Waker},
        };
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut context = Context::from_waker(&waker);
        let cancellation = Cancellation::new();
        let mut wait = cancellation.cancelled();
        let before = Pin::new(&mut wait).poll(&mut context);
        cancellation.cancel();
        let after = Pin::new(&mut wait).poll(&mut context);
        drop(wait);
        let wake_count = count.0.load(Ordering::SeqCst);
        let mut next = cancellation.cancelled();
        let next_result = Pin::new(&mut next).poll(&mut context);
        drop(next);
        assert_eq!(before, Poll::Pending);
        assert_eq!(after, Poll::Ready(()));
        assert_eq!(wake_count, 1);
        assert_eq!(next_result, Poll::Ready(()));
        assert!(!cancellation.0.waiting.lock().unwrap().active);
        assert!(cancellation.0.waiting.lock().unwrap().waker.is_none());
    }
}
