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

use morpheus_config::{BuiltInStylePreset, ConversationHeaderMode, NavigationMode};
use slint::ComponentHandle;

use super::{AppWindow, Config, apply_config, apply_state, fixtures};

const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;
const FILES: [&str; 14] = [
    "desktop-preview.ppm",
    "account-switcher.ppm",
    "sign-in.ppm",
    "incoming-call.ppm",
    "room-settings.ppm",
    "paper-preview.ppm",
    "manage-accounts.ppm",
    "account-password.ppm",
    "account-password-large.ppm",
    "sign-in-large.ppm",
    "terminal-sidebar.ppm",
    "terminal-space-labels.ppm",
    "send-recovery.ppm",
    "send-recovery-thread-large.ppm",
];

/// Presentation-only rows for the saved-send status strips. The keys are
/// fictional and no callback that could act on them is registered here.
fn recovery_items(kept: bool) -> slint::ModelRc<super::PendingSendItem> {
    let mut items = vec![
        super::PendingSendItem {
            key: "preview-waiting".into(),
            status: "Waiting to send".into(),
            preview: "Notes from the workshop are in the shared folder".into(),
            can_cancel: true,
            can_retry: false,
            unconfirmed: false,
            kept: false,
        },
        super::PendingSendItem {
            key: "preview-paused".into(),
            status: "Sending is paused".into(),
            preview: "Running ten minutes late, start without me".into(),
            can_cancel: true,
            can_retry: true,
            unconfirmed: false,
            kept: false,
        },
        super::PendingSendItem {
            key: "preview-unconfirmed".into(),
            status: "May not have finished sending".into(),
            preview: "Thanks everyone, see you next week".into(),
            can_cancel: false,
            can_retry: false,
            unconfirmed: true,
            kept: false,
        },
    ];
    if kept {
        items.push(super::PendingSendItem {
            key: "preview-kept".into(),
            status: "Unsent text kept".into(),
            preview: "Draft of the agenda I decided not to send yet".into(),
            can_cancel: false,
            can_retry: false,
            unconfirmed: false,
            kept: true,
        });
    }
    slint::ModelRc::new(slint::VecModel::from(items))
}

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
    let mut terminal_sidebar = terminal.clone();
    terminal_sidebar.layout.navigation = NavigationMode::Sidebar;
    terminal_sidebar.layout.group_direct_messages = false;
    terminal_sidebar.layout.sort_unread_first = true;
    let mut terminal_labels = terminal_sidebar.clone();
    terminal_labels.layout.navigation = NavigationMode::Rail;
    terminal_labels.layout.show_space_labels = true;
    let mut large_sign_in = nocturne.clone();
    large_sign_in.appearance.scale_percent = 200;
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
                            ..Default::default()
                        },
                        super::SavedAccountItem {
                            id: "preview-personal".into(),
                            handle: "@river:example.net".into(),
                            active: false,
                            removal_pending: false,
                            ..Default::default()
                        },
                        super::SavedAccountItem {
                            id: "preview-community".into(),
                            handle: "@workshop:example.org".into(),
                            active: false,
                            removal_pending: true,
                            ..Default::default()
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
                8 => {
                    // The next frame is the actual large-text sign-in form,
                    // without a saved-account shortcut or another modal over it.
                    reset_capture_overlays(&window);
                    window.set_saved_accounts(slint::ModelRc::new(slint::VecModel::from(Vec::<
                        super::SavedAccountItem,
                    >::new(
                    ))));
                    window.set_app_password_enabled(false);
                    window.set_live_session(false);
                    window.set_preview_session(false);
                    window.set_active_account_local_id("".into());
                    window.set_login_loading(false);
                    window.set_login_error("".into());
                    window.set_login_homeserver("https://matrix.org".into());
                    apply_config(&window, &large_sign_in, Some(&nocturne_theme));
                    window.set_show_sign_in(true);
                }
                9 => {
                    // Rebuild only the same in-memory Home fixtures. The Space
                    // names below are presentation rows, not live membership.
                    reset_capture_overlays(&window);
                    window.set_show_sign_in(false);
                    apply_state(&window, &fixtures());
                    window.set_active_space_id("".into());
                    window.set_active_space_name("Home".into());
                    window.set_spaces(slint::ModelRc::new(slint::VecModel::from(vec![
                        super::SpaceItem {
                            id: "!preview-workshop:example.org".into(),
                            name: "Workshop".into(),
                            initials: "W".into(),
                            accent: super::rgb(109, 180, 156),
                        },
                        super::SpaceItem {
                            id: "!preview-study:example.org".into(),
                            name: "Quiet Study and Shared Notes".into(),
                            initials: "QS".into(),
                            accent: super::rgb(135, 157, 208),
                        },
                    ])));
                    apply_config(&window, &terminal_sidebar, Some(&terminal_theme));
                    window
                        .set_native_call_status("Select a room to check call availability".into());
                }
                10 => {
                    // Same window, Home rooms and fictional Spaces; just change
                    // navigation to the labeled rail through real configuration.
                    reset_capture_overlays(&window);
                    apply_config(&window, &terminal_labels, Some(&terminal_theme));
                }
                11 => {
                    // The room composer's saved-send strip with every record
                    // state, in the wide layout: fictional rows, no account,
                    // no queue and no callback behind the controls.
                    reset_capture_overlays(&window);
                    apply_config(&window, &nocturne, Some(&nocturne_theme));
                    apply_state(&window, &fixtures());
                    window.set_live_session(true);
                    window.set_composer_text("".into());
                    window.set_composer_pending_count(2);
                    window.set_composer_pending_blocked(true);
                    window.set_composer_unconfirmed(true);
                    window.set_composer_pending_items(recovery_items(true));
                    window.set_composer_blocked_notice("".into());
                }
                12 => {
                    // The same strip in the narrow thread pane at 200% text,
                    // where the controls must stack instead of clipping, plus
                    // the fixed notice for saved data this version cannot read.
                    window.set_composer_pending_items(slint::ModelRc::new(slint::VecModel::from(
                        Vec::<super::PendingSendItem>::new(),
                    )));
                    window.set_composer_pending_count(0);
                    window.set_composer_pending_blocked(false);
                    window.set_composer_unconfirmed(false);
                    window.set_font_scale_percent(200);
                    window.invoke_show_thread_panel();
                    window.set_thread_root_id("$preview-thread-root".into());
                    window.set_thread_root_sender("@jules:example.org".into());
                    window.set_thread_root_body("Can someone post the agenda before Thursday?".into());
                    window.set_thread_composer_text("".into());
                    window.set_thread_send_pending(true);
                    window.set_thread_pending_blocked(true);
                    window.set_thread_unconfirmed(true);
                    window.set_thread_pending_items(recovery_items(false));
                    window.set_thread_blocked_notice(
                        "Saved send status here could not be read by this version. It is kept with your drafts; sending here waits until you review it.".into(),
                    );
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

// These are UI properties only. early_dispatch never registers callbacks that
// can sign in, start a call, change accounts, or read/write saved credentials.
fn reset_capture_overlays(ui: &AppWindow) {
    ui.set_access_open(false);
    ui.set_manage_accounts_open(false);
    ui.set_account_menu_open(false);
    ui.set_room_admin_open(false);
    ui.set_room_actions_open(false);
    ui.set_member_confirm_open(false);
    ui.set_native_capture_picker_open(false);
    ui.set_leave_call_dialog(false);
    ui.set_quick_switcher_open(false);
    ui.set_people_open(false);
    ui.set_image_preview_open(false);
    ui.set_poll_dialog_open(false);
    ui.set_thread_root_id("".into());
    ui.set_thread_send_pending(false);
    ui.set_thread_pending_blocked(false);
    ui.set_thread_unconfirmed(false);
    ui.set_thread_blocked_notice("".into());
    ui.set_thread_pending_items(slint::ModelRc::new(slint::VecModel::from(Vec::<
        super::PendingSendItem,
    >::new())));
    ui.set_composer_pending_items(slint::ModelRc::new(slint::VecModel::from(Vec::<
        super::PendingSendItem,
    >::new())));
    ui.set_composer_blocked_notice("".into());
    ui.set_access_current_password("".into());
    ui.set_access_new_password("".into());
    ui.set_access_confirm_password("".into());
    ui.set_access_target("".into());
    ui.set_access_error("".into());
    ui.set_access_busy(false);
    ui.set_account_access_notice("".into());
    ui.set_session_error("".into());
    ui.set_incoming_call_notice("".into());
    ui.set_incoming_call_account_id("".into());
    ui.set_incoming_call_offers(slint::ModelRc::new(slint::VecModel::from(Vec::<
        super::IncomingCallItem,
    >::new())));
    ui.set_page(0);
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
