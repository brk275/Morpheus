//! Opt-in actual ScreenCast portal/PipeWire acceptance on disposable Sway.
//! The helper owns two differently colored outputs and a deterministic portal
//! chooser. Pixel data never leaves this process and is never written to disk.
#![cfg(all(target_os = "linux", feature = "device-screen"))]

use morpheus_rtc::{I420Buffer, ScreenCapture};
use std::{
    process::Command,
    time::{Duration, Instant},
};

fn stage(value: &'static str) {
    // Libtest may lose its captured output if a native callback aborts. This
    // private file records only fixed test phases, never pixels or device IDs.
    let path = std::env::var_os("MORPHEUS_WAYLAND_STAGE_PATH").expect("private fixture stage file");
    std::fs::write(path, value).expect("record private fixture phase");
}

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
    stage("capabilities");
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

    for iteration in 0..2 {
        stage(if iteration == 0 {
            "opening_display_1"
        } else {
            "opening_display_2"
        });
        let capture = ScreenCapture::open_key("portal:monitor").expect("portal display selection");
        stage(if iteration == 0 {
            "first_frame_1"
        } else {
            "first_frame_2"
        });
        let frame = capture
            .next_frame_sized(320, 180)
            .expect("first PipeWire frame");
        stage(if iteration == 0 {
            "checking_pixels_1"
        } else {
            "checking_pixels_2"
        });
        let (u, v) = center_chroma(&frame.buffer);
        assert!(
            v as i16 - u as i16 > 40,
            "selected red HEADLESS-2 was not captured"
        );
        let started = Instant::now();
        stage(if iteration == 0 {
            "stopping_display_1"
        } else {
            "stopping_display_2"
        });
        drop(capture);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "session shutdown exceeded stop budget"
        );
    }

    stage("reopening_display");
    let capture = ScreenCapture::open_key("portal:monitor").expect("portal reopen");
    stage("reopen_frame");
    capture.next_frame_sized(320, 180).expect("initial frame");
    stage("changing_source");
    output(&["output", "HEADLESS-2", "bg", "#20f020", "solid_color"]);
    let deadline = Instant::now() + Duration::from_secs(8);
    stage("awaiting_changed_frame");
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
    stage("removing_source");
    output(&["output", "HEADLESS-2", "disable"]);
    let deadline = Instant::now() + Duration::from_secs(8);
    stage("awaiting_source_loss");
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
    stage("stopping_lost_source");
    drop(capture);
    stage("restoring_source");
    output(&["output", "HEADLESS-2", "enable"]);

    let fixture_mode = std::path::PathBuf::from(
        std::env::var_os("MORPHEUS_WAYLAND_CHOOSER_MODE").expect("private chooser fixture"),
    );
    std::fs::write(&fixture_mode, "deny").unwrap();
    stage("denying_selection");
    assert!(
        ScreenCapture::open_key("portal:monitor").is_err(),
        "denied portal must not capture"
    );
    std::fs::write(&fixture_mode, "delay").unwrap();
    stage("cancelling_selection");
    let started = Instant::now();
    let result = ScreenCapture::open_key_cancellable("portal:monitor", &|| {
        started.elapsed() > Duration::from_millis(500)
    });
    assert!(result.is_err(), "cancelled selection must not capture");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "selection cancellation exceeded worker stop budget"
    );
    stage("complete");
    println!(
        "Wayland portal acceptance: selected display, continuous pixels, repeated start/stop, source departure, denial and cancellation passed; native-window/GNOME/KDE remain separate gates."
    );
}
