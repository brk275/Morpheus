//! Native desktop shell with a live Matrix session and an explicit offline preview.
#![recursion_limit = "512"]

#[cfg(feature = "native-calls")]
mod calls;
mod live;
mod sso;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, atomic::Ordering};
use std::thread;
use std::time::Duration;

use directories::ProjectDirs;
use morpheus_config::{Config, ConfigStore, Density, LoadedConfig, ThemeMode, ThemePack};
use morpheus_matrix::AccountManager;
use slint::{Color, Model, ModelRc, SharedString, VecModel};

#[cfg(feature = "native-calls")]
use crate::calls::{CallController, CallStatus};
use crate::live::{LiveUi, MessageTarget, show_error};

slint::include_modules!();

#[derive(Clone)]
struct PreviewRoom {
    item: RoomItem,
    topic: SharedString,
    messages: Vec<MessageItem>,
}

#[derive(Clone)]
struct PreviewAccount {
    item: AccountItem,
    channels: Vec<PreviewRoom>,
    directs: Vec<PreviewRoom>,
    members: Vec<MemberItem>,
}

struct PreviewState {
    accounts: Vec<PreviewAccount>,
    active_account: usize,
    active_room: String,
    stress_backup: Option<Vec<MessageItem>>,
    thread_replies: HashMap<(usize, String, String), Vec<MessageItem>>,
}

fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::from_rgb_u8(red, green, blue)
}

fn account(name: &str, handle: &str, initials: &str, status: &str, accent: Color) -> AccountItem {
    AccountItem {
        name: name.into(),
        handle: handle.into(),
        initials: initials.into(),
        status: status.into(),
        accent,
    }
}

fn room(
    id: &str,
    name: &str,
    unread: i32,
    private_room: bool,
    topic: &str,
    messages: Vec<MessageItem>,
) -> PreviewRoom {
    PreviewRoom {
        item: RoomItem {
            id: id.into(),
            name: name.into(),
            unread,
            private_room,
        },
        topic: topic.into(),
        messages,
    }
}

fn message(
    sender: &str,
    initials: &str,
    time: &str,
    body: &str,
    reply_to: &str,
    attachment: &str,
    reaction: &str,
    accent: Color,
) -> MessageItem {
    MessageItem {
        event_id: SharedString::default(),
        is_own: false,
        sender: sender.into(),
        initials: initials.into(),
        time: time.into(),
        body: body.into(),
        reply_to: reply_to.into(),
        attachment: attachment.into(),
        reaction: reaction.into(),
        accent,
    }
}

fn member(name: &str, initials: &str, detail: &str, accent: Color, online: bool) -> MemberItem {
    MemberItem {
        name: name.into(),
        initials: initials.into(),
        detail: detail.into(),
        accent,
        online,
    }
}

fn fixtures() -> PreviewState {
    let violet = rgb(168, 140, 255);
    let peach = rgb(243, 159, 120);
    let mint = rgb(120, 200, 177);
    let blue = rgb(100, 165, 232);

    let workshop = PreviewAccount {
        item: account("Alex", "@alex:example.org", "AM", "Available", violet),
        channels: vec![
            room(
                "#general",
                "general",
                0,
                false,
                "Start here, share ideas, and find your people.",
                vec![
                    message(
                        "Jules",
                        "JN",
                        "9:41 AM",
                        "Morning everyone! I finished the first pass on the desktop layout. The space rail is intentionally quiet, so channels and conversation can carry the visual weight.",
                        "",
                        "",
                        "♥  4",
                        peach,
                    ),
                    message(
                        "Sam",
                        "SK",
                        "9:45 AM",
                        "It looks great. I especially like having a reliable place for account switching. It should feel the same whether I have one account or several.",
                        "",
                        "",
                        "✦  2",
                        mint,
                    ),
                    message(
                        "Alex",
                        "AM",
                        "9:48 AM",
                        "The bottom-left card opens upward over the room list. I kept the active identity visible, including the full Matrix ID, so switching contexts is harder to miss.",
                        "Sam: It looks great.",
                        "",
                        "",
                        violet,
                    ),
                    message(
                        "Morgan",
                        "MR",
                        "10:02 AM",
                        "Here is the palette study. Dark, light, and high contrast use the same layout and can switch at runtime.",
                        "",
                        "palette-study.png · 1.8 MB",
                        "♥  3",
                        blue,
                    ),
                    message(
                        "Jules",
                        "JN",
                        "10:42 AM",
                        "I put together a few notes on message density. This longer example is here to make sure lines wrap naturally and that each message row grows with its content. The timeline should never make a conversation feel squeezed into identical blocks.",
                        "",
                        "",
                        "",
                        peach,
                    ),
                    message(
                        "Sam",
                        "SK",
                        "10:47 AM",
                        "A good thread panel needs room to breathe too. We can keep the conversation visible while opening context on the right.",
                        "Jules: I put together a few notes",
                        "",
                        "✦  1",
                        mint,
                    ),
                ],
            ),
            room(
                "#design",
                "design",
                3,
                false,
                "Share concepts, critique, and polished work.",
                vec![
                    message(
                        "Morgan",
                        "MR",
                        "Yesterday",
                        "The updated color tokens are ready for review. Contrast stays strong in all three themes.",
                        "",
                        "theme-tokens.txt · 4 KB",
                        "",
                        blue,
                    ),
                    message(
                        "Alex",
                        "AM",
                        "Today",
                        "The account card and member rail are in place. Next I am testing the conversation at narrow desktop widths.",
                        "",
                        "",
                        "♥  2",
                        violet,
                    ),
                    message(
                        "Jules",
                        "JN",
                        "Today",
                        "I would keep a generous reading width in the center. A long message should wrap without changing the height of its neighbors.",
                        "",
                        "",
                        "",
                        peach,
                    ),
                ],
            ),
            room(
                "#releases",
                "releases",
                1,
                false,
                "Builds, notes, and release discussions.",
                vec![message(
                    "Sam",
                    "SK",
                    "Today",
                    "The next preview should include the account picker, theme controls, room switching, and composer interactions.",
                    "",
                    "",
                    "✓  3",
                    mint,
                )],
            ),
            room(
                "#lounge",
                "lounge",
                0,
                false,
                "Take a break and talk about anything.",
                vec![
                    message(
                        "Jules",
                        "JN",
                        "Yesterday",
                        "Who's up for a voice chat after the review?",
                        "",
                        "",
                        "",
                        peach,
                    ),
                    message(
                        "Morgan",
                        "MR",
                        "Yesterday",
                        "Count me in. The lounge call stage is looking good.",
                        "",
                        "",
                        "",
                        blue,
                    ),
                ],
            ),
        ],
        directs: vec![
            room(
                "@jules",
                "Jules",
                2,
                true,
                "Direct message with Jules",
                vec![
                    message(
                        "Jules",
                        "JN",
                        "10:12 AM",
                        "Can you send me the layout notes before lunch?",
                        "",
                        "",
                        "",
                        peach,
                    ),
                    message(
                        "Alex",
                        "AM",
                        "10:18 AM",
                        "Absolutely. I will attach the current draft here.",
                        "",
                        "desktop-layout.md · 12 KB",
                        "",
                        violet,
                    ),
                ],
            ),
            room(
                "@sam",
                "Sam",
                0,
                true,
                "Direct message with Sam",
                vec![message(
                    "Sam",
                    "SK",
                    "Yesterday",
                    "The new sidebar feels much easier to scan.",
                    "",
                    "",
                    "",
                    mint,
                )],
            ),
        ],
        members: vec![
            member("Alex", "AM", "Online", violet, true),
            member("Jules", "JN", "Online", peach, true),
            member("Sam", "SK", "Online", mint, true),
            member("Morgan", "MR", "Away", blue, false),
            member("Riley", "RL", "Offline", rgb(231, 139, 164), false),
        ],
    };

    let personal = PreviewAccount {
        item: account(
            "Alex · personal",
            "@alex:personal.example",
            "AP",
            "Away",
            blue,
        ),
        channels: vec![
            room(
                "#friends",
                "friends",
                2,
                false,
                "A small place to catch up.",
                vec![
                    message(
                        "Riley",
                        "RL",
                        "11:18 AM",
                        "Are you free for coffee this weekend?",
                        "",
                        "",
                        "",
                        rgb(231, 139, 164),
                    ),
                    message(
                        "Alex · personal",
                        "AP",
                        "11:24 AM",
                        "Yes! Saturday afternoon works.",
                        "",
                        "",
                        "♥  1",
                        blue,
                    ),
                ],
            ),
            room(
                "#photos",
                "photos",
                0,
                false,
                "Photos and moments.",
                vec![message(
                    "Riley",
                    "RL",
                    "Monday",
                    "A few shots from the coast.",
                    "",
                    "coast-weekend.jpg · 2.4 MB",
                    "♥  6",
                    rgb(231, 139, 164),
                )],
            ),
        ],
        directs: vec![room(
            "@riley",
            "Riley",
            1,
            true,
            "Direct message with Riley",
            vec![message(
                "Riley",
                "RL",
                "11:18 AM",
                "I'll send the details later today.",
                "",
                "",
                "",
                rgb(231, 139, 164),
            )],
        )],
        members: vec![
            member("Alex · personal", "AP", "Away", blue, false),
            member("Riley", "RL", "Online", rgb(231, 139, 164), true),
        ],
    };

    let community = PreviewAccount {
        item: account(
            "Community team",
            "@team:community.example",
            "CT",
            "Available",
            mint,
        ),
        channels: vec![
            room(
                "#welcome",
                "welcome",
                0,
                false,
                "Introductions and community news.",
                vec![
                    message(
                        "Community team",
                        "CT",
                        "8:00 AM",
                        "Welcome! Introduce yourself and tell us what you are making.",
                        "",
                        "",
                        "✦  5",
                        mint,
                    ),
                    message(
                        "Nico",
                        "NC",
                        "9:15 AM",
                        "Hi all. I am here to learn about building on Matrix.",
                        "",
                        "",
                        "",
                        peach,
                    ),
                ],
            ),
            room(
                "#support",
                "support",
                4,
                false,
                "Questions, answers, and friendly help.",
                vec![
                    message(
                        "Nico",
                        "NC",
                        "Today",
                        "Where can I find the client setup guide?",
                        "",
                        "",
                        "",
                        peach,
                    ),
                    message(
                        "Community team",
                        "CT",
                        "Today",
                        "The getting started guide is pinned in this room. Ask here if anything is unclear.",
                        "",
                        "",
                        "✓  2",
                        mint,
                    ),
                ],
            ),
        ],
        directs: vec![room(
            "@nico",
            "Nico",
            0,
            true,
            "Direct message with Nico",
            vec![message(
                "Nico",
                "NC",
                "Yesterday",
                "Thanks for the welcome!",
                "",
                "",
                "",
                peach,
            )],
        )],
        members: vec![
            member("Community team", "CT", "Online", mint, true),
            member("Nico", "NC", "Online", peach, true),
            member("Morgan", "MR", "Away", blue, false),
        ],
    };

    PreviewState {
        accounts: vec![workshop, personal, community],
        active_account: 0,
        active_room: "#general".to_owned(),
        stress_backup: None,
        thread_replies: HashMap::new(),
    }
}

fn apply_state(ui: &AppWindow, state: &PreviewState) {
    if ui.get_live_session()
        || ui.get_active_account_index() != state.active_account as i32
        || ui.get_active_room_id().as_str() != state.active_room
    {
        ui.set_composer_mode(0);
        ui.set_composer_target_id("".into());
        ui.set_composer_target_label("".into());
        ui.set_composer_saved_draft("".into());
        ui.set_composer_busy(false);
        ui.set_composer_operation(ui.get_composer_operation().wrapping_add(1));
        ui.set_thread_root_id("".into());
        ui.set_thread_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
        ui.set_thread_composer_text("".into());
    }
    ui.set_live_session(false);
    ui.set_stress_timeline_active(state.stress_backup.is_some());
    let account = &state.accounts[state.active_account];
    ui.set_accounts(ModelRc::new(VecModel::from(
        state
            .accounts
            .iter()
            .map(|a| a.item.clone())
            .collect::<Vec<_>>(),
    )));
    let account_options = std::iter::once(state.active_account)
        .chain((0..state.accounts.len()).filter(|index| *index != state.active_account))
        .map(|index| {
            let item = &state.accounts[index].item;
            AccountOption {
                index: index as i32,
                name: item.name.clone(),
                handle: item.handle.clone(),
                initials: item.initials.clone(),
                accent: item.accent,
            }
        })
        .collect::<Vec<_>>();
    ui.set_account_options(ModelRc::new(VecModel::from(account_options)));
    ui.set_active_account_index(state.active_account as i32);
    ui.set_active_account_local_id("".into());
    ui.set_active_account_name(account.item.name.clone());
    ui.set_active_account_handle(account.item.handle.clone());
    ui.set_active_account_initials(account.item.initials.clone());
    ui.set_active_account_status(account.item.status.clone());
    ui.set_active_account_accent(account.item.accent);
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(
        account
            .channels
            .iter()
            .map(|r| r.item.clone())
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(
        account
            .directs
            .iter()
            .map(|r| r.item.clone())
            .collect::<Vec<_>>(),
    )));
    apply_room_sort(ui);
    ui.set_members(ModelRc::new(VecModel::from(account.members.clone())));

    if state.active_room == "#discover" {
        ui.set_active_room_id("#discover".into());
        ui.set_active_room_name("Explore".into());
        ui.set_active_room_topic("Discover rooms and spaces from your Matrix homeserver.".into());
        ui.set_messages(ModelRc::new(VecModel::from(vec![message(
            "Morpheus",
            "M",
            "Now",
            "This preview shows where directory results will appear once a Matrix account is connected.",
            "",
            "",
            "",
            rgb(168, 140, 255),
        )])));
        return;
    }

    let active_room = account
        .channels
        .iter()
        .chain(&account.directs)
        .find(|room| room.item.id.as_str() == state.active_room);

    if let Some(room) = active_room {
        ui.set_active_room_id(room.item.id.clone());
        ui.set_active_room_name(room.item.name.clone());
        ui.set_active_room_topic(room.topic.clone());
        ui.set_messages(ModelRc::new(VecModel::from(
            room.messages
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, mut item)| {
                    item.event_id = format!("preview:{index}").into();
                    item.is_own = item.sender == account.item.name;
                    item
                })
                .collect::<Vec<_>>(),
        )));
    }
}

fn preview_event_index(event_id: &str) -> Option<usize> {
    event_id.strip_prefix("preview:")?.parse().ok()
}

fn finish_composer_submission(
    ui: &AppWindow,
    operation: i32,
    mode: i32,
    target_id: &str,
    submitted: &str,
    saved_draft: &str,
) {
    if ui.get_composer_operation() != operation {
        return;
    }
    ui.set_composer_busy(false);
    if ui.get_composer_mode() != mode
        || ui.get_composer_target_id().as_str() != target_id
        || ui.get_composer_text().as_str() != submitted
    {
        return;
    }
    if mode == 0 {
        ui.set_composer_text("".into());
    } else {
        ui.set_composer_mode(0);
        ui.set_composer_target_id("".into());
        ui.set_composer_target_label("".into());
        ui.set_composer_saved_draft("".into());
        ui.set_composer_text(saved_draft.into());
    }
}

fn switch_account(state: &mut PreviewState, index: usize) {
    if let Some(account) = state.accounts.get(index) {
        state.active_account = index;
        state.active_room = account
            .channels
            .first()
            .or_else(|| account.directs.first())
            .map(|room| room.item.id.to_string())
            .unwrap_or_default();
    }
}

fn make_stress_messages() -> Vec<MessageItem> {
    const BODIES: [&str; 4] = [
        "Short message for checking compact rows.",
        "This longer message wraps across the conversation width. It helps check that scrolling remains smooth when a room contains years of varied discussion and different text lengths.",
        "A decision from the project chat: keep the room list configurable, allow a narrow details panel, and make account switching easy to find from the bottom-left card.",
        "A deliberately long example message with more than one line. The timeline needs to calculate each row's actual height while only creating delegates for messages near the visible area. Replies, attachments, and reactions create several distinct row shapes for this preview.",
    ];
    (0..10_000)
        .map(|index| {
            let index = index as usize;
            message(
                if index % 3 == 0 { "Alex" } else { "Morgan" },
                if index % 3 == 0 { "AM" } else { "MR" },
                &format!("#{:05}", index + 1),
                BODIES[index % BODIES.len()],
                if index % 7 == 0 {
                    "Earlier message"
                } else {
                    ""
                },
                if index % 11 == 0 {
                    "design-notes.pdf"
                } else {
                    ""
                },
                if index % 5 == 0 { "✦  3" } else { "" },
                if index % 3 == 0 {
                    rgb(168, 140, 255)
                } else {
                    rgb(100, 165, 232)
                },
            )
        })
        .collect()
}

fn color_from_hex(value: &str) -> Color {
    let rgb = u32::from_str_radix(value.strip_prefix('#').unwrap_or_default(), 16).unwrap_or(0);
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn apply_room_sort(ui: &AppWindow) {
    let sort_unread_first = ui.get_sort_unread_first();
    let mut channels = ui.get_unsorted_channels().iter().collect::<Vec<_>>();
    let mut direct_messages = ui.get_unsorted_direct_messages().iter().collect::<Vec<_>>();
    if sort_unread_first {
        channels.sort_by_key(|room| room.unread == 0);
        direct_messages.sort_by_key(|room| room.unread == 0);
    }
    ui.set_channels(ModelRc::new(VecModel::from(channels)));
    ui.set_direct_messages(ModelRc::new(VecModel::from(direct_messages)));
}

fn apply_config(ui: &AppWindow, config: &Config, theme: Option<&ThemePack>) {
    let mode = match config.appearance.theme {
        ThemeMode::Dark => 0,
        ThemeMode::Light => 1,
        ThemeMode::HighContrast => 2,
        ThemeMode::System => 3,
    };
    ui.set_theme_mode(mode);
    ui.set_font_scale_percent(config.appearance.scale_percent.into());
    ui.set_density_mode(match config.appearance.density {
        Density::Compact => 0,
        Density::Comfortable => 1,
        Density::Spacious => 2,
    });
    ui.set_sort_unread_first(config.layout.sort_unread_first);
    apply_room_sort(ui);
    ui.set_room_list_width(config.layout.room_list_width.into());
    ui.set_details_width(config.layout.details_width.into());
    ui.set_show_side_panel(config.layout.show_member_panel);
    ui.set_ui_font_family(config.appearance.font_family.as_str().into());
    if let Some(theme) = theme {
        let colors = &theme.colors;
        ui.set_custom_rail(color_from_hex(&colors.rail));
        ui.set_custom_sidebar(color_from_hex(&colors.sidebar));
        ui.set_custom_canvas(color_from_hex(&colors.canvas));
        ui.set_custom_panel(color_from_hex(&colors.panel));
        ui.set_custom_elevated(color_from_hex(&colors.elevated));
        ui.set_custom_hover(color_from_hex(&colors.hover));
        ui.set_custom_ink(color_from_hex(&colors.ink));
        ui.set_custom_muted(color_from_hex(&colors.muted));
        ui.set_custom_divider(color_from_hex(&colors.divider));
        ui.set_custom_accent(color_from_hex(&colors.accent));
        ui.set_custom_accent_ink(color_from_hex(&colors.accent_ink));
        ui.set_custom_theme_active(true);
    } else {
        ui.set_custom_theme_active(false);
    }
}

fn apply_loaded_config(ui: &AppWindow, loaded: &LoadedConfig) {
    apply_config(ui, &loaded.config, loaded.theme.as_ref());
    ui.set_config_error("".into());
}

fn save_config_change(ui: &AppWindow, store: &ConfigStore, change: impl FnOnce(&mut Config)) {
    // Read from disk first so an invalid external edit is never overwritten by a UI action.
    let result = (|| {
        let mut config = store.load()?;
        change(&mut config);
        store.save(&config)?;
        let theme = store.load_theme(&config)?;
        Ok::<_, morpheus_config::ConfigError>(LoadedConfig { config, theme })
    })();
    match result {
        Ok(loaded) => apply_loaded_config(ui, &loaded),
        Err(error) => ui.set_config_error(error.to_string().into()),
    }
}

fn show_login_error(ui: &slint::Weak<AppWindow>, message: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_login_loading(false);
        ui.set_login_error(message.into());
    });
}

#[cfg(feature = "native-calls")]
fn call_status_label(status: &CallStatus) -> String {
    match status {
        CallStatus::Idle => "Select a room, then open or join its audio call".into(),
        CallStatus::Starting { .. } => "Joining MatrixRTC audio…".into(),
        CallStatus::Joined { .. } => "Connected · native microphone and speaker".into(),
        CallStatus::Ending { .. } => "Leaving call…".into(),
        CallStatus::CleanupFailed { reason, .. } => {
            format!("Could not confirm call leave: {reason}. Confirm Leave again to continue.")
        }
        CallStatus::Failed { reason, .. } => format!("Call failed: {reason}"),
    }
}

#[cfg(feature = "native-calls")]
fn ask_to_leave_call(ui: &slint::Weak<AppWindow>, calls: &CallController, action: i32) -> bool {
    if !calls.status().blocks_account_switch() {
        return false;
    }
    if let Some(ui) = ui.upgrade() {
        ui.set_pending_account_index(action);
        ui.set_leave_call_dialog(true);
    }
    true
}

fn continue_account_action(ui: &AppWindow, action: i32) {
    ui.set_leave_call_busy(false);
    ui.set_leave_call_dialog(false);
    ui.set_pending_account_index(-1);
    ui.set_account_menu_open(false);
    match action {
        -2 => ui.invoke_add_account(),
        -3 => ui.invoke_log_out(),
        index if index >= 0 => ui.invoke_switch_account(index),
        _ => {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "native-calls")]
    morpheus_rtc::install_crypto_provider();
    let ui = AppWindow::new()?;
    ui.set_login_loading(true);
    let data_dir = ProjectDirs::from("io.github", "brk275", "Morpheus")
        .ok_or("No application data directory is available")?
        .data_dir()
        .to_path_buf();
    let manager = Arc::new(AccountManager::open(data_dir)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let live = LiveUi::new(manager.clone());
    ui.set_native_call_enabled(cfg!(feature = "native-calls"));
    ui.set_native_call_status("Select a room to check call availability".into());
    #[cfg(feature = "native-calls")]
    let calls = Arc::new(CallController::new(manager.clone()));
    #[cfg(feature = "native-calls")]
    {
        let calls = calls.clone();
        let weak = ui.as_weak();
        runtime.spawn(async move {
            let mut status = calls.subscribe_status();
            loop {
                let current = status.borrow_and_update().clone();
                let connected = current.blocks_account_switch();
                let label = call_status_label(&current);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_call_connected(connected);
                    ui.set_native_call_status(label.into());
                });
                if status.changed().await.is_err() {
                    break;
                }
            }
        });
    }
    runtime.spawn(live.clone().watch_updates(ui.as_weak()));
    runtime.spawn(live.clone().watch_sync(ui.as_weak()));
    {
        let live = live.clone();
        let weak = ui.as_weak();
        runtime.spawn(async move { live.restore(weak).await });
    }
    let store = ConfigStore::standard()?;
    ui.set_config_path(store.path().display().to_string().into());
    match store.load_or_create() {
        Ok(config) => match store.load_theme(&config) {
            Ok(theme) => apply_config(&ui, &config, theme.as_ref()),
            Err(error) => {
                apply_config(&ui, &config, None);
                ui.set_config_error(error.to_string().into());
            }
        },
        Err(error) => {
            apply_config(&ui, &Config::default(), None);
            ui.set_config_error(error.to_string().into());
        }
    }

    let watch = store.watch_with_theme(Duration::from_millis(700));
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(result) = watch.recv() {
            let closed = weak
                .upgrade_in_event_loop(move |ui| match result {
                    Ok(loaded) => apply_loaded_config(&ui, &loaded),
                    Err(error) => ui.set_config_error(error.to_string().into()),
                })
                .is_err();
            if closed {
                break;
            }
        }
    });

    let state = Rc::new(RefCell::new(fixtures()));
    apply_state(&ui, &state.borrow());

    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let handle = runtime.handle().clone();
        ui.on_open_native_call_slot(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let room_id = window.get_active_room_id().to_string();
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                match calls.open_slot(&room_id).await {
                    Ok(()) => {
                        let _ = weak.upgrade_in_event_loop(|ui| {
                            ui.set_native_call_status(
                                "Room call opened · join audio when ready".into(),
                            );
                        });
                    }
                    Err(error) => show_error(&weak, error.to_string()),
                }
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let handle = runtime.handle().clone();
        ui.on_start_native_call(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let room_id = window.get_active_room_id().to_string();
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                if let Err(error) = calls.start_two_person_audio(&room_id).await {
                    show_error(&weak, error.to_string());
                }
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let handle = runtime.handle().clone();
        ui.on_end_native_call(move || {
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                if let Err(error) = calls.end_call().await {
                    show_error(&weak, error.to_string());
                }
            });
        });
    }

    let weak = ui.as_weak();
    let switch_state = state.clone();
    let switch_live = live.clone();
    let switch_handle = runtime.handle().clone();
    #[cfg(feature = "native-calls")]
    let switch_calls = calls.clone();
    ui.on_switch_account(move |index| {
        if switch_live.is_live.load(Ordering::Acquire) {
            #[cfg(feature = "native-calls")]
            if ask_to_leave_call(&weak, &switch_calls, index) {
                return;
            }
            if let Ok(index) = usize::try_from(index) {
                switch_live.invalidate_navigation();
                let live = switch_live.clone();
                let ui = weak.clone();
                #[cfg(feature = "native-calls")]
                let calls = switch_calls.clone();
                switch_handle.spawn(async move {
                    #[cfg(feature = "native-calls")]
                    if calls.status().blocks_account_switch() {
                        let _ = ui.upgrade_in_event_loop(move |window| {
                            window.set_pending_account_index(index as i32);
                            window.set_leave_call_dialog(true);
                        });
                        return;
                    }
                    if let Err(error) = live.select_account(index, ui.clone()).await {
                        show_error(&ui, error.to_string());
                    }
                });
            }
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let mut state = switch_state.borrow_mut();
            if let Ok(index) = usize::try_from(index) {
                switch_account(&mut state, index);
                apply_state(&ui, &state);
            }
        }
    });

    let weak = ui.as_weak();
    let room_state = state.clone();
    let room_live = live.clone();
    let room_handle = runtime.handle().clone();
    ui.on_open_room(move |room_id| {
        if room_live.is_live.load(Ordering::Acquire) {
            room_live.invalidate_navigation();
            let live = room_live.clone();
            let ui = weak.clone();
            room_handle.spawn(async move {
                live.open_room(room_id.to_string(), ui.clone()).await;
                if let Err(error) = live.refresh(ui.clone()).await {
                    show_error(&ui, error.to_string());
                }
            });
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let mut state = room_state.borrow_mut();
            state.active_room = room_id.to_string();
            apply_state(&ui, &state);
        }
    });

    let weak = ui.as_weak();
    let space_live = live.clone();
    let space_handle = runtime.handle().clone();
    ui.on_select_space(move |space_id| {
        if !space_live.is_live.load(Ordering::Acquire) {
            return;
        }
        space_live.invalidate_navigation();
        let live = space_live.clone();
        let ui = weak.clone();
        space_handle.spawn(async move {
            if let Err(error) = live.select_space(space_id.to_string(), ui.clone()).await {
                show_error(&ui, error.to_string());
            }
        });
    });

    let weak = ui.as_weak();
    let draft_live = live.clone();
    ui.on_composer_edited(move |body| {
        if !draft_live.is_live.load(Ordering::Acquire) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            if ui.get_composer_mode() != 0 {
                return;
            }
            draft_live.remember_draft(
                ui.get_active_account_local_id().as_str(),
                ui.get_active_room_id().as_str(),
                body.as_str(),
            );
        }
    });

    let weak = ui.as_weak();
    let send_state = state.clone();
    let send_live = live.clone();
    let send_handle = runtime.handle().clone();
    ui.on_send_message(move |body| {
        let submitted_text = body.to_string();
        let message_text = submitted_text.trim().to_owned();
        if message_text.is_empty() {
            return;
        }
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_composer_busy() {
            return;
        }
        let mode = ui.get_composer_mode();
        let target_id = ui.get_composer_target_id().to_string();
        let saved_draft = ui.get_composer_saved_draft().to_string();
        if mode != 0 && target_id.is_empty() {
            ui.set_session_error("Select a message before replying or editing".into());
            return;
        }
        let operation = ui.get_composer_operation().wrapping_add(1);
        ui.set_composer_operation(operation);
        ui.set_composer_busy(true);
        if send_live.is_live.load(Ordering::Acquire) {
            let account_id = ui.get_active_account_local_id().to_string();
            let room_id = ui.get_active_room_id().to_string();
            let target = match mode {
                0 => MessageTarget::Room,
                1 => MessageTarget::Reply(target_id.clone()),
                2 => MessageTarget::Edit(target_id.clone()),
                _ => {
                    ui.set_composer_busy(false);
                    ui.set_session_error("Unknown composer action".into());
                    return;
                }
            };
            let account_for_ui = account_id.clone();
            let room_for_ui = room_id.clone();
            let live = send_live.clone();
            let weak = weak.clone();
            send_handle.spawn(async move {
                match live.send(account_id, room_id, message_text, target).await {
                    Ok(()) => {
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            if ui.get_active_account_local_id().as_str() == account_for_ui
                                && ui.get_active_room_id().as_str() == room_for_ui
                                && ui.get_composer_operation() == operation
                            {
                                if mode == 0 {
                                    live.clear_draft_if_unchanged(
                                        &account_for_ui,
                                        &room_for_ui,
                                        &submitted_text,
                                    );
                                }
                                finish_composer_submission(
                                    &ui,
                                    operation,
                                    mode,
                                    &target_id,
                                    &submitted_text,
                                    &saved_draft,
                                );
                            }
                        });
                    }
                    Err(error) => {
                        let error = error.to_string();
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            if ui.get_active_account_local_id().as_str() == account_for_ui
                                && ui.get_active_room_id().as_str() == room_for_ui
                                && ui.get_composer_operation() == operation
                            {
                                ui.set_composer_busy(false);
                                if ui.get_composer_mode() == mode
                                    && ui.get_composer_target_id().as_str() == target_id
                                {
                                    ui.set_session_error(error.into());
                                }
                            }
                        });
                    }
                }
            });
            return;
        }
        let mut state = send_state.borrow_mut();
        let active_room = state.active_room.clone();
        let active_account = state.active_account;
        let account = &mut state.accounts[active_account];
        let item = account.item.clone();
        if let Some(room) = account
            .channels
            .iter_mut()
            .chain(&mut account.directs)
            .find(|room| room.item.id.as_str() == active_room)
        {
            let result = match mode {
                0 => {
                    room.messages.push(message(
                        item.name.as_str(),
                        item.initials.as_str(),
                        "Now",
                        &message_text,
                        "",
                        "",
                        "",
                        item.accent,
                    ));
                    true
                }
                1 => {
                    let context = preview_event_index(&target_id)
                        .and_then(|index| room.messages.get(index))
                        .map(|target| {
                            format!(
                                "{}: {}",
                                target.sender,
                                target.body.chars().take(48).collect::<String>()
                            )
                        });
                    if let Some(context) = context {
                        room.messages.push(message(
                            item.name.as_str(),
                            item.initials.as_str(),
                            "Now",
                            &message_text,
                            &context,
                            "",
                            "",
                            item.accent,
                        ));
                        true
                    } else {
                        false
                    }
                }
                2 => preview_event_index(&target_id)
                    .and_then(|index| room.messages.get_mut(index))
                    .filter(|target| target.sender == item.name)
                    .map(|target| {
                        target.body = message_text.clone().into();
                    })
                    .is_some(),
                _ => false,
            };
            if result {
                apply_state(&ui, &state);
                finish_composer_submission(
                    &ui,
                    operation,
                    mode,
                    &target_id,
                    &submitted_text,
                    &saved_draft,
                );
            } else {
                ui.set_composer_busy(false);
                ui.set_session_error("That preview message is no longer available".into());
            }
        } else {
            ui.set_composer_busy(false);
        }
    });

    let weak = ui.as_weak();
    let reaction_state = state.clone();
    let reaction_live = live.clone();
    let reaction_handle = runtime.handle().clone();
    ui.on_toggle_message_reaction(move |event_id, reaction| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let event_id = event_id.to_string();
        let reaction = reaction.to_string();
        if reaction_live.is_live.load(Ordering::Acquire) {
            let account_id = ui.get_active_account_local_id().to_string();
            let room_id = ui.get_active_room_id().to_string();
            let live = reaction_live.clone();
            let weak = weak.clone();
            reaction_handle.spawn(async move {
                if let Err(error) = live
                    .toggle_reaction(account_id, room_id, event_id, reaction)
                    .await
                {
                    show_error(&weak, error.to_string());
                }
            });
            return;
        }
        let Some(index) = preview_event_index(&event_id) else {
            return;
        };
        let mut state = reaction_state.borrow_mut();
        let active_room = state.active_room.clone();
        let active_account = state.active_account;
        let account = &mut state.accounts[active_account];
        if let Some(message) = account
            .channels
            .iter_mut()
            .chain(&mut account.directs)
            .find(|room| room.item.id.as_str() == active_room)
            .and_then(|room| room.messages.get_mut(index))
        {
            let label = format!("{reaction}  1");
            message.reaction = if message.reaction.as_str() == label {
                "".into()
            } else {
                label.into()
            };
            apply_state(&ui, &state);
        }
    });

    let weak = ui.as_weak();
    let thread_state = state.clone();
    let thread_live = live.clone();
    let thread_handle = runtime.handle().clone();
    ui.on_open_thread(move |event_id, _sender, _body| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let event_id = event_id.to_string();
        let room_id = ui.get_active_room_id().to_string();
        if thread_live.is_live.load(Ordering::Acquire) {
            let account_id = ui.get_active_account_local_id().to_string();
            let live = thread_live.clone();
            let generation = live.begin_thread_request();
            let weak = weak.clone();
            thread_handle.spawn(async move {
                if let Err(error) = live
                    .open_thread(account_id, room_id, event_id, generation, weak.clone())
                    .await
                {
                    show_error(&weak, error.to_string());
                }
            });
            return;
        }
        let state = thread_state.borrow();
        let replies = state
            .thread_replies
            .get(&(state.active_account, room_id, event_id))
            .cloned()
            .unwrap_or_default();
        ui.set_thread_messages(ModelRc::new(VecModel::from(replies)));
    });

    let close_live = live.clone();
    let close_handle = runtime.handle().clone();
    ui.on_close_thread(move || {
        let live = close_live.clone();
        let generation = live.begin_thread_request();
        close_handle.spawn(async move { live.clear_thread_if_current(generation).await });
    });

    let weak = ui.as_weak();
    let thread_send_state = state.clone();
    let thread_send_live = live.clone();
    let thread_send_handle = runtime.handle().clone();
    ui.on_send_thread_message(move |body| {
        let submitted = body.to_string();
        let trimmed = submitted.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let root_id = ui.get_thread_root_id().to_string();
        if root_id.is_empty() {
            return;
        }
        let room_id = ui.get_active_room_id().to_string();
        if thread_send_live.is_live.load(Ordering::Acquire) {
            let account_id = ui.get_active_account_local_id().to_string();
            let live = thread_send_live.clone();
            let weak = weak.clone();
            thread_send_handle.spawn(async move {
                let result = live
                    .send(
                        account_id.clone(),
                        room_id.clone(),
                        trimmed,
                        MessageTarget::Thread(root_id.clone()),
                    )
                    .await;
                match result {
                    Ok(()) => {
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            if ui.get_active_account_local_id().as_str() == account_id
                                && ui.get_active_room_id().as_str() == room_id
                                && ui.get_thread_root_id().as_str() == root_id
                                && ui.get_thread_composer_text().as_str() == submitted
                            {
                                ui.set_thread_composer_text("".into());
                            }
                        });
                    }
                    Err(error) => show_error(&weak, error.to_string()),
                }
            });
            return;
        }
        let mut state = thread_send_state.borrow_mut();
        let account_index = state.active_account;
        let account = state.accounts[account_index].item.clone();
        let replies = state
            .thread_replies
            .entry((account_index, room_id, root_id))
            .or_default();
        replies.push(message(
            account.name.as_str(),
            account.initials.as_str(),
            "Now",
            &trimmed,
            "",
            "",
            "",
            account.accent,
        ));
        ui.set_thread_messages(ModelRc::new(VecModel::from(replies.clone())));
        if ui.get_thread_composer_text().as_str() == submitted {
            ui.set_thread_composer_text("".into());
        }
    });

    let weak = ui.as_weak();
    #[cfg(feature = "native-calls")]
    let add_calls = calls.clone();
    ui.on_add_account(move || {
        #[cfg(feature = "native-calls")]
        if ask_to_leave_call(&weak, &add_calls, -2) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_login_error("".into());
            ui.set_show_sign_in(true);
        }
    });

    let weak = ui.as_weak();
    let preview_live = live.clone();
    let preview_handle = runtime.handle().clone();
    let preview_state = state.clone();
    ui.on_preview_demo(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_login_loading() {
                return;
            }
            preview_live.invalidate_navigation();
            preview_live.is_live.store(false, Ordering::Release);
            let live = preview_live.clone();
            preview_handle.spawn(async move {
                live.clear_feed().await;
                live.manager.stop().await;
            });
            ui.set_login_error("".into());
            ui.set_session_error("Interactive preview · no Matrix connection".into());
            ui.set_show_sign_in(false);
            ui.set_composer_text("".into());
            apply_state(&ui, &preview_state.borrow());
        }
    });

    let weak = ui.as_weak();
    let login_live = live.clone();
    let login_handle = runtime.handle().clone();
    ui.on_submit_login(move |homeserver, username, password| {
        login_live.invalidate_navigation();
        let live = login_live.clone();
        let ui = weak.clone();
        login_handle.spawn(async move {
            match live
                .manager
                .add_password_account(&homeserver, &username, &password)
                .await
            {
                Ok(_) => {
                    live.is_live.store(true, Ordering::Release);
                    if let Err(error) = live.refresh(ui.clone()).await {
                        show_login_error(&ui, error.to_string());
                    } else {
                        tokio::spawn(live.clone().scan_spaces(ui));
                    }
                }
                Err(error) => show_login_error(&ui, error.to_string()),
            }
        });
    });

    let weak = ui.as_weak();
    let sso_live = live.clone();
    let sso_handle = runtime.handle().clone();
    ui.on_submit_sso(move |homeserver| {
        sso_live.invalidate_navigation();
        let live = sso_live.clone();
        let ui = weak.clone();
        sso_handle.spawn(async move {
            match sso::sign_in(live.manager.clone(), homeserver.to_string()).await {
                Ok(_) => {
                    live.is_live.store(true, Ordering::Release);
                    if let Err(error) = live.refresh(ui.clone()).await {
                        show_login_error(&ui, error.to_string());
                    } else {
                        tokio::spawn(live.clone().scan_spaces(ui));
                    }
                }
                Err(error) => show_login_error(&ui, error),
            }
        });
    });

    let weak = ui.as_weak();
    let stress_state = state.clone();
    let stress_live = live.clone();
    ui.on_toggle_stress_timeline(move || {
        if stress_live.is_live.load(Ordering::Acquire) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let mut state = stress_state.borrow_mut();
            let backup = state.stress_backup.take();
            let mut old_messages = None;
            let Some(room) = state.accounts[0].channels.first_mut() else {
                return;
            };
            if let Some(backup) = backup {
                room.messages = backup;
            } else {
                old_messages = Some(std::mem::replace(
                    &mut room.messages,
                    make_stress_messages(),
                ));
            }
            let room_id = room.item.id.to_string();
            state.stress_backup = old_messages;
            state.active_account = 0;
            state.active_room = room_id;
            apply_state(&ui, &state);
        }
    });

    let weak = ui.as_weak();
    let logout_live = live.clone();
    let logout_handle = runtime.handle().clone();
    #[cfg(feature = "native-calls")]
    let logout_calls = calls.clone();
    ui.on_log_out(move || {
        if !logout_live.is_live.load(Ordering::Acquire) {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_sign_in(true);
            }
            return;
        }
        #[cfg(feature = "native-calls")]
        if ask_to_leave_call(&weak, &logout_calls, -3) {
            return;
        }
        logout_live.invalidate_navigation();
        let live = logout_live.clone();
        let ui = weak.clone();
        #[cfg(feature = "native-calls")]
        let calls = logout_calls.clone();
        logout_handle.spawn(async move {
            #[cfg(feature = "native-calls")]
            if calls.status().blocks_account_switch() {
                let _ = ui.upgrade_in_event_loop(|window| {
                    window.set_pending_account_index(-3);
                    window.set_leave_call_dialog(true);
                });
                return;
            }
            let Some(active) = live.manager.active_account().await else {
                return;
            };
            match live.manager.logout_account(&active.id).await {
                Ok(_) => {
                    live.forget_account_drafts(&active.id);
                    live.clear_feed().await;
                    let accounts = live.manager.accounts().await;
                    if accounts.is_empty() {
                        live.is_live.store(false, Ordering::Release);
                        let _ = ui.upgrade_in_event_loop(|ui| {
                            ui.set_show_sign_in(true);
                            ui.set_session_error("".into());
                        });
                    } else if let Err(error) = live.select_account(0, ui.clone()).await {
                        show_error(&ui, error.to_string());
                    }
                }
                Err(error) => show_error(&ui, error.to_string()),
            }
        });
    });

    let weak = ui.as_weak();
    let confirm_live = live.clone();
    #[cfg(feature = "native-calls")]
    let confirm_calls = calls.clone();
    #[cfg(feature = "native-calls")]
    let confirm_handle = runtime.handle().clone();
    ui.on_confirm_leave_call(move |action| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !ui.get_leave_call_dialog()
            || ui.get_leave_call_busy()
            || ui.get_pending_account_index() != action
        {
            return;
        }
        ui.set_leave_call_busy(true);
        if !confirm_live.is_live.load(Ordering::Acquire) {
            ui.set_call_connected(false);
            continue_account_action(&ui, action);
            return;
        }
        #[cfg(feature = "native-calls")]
        {
            let calls = confirm_calls.clone();
            let weak = weak.clone();
            confirm_handle.spawn(async move {
                let result = calls.end_call().await.map_err(|error| error.to_string());
                let _ = weak.upgrade_in_event_loop(move |ui| match result {
                    Ok(()) => continue_account_action(&ui, action),
                    Err(error) => {
                        ui.set_leave_call_busy(false);
                        ui.set_session_error(error.into());
                    }
                });
            });
        }
        #[cfg(not(feature = "native-calls"))]
        continue_account_action(&ui, action);
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_choose_theme(move |index| {
        if let Some(ui) = weak.upgrade() {
            let theme = match index {
                0 => ThemeMode::Dark,
                1 => ThemeMode::Light,
                2 => ThemeMode::HighContrast,
                3 => ThemeMode::System,
                _ => return,
            };
            save_config_change(&ui, &setting_store, |config| {
                config.appearance.theme = theme;
                config.appearance.theme_pack = None;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_change_font_scale(move |percent| {
        if let Some(ui) = weak.upgrade() {
            save_config_change(&ui, &setting_store, |config| {
                config.appearance.scale_percent = percent.clamp(75, 200) as u16;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_choose_density(move |index| {
        if let Some(ui) = weak.upgrade() {
            let density = match index {
                0 => Density::Compact,
                1 => Density::Comfortable,
                2 => Density::Spacious,
                _ => return,
            };
            save_config_change(&ui, &setting_store, |config| {
                config.appearance.density = density;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_set_sort_unread_first(move |sort_unread_first| {
        if let Some(ui) = weak.upgrade() {
            save_config_change(&ui, &setting_store, |config| {
                config.layout.sort_unread_first = sort_unread_first;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_set_side_panel(move |visible| {
        if let Some(ui) = weak.upgrade() {
            save_config_change(&ui, &setting_store, |config| {
                config.layout.show_member_panel = visible;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_change_room_list_width(move |width| {
        if let Some(ui) = weak.upgrade() {
            save_config_change(&ui, &setting_store, |config| {
                config.layout.room_list_width = width.clamp(180, 480) as u16;
            });
        }
    });

    let weak = ui.as_weak();
    ui.on_change_details_width(move |width| {
        if let Some(ui) = weak.upgrade() {
            save_config_change(&ui, &store, |config| {
                config.layout.details_width = width.clamp(200, 520) as u16;
            });
        }
    });

    ui.run()?;
    #[cfg(feature = "native-calls")]
    if let Err(error) = runtime.block_on(calls.end_call()) {
        eprintln!("Could not leave native call during shutdown: {error}");
    }
    runtime.block_on(manager.stop());
    Ok(())
}
