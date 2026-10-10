//! Bounded I420-to-Slint conversion for the isolated native image proof.
//!
//! This does not subscribe to a MatrixRTC track. The proof window feeds it
//! synthetic `VideoFrame`s with the same owned type exposed by Morpheus RTC.

use morpheus_rtc::{I420Buffer, VideoFrame, VideoRotation};
use slint::{Rgb8Pixel, SharedPixelBuffer};

const MAX_EDGE: u32 = 1920;
const MAX_PIXELS: usize = 1920 * 1080;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    InvalidDimensions,
    InvalidPlane,
}

/// Convert one BT.601 limited-range I420 frame to packed RGB8, applying the
/// clockwise display rotation supplied by the transport. Call this on a worker;
/// construct `slint::Image` only on Slint's UI thread.
pub fn rgb_buffer(frame: &VideoFrame) -> Result<SharedPixelBuffer<Rgb8Pixel>, FrameError> {
    let source = &frame.buffer;
    let width = usize::try_from(source.width).map_err(|_| FrameError::InvalidDimensions)?;
    let height = usize::try_from(source.height).map_err(|_| FrameError::InvalidDimensions)?;
    let pixels = width
        .checked_mul(height)
        .ok_or(FrameError::InvalidDimensions)?;
    if width == 0
        || height == 0
        || source.width > MAX_EDGE
        || source.height > MAX_EDGE
        || pixels > MAX_PIXELS
    {
        return Err(FrameError::InvalidDimensions);
    }

    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let stride_y = checked_plane(&source.data_y, source.stride_y, width, height)?;
    let stride_u = checked_plane(&source.data_u, source.stride_u, chroma_width, chroma_height)?;
    let stride_v = checked_plane(&source.data_v, source.stride_v, chroma_width, chroma_height)?;

    let (output_width, output_height) = match frame.rotation {
        VideoRotation::Deg0 | VideoRotation::Deg180 => (source.width, source.height),
        VideoRotation::Deg90 | VideoRotation::Deg270 => (source.height, source.width),
    };
    let mut output = SharedPixelBuffer::<Rgb8Pixel>::new(output_width, output_height);
    let output_bytes = output.make_mut_bytes();
    let output_width = output_width as usize;
    for y in 0..height {
        for x in 0..width {
            let luma = source.data_y[y * stride_y + x];
            let chroma_u = source.data_u[(y / 2) * stride_u + x / 2];
            let chroma_v = source.data_v[(y / 2) * stride_v + x / 2];
            let rgb = yuv_to_rgb(luma, chroma_u, chroma_v);
            let (display_x, display_y) = match frame.rotation {
                VideoRotation::Deg0 => (x, y),
                VideoRotation::Deg90 => (height - 1 - y, x),
                VideoRotation::Deg180 => (width - 1 - x, height - 1 - y),
                VideoRotation::Deg270 => (y, width - 1 - x),
            };
            let offset = (display_y * output_width + display_x) * 3;
            output_bytes[offset..offset + 3].copy_from_slice(&rgb);
        }
    }
    Ok(output)
}

fn checked_plane(
    data: &[u8],
    stride: u32,
    visible_width: usize,
    rows: usize,
) -> Result<usize, FrameError> {
    let stride = usize::try_from(stride).map_err(|_| FrameError::InvalidPlane)?;
    let length = stride.checked_mul(rows).ok_or(FrameError::InvalidPlane)?;
    if stride < visible_width || data.len() < length {
        return Err(FrameError::InvalidPlane);
    }
    Ok(stride)
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let channel = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    [
        channel(298 * c + 409 * e),
        channel(298 * c - 100 * d - 208 * e),
        channel(298 * c + 516 * d),
    ]
}

/// A moving, eight-bar test frame with visible luma ticks. No capture device,
/// homeserver, or call service is needed to run the proof window.
pub fn synthetic_frame(index: u32) -> VideoFrame {
    const WIDTH: usize = 320;
    const HEIGHT: usize = 180;
    const BARS: [(u8, u8, u8); 8] = [
        (235, 128, 128), // white
        (210, 16, 146),  // yellow
        (170, 166, 16),  // cyan
        (145, 54, 34),   // green
        (106, 202, 222), // magenta
        (81, 90, 240),   // red
        (41, 240, 110),  // blue
        (16, 128, 128),  // black
    ];
    let mut data_y = vec![0; WIDTH * HEIGHT];
    let mut data_u = vec![0; WIDTH.div_ceil(2) * HEIGHT.div_ceil(2)];
    let mut data_v = vec![0; data_u.len()];
    let shift = (index / 12) as usize;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let bar = ((x / 40) + shift) % BARS.len();
            let tick = if y > 144 && (x / 8 + index as usize / 2) % 4 == 0 {
                -45
            } else {
                0
            };
            data_y[y * WIDTH + x] = (i32::from(BARS[bar].0) + tick).clamp(16, 235) as u8;
        }
    }
    for y in 0..HEIGHT.div_ceil(2) {
        for x in 0..WIDTH.div_ceil(2) {
            let bar = (((x * 2) / 40) + shift) % BARS.len();
            let offset = y * WIDTH.div_ceil(2) + x;
            data_u[offset] = BARS[bar].1;
            data_v[offset] = BARS[bar].2;
        }
    }
    let rotation = match (index / 45) % 4 {
        0 => VideoRotation::Deg0,
        1 => VideoRotation::Deg90,
        2 => VideoRotation::Deg180,
        _ => VideoRotation::Deg270,
    };
    VideoFrame {
        buffer: I420Buffer {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            data_y,
            stride_y: WIDTH as u32,
            data_u,
            stride_u: WIDTH.div_ceil(2) as u32,
            data_v,
            stride_v: WIDTH.div_ceil(2) as u32,
        },
        rotation,
        timestamp_us: i64::from(index) * 66_667,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded_frame(rotation: VideoRotation) -> VideoFrame {
        VideoFrame {
            buffer: I420Buffer {
                width: 2,
                height: 3,
                data_y: vec![16, 235, 0, 81, 145, 0, 41, 106, 0],
                stride_y: 3,
                data_u: vec![128, 0, 128, 0],
                stride_u: 2,
                data_v: vec![128, 0, 128, 0],
                stride_v: 2,
            },
            rotation,
            timestamp_us: 0,
        }
    }

    #[test]
    fn respects_padding_and_limited_range() {
        let image = rgb_buffer(&padded_frame(VideoRotation::Deg0)).unwrap();
        assert_eq!((image.width(), image.height()), (2, 3));
        let rgb = image.as_bytes();
        assert_eq!(&rgb[..3], &[0, 0, 0]);
        assert_eq!(&rgb[3..6], &[255, 255, 255]);
        assert_eq!(&rgb[6..9], &[76, 76, 76]);
    }

    #[test]
    fn applies_clockwise_quarter_turn() {
        let image = rgb_buffer(&padded_frame(VideoRotation::Deg90)).unwrap();
        assert_eq!((image.width(), image.height()), (3, 2));
        let rgb = image.as_bytes();
        // The top row is the original left column, from bottom to top.
        assert_eq!(&rgb[0..3], &[29, 29, 29]);
        assert_eq!(&rgb[3..6], &[76, 76, 76]);
        assert_eq!(&rgb[6..9], &[0, 0, 0]);
    }

    #[test]
    fn rejects_short_planes_and_excessive_dimensions() {
        let mut frame = padded_frame(VideoRotation::Deg0);
        frame.buffer.data_u.pop();
        assert_eq!(rgb_buffer(&frame).unwrap_err(), FrameError::InvalidPlane);
        frame.buffer.width = MAX_EDGE + 1;
        assert_eq!(
            rgb_buffer(&frame).unwrap_err(),
            FrameError::InvalidDimensions
        );
    }
}
