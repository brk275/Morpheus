//! Opt-in actual ScreenCast portal/PipeWire acceptance on disposable Sway.
//! The helper owns two differently colored outputs and a deterministic portal
//! chooser. Pixel data never leaves this process and is never written to disk.
#![cfg(all(target_os = "linux", feature = "device-screen"))]

#[path = "../../../packaging/linux/appimage_peer.rs"]
mod appimage_peer;

#[path = "support/pipewire_probe.rs"]
mod pipewire_probe;

use morpheus_rtc::{I420Buffer, ScreenCapture};
use std::{
    ffi::{CStr, c_void},
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum BindingOrigin {
    SystemPipeWire = 0,
    Executable = 1,
    Other = 2,
    Unavailable = 3,
}

fn binding_origin(address: *const c_void) -> BindingOrigin {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // These are linked function addresses, never calls through private ABI.
    if unsafe { libc::dladdr(address, info.as_mut_ptr()) } == 0 {
        return BindingOrigin::Unavailable;
    }
    let info = unsafe { info.assume_init() };
    if info.dli_fname.is_null() {
        return BindingOrigin::Unavailable;
    }
    let Ok(name) = (unsafe { CStr::from_ptr(info.dli_fname) }).to_str() else {
        return BindingOrigin::Unavailable;
    };
    let Ok(path) = std::fs::canonicalize(name) else {
        return BindingOrigin::Unavailable;
    };
    if std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .as_ref()
        == Some(&path)
    {
        return BindingOrigin::Executable;
    }
    let expected_directory = Path::new("/usr/lib/x86_64-linux-gnu");
    let expected_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| {
            name.strip_prefix("libpipewire-0.3.so.")
                .is_some_and(|version| {
                    !version.is_empty()
                        && version
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || byte == b'.')
                })
        });
    // The free Ubuntu fixture uses the distribution's ordinary root-owned
    // library. Do not accept a similarly named library in a build/temp path.
    let trusted_file = std::fs::metadata(&path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
    });
    if path.parent() == Some(expected_directory) && expected_name && trusted_file {
        BindingOrigin::SystemPipeWire
    } else {
        BindingOrigin::Other
    }
}

fn pipewire_addresses() -> [*const c_void; 33] {
    use pipewire::sys as pw;
    macro_rules! addresses {
        ($($name:ident),+ $(,)?) => { [$(pw::$name as *const () as *const c_void),+] };
    }
    // Complete collision inventory, checked against the pinned archive map
    // by the portable fixture tests. All resolutions precede pw_init.
    let addresses = addresses![
        pw_context_connect,
        pw_context_connect_fd,
        pw_context_destroy,
        pw_context_new,
        pw_core_disconnect,
        pw_deinit,
        pw_get_library_version,
        pw_init,
        pw_loop_destroy,
        pw_loop_new,
        pw_properties_new_string,
        pw_proxy_destroy,
        pw_stream_add_listener,
        pw_stream_connect,
        pw_stream_dequeue_buffer,
        pw_stream_destroy,
        pw_stream_disconnect,
        pw_stream_get_node_id,
        pw_stream_get_state,
        pw_stream_new,
        pw_stream_queue_buffer,
        pw_stream_set_active,
        pw_stream_state_as_string,
        pw_stream_update_params,
        pw_thread_loop_destroy,
        pw_thread_loop_get_loop,
        pw_thread_loop_lock,
        pw_thread_loop_new,
        pw_thread_loop_signal,
        pw_thread_loop_start,
        pw_thread_loop_stop,
        pw_thread_loop_unlock,
        pw_thread_loop_wait,
    ];
    addresses
}

fn verify_pipewire_bindings() {
    let addresses = pipewire_addresses();
    let mut counts = [0_u32; 4];
    for address in addresses {
        counts[binding_origin(address) as usize] += 1;
    }
    let facts = serde_json::json!({
        "checked": addresses.len(), "system_pipewire": counts[0],
        "executable": counts[1], "other": counts[2], "unavailable": counts[3],
    });
    let path =
        std::env::var_os("MORPHEUS_WAYLAND_BINDING_PATH").expect("private binding facts file");
    std::fs::write(path, serde_json::to_vec(&facts).unwrap())
        .expect("record private binding facts");
    assert_eq!(
        counts,
        [33, 0, 0, 0],
        "PipeWire function origin was not the expected system library"
    );
}

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

struct OutputCommand(Child);

impl Drop for OutputCommand {
    fn drop(&mut self) {
        // No other code reaps this child. Child retains its exit status after
        // try_wait, so kill cannot target a reused PID after observed exit.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn change_output(arguments: &[&str]) -> Result<(), &'static str> {
    // Sway 1.9's quiet mode still returns a failure status for an unsuccessful
    // IPC command. Do not collect arbitrary output or inherit a terminal.
    let mut command = OutputCommand(
        Command::new("/usr/bin/swaymsg")
            .arg("--quiet")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "fixture output command could not start")?,
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = command
            .0
            .try_wait()
            .map_err(|_| "fixture output command wait failed")?
        {
            return if status.success() {
                Ok(())
            } else {
                Err("fixture output command failed")
            };
        }
        if Instant::now() >= deadline {
            return Err("fixture output command deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn output(arguments: &[&str]) {
    change_output(arguments).expect("cannot change acceptance output");
}

struct SourceAnimation {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<Result<(), &'static str>>>,
}

impl SourceAnimation {
    fn start(colors: [&'static str; 2]) -> Self {
        let (stop, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut index = 0;
            loop {
                // xdpw 0.7.1 uses copy_with_damage on a shared screencopy
                // manager. Keep supplying owned damage during negotiation;
                // a single transition can be consumed by a closing session.
                change_output(&["output", "HEADLESS-2", "bg", colors[index], "solid_color"])?;
                match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if Instant::now() >= deadline {
                    return Err("fixture pixel animation deadline");
                }
                index ^= 1;
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn stop(&mut self) -> Result<(), &'static str> {
        let _ = self.stop.send(());
        self.worker.take().map_or(Ok(()), |worker| {
            worker
                .join()
                .map_err(|_| "fixture pixel animation panicked")?
        })
    }
}

impl Drop for SourceAnimation {
    fn drop(&mut self) {
        // Also stop and join when the capture assertion unwinds. Each owned
        // swaymsg command has a deadline and is killed/reaped on error.
        let _ = self.stop();
    }
}

#[derive(Clone, Copy)]
enum ExpectedColor {
    Green,
    Yellow,
}

fn wait_for_selected_pixels(capture: &ScreenCapture, color: ExpectedColor) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frame = capture
            .next_frame_sized(320, 180)
            .expect("first PipeWire frame");
        let (u, v) = center_chroma(&frame.buffer);
        let matches = match color {
            ExpectedColor::Green => u < 110 && v < 110,
            ExpectedColor::Yellow => u < 85 && (115..155).contains(&v),
        };
        if matches {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "selected display did not deliver fresh expected pixels"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "requires the isolated headless Sway/portal fixture"]
fn wayland_selected_display_streams_changes_and_stops() {
    assert_eq!(
        std::env::var("MORPHEUS_WAYLAND_ACCEPTANCE").as_deref(),
        Ok("1")
    );
    assert!(std::env::var_os("DISPLAY").is_none(), "must not use X11");
    stage("pipewire_binding_origin");
    verify_pipewire_bindings();
    capture_scenario();
}

#[test]
#[ignore = "requires the explicitly selected hosted private-client diagnostic"]
fn wayland_private_client_startup_diagnostic() {
    assert_eq!(
        std::env::var("MORPHEUS_WAYLAND_ACCEPTANCE").as_deref(),
        Ok("1")
    );
    assert!(std::env::var_os("DISPLAY").is_none(), "must not use X11");
    stage("pipewire_binding_origin");
    pipewire_probe::verify(&pipewire_addresses()).expect("private client provenance rejected");
    capture_scenario();
}

// Both entries exercise identical capture assertions. Only the ordinary
// system-library entry can supply the required Wayland acceptance result.
fn capture_scenario() {
    stage("pipewire_initializer");
    // Invoke the same public initializer used by PortalCapture only after the
    // origin check. Its internal Once keeps normal capture initialization safe.
    pipewire::init();
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
        if iteration == 0 {
            let frame = capture
                .next_frame_sized(320, 180)
                .expect("first PipeWire frame");
            let (u, v) = center_chroma(&frame.buffer);
            assert!(
                v as i16 - u as i16 > 40,
                "selected red HEADLESS-2 was not captured"
            );
        } else {
            let mut animation = SourceAnimation::start(["#20f020", "#10d010"]);
            wait_for_selected_pixels(&capture, ExpectedColor::Green);
            // A failed animator must not be accepted merely because an older
            // frame happened to match. Check the joined worker before success.
            animation.stop().expect("fixture pixel animation failed");
        }
        stage(if iteration == 0 {
            "checking_pixels_1"
        } else {
            "checking_pixels_2"
        });
        // The stop budget measures the native session, after the fixture
        // animation has stopped and joined.
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
    let mut animation = SourceAnimation::start(["#f0f020", "#d0d010"]);
    wait_for_selected_pixels(&capture, ExpectedColor::Yellow);
    animation.stop().expect("fixture pixel animation failed");
    stage("changing_source");
    output(&["output", "HEADLESS-2", "bg", "#f020f0", "solid_color"]);
    let deadline = Instant::now() + Duration::from_secs(8);
    stage("awaiting_changed_frame");
    loop {
        let frame = capture
            .next_frame_sized(320, 180)
            .expect("continuous PipeWire frame");
        let (u, v) = center_chroma(&frame.buffer);
        if u > 160 && v > 160 {
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
                    u > 160 && v > 160,
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
}
