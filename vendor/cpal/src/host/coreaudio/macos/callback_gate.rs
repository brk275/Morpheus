//! Stable non-reused userdata tokens. CoreAudio never receives a per-stream pointer.
//! See OWNED_COREAUDIO.md for the pinned ABI and off-RT prewarming proof.
use crate::{Error, ErrorKind, SampleFormat};
use objc2_audio_toolbox::{AudioUnit, AudioUnitRender, AudioUnitRenderActionFlags};
use objc2_core_audio::{AudioObjectID, AudioObjectPropertyAddress};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use std::{
    any::Any,
    cell::UnsafeCell,
    ffi::c_void,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};

const COUNT: usize = 64;
const TOKEN_SHIFT: usize = 12;
const MAX_BYTES: usize = 1024 * 1024;
const BAD_CALLBACK: i32 = -10863;
pub(super) const PANIC: u32 = 1 << 31;

pub(super) type UserCallback = Box<dyn FnMut(RawArgs) -> Result<(), ()> + Send>;
pub(super) struct RawArgs {
    pub data: *mut AudioBufferList,
    pub time_stamp: AudioTimeStamp,
}

pub(super) enum Body {
    Render(UserCallback),
    Input {
        callback: UserCallback,
        unit: AudioUnit,
        samples: Vec<u64>,
        list: AudioBufferList,
        bytes_per_frame: usize,
    },
    Property {
        events: Arc<AtomicU32>,
        mask: u32,
    },
}
// The raw unit/list data is touched only under its stable slot gate by a native callback.
// Its control owner drains this gate before any unit stop/uninitialization/disposal.
unsafe impl Send for Body {}

struct State {
    token: usize,
    body: Option<Body>,
    panic: Option<Box<dyn Any + Send>>,
}
/// Fixed-address, explicitly initialized pthread NORMAL mutex. Unlike std Mutex,
/// the RT guard has no poison::Flag/thread::panicking/TLV path.
struct Gate {
    native: UnsafeCell<MaybeUninit<libc::pthread_mutex_t>>,
    state: UnsafeCell<State>,
    initialized: AtomicBool,
    fault: AtomicBool,
}
unsafe impl Sync for Gate {}
impl Gate {
    const fn new() -> Self {
        Self {
            native: UnsafeCell::new(MaybeUninit::uninit()),
            state: UnsafeCell::new(State {
                token: 0,
                body: None,
                panic: None,
            }),
            initialized: AtomicBool::new(false),
            fault: AtomicBool::new(false),
        }
    }
    fn raw(&self) -> *mut libc::pthread_mutex_t {
        self.native.get().cast()
    }
    /// Once only, OFF RT and while no native refcon has been registered.
    fn initialize(&'static self) -> Result<(), ()> {
        let mut attr = MaybeUninit::<libc::pthread_mutexattr_t>::uninit();
        if unsafe { libc::pthread_mutexattr_init(attr.as_mut_ptr()) } != 0 {
            self.fault.store(true, Ordering::Release);
            return Err(());
        }
        let configured = unsafe {
            libc::pthread_mutexattr_settype(attr.as_mut_ptr(), libc::PTHREAD_MUTEX_NORMAL)
        };
        let initialized = if configured == 0 {
            unsafe { libc::pthread_mutex_init(self.raw(), attr.as_ptr()) }
        } else {
            configured
        };
        let destroyed = unsafe { libc::pthread_mutexattr_destroy(attr.as_mut_ptr()) };
        if initialized != 0 || destroyed != 0 {
            self.fault.store(true, Ordering::Release);
            return Err(());
        }
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }
    fn lock(&self) -> Result<Guard<'_>, ()> {
        self.acquire(false)
    }
    fn try_lock(&self) -> Result<Guard<'_>, ()> {
        self.acquire(true)
    }
    fn acquire(&self, nonblocking: bool) -> Result<Guard<'_>, ()> {
        if !self.initialized.load(Ordering::Acquire) || self.fault.load(Ordering::Acquire) {
            return Err(());
        }
        let status = unsafe {
            if nonblocking {
                libc::pthread_mutex_trylock(self.raw())
            } else {
                libc::pthread_mutex_lock(self.raw())
            }
        };
        if status != 0 {
            if status != libc::EBUSY {
                self.fault.store(true, Ordering::Release);
            }
            return Err(());
        }
        // A concurrent prior unlock fault cannot grant body access/reuse.
        if self.fault.load(Ordering::Acquire) {
            let result = unsafe { libc::pthread_mutex_unlock(self.raw()) };
            if result != 0 {
                self.fault.store(true, Ordering::Release);
            }
            return Err(());
        }
        Ok(Guard {
            gate: self,
            _not_send: std::marker::PhantomData,
        })
    }
}
struct Guard<'a> {
    gate: &'a Gate,
    _not_send: std::marker::PhantomData<*mut ()>,
}
impl Deref for Guard<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        unsafe { &*self.gate.state.get() }
    }
}
impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut State {
        unsafe { &mut *self.gate.state.get() }
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if unsafe { libc::pthread_mutex_unlock(self.gate.raw()) } != 0 {
            // Never free/take/reuse body storage or acknowledge close after this fault.
            self.gate.fault.store(true, Ordering::Release);
        }
    }
}
struct Slot {
    admitted: AtomicUsize,
    active_thread: AtomicUsize,
    failed: AtomicBool,
    gate: Gate,
}
impl Slot {
    const fn new() -> Self {
        Self {
            admitted: AtomicUsize::new(0),
            active_thread: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            gate: Gate::new(),
        }
    }
}
static SLOTS: [Slot; COUNT] = [const { Slot::new() }; COUNT];
static NEXT: AtomicUsize = AtomicUsize::new(1);
static WARMED: OnceLock<Result<(), ()>> = OnceLock::new();

fn error() -> Error {
    Error::with_message(
        ErrorKind::StreamInvalidated,
        "owned CoreAudio callback unavailable",
    )
}
fn tag(format: SampleFormat) -> Result<usize, Error> {
    Ok(match format {
        SampleFormat::I8 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::I32 => 3,
        SampleFormat::I64 => 4,
        SampleFormat::F32 => 5,
        SampleFormat::F64 => 6,
        SampleFormat::U8 => 7,
        SampleFormat::U16 => 8,
        SampleFormat::U24 => 9,
        SampleFormat::U32 => 10,
        SampleFormat::U64 => 11,
        _ => return Err(error()),
    })
}

/// Called on the native control owner BEFORE any callback can be registered.
/// Native mutex initialization and attribute cleanup occur only off RT.
pub(super) fn prewarm() -> Result<(), Error> {
    match WARMED.get_or_init(|| {
        for slot in &SLOTS {
            slot.gate.initialize()?;
        }
        Ok(())
    }) {
        Ok(()) => Ok(()),
        Err(()) => Err(error()),
    }
}

pub(super) struct Token(pub usize);
pub(super) struct Retired {
    // Retained on the native owner until every checked native close succeeds.
    _body: Option<Body>,
    _panic: Option<Box<dyn Any + Send>>,
}
impl Token {
    pub fn install(body: Body, format: SampleFormat) -> Result<Self, Error> {
        prewarm()?;
        let kind = match &body {
            Body::Render(_) => 1,
            Body::Input { .. } => 2,
            Body::Property { .. } => 3,
        };
        let format = tag(format)?;
        for (index, slot) in SLOTS.iter().enumerate() {
            let mut guard = slot.gate.lock().map_err(|_| error())?;
            if guard.token != 0 {
                continue;
            }
            let generation = NEXT
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    n.checked_add(1)
                        .filter(|v| *v <= (usize::MAX >> TOKEN_SHIFT))
                })
                .map_err(|_| error())?;
            let token = (generation << TOKEN_SHIFT) | (format << 8) | (kind << 6) | index;
            guard.body = Some(body);
            guard.panic = None;
            guard.token = token;
            slot.failed.store(false, Ordering::Relaxed);
            slot.admitted.store(token, Ordering::Release);
            return Ok(Self(token));
        }
        Err(Error::with_message(
            ErrorKind::ResourceExhausted,
            "owned CoreAudio callback slots exhausted",
        ))
    }
    pub fn failed(&self) -> bool {
        let slot = &SLOTS[self.0 & 63];
        slot.failed.load(Ordering::Acquire) || slot.gate.fault.load(Ordering::Acquire)
    }
    pub fn refcon(&self) -> *mut c_void {
        std::ptr::without_provenance_mut(self.0)
    }
    pub fn revoke(&self) {
        revoke_exact(self.0);
    }
    /// Off RT, after reentrancy refusal. Waits only for an already admitted callback.
    pub fn retire(&self) -> Result<Retired, Error> {
        self.revoke();
        let slot = &SLOTS[self.0 & 63];
        let mut guard = slot.gate.lock().map_err(|_| error())?;
        if guard.token != self.0 {
            return Err(error());
        }
        // Keep token reserved until successful native close releases this registration.
        let retired = Retired {
            _body: guard.body.take(),
            _panic: guard.panic.take(),
        };
        drop(guard);
        if slot.gate.fault.load(Ordering::Acquire) {
            // Ownership is still retained on the native owner, never destroyed here.
            return Ok(retired);
        }
        Ok(retired)
    }
    pub fn release(&self) -> Result<(), Error> {
        let slot = &SLOTS[self.0 & 63];
        let mut guard = slot.gate.lock().map_err(|_| error())?;
        if guard.token != self.0 || guard.body.is_some() || guard.panic.is_some() {
            return Err(error());
        }
        guard.token = 0;
        drop(guard);
        if slot.gate.fault.load(Ordering::Acquire) {
            return Err(error());
        }
        Ok(())
    }
}

pub(super) fn is_callback_thread() -> bool {
    let current = unsafe { libc::pthread_self() };
    SLOTS.iter().any(|slot| {
        let active = slot.active_thread.load(Ordering::Acquire);
        active != 0 && current as usize == active
    })
}

fn enter<R: Copy>(token: usize, fallback: R, f: impl FnOnce(&mut Body) -> R) -> R {
    let slot = &SLOTS[token & 63];
    if token >> TOKEN_SHIFT == 0 || slot.admitted.load(Ordering::Acquire) != token {
        return fallback;
    }
    // Already prewarmed; no allocation or blocking wait. A concurrent close gets inert output.
    let Ok(mut guard) = slot.gate.try_lock() else {
        return fallback;
    };
    if guard.token != token || slot.admitted.load(Ordering::Acquire) != token {
        return fallback;
    }
    let Some(body) = guard.body.as_mut() else {
        return fallback;
    };
    slot.active_thread
        .store(unsafe { libc::pthread_self() } as usize, Ordering::Release);
    // The guard is outside the unwind boundary. A user panic never poisons this gate,
    // crosses the FFI ABI, or frees its captured closure/payload on the callback thread.
    let result = catch_unwind(AssertUnwindSafe(|| f(body)));
    slot.active_thread.store(0, Ordering::Release);
    let value = match result {
        Ok(value) => value,
        Err(payload) => {
            slot.admitted.store(0, Ordering::Release);
            slot.failed.store(true, Ordering::Release);
            // First panic closes admission; this Option cannot replace another payload.
            guard.panic = Some(payload);
            if let Some(Body::Property { events, .. }) = guard.body.as_ref() {
                events.fetch_or(PANIC, Ordering::Release);
            }
            fallback
        }
    };
    drop(guard);
    if slot.gate.fault.load(Ordering::Acquire) {
        let _ = slot
            .admitted
            .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
        fallback
    } else {
        value
    }
}

pub(super) fn revoke_exact(token: usize) {
    if token >> TOKEN_SHIFT != 0 {
        let _ = SLOTS[token & 63].admitted.compare_exchange(
            token,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Byte-pattern equilibrium avoids unaligned typed stores. Tags are immutable token bits,
/// so a stale output callback never reads a later registration's sample format.
unsafe fn silence(
    token: usize,
    flags: NonNull<AudioUnitRenderActionFlags>,
    data: *mut AudioBufferList,
) -> i32 {
    if data.is_null() {
        return BAD_CALLBACK;
    }
    let count = unsafe { (*data).mNumberBuffers as usize };
    if count == 0 || count > COUNT {
        return BAD_CALLBACK;
    }
    let format = (token >> 8) & 15;
    let width = match format {
        0 | 7 => 1,
        1 | 8 => 2,
        2 | 9 | 3 | 5 | 10 => 4,
        4 | 6 | 11 => 8,
        _ => return BAD_CALLBACK,
    };
    let buffers = unsafe { std::slice::from_raw_parts_mut((*data).mBuffers.as_mut_ptr(), count) };
    let mut total = 0usize;
    // Validate ALL buffers before any write.
    for b in buffers.iter() {
        let size = b.mDataByteSize as usize;
        let Some(sum) = total.checked_add(size) else {
            return BAD_CALLBACK;
        };
        total = sum;
        if total > MAX_BYTES
            || size % width != 0
            || (size != 0 && (b.mData.is_null() || b.mData.addr() % width != 0))
        {
            return BAD_CALLBACK;
        }
    }
    for b in buffers {
        let size = b.mDataByteSize as usize;
        if size == 0 {
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts_mut(b.mData.cast::<u8>(), size) };
        bytes.fill(0);
        if format >= 7 {
            // Native-endian unsigned PCM equilibrium is its sign/midpoint bit.
            // CPAL I24/U24 use FOUR-byte i32/u32 storage, with 24-bit depth.
            // U24 equilibrium is 1<<23, not the high bit of its 32-bit container.
            let little_index = if format == 9 { 2 } else { width - 1 };
            let high = if cfg!(target_endian = "little") {
                little_index
            } else {
                width - 1 - little_index
            };
            for sample in bytes.chunks_exact_mut(width) {
                sample[high] = 0x80;
            }
        }
    }
    unsafe {
        (*flags.as_ptr()).insert(AudioUnitRenderActionFlags::UnitRenderAction_OutputIsSilence);
    }
    0
}

pub(super) extern "C-unwind" fn render(
    refcon: NonNull<c_void>,
    flags: NonNull<AudioUnitRenderActionFlags>,
    timestamp: NonNull<AudioTimeStamp>,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    let token = refcon.as_ptr().addr();
    let kind = (token >> 6) & 3;
    let fallback = if kind == 1 {
        unsafe { silence(token, flags, data) }
    } else {
        0
    };
    if fallback != 0 {
        return fallback;
    }
    let used = enter(token, None, |body| {
        let status = (|| {
            let (callback, list) = match body {
                Body::Render(callback) => {
                    if data.is_null()
                        || unsafe { (*data).mNumberBuffers } != 1
                        || unsafe { (*data).mBuffers[0].mNumberChannels } == 0
                    {
                        return BAD_CALLBACK;
                    }
                    if unsafe { (*data).mBuffers[0].mDataByteSize } == 0 {
                        return 0;
                    }
                    if unsafe { (*data).mBuffers[0].mData }.is_null() {
                        return BAD_CALLBACK;
                    }
                    (callback, data)
                }
                Body::Input {
                    callback,
                    unit,
                    samples,
                    list,
                    bytes_per_frame,
                } => {
                    let Some(bytes) = (frames as usize).checked_mul(*bytes_per_frame) else {
                        return BAD_CALLBACK;
                    };
                    if bytes > samples.len() * 8 || bytes > MAX_BYTES {
                        return BAD_CALLBACK;
                    }
                    list.mBuffers[0].mData = samples.as_mut_ptr().cast();
                    list.mBuffers[0].mDataByteSize = bytes as u32;
                    let status = unsafe {
                        AudioUnitRender(
                            *unit,
                            flags.as_ptr(),
                            timestamp,
                            bus,
                            frames,
                            NonNull::from(&mut *list),
                        )
                    };
                    if status != 0 {
                        return status;
                    }
                    if list.mNumberBuffers != 1
                        || list.mBuffers[0].mDataByteSize as usize > bytes
                        || list.mBuffers[0].mData != samples.as_mut_ptr().cast()
                        || list.mBuffers[0].mNumberChannels == 0
                    {
                        return BAD_CALLBACK;
                    }
                    (callback, list as *mut _)
                }
                Body::Property { .. } => return BAD_CALLBACK,
            };
            match callback(RawArgs {
                data: list,
                time_stamp: unsafe { timestamp.read() },
            }) {
                Ok(()) => 0,
                Err(()) => BAD_CALLBACK,
            }
        })();
        if status != 0 {
            let slot = &SLOTS[token & 63];
            slot.failed.store(true, Ordering::Release);
            let _ = slot
                .admitted
                .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        Some(status)
    });
    match used {
        Some(0) => {
            if kind == 1 {
                unsafe {
                    (*flags.as_ptr())
                        .remove(AudioUnitRenderActionFlags::UnitRenderAction_OutputIsSilence);
                }
            }
            0
        }
        Some(status) => {
            if kind == 1 {
                let _ = unsafe { silence(token, flags, data) };
            }
            status
        }
        None => fallback,
    }
}

pub(super) unsafe extern "C-unwind" fn property(
    _: AudioObjectID,
    _: u32,
    _: NonNull<AudioObjectPropertyAddress>,
    refcon: *mut c_void,
) -> i32 {
    let token = refcon.addr();
    enter(token, 0, |body| {
        if let Body::Property { events, mask } = body {
            events.fetch_or(*mask, Ordering::Release);
        }
        0
    })
}

pub(super) fn input_body(
    unit: AudioUnit,
    channels: u32,
    bytes_per_frame: usize,
    max_frames: usize,
    callback: UserCallback,
) -> Result<Body, Error> {
    let bytes = max_frames
        .checked_mul(bytes_per_frame)
        .filter(|n| *n != 0 && *n <= MAX_BYTES)
        .ok_or_else(error)?;
    Ok(Body::Input {
        callback,
        unit,
        samples: vec![0; bytes.div_ceil(8)],
        list: AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: bytes as u32,
                mData: std::ptr::null_mut(),
            }],
        },
        bytes_per_frame,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn output(token: usize, words: &mut [u32], channels: u32) -> (i32, u32) {
        output_with_flags(token, words, channels, 0)
    }
    fn output_with_flags(
        token: usize,
        words: &mut [u32],
        channels: u32,
        initial_flags: u32,
    ) -> (i32, u32) {
        let mut flags = AudioUnitRenderActionFlags(initial_flags);
        let mut stamp: AudioTimeStamp = unsafe { std::mem::zeroed() };
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: std::mem::size_of_val(words) as u32,
                mData: if words.is_empty() {
                    std::ptr::null_mut()
                } else {
                    words.as_mut_ptr().cast()
                },
            }],
        };
        let result = render(
            NonNull::new(std::ptr::without_provenance_mut(token)).unwrap(),
            NonNull::from(&mut flags),
            NonNull::from(&mut stamp),
            0,
            words.len() as u32,
            &mut list,
        );
        (result, flags.0)
    }
    fn retire(token: Token) {
        let body = token.retire().unwrap();
        token.release().unwrap();
        drop(body);
    }
    #[test]
    fn stale_generation_cannot_revoke_or_fail_current_callback() {
        let old = Token::install(Body::Render(Box::new(|_| Ok(()))), SampleFormat::I32).unwrap();
        let generation = old.0 >> TOKEN_SHIFT;
        retire(old);
        let called = Arc::new(AtomicUsize::new(0));
        let count = called.clone();
        let current = Token::install(
            Body::Render(Box::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })),
            SampleFormat::I32,
        )
        .unwrap();
        let stale = (generation << TOKEN_SHIFT) | (current.0 & ((1 << TOKEN_SHIFT) - 1));
        assert_ne!(stale, current.0);
        revoke_exact(stale);
        // Preserve other native flags, including bits unknown to this SDK version.
        let stale_initial_flags = (1 << 31) | (1 << 2);
        let stale_result = output_with_flags(stale, &mut [u32::MAX; 2], 1, stale_initial_flags);
        let still_admitted = SLOTS[current.0 & 63].admitted.load(Ordering::Acquire) == current.0;
        let not_failed = !current.failed();
        let current_result = output_with_flags(current.0, &mut [u32::MAX; 2], 1, u32::MAX);
        retire(current);
        assert_eq!(stale_result.0, 0);
        assert_eq!(stale_result.1, stale_initial_flags | (1 << 4));
        assert!(still_admitted && not_failed);
        assert_eq!(current_result.0, 0);
        assert_eq!(current_result.1, u32::MAX & !(1 << 4));
        assert_eq!(called.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn admitted_error_revokes_exact_generation_before_return() {
        let token = Token::install(Body::Render(Box::new(|_| Err(()))), SampleFormat::I32).unwrap();
        let mut words = [u32::MAX; 2];
        let (status, flags) = output(token.0, &mut words, 1);
        let failed = token.failed();
        let revoked = SLOTS[token.0 & 63].admitted.load(Ordering::Acquire) == 0;
        retire(token);
        assert_ne!(status, 0);
        assert!(failed && revoked);
        assert_ne!(flags & (1 << 4), 0);
        assert_eq!(words, [0; 2]);
    }
    #[test]
    fn twenty_four_bit_storage_and_unsigned_midpoint_are_four_bytes() {
        for (format, equilibrium) in [(SampleFormat::I24, 0), (SampleFormat::U24, 1 << 23)] {
            let token = Token::install(Body::Render(Box::new(|_| Ok(()))), format).unwrap();
            token.revoke();
            let mut words = [u32::MAX; 2]; // eight bytes: not divisible by three
            let (status, flags) = output(token.0, &mut words, 1);
            retire(token);
            assert_eq!(status, 0);
            assert_ne!(flags & (1 << 4), 0);
            assert_eq!(words, [equilibrium; 2]);
        }
    }
    #[test]
    fn empty_null_output_does_not_construct_or_invoke_user_data() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let token = Token::install(
            Body::Render(Box::new(move |_| {
                seen.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })),
            SampleFormat::I32,
        )
        .unwrap();
        let result = output(token.0, &mut [], 1);
        retire(token);
        assert_eq!(result.0, 0);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn caught_panic_and_callback_body_are_retained_until_off_callback_release() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let payload = dropped.clone();
        let token = Token::install(
            Body::Render(Box::new(move |_| {
                std::panic::panic_any(Payload(payload.clone()))
            })),
            SampleFormat::I32,
        )
        .unwrap();
        let result = output(token.0, &mut [0; 2], 1);
        let failed = token.failed();
        let on_callback = dropped.load(Ordering::SeqCst);
        let retired = token.retire().unwrap();
        let before_release = dropped.load(Ordering::SeqCst);
        token.release().unwrap();
        drop(retired);
        assert_eq!(result.0, 0); // caught panic produces inert silence; owner reports the failure
        assert!(failed);
        assert_eq!(on_callback, 0);
        assert_eq!(before_release, 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
