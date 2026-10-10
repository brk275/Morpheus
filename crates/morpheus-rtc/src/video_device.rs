use std::time::Instant;

use matrix_rtc_transport::{I420Buffer, VideoFrame, VideoRotation};

use crate::MediaError;

#[cfg(feature = "device-camera")]
pub struct CameraCapture {
    camera: nokhwa::Camera,
    started: Instant,
}

#[cfg(feature = "device-camera")]
impl CameraCapture {
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
        use nokhwa::pixel_format::RgbFormat;
        use nokhwa::utils::{
            CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
        };

        let preferred = CameraFormat::new_from(640, 480, FrameFormat::MJPEG, 30);
        let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(preferred));
        let mut camera = nokhwa::Camera::new(CameraIndex::Index(index), requested)
            .map_err(|error| MediaError::Device(error.to_string()))?;
        camera
            .open_stream()
            .map_err(|error| MediaError::Device(error.to_string()))?;
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

#[cfg(feature = "device-screen")]
pub struct ScreenCapture {
    source: ScreenSource,
    started: Instant,
}

#[cfg(feature = "device-screen")]
enum ScreenSource {
    Display(xcap::Monitor),
    Window(xcap::Window),
}

#[cfg(feature = "device-screen")]
impl ScreenCapture {
    /// Enumerate display names without starting a capture. The display index
    /// passed to `open` is the index in this same enumeration.
    pub fn displays() -> Result<Vec<String>, MediaError> {
        xcap::Monitor::all()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .into_iter()
            .map(|monitor| {
                monitor
                    .friendly_name()
                    .map_err(|error| MediaError::Device(error.to_string()))
            })
            .collect()
    }

    /// Capture a display through ScreenCaptureKit/CoreGraphics on macOS,
    /// platform capture on Windows, or X11/Wayland on Linux. The OS may require
    /// a screen-recording permission or portal selection.
    pub fn open(index: usize) -> Result<Self, MediaError> {
        let monitor = xcap::Monitor::all()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .into_iter()
            .nth(index)
            .ok_or_else(|| MediaError::Device("display index is unavailable".into()))?;
        Ok(Self {
            source: ScreenSource::Display(monitor),
            started: Instant::now(),
        })
    }

    /// Enumerate application windows. The index passed to `open_window` refers
    /// to this list; a window can disappear between enumeration and opening.
    pub fn windows() -> Result<Vec<String>, MediaError> {
        xcap::Window::all()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .into_iter()
            .map(|window| {
                let app = window
                    .app_name()
                    .map_err(|error| MediaError::Device(error.to_string()))?;
                let title = window
                    .title()
                    .map_err(|error| MediaError::Device(error.to_string()))?;
                Ok(format!("{app} — {title}"))
            })
            .collect()
    }

    /// Capture one native application window rather than an entire display.
    /// The OS may reject protected, minimized, or permission-denied windows.
    pub fn open_window(index: usize) -> Result<Self, MediaError> {
        let window = xcap::Window::all()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .into_iter()
            .nth(index)
            .ok_or_else(|| MediaError::Device("window index is unavailable".into()))?;
        Ok(Self {
            source: ScreenSource::Window(window),
            started: Instant::now(),
        })
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
        let result = match &self.source {
            ScreenSource::Display(monitor) => monitor.capture_image(),
            ScreenSource::Window(window) => window.capture_image(),
        };
        result.map_err(|error| MediaError::Device(error.to_string()))
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
