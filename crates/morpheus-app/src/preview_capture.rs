//! Capture the real Slint window with synthetic, in-memory preview data.
//!
//! Debug builds only. This dispatch runs before any normal app initialization:
//! no saved configuration, accounts, credentials, updater or network services.
//! The PPM files contain the renderer's pixels without retouching; the hosted
//! helper losslessly encodes them as PNG and records their provenance.

use std::{
    cell::{Cell, RefCell},
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use slint::ComponentHandle;

use super::{AppWindow, Config, apply_config, apply_state, fixtures};

const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;
const FILES: [&str; 5] = [
    "desktop-preview.ppm",
    "account-switcher.ppm",
    "sign-in.ppm",
    "incoming-call.ppm",
    "room-settings.ppm",
];

pub(super) fn early_dispatch() -> Result<bool, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--morpheus-capture-preview")) {
        return Ok(false);
    }
    let output = PathBuf::from(
        args.next()
            .ok_or("An absolute screenshot directory is required")?,
    );
    if !output.is_absolute() || args.next().is_some() {
        return Err("Usage: --morpheus-capture-preview ABSOLUTE_NEW_DIRECTORY".into());
    }
    // Never overwrite existing screenshots or follow an existing output link.
    fs::create_dir(&output)?;
    let ui = AppWindow::new()?;
    apply_config(&ui, &Config::default(), None);
    apply_state(&ui, &fixtures());
    ui.set_native_call_enabled(cfg!(feature = "native-calls"));
    ui.set_native_call_status("Select a room to check call availability".into());
    ui.set_show_side_panel(true);
    ui.set_show_sign_in(false);
    ui.set_session_error("".into());
    ui.show()?;
    ui.window()
        .set_size(slint::PhysicalSize::new(WIDTH, HEIGHT));

    // Capture after the real window has completed layout/rendering. Advance
    // only in-memory preview properties needed for the next screenshot.
    let phase = Rc::new(Cell::new(0_usize));
    let outcome = Rc::new(RefCell::new(None));
    let captured = Rc::clone(&outcome);
    let window = ui.clone_strong();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(400),
        move || {
            if captured.borrow().is_some() {
                return;
            }
            let index = phase.get();
            if let Err(error) = save_snapshot(&window, &output.join(FILES[index])) {
                *captured.borrow_mut() = Some(Err(error.to_string()));
                let _ = slint::quit_event_loop();
                return;
            }
            match index {
                0 => window.set_account_menu_open(true),
                1 => {
                    window.set_account_menu_open(false);
                    window.set_show_sign_in(true);
                    window.set_session_error("".into());
                }
                2 => {
                    // Render the actual incoming banner with fictional rows.
                    // These are presentation fixtures only: early_dispatch
                    // never starts an account, actor, network or media device.
                    window.set_show_sign_in(false);
                    window.set_live_session(true);
                    window.set_active_account_local_id("preview-casey".into());
                    window.set_incoming_call_account_id("preview-casey".into());
                    window.set_session_error("".into());
                    window.set_incoming_call_offers(slint::ModelRc::new(slint::VecModel::from(
                        vec![super::IncomingCallItem {
                            token: "preview-offer".into(),
                            caller: "@jules:example.org".into(),
                            room: "!workshop:example.org".into(),
                            account: "@casey:example.org".into(),
                        }],
                    )));
                }
                3 => {
                    // Presentation-only settings fixture: no controller, file
                    // chooser, account session, upload or state write exists.
                    window.set_incoming_call_offers(slint::ModelRc::new(slint::VecModel::from(
                        Vec::<super::IncomingCallItem>::new(),
                    )));
                    window.set_room_admin_mode(0);
                    window.set_room_admin_name_input("The Workshop".into());
                    window.set_room_admin_topic_input(
                        "A place to make things, share progress, and help each other.".into(),
                    );
                    window.set_room_settings_loaded(true);
                    window.set_room_settings_name_allowed(true);
                    window.set_room_settings_topic_allowed(true);
                    window.set_room_settings_avatar_allowed(true);
                    window.set_room_settings_join_allowed(true);
                    window.set_room_settings_join_options(slint::ModelRc::new(
                        slint::VecModel::from(vec![
                            "Invite only".into(),
                            "Public".into(),
                            "Knock".into(),
                            "Restricted".into(),
                            "Knock or restricted".into(),
                        ]),
                    ));
                    window.set_room_settings_join_index(0);
                    window.set_room_settings_access_description(
                        "Only invited people can join.".into(),
                    );
                    window.set_room_admin_open(true);
                }
                _ => {
                    *captured.borrow_mut() = Some(Ok(()));
                    let _ = slint::quit_event_loop();
                }
            }
            phase.set(index + 1);
        },
    );
    slint::run_event_loop()?;
    timer.stop();
    ui.hide()?;
    let result = outcome
        .borrow_mut()
        .take()
        .ok_or("The preview window closed before screenshot capture completed")?;
    result.map_err(|error: String| -> Box<dyn std::error::Error> { error.into() })?;
    Ok(true)
}

fn save_snapshot(ui: &AppWindow, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let pixels = ui.window().take_snapshot()?;
    if pixels.width() != WIDTH || pixels.height() != HEIGHT {
        return Err("The preview renderer returned an unexpected screenshot size".into());
    }
    let rgba = pixels.as_bytes();
    if rgba.chunks_exact(4).any(|pixel| pixel[3] != 255) {
        return Err("The preview renderer returned transparent pixels".into());
    }
    let mut file = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(path)?);
    write!(file, "P6\n{WIDTH} {HEIGHT}\n255\n")?;
    for pixel in rgba.chunks_exact(4) {
        file.write_all(&pixel[..3])?;
    }
    file.flush()?;
    Ok(())
}
