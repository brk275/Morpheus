//! Subject window for scripts/windows_window_probe.py.
//!
//! Shows one ordinary Slint window through the default backend selection the
//! app uses, as a GUI-subsystem program, then closes itself after a fixed
//! lifetime. An independent thread ends the process if closing hangs. It
//! uses no project crate and makes no file, network, account, profile or
//! credential-store calls of its own; Slint still loads system libraries and
//! fonts. The companion driver inspects this process's top-level windows from
//! outside, so this file makes no Win32 calls of its own. See
//! docs/WINDOWS_WINDOW_PROBE.md.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{process::ExitCode, sync::mpsc, thread, time::Duration};

use slint::ComponentHandle;

/// Longer than the driver's first-window wait plus its bounded snapshots.
const LIFETIME: Duration = Duration::from_secs(30);
/// Independent of the event loop: ends the process if closing hangs.
const WATCHDOG: Duration = Duration::from_secs(45);

slint::slint! {
    export component ProbeWindow inherits Window {
        title: "Morpheus window probe";
        preferred-width: 480px;
        preferred-height: 320px;
        background: darkslategray;

        Rectangle {
            width: 96px;
            height: 96px;
            border-radius: 16px;
            background: lightskyblue;
        }
    }
}

fn show_for_lifetime() -> Result<(), slint::PlatformError> {
    let window = ProbeWindow::new()?;
    let weak = window.as_weak();
    slint::Timer::single_shot(LIFETIME, move || {
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
        let _ = slint::quit_event_loop();
    });
    // Shows the window, runs the event loop until the timer closes it, hides.
    window.run()
}

fn main() -> ExitCode {
    let (done, finished) = mpsc::channel::<()>();
    let spawned = thread::Builder::new()
        .name("window-probe-watchdog".into())
        .spawn(move || {
            if matches!(
                finished.recv_timeout(WATCHDOG),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                std::process::exit(3);
            }
        });
    // Fixed exit codes only; the driver maps them to closed tokens.
    let Ok(watchdog) = spawned else {
        return ExitCode::from(4);
    };
    let shown = show_for_lifetime();
    drop(done);
    if watchdog.join().is_err() {
        return ExitCode::from(5);
    }
    match shown {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
