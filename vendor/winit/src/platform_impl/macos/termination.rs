//! Opt-in application termination on the existing Winit delegate.
//!
//! The timer progresses only the registered owner. It does not replay Winit
//! events, run Slint's default-mode queue, or treat a stopped loop as cleanup.

use std::any::Any;
use std::cell::Cell;
use std::fmt;
use std::mem;
use std::os::raw::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use core_foundation::base::CFRelease;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopAddTimer, CFRunLoopContainsTimer, CFRunLoopGetMain,
    CFRunLoopRef, CFRunLoopRemoveTimer, CFRunLoopTimerContext, CFRunLoopTimerCreate,
    CFRunLoopTimerInvalidate, CFRunLoopTimerIsValid, CFRunLoopTimerRef,
};
use core_foundation::string::CFStringRef;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply, NSModalPanelRunLoopMode};
use objc2_foundation::MainThreadMarker;

use super::app_state::ApplicationDelegate;

/// The synchronous admission decision for the actual application owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationTerminationRequest {
    /// Refuse without scheduling a substitute process or event-loop exit.
    Cancel,
    /// Admission is closed and real cleanup ownership has been retained.
    Later,
    /// Ordinary exit was already successfully posted after an actual Clean
    /// receipt. Re-observe that receipt and backend before native termination.
    Now,
}

/// Observed cleanup progress, independently of an ordinary event-loop return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationTerminationProgress {
    /// The same admitted owners have not all completed.
    Pending,
    /// Cancel this OS request; closed admission and retained owners stay owned.
    Cancel,
    /// All required product owners and runtime finalization have completed.
    Allow,
}

/// Main-thread application ownership, retained strongly by the application.
/// Winit stores only a Weak reference. Neither method may unwind across FFI.
/// A poll must only observe the one admitted operation, never start a new one.
pub trait ApplicationTerminationHandler {
    /// Synchronously check authority, revoke admission, and retain actual work.
    fn requested(&self) -> ApplicationTerminationRequest;
    /// Observe actual retained completion without replaying UI callbacks.
    fn poll(&self) -> ApplicationTerminationProgress;
}

/// Closed, nonsecret installation failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationTerminationInstallError {
    /// Installation did not run on the process main thread.
    WrongThread,
    /// The actual existing Winit delegate could not be authenticated.
    MissingDelegate,
    /// The same delegate already owns a registration.
    AlreadyInstalled,
    /// The application did not retain the handler.
    HandlerGone,
    /// Both required timer modes could not be established.
    Timer,
}

impl fmt::Display for ApplicationTerminationInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongThread => "Application termination requires the main thread.",
            Self::MissingDelegate => "The Winit application delegate is unavailable.",
            Self::AlreadyInstalled => "Application termination is already registered.",
            Self::HandlerGone => "The application termination owner is unavailable.",
            Self::Timer => "Application termination observation could not be installed.",
        })
    }
}

impl std::error::Error for ApplicationTerminationInstallError {}

/// Install once after explicitly selecting Winit, before opening application
/// data. The application must retain the handler through all failed/pending
/// observations. There is no uninstall-to-immediate-termination operation.
pub fn install_application_termination_handler(
    handler: Weak<dyn ApplicationTerminationHandler>,
) -> Result<(), ApplicationTerminationInstallError> {
    let mtm = MainThreadMarker::new().ok_or(ApplicationTerminationInstallError::WrongThread)?;
    if handler.strong_count() == 0 {
        return Err(ApplicationTerminationInstallError::HandlerGone);
    }
    let delegate = ApplicationDelegate::termination_delegate(mtm)
        .ok_or(ApplicationTerminationInstallError::MissingDelegate)?;
    delegate.install_termination(handler)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Admitting,
    Pending(u64),
    Replying,
}

pub(super) struct Context {
    handler: Weak<dyn ApplicationTerminationHandler>,
    phase: Cell<Phase>,
    epoch: Cell<u64>,
    requesting: Cell<bool>,
    polling: Cell<bool>,
    failed: Cell<bool>,
    closed: Cell<bool>,
}

impl Context {
    pub(super) fn requested(&self) -> NSApplicationTerminateReply {
        if self.closed.get() || self.failed.get() || self.requesting.replace(true) {
            return NSApplicationTerminateReply::NSTerminateCancel;
        }
        struct Requesting<'a>(&'a Cell<bool>);
        impl Drop for Requesting<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        // This is the last guard dropped at the return boundary. Any app-code
        // call or handler release happens while timer replies are inhibited.
        let _requesting = Requesting(&self.requesting);
        match self.phase.get() {
            Phase::Pending(_) => return NSApplicationTerminateReply::NSTerminateLater,
            Phase::Admitting | Phase::Replying => {
                return NSApplicationTerminateReply::NSTerminateCancel;
            },
            Phase::Idle => {},
        }
        let Some(epoch) = self.epoch.get().checked_add(1) else {
            self.failed.set(true);
            return NSApplicationTerminateReply::NSTerminateCancel;
        };
        self.phase.set(Phase::Admitting);
        let request = self.call(|handler| handler.requested());
        if request == Some(ApplicationTerminationRequest::Now) && !self.failed.get() {
            // This cannot be authorized by a posted bit or phase alone. The
            // app must return its actual checked cleanup receipt again, and
            // the delegate rechecks backend/PanicInfo after this returns.
            let observed = self.call(|handler| handler.poll());
            self.phase.set(Phase::Idle);
            if observed == Some(ApplicationTerminationProgress::Allow) && !self.failed.get() {
                NSApplicationTerminateReply::NSTerminateNow
            } else {
                NSApplicationTerminateReply::NSTerminateCancel
            }
        } else if request == Some(ApplicationTerminationRequest::Later) && !self.failed.get() {
            self.epoch.set(epoch);
            self.phase.set(Phase::Pending(epoch));
            NSApplicationTerminateReply::NSTerminateLater
        } else {
            self.phase.set(Phase::Idle);
            NSApplicationTerminateReply::NSTerminateCancel
        }
    }

    fn call<T: Copy>(
        &self,
        call: impl FnOnce(&dyn ApplicationTerminationHandler) -> T,
    ) -> Option<T> {
        let Some(handler) = self.handler.upgrade() else {
            self.failed.set(true);
            return None;
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| call(&*handler)));
        // Releasing a user's last Rc must also stay inside the Rust boundary.
        let released = catch_unwind(AssertUnwindSafe(|| drop(handler)));
        if let Err(payload) = released {
            self.failed.set(true);
            retain_panic(payload);
        }
        match outcome {
            Ok(result) if !self.failed.get() => Some(result),
            Ok(_) => None,
            Err(payload) => {
                self.failed.set(true);
                retain_panic(payload);
                None
            },
        }
    }

    fn poll(&self, delegate: &ApplicationDelegate, app: &NSApplication) {
        if self.closed.get()
            || self.requesting.get()
            || delegate.termination_in_use()
            || self.polling.replace(true)
        {
            return;
        }
        struct Polling<'a>(&'a Cell<bool>);
        impl Drop for Polling<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _polling = Polling(&self.polling);
        let progress = if self.failed.get() || !delegate.termination_ready() {
            ApplicationTerminationProgress::Cancel
        } else {
            self.call(|handler| handler.poll()).unwrap_or(ApplicationTerminationProgress::Cancel)
        };
        let Phase::Pending(epoch) = self.phase.get() else {
            // The same timer also allows an admitted ordinary close to poll
            // its app owner. Only a real outstanding Later can receive a reply.
            return;
        };
        if progress == ApplicationTerminationProgress::Pending {
            return;
        }
        let allow = progress == ApplicationTerminationProgress::Allow
            && !self.failed.get()
            && delegate.termination_ready()
            && delegate.termination_is_current();
        self.reply(app, epoch, allow);
    }

    pub(super) fn fail(&self) {
        self.failed.set(true);
    }

    fn reply(&self, app: &NSApplication, epoch: u64, allow: bool) {
        if self.requesting.get() || self.phase.get() != Phase::Pending(epoch) {
            return;
        }
        // Consume authority BEFORE calling AppKit: a nested request/reply can
        // neither duplicate this reply nor invoke application cleanup again.
        self.phase.set(Phase::Replying);
        // SAFETY: this is the actual main-thread application, exactly one
        // outstanding shouldTerminate Later, and the reply argument is BOOL.
        unsafe { app.replyToApplicationShouldTerminate(allow) };
        self.phase.set(Phase::Idle);
    }
}

/// CF may retain/release its context independently of timer callback delivery.
/// Its native info therefore contains only an atomic pointer and an Arc count,
/// never an Rc or an application destructor. Only the main-thread callback may
/// dereference context; the origin owner clears it before retiring its Rc.
struct TimerInfo {
    context: AtomicPtr<Context>,
}

/// The delegate owns the timer and the actual main-thread Context. Native info
/// owns only TimerInfo. Neither object owns a strong application/UI handle.
pub(super) struct Termination {
    main: CFRunLoopRef,
    timer: CFRunLoopTimerRef,
    info: Arc<TimerInfo>,
    context: Rc<Context>,
}

impl fmt::Debug for Termination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Termination").finish_non_exhaustive()
    }
}

impl Termination {
    pub(super) fn new(
        handler: Weak<dyn ApplicationTerminationHandler>,
    ) -> Result<Self, ApplicationTerminationInstallError> {
        let context = Rc::new(Context {
            handler,
            phase: Cell::new(Phase::Idle),
            epoch: Cell::new(0),
            requesting: Cell::new(false),
            polling: Cell::new(false),
            failed: Cell::new(false),
            closed: Cell::new(false),
        });
        let info = Arc::new(TimerInfo { context: AtomicPtr::new(Rc::as_ptr(&context).cast_mut()) });
        let mut native_context = CFRunLoopTimerContext {
            version: 0,
            info: Arc::as_ptr(&info).cast_mut().cast(),
            retain: Some(retain_context),
            release: Some(release_context),
            copyDescription: None,
        };
        // SAFETY: caller is the actual main-thread delegate; the ABI and all
        // callbacks use the locked CoreFoundation declarations. Create retains
        // TimerInfo through atomic callbacks before the local Arc can drop.
        let (main, timer) = unsafe {
            let main = CFRunLoopGetMain();
            let timer = CFRunLoopTimerCreate(
                ptr::null(),
                CFAbsoluteTimeGetCurrent() + 0.02,
                0.02,
                0,
                0,
                timer_callback,
                &mut native_context,
            );
            (main, timer)
        };
        if main.is_null() || timer.is_null() {
            if !timer.is_null() {
                unsafe { CFRelease(timer.cast()) };
            }
            return Err(ApplicationTerminationInstallError::Timer);
        }
        let owner = Self { main, timer, info, context };
        // SAFETY: NSString/CFString are toll-free bridged; this is AppKit's
        // exported mode object, not a guessed string or NSRunLoop cast.
        let modal = unsafe { NSModalPanelRunLoopMode as *const _ as CFStringRef };
        let installed = unsafe {
            CFRunLoopAddTimer(main, timer, kCFRunLoopDefaultMode);
            CFRunLoopAddTimer(main, timer, modal);
            CFRunLoopTimerIsValid(timer) != 0
                && CFRunLoopContainsTimer(main, timer, kCFRunLoopDefaultMode) != 0
                && CFRunLoopContainsTimer(main, timer, modal) != 0
        };
        if !installed {
            return Err(ApplicationTerminationInstallError::Timer);
        }
        Ok(owner)
    }

    pub(super) fn context(&self) -> Rc<Context> {
        self.context.clone()
    }

    pub(super) fn is_context(&self, context: &Rc<Context>) -> bool {
        Rc::ptr_eq(&self.context, context)
    }
}

impl Drop for Termination {
    fn drop(&mut self) {
        // No native context callback dereferences this pointer. On main, clear
        // it before any callout/invalidation or before our actual Rc can drop.
        self.info.context.store(ptr::null_mut(), Ordering::Release);
        self.context.closed.set(true);
        if let Some(mtm) = MainThreadMarker::new() {
            let app = NSApplication::sharedApplication(mtm);
            if let Phase::Pending(epoch) = self.context.phase.get() {
                self.context.reply(&app, epoch, false);
            }
            unsafe {
                let modal = NSModalPanelRunLoopMode as *const _ as CFStringRef;
                CFRunLoopTimerInvalidate(self.timer);
                CFRunLoopRemoveTimer(self.main, self.timer, kCFRunLoopDefaultMode);
                CFRunLoopRemoveTimer(self.main, self.timer, modal);
                if CFRunLoopTimerIsValid(self.timer) == 0
                    && CFRunLoopContainsTimer(self.main, self.timer, kCFRunLoopDefaultMode) == 0
                    && CFRunLoopContainsTimer(self.main, self.timer, modal) == 0
                {
                    CFRelease(self.timer.cast());
                }
                // Unproved retirement retains the actual native timer, whose
                // TimerInfo retain stays alive with a null app pointer. Never
                // free by guess or reach the retired main-thread Context.
            }
        }
    }
}

extern "C" fn retain_context(info: *const c_void) -> *const c_void {
    if !info.is_null() {
        // SAFETY: CF copies of the live Arc TimerInfo balance this atomic
        // retain. This never accesses the main-thread Context or app payload.
        unsafe { Arc::increment_strong_count(info.cast::<TimerInfo>()) };
    }
    info
}

extern "C" fn release_context(info: *const c_void) {
    if !info.is_null() {
        // SAFETY: balances a native atomic retain on any calling thread. The
        // last TimerInfo drop destroys only AtomicPtr, never the pointed value.
        unsafe { Arc::decrement_strong_count(info.cast::<TimerInfo>()) };
    }
}

extern "C" fn timer_callback(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if info.is_null() {
        return;
    }
    // SAFETY: CF owns TimerInfo for this active callout. Hold a local atomic
    // retain so reentrant invalidation cannot retire the callback's info.
    let info = unsafe {
        Arc::increment_strong_count(info.cast::<TimerInfo>());
        Arc::from_raw(info.cast::<TimerInfo>())
    };
    let pointer = info.context.load(Ordering::Acquire);
    if pointer.is_null() {
        return;
    }
    // SAFETY: this callback and the sole Context owner run on main. There is
    // no callout between loading the pointer and retaining it. The origin owner
    // clears that pointer before any callout/drop, and this local Rc keeps the
    // Context alive if app code reentrantly invalidates/removes the timer.
    let context = unsafe {
        Rc::increment_strong_count(pointer);
        Rc::from_raw(pointer)
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        autoreleasepool(|_| {
            let app = NSApplication::sharedApplication(mtm);
            let Some(delegate) = ApplicationDelegate::termination_delegate(mtm) else {
                context.failed.set(true);
                if let Phase::Pending(epoch) = context.phase.get() {
                    context.reply(&app, epoch, false);
                }
                return;
            };
            if !delegate.owns_termination(&context) {
                context.failed.set(true);
                if let Phase::Pending(epoch) = context.phase.get() {
                    context.reply(&app, epoch, false);
                }
                return;
            }
            context.poll(&delegate, &app);
        });
    }));
    if let Err(payload) = result {
        context.failed.set(true);
        retain_panic(payload);
        // Even failure-path Objective-C dispatch remains inside a Rust panic
        // boundary. No second unwind may escape this extern C callback.
        let refused = catch_unwind(AssertUnwindSafe(|| {
            if let Phase::Pending(epoch) = context.phase.get() {
                let app = NSApplication::sharedApplication(mtm);
                context.reply(&app, epoch, false);
            }
        }));
        if let Err(payload) = refused {
            retain_panic(payload);
        }
    }
}

fn retain_panic(payload: Box<dyn Any + Send>) {
    // Fixed Failed state is recorded first. Do not print or drop an arbitrary
    // payload destructor across an Objective-C or C callback boundary.
    mem::forget(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Handler {
        requests: Cell<usize>,
        polls: Cell<usize>,
        request: Cell<ApplicationTerminationRequest>,
        progress: Cell<ApplicationTerminationProgress>,
    }

    impl ApplicationTerminationHandler for Handler {
        fn requested(&self) -> ApplicationTerminationRequest {
            self.requests.set(self.requests.get() + 1);
            self.request.get()
        }

        fn poll(&self) -> ApplicationTerminationProgress {
            self.polls.set(self.polls.get() + 1);
            self.progress.get()
        }
    }

    fn fixture() -> (Rc<Handler>, Context) {
        let handler = Rc::new(Handler {
            requests: Cell::new(0),
            polls: Cell::new(0),
            request: Cell::new(ApplicationTerminationRequest::Later),
            progress: Cell::new(ApplicationTerminationProgress::Pending),
        });
        let native: Rc<dyn ApplicationTerminationHandler> = handler.clone();
        let context = Context {
            handler: Rc::downgrade(&native),
            phase: Cell::new(Phase::Idle),
            epoch: Cell::new(0),
            requesting: Cell::new(false),
            polling: Cell::new(false),
            failed: Cell::new(false),
            closed: Cell::new(false),
        };
        (handler, context)
    }

    // These exercise the actual protocol decisions only. They do not call
    // AppKit or prove timer-mode delivery, native reply, or ordinary UI quit.
    #[test]
    fn repeated_later_does_not_readmit_and_in_progress_refuses_before_callback() {
        let (handler, context) = fixture();
        context.requesting.set(true);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateCancel);
        assert_eq!(handler.requests.get(), 0);
        context.requesting.set(false);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateLater);
        assert_eq!(context.phase.get(), Phase::Pending(1));
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateLater);
        assert_eq!(handler.requests.get(), 1);
        assert!(!context.requesting.get());
    }

    #[test]
    fn native_now_requires_a_new_actual_allow_observation() {
        let (handler, context) = fixture();
        handler.request.set(ApplicationTerminationRequest::Now);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateCancel);
        assert_eq!(handler.polls.get(), 1);
        handler.progress.set(ApplicationTerminationProgress::Allow);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateNow);
        assert_eq!(handler.polls.get(), 2);
        assert_eq!(context.phase.get(), Phase::Idle);
        context.failed.set(true);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateCancel);
        assert_eq!(handler.polls.get(), 2);
    }

    #[test]
    fn exhausted_or_gone_owner_never_grants_termination() {
        let (handler, context) = fixture();
        context.epoch.set(u64::MAX);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateCancel);
        assert_eq!(handler.requests.get(), 0);
        let (handler, context) = fixture();
        drop(handler);
        assert_eq!(context.requested(), NSApplicationTerminateReply::NSTerminateCancel);
        assert!(context.failed.get());
        assert_eq!(context.phase.get(), Phase::Idle);
    }
    #[test]
    fn native_context_retain_release_never_owns_the_main_thread_payload() {
        let (_handler, context) = fixture();
        let context = Rc::new(context);
        let info = Arc::new(TimerInfo { context: AtomicPtr::new(Rc::as_ptr(&context).cast_mut()) });
        let raw = retain_context(Arc::as_ptr(&info).cast());
        assert_eq!(Arc::strong_count(&info), 2);
        assert_eq!(Rc::strong_count(&context), 1);
        // Actual production retirement clears the pointer on main BEFORE its
        // Rc is destroyed. The CF release entry must then be thread-independent.
        info.context.store(ptr::null_mut(), Ordering::Release);
        drop(context);
        let raw = raw as usize;
        std::thread::spawn(move || release_context(raw as *const c_void)).join().unwrap();
        assert_eq!(Arc::strong_count(&info), 1);
        assert!(info.context.load(Ordering::Acquire).is_null());
    }
}
