//! Default-capture activation originates on the actual, still-pumping UI STA.
//! Only copied request generations and standard GIT cookies cross apartments.
//! Native references, operations and cookie revocation stay with their owners.
use super::{
    apartment::{self, Sta},
    owner::{Control, WasapiCloseError, WasapiCloseObservation},
    stream::OwnedEvent,
};
use crate::{host::com::ComString, Error, ErrorKind};
use std::{
    collections::VecDeque,
    mem, ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{IUnknown, Interface, GUID},
    Win32::{Media::Audio, System::Com},
};

#[link(name = "uuid")]
unsafe extern "system" {
    // Pinned SDK cguid.h declares this UUID-library symbol; use the SDK's
    // definition, not a guessed class identifier or a private COM proxy.
    static CLSID_StdGlobalInterfaceTable: GUID;
}
fn git() -> windows::core::Result<Com::IGlobalInterfaceTable> {
    unsafe {
        Com::CoCreateInstance(
            &CLSID_StdGlobalInterfaceTable,
            None,
            Com::CLSCTX_INPROC_SERVER,
        )
    }
}
fn error(kind: WasapiCloseError) -> Error {
    Error::with_message(ErrorKind::StreamInvalidated, kind.to_string())
}

#[windows::core::implement(Audio::IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    done: Arc<AtomicBool>,
    event: Arc<OwnedEvent>,
}
impl Audio::IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _operation: windows::core::Ref<'_, Audio::IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // No GetActivateResult, interface transfer, COM TLS, user callback or
        // final native reference release on the OS callback thread.
        self.done.store(true, Ordering::Release);
        self.event.signal()
    }
}
struct Activation {
    operation: Audio::IActivateAudioInterfaceAsyncOperation,
    _handler: Audio::IActivateAudioInterfaceCompletionHandler,
    done: Arc<AtomicBool>,
}
impl Activation {
    fn begin(capture: bool, event: Arc<OwnedEvent>) -> windows::core::Result<Self> {
        let done = Arc::new(AtomicBool::new(false));
        let handler: Audio::IActivateAudioInterfaceCompletionHandler = Completion {
            done: done.clone(),
            event,
        }
        .into();
        let path = unsafe {
            Com::StringFromIID(if capture {
                &Audio::DEVINTERFACE_AUDIO_CAPTURE
            } else {
                &Audio::DEVINTERFACE_AUDIO_RENDER
            })?
        };
        let _path = ComString(path);
        let operation = unsafe {
            Audio::ActivateAudioInterfaceAsync(path, &Audio::IAudioClient::IID, None, &handler)?
        };
        Ok(Self {
            operation,
            _handler: handler,
            done,
        })
    }
    fn completed(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    fn result(&self) -> windows::core::Result<Audio::IAudioClient> {
        let mut status = windows::core::HRESULT::default();
        let mut value: Option<IUnknown> = None;
        unsafe {
            self.operation.GetActivateResult(&mut status, &mut value)?;
        }
        status.ok()?;
        value
            .ok_or_else(|| {
                windows::core::Error::new(
                    Audio::AUDCLNT_E_DEVICE_INVALIDATED,
                    "Audio activation returned no interface",
                )
            })?
            .cast()
    }
}

/// Render activation has no capture-consent UI requirement. Both the operation
/// and GetActivateResult remain on this checked native STA until completion.
pub(crate) fn render(
    control: &Control,
    bound: Option<Duration>,
) -> Result<Audio::IAudioClient, Error> {
    if !control.active() {
        return Err(error(WasapiCloseError::AdmissionClosed));
    }
    let operation = Activation::begin(false, control.event()).map_err(Error::from)?;
    let deadline = bound.and_then(|bound| Instant::now().checked_add(bound));
    let mut timed_out = false;
    while !operation.completed() {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) && !timed_out {
            timed_out = true;
            control.failure(WasapiCloseError::TimedOut);
            control.request_close();
        }
        // A timeout cannot destroy an activation still referenced by Windows.
        if apartment::wait(&[control.event().handle()], Duration::from_millis(20)).is_err() {
            control.retain_failure(WasapiCloseError::Apartment);
        }
    }
    let result = operation.result().map_err(Error::from);
    drop(operation);
    if timed_out || !control.active() {
        drop(result);
        return Err(error(WasapiCloseError::AdmissionClosed));
    }
    result
}

#[derive(Clone, Copy)]
enum Transfer {
    Queued,
    Activating,
    Available(u32),
    Acquired,
    NativeReleased,
    OriginReleased,
    Failed,
    Retained(WasapiCloseError),
}
struct Request {
    generation: u64,
    control: Control,
    state: Mutex<Transfer>,
    failure: Mutex<Option<Error>>,
}
struct Bridge {
    next: u64,
    attached: bool,
    closed: bool,
    activating: bool,
    failure: Option<WasapiCloseError>,
    pending: VecDeque<Arc<Request>>,
}
static BRIDGE: OnceLock<Mutex<Bridge>> = OnceLock::new();
fn bridge() -> &'static Mutex<Bridge> {
    BRIDGE.get_or_init(|| {
        Mutex::new(Bridge {
            next: 0,
            attached: false,
            closed: false,
            activating: false,
            failure: None,
            pending: VecDeque::new(),
        })
    })
}

pub(crate) struct CaptureLease {
    request: Arc<Request>,
}
impl CaptureLease {
    pub(crate) fn acquire(control: &Control) -> Result<(Audio::IAudioClient, Self), Error> {
        let request = {
            let mut bridge = bridge()
                .lock()
                .map_err(|_| error(WasapiCloseError::RegistryPoisoned))?;
            if !bridge.attached || bridge.closed || bridge.failure.is_some() || !control.active() {
                return Err(error(WasapiCloseError::AdmissionClosed));
            }
            // Actual product opens one default microphone at a time. Requests
            // do not queue future prompts behind an entered activation.
            if bridge.activating || !bridge.pending.is_empty() {
                return Err(error(WasapiCloseError::Capacity));
            }
            bridge.next = bridge
                .next
                .checked_add(1)
                .ok_or_else(|| error(WasapiCloseError::Capacity))?;
            let request = Arc::new(Request {
                generation: bridge.next,
                control: control.clone(),
                state: Mutex::new(Transfer::Queued),
                failure: Mutex::new(None),
            });
            bridge.pending.push_back(request.clone());
            request
        };
        let lease = Self { request };
        let cookie = loop {
            let state = *lease
                .request
                .state
                .lock()
                .map_err(|_| error(WasapiCloseError::RegistryPoisoned))?;
            match state {
                Transfer::Available(cookie) => break cookie,
                Transfer::Failed | Transfer::OriginReleased => {
                    return Err(lease
                        .request
                        .failure
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone()
                        .unwrap_or_else(|| error(WasapiCloseError::Activation)))
                }
                Transfer::Retained(kind) => control.retain_failure(kind),
                _ => {}
            }
            if apartment::wait(&[control.event().handle()], Duration::from_millis(20)).is_err() {
                control.retain_failure(WasapiCloseError::Apartment);
            }
        };
        if !control.active() {
            lease.close();
            return Err(error(WasapiCloseError::AdmissionClosed));
        }
        let table = match git() {
            Ok(table) => table,
            Err(original) => {
                lease.close();
                return Err(Error::from(original));
            }
        };
        let mut pointer = ptr::null_mut();
        let status = unsafe {
            (Interface::vtable(&table).GetInterfaceFromGlobal)(
                Interface::as_raw(&table),
                cookie,
                &Audio::IAudioClient::IID,
                &mut pointer,
            )
        };
        if status.0 != 0 || pointer.is_null() {
            // Unknown return/output is not a safe revoke/retry permission.
            // Keep the table, opaque output and origin lease on this STA.
            *lease
                .request
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                Transfer::Retained(WasapiCloseError::Transfer);
            let _retained = (table, pointer, lease);
            control.retain_failure(WasapiCloseError::Transfer);
        }
        let client = unsafe { Audio::IAudioClient::from_raw(pointer) };
        drop(table);
        *lease
            .request
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Transfer::Acquired;
        Ok((client, lease))
    }
    /// Call only AFTER all worker-apartment client/service references drop.
    pub(crate) fn close(self) {
        let control = &self.request.control;
        *self
            .request
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Transfer::NativeReleased;
        let started = Instant::now();
        let mut timed_out = false;
        loop {
            let state = *self
                .request
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match state {
                Transfer::OriginReleased | Transfer::Failed => return,
                Transfer::Retained(kind) => control.retain_failure(kind),
                _ => {}
            }
            if !timed_out && started.elapsed() >= Duration::from_secs(3) {
                timed_out = true;
                control.failure(WasapiCloseError::TimedOut);
            }
            // This is observation only: never retry native revoke or leave the
            // apartment while the UI still owns the registration/origin.
            if apartment::wait(&[control.event().handle()], Duration::from_millis(20)).is_err() {
                control.retain_failure(WasapiCloseError::Apartment);
            }
        }
    }
}

struct Origin {
    request: Arc<Request>,
    operation: Option<Activation>,
    table: Option<Com::IGlobalInterfaceTable>,
    client: Option<Audio::IAudioClient>,
    cookie: Option<u32>,
}
/// Thread-affine UI owner. `poll` is called by the ordinary Slint timer while
/// its normal event loop remains alive, including asynchronous shutdown.
pub struct WasapiUiContext {
    apartment: Option<Sta>,
    origins: Vec<Origin>,
    failed: Option<WasapiCloseError>,
}
impl WasapiUiContext {
    pub fn attach() -> Result<Self, WasapiCloseError> {
        let apartment = Sta::attach_ui()?;
        let mut bridge = bridge()
            .lock()
            .map_err(|_| WasapiCloseError::RegistryPoisoned)?;
        if bridge.attached || bridge.closed {
            return Err(WasapiCloseError::AdmissionClosed);
        }
        bridge.attached = true;
        Ok(Self {
            apartment: Some(apartment),
            origins: Vec::new(),
            failed: None,
        })
    }
    pub fn begin_shutdown(&mut self) {
        super::owner::begin_shutdown();
        if let Ok(mut bridge) = bridge().lock() {
            bridge.closed = true;
        }
    }
    fn retain(&mut self, index: usize, kind: WasapiCloseError) {
        self.failed.get_or_insert(kind);
        if let Ok(mut bridge) = bridge().lock() {
            bridge.failure.get_or_insert(kind);
            bridge.closed = true;
        }
        *self.origins[index]
            .request
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Transfer::Retained(kind);
        let _ = self.origins[index].request.control.event().signal();
    }
    pub fn poll(&mut self) -> WasapiCloseObservation {
        if self.failed.is_none() {
            let pending = match bridge().lock() {
                Ok(mut bridge) => {
                    let request = bridge.pending.pop_front();
                    if request.is_some() {
                        bridge.activating = true;
                    }
                    request
                }
                Err(_) => {
                    self.failed = Some(WasapiCloseError::RegistryPoisoned);
                    None
                }
            };
            if let Some(request) = pending {
                let mut state = request
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let admitted = bridge()
                    .lock()
                    .is_ok_and(|bridge| !bridge.closed && bridge.failure.is_none());
                if !admitted || !request.control.active() {
                    *request
                        .failure
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(error(WasapiCloseError::AdmissionClosed));
                    *state = Transfer::Failed;
                } else if request.generation == 0
                    || self
                        .origins
                        .iter()
                        .any(|origin| origin.request.generation == request.generation)
                    || self.origins.len() >= 2
                    || self.origins.iter().any(|origin| origin.operation.is_some())
                {
                    *request
                        .failure
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(error(WasapiCloseError::Capacity));
                    *state = Transfer::Failed;
                } else {
                    *state = Transfer::Activating;
                    match Activation::begin(true, request.control.event()) {
                        Ok(operation) => {
                            drop(state);
                            self.origins.push(Origin {
                                request,
                                operation: Some(operation),
                                table: None,
                                client: None,
                                cookie: None,
                            });
                            return WasapiCloseObservation::Pending;
                        }
                        Err(original) => {
                            *request
                                .failure
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) =
                                Some(Error::from(original));
                            *state = Transfer::Failed;
                        }
                    }
                }
                drop(state);
                if let Ok(mut bridge) = bridge().lock() {
                    bridge.activating = false;
                }
                let _ = request.control.event().signal();
            }
        }
        let mut index = 0;
        while index < self.origins.len() {
            let state = *self.origins[index]
                .request
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Transfer::Retained(kind) = state {
                self.failed.get_or_insert(kind);
                index += 1;
                continue;
            }
            if self.origins[index]
                .operation
                .as_ref()
                .is_some_and(Activation::completed)
            {
                let result = self.origins[index].operation.as_ref().unwrap().result();
                // Completion/result consumption and final operation/handler
                // release remain on the original UI apartment.
                drop(self.origins[index].operation.take());
                if let Ok(mut bridge) = bridge().lock() {
                    bridge.activating = false;
                }
                match result {
                    Ok(client) => {
                        self.origins[index].client = Some(client);
                        match git() {
                            Ok(table) => self.origins[index].table = Some(table),
                            Err(_) => {
                                self.retain(index, WasapiCloseError::Transfer);
                                index += 1;
                                continue;
                            }
                        }
                        let origin = &mut self.origins[index];
                        let table = origin.table.as_ref().unwrap();
                        let client = origin.client.as_ref().unwrap();
                        let mut cookie = 0;
                        let status = unsafe {
                            (Interface::vtable(table).RegisterInterfaceInGlobal)(
                                Interface::as_raw(table),
                                Interface::as_raw(client),
                                &Audio::IAudioClient::IID,
                                &mut cookie,
                            )
                        };
                        // Even a failed/unknown registration may have created a
                        // native reference. Never guess a revoke permission.
                        origin.cookie = Some(cookie);
                        if status.0 != 0 || cookie == 0 {
                            self.retain(index, WasapiCloseError::Transfer);
                            index += 1;
                            continue;
                        }
                        *origin
                            .request
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) =
                            Transfer::Available(cookie);
                        let _ = origin.request.control.event().signal();
                    }
                    Err(original) => {
                        let origin = self.origins.remove(index);
                        *origin
                            .request
                            .failure
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) =
                            Some(Error::from(original));
                        *origin
                            .request
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Transfer::Failed;
                        let _ = origin.request.control.event().signal();
                        continue;
                    }
                }
            }
            let state = *self.origins[index]
                .request
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if matches!(state, Transfer::NativeReleased) {
                let origin = &mut self.origins[index];
                let (Some(table), Some(cookie)) = (origin.table.as_ref(), origin.cookie) else {
                    self.retain(index, WasapiCloseError::Transfer);
                    index += 1;
                    continue;
                };
                let status = unsafe {
                    (Interface::vtable(table).RevokeInterfaceFromGlobal)(
                        Interface::as_raw(table),
                        cookie,
                    )
                };
                if status.0 != 0 {
                    self.retain(index, WasapiCloseError::Transfer);
                    index += 1;
                    continue;
                }
                let mut origin = self.origins.remove(index);
                origin.cookie = None;
                drop(origin.client.take());
                drop(origin.table.take());
                *origin
                    .request
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Transfer::OriginReleased;
                let _ = origin.request.control.event().signal();
                continue;
            }
            index += 1;
        }
        if let Some(kind) = self.failed {
            return WasapiCloseObservation::Retained(kind);
        }
        match super::owner::observe_shutdown() {
            WasapiCloseObservation::Retained(kind) => WasapiCloseObservation::Retained(kind),
            observation if self.origins.is_empty() => observation,
            _ => WasapiCloseObservation::Pending,
        }
    }
}
impl Drop for WasapiUiContext {
    fn drop(&mut self) {
        // A bridge that disappears can never admit another prompt. Queued
        // requests have no native origin yet and may be cancelled safely.
        let queued = if let Ok(mut bridge) = bridge().lock() {
            bridge.closed = true;
            bridge.pending.drain(..).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for request in queued {
            *request
                .failure
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                Some(error(WasapiCloseError::AdmissionClosed));
            *request
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Transfer::Failed;
            let _ = request.control.event().signal();
        }
        if !self.origins.is_empty() {
            // Unexpected caller teardown is not a native retirement receipt.
            // Keep every live origin allocation/reference and our apartment
            // lease rather than release callbacks/proxies speculatively.
            if let Ok(mut bridge) = bridge().lock() {
                bridge.closed = true;
                bridge.failure = Some(WasapiCloseError::Transfer);
            }
            mem::forget(mem::take(&mut self.origins));
            if let Some(apartment) = self.apartment.take() {
                mem::forget(apartment);
            }
        }
    }
}
