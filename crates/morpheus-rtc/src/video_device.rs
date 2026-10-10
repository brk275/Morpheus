use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::Instant,
};

#[cfg(all(feature = "device-camera", any(target_os = "macos", test)))]
use std::{sync::mpsc, time::Duration};

#[cfg(feature = "device-screen")]
use std::{collections::hash_map::RandomState, hash::BuildHasher, sync::OnceLock};

use matrix_rtc_transport::{I420Buffer, VideoFrame, VideoRotation};

use crate::MediaError;

#[cfg(feature = "device-camera")]
pub struct CameraCapture {
    camera: nokhwa::Camera,
    started: Instant,
}

#[cfg(feature = "device-camera")]
impl CameraCapture {
    /// Stable native camera keys for the source picker. Nokhwa uses numeric
    /// indices on some backends and string identifiers on others.
    pub fn camera_sources() -> Result<Vec<(String, String)>, MediaError> {
        nokhwa::query(nokhwa::utils::ApiBackend::Auto)
            .map_err(|error| MediaError::Device(error.to_string()))
            .map(|cameras| {
                cameras
                    .into_iter()
                    .map(|camera| (camera_key(&camera), camera.human_name()))
                    .collect()
            })
    }

    /// Open the exact camera selected earlier. If it disappeared, fail instead
    /// of opening the device now occupying an old ordinal position.
    pub fn open_key(key: &str) -> Result<Self, MediaError> {
        Self::open_key_cancellable(key, || false)
    }

    /// Open a selected camera while observing cancellation during permission
    /// prompts and before native device creation or stream startup. A late OS
    /// permission grant must not start a camera after the caller has stopped.
    pub fn open_key_cancellable(
        key: &str,
        cancelled: impl Fn() -> bool,
    ) -> Result<Self, MediaError> {
        check_camera_cancelled(&cancelled)?;
        #[cfg(target_os = "macos")]
        ensure_camera_permission(&cancelled)?;
        check_camera_cancelled(&cancelled)?;
        let available = nokhwa::query(nokhwa::utils::ApiBackend::Auto)
            .map_err(|error| MediaError::Device(error.to_string()))?;
        let selected = available
            .iter()
            .find(|camera| camera_key(camera) == key)
            .ok_or_else(|| MediaError::Device("selected camera changed or disappeared".into()))?;
        let capture = Self::open_index(selected.index().clone(), &cancelled, Some(key))?;
        check_camera_cancelled(&cancelled)?;
        Ok(capture)
    }

    /// Enumerate camera names using the platform's native capture backend.
    /// The index passed to `open` refers to this enumeration.
    pub fn cameras() -> Result<Vec<String>, MediaError> {
        nokhwa::query(nokhwa::utils::ApiBackend::Auto)
            .map_err(|error| MediaError::Device(error.to_string()))
            .map(|cameras| {
                cameras
                    .into_iter()
                    .map(|camera| camera.human_name())
                    .collect()
            })
    }

    /// Open a native AVFoundation, V4L2, or MSMF camera by its platform index.
    /// The OS may request camera permission while opening the stream.
    pub fn open(index: u32) -> Result<Self, MediaError> {
        Self::open_index(nokhwa::utils::CameraIndex::Index(index), &|| false, None)
    }

    fn open_index(
        index: nokhwa::utils::CameraIndex,
        cancelled: &impl Fn() -> bool,
        expected_key: Option<&str>,
    ) -> Result<Self, MediaError> {
        use nokhwa::pixel_format::{FormatDecoder, RgbFormat};
        use nokhwa::utils::{RequestedFormat, RequestedFormatType, Resolution};

        check_camera_cancelled(cancelled)?;
        #[cfg(target_os = "macos")]
        ensure_camera_permission(cancelled)?;

        // AVFoundation's format setter accepts a range's maximum frame rate.
        // Nokhwa's `None` request can pick the first (minimum-rate) entry and
        // fail even though the camera has a usable stream. Prefer a small
        // supported resolution at its highest advertised rate. V4L2 and MSMF
        // also fall back to their first decodable format when none match.
        let requests = [
            RequestedFormatType::HighestResolution(Resolution::new(640, 480)),
            RequestedFormatType::HighestResolution(Resolution::new(1280, 720)),
            RequestedFormatType::HighestResolution(Resolution::new(320, 240)),
            RequestedFormatType::HighestResolution(Resolution::new(1920, 1080)),
            RequestedFormatType::None,
        ];
        let mut last_error = None;
        let mut camera = None;
        for request in requests {
            check_camera_cancelled(cancelled)?;
            let requested = RequestedFormat::new::<RgbFormat>(request);
            match nokhwa::Camera::new(index.clone(), requested) {
                Ok(candidate)
                    if RgbFormat::FORMATS.contains(&candidate.camera_format().format()) =>
                {
                    camera = Some(candidate);
                    break;
                }
                Ok(_) => last_error = Some("camera has no RGB-decodable format".to_owned()),
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        let mut camera = camera.ok_or_else(|| {
            MediaError::Device(
                last_error.unwrap_or_else(|| "camera has no usable format".to_owned()),
            )
        })?;
        // Numeric indices can be reused between enumeration and device open.
        // Reject a changed device before its stream can capture any frames.
        if expected_key.is_some_and(|key| camera_key(camera.info()) != key) {
            return Err(MediaError::Device(
                "selected camera changed while opening; choose it again".into(),
            ));
        }
        check_camera_cancelled(cancelled)?;
        camera
            .open_stream()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        // If the native start call was already in progress when Stop arrived,
        // dropping this local camera closes it before it can return a frame.
        check_camera_cancelled(cancelled)?;
        Ok(Self {
            camera,
            started: Instant::now(),
        })
    }

    /// Decode the latest camera frame to I420 for the native call encoder.
    pub fn next_frame(&mut self) -> Result<VideoFrame, MediaError> {
        use nokhwa::pixel_format::RgbFormat;

        let frame = self
            .camera
            .frame()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        let image = frame
            .decode_image::<RgbFormat>()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        let (width, height) = image.dimensions();
        rgb_to_i420(
            width,
            height,
            image.as_raw(),
            3,
            self.started.elapsed().as_micros() as i64,
        )
    }
}

#[cfg(feature = "device-camera")]
fn check_camera_cancelled(cancelled: &impl Fn() -> bool) -> Result<(), MediaError> {
    if cancelled() {
        Err(MediaError::CallEnded)
    } else {
        Ok(())
    }
}

#[cfg(all(feature = "device-camera", target_os = "macos"))]
fn ensure_camera_permission(cancelled: &impl Fn() -> bool) -> Result<(), MediaError> {
    // Nokhwa requires explicit AVFoundation initialization. Querying camera
    // metadata does not request TCC access; starting a session while status
    // is NotDetermined can succeed without delivering a single frame.
    check_camera_cancelled(cancelled)?;
    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    let (permission_sender, permission_receiver) = mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = permission_sender.send(granted);
    });
    wait_for_camera_permission(permission_receiver, cancelled, Duration::from_secs(25))
}

#[cfg(all(feature = "device-camera", any(target_os = "macos", test)))]
fn wait_for_camera_permission(
    receiver: mpsc::Receiver<bool>,
    cancelled: &impl Fn() -> bool,
    timeout: Duration,
) -> Result<(), MediaError> {
    let deadline = Instant::now() + timeout;
    loop {
        check_camera_cancelled(cancelled)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(MediaError::Device(
                "camera permission response timed out; choose the camera again".into(),
            ));
        }
        // The OS owns its prompt and callback. Poll only the result channel so
        // Stop/Leave can finish even if the user leaves that prompt unanswered.
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(granted) => {
                check_camera_cancelled(cancelled)?;
                return if granted {
                    Ok(())
                } else {
                    Err(MediaError::Device(
                        "camera access denied; allow Morpheus in macOS Privacy & Security > Camera"
                            .into(),
                    ))
                };
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(MediaError::Device(
                    "camera permission request ended unexpectedly".into(),
                ));
            }
        }
    }
}

#[cfg(feature = "device-camera")]
fn camera_key(info: &nokhwa::utils::CameraInfo) -> String {
    // A numeric CameraIndex can be reused after devices reorder. Include all
    // picker-time device metadata and fail closed if it changes before open.
    let mut hasher = DefaultHasher::new();
    info.hash(&mut hasher);
    let index = match info.index() {
        nokhwa::utils::CameraIndex::Index(index) => format!("n:{index}"),
        nokhwa::utils::CameraIndex::String(index) => format!("s:{index}"),
    };
    format!("{index}:{:016x}", hasher.finish())
}

#[cfg(feature = "device-screen")]
pub struct ScreenCapture {
    source: ScreenSource,
    fingerprint: ScreenFingerprint,
    started: Instant,
}

#[cfg(feature = "device-screen")]
enum ScreenSource {
    Display(xcap::Monitor),
    Window(xcap::Window),
}

/// Identity captured at picker time. All fields participate in the opaque
/// source key and are checked again before and after every captured frame.
#[cfg(feature = "device-screen")]
#[derive(Clone, Eq, Hash, PartialEq)]
enum ScreenFingerprint {
    Display {
        id: u32,
        name: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    Window {
        id: u32,
        pid: u32,
        app_name: String,
        title: String,
    },
}

#[cfg(feature = "device-screen")]
impl ScreenFingerprint {
    fn key(&self) -> String {
        // The key is only used within this process. A process-random seed and
        // two domain-separated hashes keep window titles and PIDs out of the
        // Slint model while making accidental collisions negligible.
        static HASH_STATE: OnceLock<RandomState> = OnceLock::new();
        let state = HASH_STATE.get_or_init(RandomState::new);
        let hash = |domain: u8| {
            let mut hasher = state.build_hasher();
            domain.hash(&mut hasher);
            self.hash(&mut hasher);
            hasher.finish()
        };
        let kind = match self {
            Self::Display { .. } => "display",
            Self::Window { .. } => "window",
        };
        format!("{kind}:{:016x}{:016x}", hash(0), hash(1))
    }
}

#[cfg(feature = "device-screen")]
fn display_fingerprint(monitor: &xcap::Monitor) -> Result<ScreenFingerprint, MediaError> {
    let device_error = |error: xcap::XCapError| MediaError::Device(error.to_string());
    // `friendly_name()` uses AppKit's unchecked main-thread marker on macOS.
    // `name()` uses CoreGraphics and is safe on the capture worker.
    Ok(ScreenFingerprint::Display {
        id: monitor.id().map_err(device_error)?,
        name: monitor.name().map_err(device_error)?,
        x: monitor.x().map_err(device_error)?,
        y: monitor.y().map_err(device_error)?,
        width: monitor.width().map_err(device_error)?,
        height: monitor.height().map_err(device_error)?,
    })
}

#[cfg(feature = "device-screen")]
fn window_fingerprint(window: &xcap::Window) -> Result<ScreenFingerprint, MediaError> {
    let device_error = |error: xcap::XCapError| MediaError::Device(error.to_string());
    Ok(ScreenFingerprint::Window {
        id: window.id().map_err(device_error)?,
        pid: window.pid().map_err(device_error)?,
        app_name: window.app_name().map_err(device_error)?,
        title: window.title().map_err(device_error)?,
    })
}

#[cfg(feature = "device-screen")]
fn source_changed() -> MediaError {
    // Never include a window title, PID, or source label in errors or logs.
    MediaError::Device("selected capture source is unavailable or has changed".into())
}

#[cfg(feature = "device-screen")]
impl ScreenCapture {
    /// Return process-local opaque keys and display labels for the picker.
    /// Opening a key succeeds only while the complete picker-time fingerprint
    /// still matches exactly one native display.
    pub fn display_sources() -> Result<Vec<(String, String)>, MediaError> {
        let monitors =
            xcap::Monitor::all().map_err(|error| MediaError::Device(error.to_string()))?;
        let mut sources = Vec::new();
        for monitor in monitors {
            let Ok(fingerprint) = display_fingerprint(&monitor) else {
                continue;
            };
            let ScreenFingerprint::Display {
                id,
                ref name,
                width,
                height,
                ..
            } = fingerprint
            else {
                unreachable!();
            };
            sources.push((
                fingerprint.key(),
                format!("{name} · {width}×{height} · #{id}"),
            ));
        }
        Ok(sources)
    }

    /// Return process-local opaque keys and current application/title labels.
    /// A closed window or reused native ID cannot silently change the target.
    pub fn window_sources() -> Result<Vec<(String, String)>, MediaError> {
        let windows = xcap::Window::all().map_err(|error| MediaError::Device(error.to_string()))?;
        let mut sources = Vec::new();
        for window in windows {
            let Ok(fingerprint) = window_fingerprint(&window) else {
                continue;
            };
            let ScreenFingerprint::Window {
                ref app_name,
                ref title,
                ..
            } = fingerprint
            else {
                unreachable!();
            };
            sources.push((fingerprint.key(), format!("{app_name} — {title}")));
        }
        Ok(sources)
    }

    /// Open the selected source only if exactly one currently enumerated
    /// display or window matches its complete picker-time fingerprint.
    pub fn open_key(key: &str) -> Result<Self, MediaError> {
        if key.starts_with("display:") {
            let monitors =
                xcap::Monitor::all().map_err(|error| MediaError::Device(error.to_string()))?;
            let mut matches = monitors
                .into_iter()
                .filter_map(|monitor| {
                    let fingerprint = display_fingerprint(&monitor).ok()?;
                    (fingerprint.key() == key).then_some((monitor, fingerprint))
                })
                .take(2);
            let (monitor, fingerprint) = matches.next().ok_or_else(source_changed)?;
            if matches.next().is_some() {
                return Err(source_changed());
            }
            Ok(Self {
                source: ScreenSource::Display(monitor),
                fingerprint,
                started: Instant::now(),
            })
        } else if key.starts_with("window:") {
            let windows =
                xcap::Window::all().map_err(|error| MediaError::Device(error.to_string()))?;
            let mut matches = windows
                .into_iter()
                .filter_map(|window| {
                    let fingerprint = window_fingerprint(&window).ok()?;
                    (fingerprint.key() == key).then_some((window, fingerprint))
                })
                .take(2);
            let (window, fingerprint) = matches.next().ok_or_else(source_changed)?;
            if matches.next().is_some() {
                return Err(source_changed());
            }
            Ok(Self {
                source: ScreenSource::Window(window),
                fingerprint,
                started: Instant::now(),
            })
        } else {
            Err(source_changed())
        }
    }

    /// Resolve a display by ID for older callers. The complete current
    /// fingerprint is still checked on every frame after opening.
    pub fn open_display_id(id: u32) -> Result<Self, MediaError> {
        let monitor = xcap::Monitor::all()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .into_iter()
            .find(|monitor| monitor.id().ok() == Some(id))
            .ok_or_else(source_changed)?;
        Self::open_key(&display_fingerprint(&monitor)?.key())
    }

    /// Resolve a window by ID for older callers. Prefer the picker-time key:
    /// an ID alone cannot detect a reused window between picking and opening.
    pub fn open_window_id(id: u32) -> Result<Self, MediaError> {
        let window = xcap::Window::all()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .into_iter()
            .find(|window| window.id().ok() == Some(id))
            .ok_or_else(source_changed)?;
        Self::open_key(&window_fingerprint(&window)?.key())
    }

    /// Enumerate display names without starting a capture. The display index
    /// passed to `open` is the index in this same enumeration.
    pub fn displays() -> Result<Vec<String>, MediaError> {
        Ok(Self::display_sources()?
            .into_iter()
            .map(|(_, label)| label)
            .collect())
    }

    /// Capture a display through ScreenCaptureKit/CoreGraphics on macOS,
    /// platform capture on Windows, or X11/Wayland on Linux. The OS may require
    /// a screen-recording permission or portal selection.
    pub fn open(index: usize) -> Result<Self, MediaError> {
        let key = Self::display_sources()?
            .get(index)
            .map(|(key, _)| key.clone())
            .ok_or_else(source_changed)?;
        Self::open_key(&key)
    }

    /// Enumerate application windows. The index passed to `open_window` refers
    /// to this list; a window can disappear between enumeration and opening.
    pub fn windows() -> Result<Vec<String>, MediaError> {
        Ok(Self::window_sources()?
            .into_iter()
            .map(|(_, label)| label)
            .collect())
    }

    /// Capture one native application window rather than an entire display.
    /// The OS may reject protected, minimized, or permission-denied windows.
    pub fn open_window(index: usize) -> Result<Self, MediaError> {
        let key = Self::window_sources()?
            .get(index)
            .map(|(key, _)| key.clone())
            .ok_or_else(source_changed)?;
        Self::open_key(&key)
    }

    /// Capture one still frame. The caller must pace repeated captures; a
    /// first native proof can use 5-10 fps before a streaming capture path.
    pub fn next_frame(&self) -> Result<VideoFrame, MediaError> {
        let image = self.capture_image()?;
        let (width, height) = image.dimensions();
        rgb_to_i420(
            width,
            height,
            image.as_raw(),
            4,
            self.started.elapsed().as_micros() as i64,
        )
    }

    /// Capture into a fixed RTC publication size, preserving the source's
    /// aspect ratio with black bars. This bounds CPU and encoder load even on
    /// high-resolution displays and keeps the publication format stable if a
    /// window resizes during a call.
    pub fn next_frame_sized(&self, width: u32, height: u32) -> Result<VideoFrame, MediaError> {
        let image = self.capture_image()?;
        let (source_width, source_height) = image.dimensions();
        let pixels = fit_rgba(source_width, source_height, image.as_raw(), width, height)?;
        rgb_to_i420(
            width,
            height,
            &pixels,
            4,
            self.started.elapsed().as_micros() as i64,
        )
    }

    fn capture_image(&self) -> Result<xcap::image::RgbaImage, MediaError> {
        self.validate_source()?;
        let result = match &self.source {
            ScreenSource::Display(monitor) => monitor.capture_image(),
            ScreenSource::Window(window) => window.capture_image(),
        };
        let image = result.map_err(|error| MediaError::Device(error.to_string()))?;
        // A window can close and its native ID can be reused during capture.
        // Recheck after capture so that frame is not published under the old
        // source selection if its identity changed in the meantime.
        self.validate_source()?;
        Ok(image)
    }

    fn validate_source(&self) -> Result<(), MediaError> {
        let count = match &self.fingerprint {
            ScreenFingerprint::Display { .. } => xcap::Monitor::all()
                .map_err(|error| MediaError::Device(error.to_string()))?
                .iter()
                .filter_map(|monitor| display_fingerprint(monitor).ok())
                .filter(|fingerprint| fingerprint == &self.fingerprint)
                .take(2)
                .count(),
            ScreenFingerprint::Window { .. } => xcap::Window::all()
                .map_err(|error| MediaError::Device(error.to_string()))?
                .iter()
                .filter_map(|window| window_fingerprint(window).ok())
                .filter(|fingerprint| fingerprint == &self.fingerprint)
                .take(2)
                .count(),
        };
        if count == 1 {
            Ok(())
        } else {
            Err(source_changed())
        }
    }
}

/// Nearest-neighbor scaling keeps the capture adapter independent of a second
/// image-processing dependency. RTC encoding smooths the final 4:2:0 frame.
#[cfg(feature = "device-screen")]
fn fit_rgba(
    source_width: u32,
    source_height: u32,
    source: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, MediaError> {
    if source_width == 0
        || source_height == 0
        || width == 0
        || height == 0
        || width > 7680
        || height > 4320
    {
        return Err(MediaError::InvalidFormat);
    }
    let source_width = source_width as usize;
    let source_height = source_height as usize;
    let width = width as usize;
    let height = height as usize;
    if source_width
        .checked_mul(source_height)
        .and_then(|pixels| pixels.checked_mul(4))
        != Some(source.len())
    {
        return Err(MediaError::FrameFormatMismatch);
    }
    let mut fitted = vec![0; width * height * 4];
    for pixel in fitted.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    let (source_width_u64, source_height_u64) = (source_width as u64, source_height as u64);
    let (width_u64, height_u64) = (width as u64, height as u64);
    let (fit_width, fit_height) = if source_width_u64 * height_u64 > source_height_u64 * width_u64 {
        (
            width,
            (source_height_u64 * width_u64 / source_width_u64).max(1) as usize,
        )
    } else {
        (
            (source_width_u64 * height_u64 / source_height_u64).max(1) as usize,
            height,
        )
    };
    let left = (width - fit_width) / 2;
    let top = (height - fit_height) / 2;
    for row in 0..fit_height {
        let source_row = ((2 * row as u64 + 1) * source_height_u64 / (2 * fit_height as u64))
            .min(source_height_u64 - 1) as usize;
        for column in 0..fit_width {
            let source_column = ((2 * column as u64 + 1) * source_width_u64
                / (2 * fit_width as u64))
                .min(source_width_u64 - 1) as usize;
            let source_offset = (source_row * source_width + source_column) * 4;
            let target_offset = ((top + row) * width + left + column) * 4;
            fitted[target_offset..target_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    Ok(fitted)
}

fn rgb_to_i420(
    width: u32,
    height: u32,
    pixels: &[u8],
    channels: usize,
    timestamp_us: i64,
) -> Result<VideoFrame, MediaError> {
    if width == 0
        || height == 0
        || width > 7680
        || height > 4320
        || !(channels == 3 || channels == 4)
    {
        return Err(MediaError::InvalidFormat);
    }
    let w = width as usize;
    let h = height as usize;
    if pixels.len() != w * h * channels {
        return Err(MediaError::FrameFormatMismatch);
    }
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut y = vec![0; w * h];
    let mut u = vec![0; cw * ch];
    let mut v = vec![0; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let mut r_total = 0i32;
            let mut g_total = 0i32;
            let mut b_total = 0i32;
            let mut count = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = cx * 2 + dx;
                    let py = cy * 2 + dy;
                    if px >= w || py >= h {
                        continue;
                    }
                    let offset = (py * w + px) * channels;
                    let r = i32::from(pixels[offset]);
                    let g = i32::from(pixels[offset + 1]);
                    let b = i32::from(pixels[offset + 2]);
                    y[py * w + px] = clip((66 * r + 129 * g + 25 * b + 128) / 256 + 16);
                    r_total += r;
                    g_total += g;
                    b_total += b;
                    count += 1;
                }
            }
            let r = r_total / count;
            let g = g_total / count;
            let b = b_total / count;
            u[cy * cw + cx] = clip((-38 * r - 74 * g + 112 * b + 128) / 256 + 128);
            v[cy * cw + cx] = clip((112 * r - 94 * g - 18 * b + 128) / 256 + 128);
        }
    }
    Ok(VideoFrame {
        buffer: I420Buffer {
            width,
            height,
            data_y: y,
            stride_y: width,
            data_u: u,
            stride_u: cw as u32,
            data_v: v,
            stride_v: cw as u32,
        },
        rotation: VideoRotation::Deg0,
        timestamp_us,
    })
}

fn clip(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "device-camera")]
    mod camera_permission {
        use super::*;
        use std::{
            cell::Cell,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
        };

        #[test]
        fn stop_while_permission_is_pending_drops_late_grant() {
            let (permission_sender, permission_receiver) = mpsc::channel();
            let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
            let (done_sender, done_receiver) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                let result = wait_for_camera_permission(
                    permission_receiver,
                    &|| {
                        let _ = ready_sender.try_send(());
                        worker_stop.load(Ordering::Acquire)
                    },
                    Duration::from_secs(25),
                );
                done_sender.send(result).unwrap();
            });
            ready_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            stop.store(true, Ordering::Release);
            assert!(matches!(
                done_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
                Err(MediaError::CallEnded)
            ));
            // A user may still accept the OS prompt after Leave. Its callback
            // can only send to a dropped receiver; it cannot open a device.
            assert!(permission_sender.send(true).is_err());
            worker.join().unwrap();
        }

        #[test]
        fn cancellation_wins_over_an_already_queued_grant() {
            let (sender, receiver) = mpsc::channel();
            sender.send(true).unwrap();
            assert!(matches!(
                wait_for_camera_permission(receiver, &|| true, Duration::from_secs(1)),
                Err(MediaError::CallEnded)
            ));
        }

        #[test]
        fn cancellation_is_rechecked_after_receiving_a_grant() {
            let (sender, receiver) = mpsc::channel();
            sender.send(true).unwrap();
            let checks = Cell::new(0);
            let cancelled = || {
                let before = checks.get();
                checks.set(before + 1);
                before > 0
            };
            assert!(matches!(
                wait_for_camera_permission(receiver, &cancelled, Duration::from_secs(1)),
                Err(MediaError::CallEnded)
            ));
        }

        #[test]
        fn permission_denial_disconnect_and_timeout_fail_closed() {
            let (sender, receiver) = mpsc::channel();
            sender.send(false).unwrap();
            assert!(matches!(
                wait_for_camera_permission(receiver, &|| false, Duration::from_secs(1)),
                Err(MediaError::Device(_))
            ));
            let (sender, receiver) = mpsc::channel();
            drop(sender);
            assert!(matches!(
                wait_for_camera_permission(receiver, &|| false, Duration::from_secs(1)),
                Err(MediaError::Device(_))
            ));
            let (_sender, receiver) = mpsc::channel();
            assert!(matches!(
                wait_for_camera_permission(receiver, &|| false, Duration::ZERO),
                Err(MediaError::Device(_))
            ));
        }

        #[test]
        fn permission_grant_proceeds_while_active() {
            let (sender, receiver) = mpsc::channel();
            sender.send(true).unwrap();
            assert!(
                wait_for_camera_permission(receiver, &|| false, Duration::from_secs(1)).is_ok()
            );
        }
    }

    #[cfg(feature = "device-camera")]
    #[test]
    fn cancelled_camera_open_returns_before_native_enumeration() {
        assert!(matches!(
            CameraCapture::open_key_cancellable("not-a-camera", || true),
            Err(MediaError::CallEnded)
        ));
    }

    #[test]
    fn converts_odd_rgb_image_to_valid_i420_planes() {
        let frame = rgb_to_i420(3, 3, &vec![255; 3 * 3 * 3], 3, 7).unwrap();
        assert_eq!(frame.buffer.data_y.len(), 9);
        assert_eq!(frame.buffer.data_u.len(), 4);
        assert_eq!(frame.buffer.data_v.len(), 4);
        assert_eq!(frame.timestamp_us, 7);
    }

    #[test]
    fn rejects_truncated_pixel_data() {
        assert!(matches!(
            rgb_to_i420(2, 2, &[0; 11], 3, 0),
            Err(MediaError::FrameFormatMismatch)
        ));
    }

    #[cfg(feature = "device-screen")]
    #[test]
    fn letterboxes_wide_capture_into_fixed_publication() {
        let mut source = vec![0; 4 * 2 * 4];
        for pixel in source.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[255, 0, 0, 255]);
        }
        let fitted = fit_rgba(4, 2, &source, 4, 4).unwrap();
        assert_eq!(&fitted[0..4], &[0, 0, 0, 255]);
        assert_eq!(&fitted[4 * 4..4 * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(&fitted[3 * 4 * 4..3 * 4 * 4 + 4], &[0, 0, 0, 255]);
        let frame = rgb_to_i420(4, 4, &fitted, 4, 1).unwrap();
        assert_eq!(frame.buffer.width, 4);
        assert_eq!(frame.buffer.height, 4);
        assert!(frame.buffer.data_y[0] < frame.buffer.data_y[4]);
    }

    #[cfg(feature = "device-screen")]
    #[test]
    fn rejects_bad_capture_dimensions_before_scaling() {
        assert!(matches!(
            fit_rgba(4, 2, &[0; 31], 640, 360),
            Err(MediaError::FrameFormatMismatch)
        ));
        assert!(matches!(
            fit_rgba(4, 2, &[0; 32], 0, 360),
            Err(MediaError::InvalidFormat)
        ));
    }
}
