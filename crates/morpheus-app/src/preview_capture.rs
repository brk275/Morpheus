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

use morpheus_config::{BuiltInStylePreset, ConversationHeaderMode};
use slint::ComponentHandle;

use super::{AppWindow, Config, apply_config, apply_state, fixtures};

const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;
const FILES: [&str; 9] = [
    "desktop-preview.ppm",
    "account-switcher.ppm",
    "sign-in.ppm",
    "incoming-call.ppm",
    "room-settings.ppm",
    "paper-preview.ppm",
    "manage-accounts.ppm",
    "account-password.ppm",
    "account-password-large.ppm",
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
    let mut nocturne = Config::default();
    nocturne.apply_style_preset(BuiltInStylePreset::Nocturne);
    let nocturne_theme = BuiltInStylePreset::Nocturne.theme()?;
    let mut terminal = Config::default();
    terminal.apply_style_preset(BuiltInStylePreset::Terminal);
    terminal.conversation.header = ConversationHeaderMode::Minimal;
    let terminal_theme = BuiltInStylePreset::Terminal.theme()?;
    let mut paper = Config::default();
    paper.apply_style_preset(BuiltInStylePreset::Paper);
    paper.conversation.header = ConversationHeaderMode::Compact;
    let paper_theme = BuiltInStylePreset::Paper.theme()?;
    apply_config(&ui, &nocturne, Some(&nocturne_theme));
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
                0 => {
                    // Same native window and fictional data: show a genuinely
                    // different preset without touching saved preferences.
                    apply_config(&window, &terminal, Some(&terminal_theme));
                    window.set_account_menu_open(true);
                }
                1 => {
                    window.set_account_menu_open(false);
                    apply_config(&window, &nocturne, Some(&nocturne_theme));
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
                4 => {
                    window.set_room_admin_open(false);
                    window.set_live_session(false);
                    apply_config(&window, &paper, Some(&paper_theme));
                }
                5 => {
                    apply_config(&window, &nocturne, Some(&nocturne_theme));
                    // Presentation only: these IDs never enter AccountManager.
                    window.set_saved_accounts(slint::ModelRc::new(slint::VecModel::from(vec![
                        super::SavedAccountItem {
                            id: "preview-casey".into(),
                            handle: "@casey:example.org".into(),
                            active: true,
                            removal_pending: false,
                        },
                        super::SavedAccountItem {
                            id: "preview-personal".into(),
                            handle: "@river:example.net".into(),
                            active: false,
                            removal_pending: false,
                        },
                        super::SavedAccountItem {
                            id: "preview-community".into(),
                            handle: "@workshop:example.org".into(),
                            active: false,
                            removal_pending: true,
                        },
                    ])));
                    window.set_app_password_enabled(true);
                    window.set_account_access_notice("".into());
                    window.set_manage_accounts_open(true);
                }
                6 => {
                    // Real dialog layout, with empty password properties and
                    // no callbacks capable of touching credentials or policy.
                    window.set_access_mode(2);
                    window.set_access_target("".into());
                    window.set_access_current_password("".into());
                    window.set_access_new_password("".into());
                    window.set_access_confirm_password("".into());
                    window.set_access_error("".into());
                    window.set_access_busy(false);
                    window.set_access_open(true);
                }
                7 => {
                    window.set_font_scale_percent(200);
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
