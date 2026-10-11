//! Explicit hosted-only Windows GDI capture of an owned synthetic window.
//! Pixel buffers remain in memory; no screenshots, accounts or RTC service.
#![cfg(all(target_os = "windows", feature = "device-screen"))]

use morpheus_rtc::{I420Buffer, ScreenCapture};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    child: Child,
}

impl Fixture {
    fn stage(&self, stage: &str) {
        fs::write(self.root.join("stage"), stage).expect("record capture phase");
    }

    fn wait_ack(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.child
                    .try_wait()
                    .expect("inspect fixture process")
                    .is_none(),
                "fixture window exited early"
            );
            if fs::read_to_string(self.root.join("ack")).ok().as_deref() == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture acknowledgement timed out"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn command(&mut self, value: &str) {
        fs::write(self.root.join("command"), value).expect("send fixture command");
        if value != "close" {
            self.wait_ack(value);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The outer Windows Job owns the entire tree even if this test aborts.
        // Child::kill acts through its retained process handle, never a PID.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn colors(frame: &I420Buffer, swapped: bool) -> bool {
    if frame.width != 320 || frame.height != 180 {
        return false;
    }
    let row = frame.height as usize / 4;
    let left = frame.width as usize * 3 / 16;
    let right = frame.width as usize * 5 / 16;
    let delta = |column| {
        frame.data_v[row * frame.stride_v as usize + column] as i16
            - frame.data_u[row * frame.stride_u as usize + column] as i16
    };
    if swapped {
        delta(left) < -80 && delta(right) > 80
    } else {
        delta(left) > 80 && delta(right) < -80
    }
}

fn assert_frames(capture: &ScreenCapture, swapped: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut consecutive = 0;
    loop {
        let frame = capture
            .next_frame_sized(320, 180)
            .expect("capture owned synthetic source");
        consecutive = if colors(&frame.buffer, swapped) {
            consecutive + 1
        } else {
            0
        };
        if consecutive == 3 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "synthetic source pixels did not match"
        );
        std::thread::sleep(Duration::from_millis(80));
    }
}

#[test]
#[ignore = "requires explicit disposable GitHub-hosted Windows fixture"]
fn windows_selected_sources_change_and_stop() {
    for (key, expected) in [
        ("MORPHEUS_WINDOWS_CAPTURE_ACCEPTANCE", "1"),
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Windows"),
    ] {
        assert!(
            std::env::var(key).as_deref() == Ok(expected),
            "hosted Windows capture guard rejected"
        );
    }
    let root = PathBuf::from(
        std::env::var_os("MORPHEUS_WINDOWS_CAPTURE_ROOT").expect("private fixture directory"),
    );
    let nonce = std::env::var("MORPHEUS_WINDOWS_CAPTURE_NONCE").expect("fixture nonce");
    assert!(
        nonce.len() == 32
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/windows_capture_window.ps1");
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system directory"))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let child = Command::new(powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(script)
        .arg("-FixtureRoot")
        .arg(&root)
        .arg("-Nonce")
        .arg(&nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch synthetic fixture window");
    let mut fixture = Fixture { root, child };
    fixture.stage("window_ready");
    fixture.wait_ack("ready");
    let title = format!("Morpheus capture QA {nonce}");
    let windows: Vec<_> = xcap::Window::all()
        .expect("enumerate synthetic window")
        .into_iter()
        .filter(|window| {
            window.pid().ok() == Some(fixture.child.id())
                && window.title().ok().as_deref() == Some(title.as_str())
        })
        .collect();
    assert!(
        windows.len() == 1,
        "owned nonce window is missing or ambiguous"
    );
    let native_window = &windows[0];
    let label = format!(
        "{} — {title}",
        native_window
            .app_name()
            .expect("identify fixture application")
    );
    let keys: Vec<_> = ScreenCapture::window_sources()
        .expect("enumerate capture sources")
        .into_iter()
        .filter(|(_, candidate)| candidate == &label)
        .map(|(key, _)| key)
        .collect();
    assert!(
        keys.len() == 1,
        "fixture source key is missing or ambiguous"
    );
    let key = &keys[0];
    assert!(
        xcap::Monitor::all()
            .expect("enumerate fixture monitors")
            .len()
            == 1,
        "fixture requires one isolated display"
    );
    let monitor = native_window
        .current_monitor()
        .expect("resolve owned window monitor");
    assert!(
        native_window.x().expect("fixture horizontal position")
            == monitor.x().expect("display horizontal position")
            && native_window.y().expect("fixture vertical position")
                == monitor.y().expect("display vertical position")
            && native_window.width().expect("fixture width")
                == monitor.width().expect("display width")
            && native_window.height().expect("fixture height")
                == monitor.height().expect("display height"),
        "owned fixture does not cover the full selected display"
    );

    fixture.stage("window_frames");
    let capture = ScreenCapture::open_key(key).expect("open owned window key");
    assert_frames(&capture, false);
    fixture.stage("display_frames");
    let display = ScreenCapture::open_display_id(monitor.id().expect("identify fixture display"))
        .expect("open covered fixture display");
    assert_frames(&display, false);
    fixture.stage("changed_pixels");
    fixture.command("swap");
    assert_frames(&capture, true);
    assert_frames(&display, true);
    assert!(
        capture
            .next_frame_sized_cancellable(320, 180, &|| true)
            .is_err(),
        "cancelled window capture continued"
    );
    assert!(
        display
            .next_frame_sized_cancellable(320, 180, &|| true)
            .is_err(),
        "cancelled display capture continued"
    );
    drop(display);
    drop(capture);
    fixture.stage("reopen");
    let capture = ScreenCapture::open_key(key).expect("reopen same owned source");
    assert_frames(&capture, true);
    let display = ScreenCapture::open_display_id(monitor.id().expect("identify fixture display"))
        .expect("reopen covered display");
    assert_frames(&display, true);
    drop(display);
    fixture.stage("retitle_rejection");
    fixture.command("retitle");
    assert!(
        capture.next_frame_sized(320, 180).is_err(),
        "renamed source continued capture"
    );
    assert!(
        ScreenCapture::open_key(key).is_err(),
        "stale source key reopened after retitle"
    );
    drop(capture);
    let changed_label = format!("{label} changed");
    let changed_keys: Vec<_> = ScreenCapture::window_sources()
        .expect("enumerate retitled source")
        .into_iter()
        .filter(|(_, candidate)| candidate == &changed_label)
        .map(|(key, _)| key)
        .collect();
    assert!(
        changed_keys.len() == 1,
        "retitled fixture is missing or ambiguous"
    );
    let changed_key = &changed_keys[0];
    let capture =
        ScreenCapture::open_key(changed_key).expect("explicitly reselect retitled fixture");
    assert_frames(&capture, true);
    fixture.stage("closed_rejection");
    fixture.command("close");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = fixture.child.try_wait().expect("inspect closing fixture") {
            assert!(status.success(), "fixture window shutdown failed");
            break;
        }
        assert!(Instant::now() < deadline, "fixture window did not close");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        capture.next_frame_sized(320, 180).is_err(),
        "closed source continued capture"
    );
    assert!(
        ScreenCapture::open_key(changed_key).is_err(),
        "closed source key reopened"
    );
    fixture.stage("complete");
}
