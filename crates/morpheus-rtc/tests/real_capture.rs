//! Opt-in macOS proof that native display/window pixels reach the I420 frame
//! format accepted by a MatrixRTC screen publication. No image is saved.

#![cfg(all(target_os = "macos", feature = "device-screen"))]

use std::{collections::HashSet, time::Duration};

use morpheus_rtc::{I420Buffer, ScreenCapture};

fn valid_planes(frame: &I420Buffer) -> bool {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    frame.stride_y as usize >= width
        && frame.stride_u as usize >= chroma_width
        && frame.stride_v as usize >= chroma_width
        && frame.data_y.len() >= frame.stride_y as usize * height
        && frame.data_u.len() >= frame.stride_u as usize * chroma_height
        && frame.data_v.len() >= frame.stride_v as usize * chroma_height
}

#[test]
fn macos_display_or_window_produces_live_i420() {
    if std::env::var("MORPHEUS_REAL_CAPTURE").as_deref() != Ok("1") {
        eprintln!("Skipped real macOS capture; set MORPHEUS_REAL_CAPTURE=1 to opt in.");
        return;
    }
    let source = std::env::var("MORPHEUS_CAPTURE_SOURCE").unwrap_or_else(|_| "display".into());
    let index = std::env::var("MORPHEUS_CAPTURE_INDEX")
        .ok()
        .map(|value| value.parse::<usize>().expect("Invalid capture index"))
        .unwrap_or(0);
    let capture = match source.as_str() {
        "display" => {
            assert!(
                index
                    < ScreenCapture::displays()
                        .expect("Cannot enumerate displays")
                        .len(),
                "Requested display is unavailable"
            );
            ScreenCapture::open(index).expect("Cannot open display")
        }
        "window" => {
            assert!(
                index
                    < ScreenCapture::windows()
                        .expect("Cannot enumerate windows")
                        .len(),
                "Requested window is unavailable"
            );
            ScreenCapture::open_window(index).expect("Cannot open window")
        }
        _ => panic!("MORPHEUS_CAPTURE_SOURCE must be display or window"),
    };
    let first = capture
        .next_frame_sized(640, 360)
        .expect("Native capture or I420 conversion failed");
    assert_eq!((first.buffer.width, first.buffer.height), (640, 360));
    assert!(valid_planes(&first.buffer), "Invalid first I420 frame");
    let levels = first.buffer.data_y.iter().copied().collect::<HashSet<_>>();
    assert!(
        levels.len() >= 4 && first.buffer.data_y.iter().any(|luma| *luma > 20),
        "Capture is blank or nearly uniform; check macOS Screen Recording permission and source"
    );
    std::thread::sleep(Duration::from_millis(120));
    let second = capture
        .next_frame_sized(640, 360)
        .expect("Second native capture failed");
    assert!(valid_planes(&second.buffer), "Invalid second I420 frame");
    assert!(second.timestamp_us > first.timestamp_us);
    eprintln!("Captured two nonblank {source} frames at 640×360 as I420.");
}
