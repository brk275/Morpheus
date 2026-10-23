//! A checked, thread-affine STA lease. Native interfaces never leave this lease.
//! Worker waits service the owning apartment; the Slint UI uses its normal pump.
use super::owner::WasapiCloseError;
use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Com::{
        self, APTTYPE, APTTYPEQUALIFIER, APTTYPE_MAINSTA, APTTYPE_STA, COINIT_APARTMENTTHREADED,
    },
    UI::WindowsAndMessaging::{
        self as Messages, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_QUIT,
    },
};

pub(crate) struct Sta {
    _thread_affine: PhantomData<Rc<()>>,
}
impl Sta {
    pub(crate) fn new() -> Result<Self, WasapiCloseError> {
        let status = unsafe { Com::CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if status.0 != 0 && status.0 != 1 {
            return Err(WasapiCloseError::Apartment);
        }
        let lease = Self {
            _thread_affine: PhantomData,
        };
        if !is_sta() {
            // Balance only our successful CoInitializeEx, on this exact thread.
            drop(lease);
            return Err(WasapiCloseError::Apartment);
        }
        Ok(lease)
    }
    /// The UI bridge is installed only after the real window initialized its
    /// apartment. It takes its own lease, without manufacturing UI provenance
    /// by initializing an arbitrary worker as the permission origin.
    pub(crate) fn attach_ui() -> Result<Self, WasapiCloseError> {
        if !is_sta() {
            return Err(WasapiCloseError::Apartment);
        }
        Self::new()
    }
}
impl Drop for Sta {
    fn drop(&mut self) {
        unsafe {
            Com::CoUninitialize();
        }
    }
}

pub(crate) fn is_sta() -> bool {
    let mut kind = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    (unsafe { Com::CoGetApartmentType(&mut kind, &mut qualifier).is_ok() })
        && (kind == APTTYPE_STA || kind == APTTYPE_MAINSTA)
}

/// Waits on retained owner handles while dispatching this worker's STA messages.
/// The bound is an observation bound, never a native retirement receipt.
pub(crate) fn wait(handles: &[HANDLE], bound: Duration) -> Result<Option<usize>, WasapiCloseError> {
    if handles.len() > 8 {
        return Err(WasapiCloseError::NativeReference);
    }
    let deadline = Instant::now()
        .checked_add(bound)
        .ok_or(WasapiCloseError::TimedOut)?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(u32::MAX as u128 - 1) as u32;
        let status = unsafe {
            Messages::MsgWaitForMultipleObjectsEx(
                Some(handles),
                millis,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if status == WAIT_FAILED {
            return Err(WasapiCloseError::Apartment);
        }
        if status == WAIT_TIMEOUT {
            return Ok(None);
        }
        let index = status
            .0
            .checked_sub(WAIT_OBJECT_0.0)
            .ok_or(WasapiCloseError::Apartment)? as usize;
        if index < handles.len() {
            return Ok(Some(index));
        }
        if index != handles.len() {
            return Err(WasapiCloseError::Apartment);
        }
        // Bound each batch so a busy message queue cannot hide cancellation or
        // prevent the caller's deadline/retained command handle being checked.
        for _ in 0..64 {
            let mut message = MSG::default();
            if !unsafe { Messages::PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                return Err(WasapiCloseError::Apartment);
            }
            unsafe {
                let _ = Messages::TranslateMessage(&message);
                Messages::DispatchMessageW(&message);
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

pub(crate) fn retain_apartment() -> ! {
    loop {
        // Continue servicing existing proxies without accepting new product
        // work. An unsuccessful pump is not permission to release native state.
        let _ = wait(&[], Duration::from_millis(50));
    }
}
