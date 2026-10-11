//! Opt-in actual ScreenCast portal/PipeWire acceptance on disposable Sway.
//! The helper owns two differently colored outputs and a deterministic portal
//! chooser. Pixel data never leaves this process and is never written to disk.
#![cfg(all(target_os = "linux", feature = "device-screen"))]

use morpheus_rtc::{I420Buffer, ScreenCapture};
use std::{
    process::Command,
    time::{Duration, Instant},
};

fn center_chroma(frame: &I420Buffer) -> (u8, u8) {
    let row = frame.height as usize / 4;
    let column = frame.width as usize / 4;
    (
        frame.data_u[row * frame.stride_u as usize + column],
        frame.data_v[row * frame.stride_v as usize + column],
    )
}

fn output(arguments: &[&str]) {
    let result = Command::new("swaymsg")
        .args(arguments)
        .output()
        .expect("swaymsg unavailable");
    assert!(result.status.success(), "cannot change acceptance output");
    let value: serde_json::Value =
        serde_json::from_slice(&result.stdout).expect("invalid sway reply");
    assert!(
        value
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["success"] == true)
    );
}

#[test]
#[ignore = "requires the isolated headless Sway/portal fixture"]
fn wayland_selected_display_streams_changes_and_stops() {
    assert_eq!(
        std::env::var("MORPHEUS_WAYLAND_ACCEPTANCE").as_deref(),
        Ok("1")
    );
    assert!(std::env::var_os("DISPLAY").is_none(), "must not use X11");
    let displays = ScreenCapture::display_sources().expect("display portal capability");
    assert_eq!(displays.len(), 1);
    assert_eq!(displays[0].0, "portal:monitor");
    assert!(
        ScreenCapture::window_sources().unwrap().is_empty(),
        "this fixture uses monitor-only wlr 0.7.1"
    );
    assert!(
        ScreenCapture::open_key("display:1").is_err(),
        "Xwayland identity must not fall back"
    );
    assert!(
        ScreenCapture::open_key("portal:window").is_err(),
        "window request must not fall back to display"
    );

    for _ in 0..2 {
        let capture = ScreenCapture::open_key("portal:monitor").expect("portal display selection");
        let frame = capture
            .next_frame_sized(320, 180)
            .expect("first PipeWire frame");
        let (u, v) = center_chroma(&frame.buffer);
        assert!(
            v as i16 - u as i16 > 40,
            "selected red HEADLESS-2 was not captured"
        );
        let started = Instant::now();
        drop(capture);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "session shutdown exceeded stop budget"
        );
    }

    let capture = ScreenCapture::open_key("portal:monitor").expect("portal reopen");
    capture.next_frame_sized(320, 180).expect("initial frame");
    output(&["output", "HEADLESS-2", "bg", "#20f020", "solid_color"]);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let frame = capture
            .next_frame_sized(320, 180)
            .expect("continuous PipeWire frame");
        let (u, v) = center_chroma(&frame.buffer);
        if u < 110 && v < 110 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stream did not deliver changed selected-source pixels"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    output(&["output", "HEADLESS-2", "disable"]);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match capture.next_frame_sized(320, 180) {
            Err(_) => break,
            Ok(frame) => {
                let (u, v) = center_chroma(&frame.buffer);
                assert!(
                    u < 110 && v < 110,
                    "capture silently changed to another display"
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "lost source did not terminate capture"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(capture);
    output(&["output", "HEADLESS-2", "enable"]);

    let fixture_mode = std::path::PathBuf::from(
        std::env::var_os("MORPHEUS_WAYLAND_CHOOSER_MODE").expect("private chooser fixture"),
    );
    std::fs::write(&fixture_mode, "deny").unwrap();
    assert!(
        ScreenCapture::open_key("portal:monitor").is_err(),
        "denied portal must not capture"
    );
    std::fs::write(&fixture_mode, "delay").unwrap();
    let started = Instant::now();
    let result = ScreenCapture::open_key_cancellable("portal:monitor", &|| {
        started.elapsed() > Duration::from_millis(500)
    });
    assert!(result.is_err(), "cancelled selection must not capture");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "selection cancellation exceeded worker stop budget"
    );
    println!(
        "Wayland portal acceptance: selected display, continuous pixels, repeated start/stop, source departure, denial and cancellation passed; native-window/GNOME/KDE remain separate gates."
    );
}
