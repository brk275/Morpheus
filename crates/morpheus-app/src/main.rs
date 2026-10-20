//! Native desktop shell with a live Matrix session and an explicit offline preview.
#![recursion_limit = "512"]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod account_access;
mod account_identity;
mod appearance;
mod appearance_editor;
mod attachment_workbench;
mod attachments;
mod audio_devices;
#[cfg(feature = "native-calls")]
mod call_membership;
#[cfg(feature = "native-calls")]
mod call_recovery;
mod call_stage;
#[cfg(feature = "native-calls")]
mod calls;
mod composer_mentions;
mod config_cli;
mod device_sessions;
mod draft_recovery;
mod drafts;
mod image_previews;
#[cfg(feature = "native-calls")]
mod incoming_ring;
mod kept_replies;
mod live;
#[cfg(feature = "native-calls")]
mod local_video;
mod matrix_links;
mod member_privacy;
mod message_metadata;
mod message_search;
mod metric_editor;
mod moderation_ui;
mod navigation;
mod navigation_target;
mod notification_center;
mod notifications;
#[cfg(feature = "native-calls")]
mod outgoing_ring;
mod people_directory;
mod pinned_messages;
mod polls;
#[cfg(debug_assertions)]
mod preview_capture;
mod room_activity;
mod room_addresses;
mod room_leave;
mod room_media_browser;
mod room_organization;
mod room_permissions;
mod room_settings;
mod security;
mod sender_palette_editor;
mod settings_search;
mod shortcuts;
mod space_explorer;
mod space_management;
mod sso;
#[cfg(not(morpheus_timeline_acceptance))]
mod startup_error;
mod stickers;
mod style_profiles;
mod theme_studio;
mod thread_browser;
mod thread_history;
// Instrumented release builds only (see build.rs); unit tests cover its pure checks.
#[cfg(any(test, morpheus_timeline_acceptance))]
mod timeline_acceptance;
mod timeline_delivery;
mod timeline_model;
mod update_controller;
mod updater;
mod verification;
#[cfg(feature = "native-calls")]
mod video_render;
#[cfg(feature = "native-calls")]
mod voice_rosters;
mod voice_spaces;
mod workspace_layout;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
#[cfg(feature = "native-calls")]
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::thread;
use std::time::Duration;

use directories::ProjectDirs;
use morpheus_config::{
    Config, ConfigStore, Density, LoadedConfig, RoomCallMode, ThemeMode, ThemePack,
};
use morpheus_matrix::{
    AccountManager, Generation, MatrixLinkTarget, PublicRoomInfo, PublicRoomsQuery, TextFormat,
    parse_matrix_link_target,
};
use slint::{Color, Model, ModelRc, SharedString, StyledText, VecModel};
use url::Url;

#[cfg(feature = "native-calls")]
use crate::calls::{
    AudioUpdate, CallController, CallControllerError, CallJoinMode, CallStartIntent, CallStatus,
    route_degraded, route_notice, route_reply_notice, route_reply_scope, route_summary,
};
use crate::live::{LiveUi, RoomAction, show_error};
#[cfg(feature = "native-calls")]
use crate::local_video::{CaptureKind, CapturePhase, CaptureSource, LocalVideoStatus};
use crate::notifications::DesktopNotifications;
use crate::voice_spaces::VoiceSpaces;

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
        ..Default::default()
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
            mentions: 0,
            private_room,
            voice_space: false,
            voice_local: false,
            voice_shared: false,
            favourite: false,
            low_priority: false,
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
    let clocks = message_metadata::ClockLabels::from_preview(time);
    MessageItem {
        event_id: SharedString::default(),
        is_own: false,
        sender: sender.into(),
        sender_palette_index: appearance::sender_palette_index(sender),
        initials: initials.into(),
        time: time.into(),
        time_24h: clocks.twenty_four_hour.into(),
        time_24h_seconds: clocks.twenty_four_hour_seconds.into(),
        body: body.into(),
        rich_body: StyledText::from_plain_text(body),
        rich_accessible_body: body.into(),
        edit_source: body.into(),
        edit_markdown: false,
        reply_to: reply_to.into(),
        attachment: attachment.into(),
        attachment_is_image: false,
        reaction: reaction.into(),
        poll: PollCardData::default(),
        accent,
        pinned: false,
    }
}

fn member(name: &str, initials: &str, detail: &str, accent: Color, online: bool) -> MemberItem {
    MemberItem {
        user_id: "".into(),
        name: name.into(),
        initials: initials.into(),
        detail: detail.into(),
        membership: "join".into(),
        power_level_text: "0".into(),
        is_account_user: false,
        is_creator: false,
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
        item: account("Casey", "@casey:example.org", "CY", "Available", violet),
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
                        "Morning! Anyone up for a photo walk by the river on Saturday?",
                        "",
                        "",
                        "♥  4",
                        peach,
                    ),
                    {
                        let mut sample = message(
                            "Sam",
                            "SK",
                            "9:45 AM",
                            "Count me in. Let's meet at the old bridge at ten.",
                            "",
                            "",
                            "✦  2",
                            mint,
                        );
                        sample.rich_body = StyledText::from_markdown(
                            "Count me in. Let's meet at the **old bridge** at ten.",
                        )
                        .expect("Preview formatting should be valid");
                        sample.edit_source =
                            "Count me in. Let's meet at the **old bridge** at ten.".into();
                        sample.edit_markdown = true;
                        sample
                    },
                    message(
                        "Casey",
                        "CY",
                        "9:48 AM",
                        "I'll bring coffee. Riley is joining us after breakfast.",
                        "Sam: Count me in.",
                        "",
                        "",
                        violet,
                    ),
                    message(
                        "Morgan",
                        "MR",
                        "10:02 AM",
                        "Here's the route I had in mind. There's a nice spot for a break near the gardens.",
                        "",
                        "riverside-walk-route.png · 1.8 MB",
                        "♥  3",
                        blue,
                    ),
                    message(
                        "Jules",
                        "JN",
                        "10:42 AM",
                        "I checked the route this morning. The little footbridge is closed, so we'll take the garden entrance instead. It's a slightly longer walk, but the view is worth it.",
                        "",
                        "",
                        "",
                        peach,
                    ),
                    message(
                        "Sam",
                        "SK",
                        "10:47 AM",
                        "Good catch. I'll post a photo of the meeting spot so nobody has to hunt for us.",
                        "Jules: I checked the route this morning.",
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
                        "Casey",
                        "CY",
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
                        "Casey",
                        "CY",
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
            member("Casey", "CY", "Online", violet, true),
            member("Jules", "JN", "Online", peach, true),
            member("Sam", "SK", "Online", mint, true),
            member("Morgan", "MR", "Away", blue, false),
            member("Riley", "RL", "Offline", rgb(231, 139, 164), false),
        ],
    };

    let personal = PreviewAccount {
        item: account(
            "Casey · personal",
            "@casey:personal.example",
            "CP",
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
                        "Casey · personal",
                        "CP",
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
            member("Casey · personal", "CP", "Away", blue, false),
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
        ui.set_composer_markdown(false);
        ui.set_composer_saved_markdown(false);
        ui.set_composer_busy(false);
        ui.set_composer_operation(ui.get_composer_operation().wrapping_add(1));
        ui.set_thread_root_id("".into());
        ui.set_thread_root_is_own(false);
        ui.set_thread_root_palette_index(0);
        ui.set_thread_root_accent(Color::default());
        ui.set_thread_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
        ui.set_thread_composer_text("".into());
        ui.set_thread_send_pending(false);
        ui.set_room_actions_open(false);
        ui.set_room_action_busy(false);
        ui.set_room_action_error("".into());
        ui.set_invite_error("".into());
        ui.set_room_action_operation(ui.get_room_action_operation().wrapping_add(1));
        reset_room_search(ui);
        ui.set_room_call_mode(0);
    }
    ui.set_live_session(false);
    ui.set_preview_session(true);
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
                local_id: "".into(),
                index: index as i32,
                name: item.name.clone(),
                handle: item.handle.clone(),
                initials: item.initials.clone(),
                accent: item.accent,
                ..Default::default()
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
    ui.set_invitations(ModelRc::new(VecModel::from(Vec::<RoomItem>::new())));
    apply_room_sort(ui);
    ui.set_members(ModelRc::new(VecModel::from(account.members.clone())));
    ui.set_member_loading(false);
    ui.set_member_error("".into());

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

/// The external-link confirmation: an http(s) link opens in the system
/// browser only after the person confirms it in a native dialog. Anything
/// else (a `matrix:` URI included) is never handed to the system.
fn confirm_external_link(link: &str) {
    if let Some(url) = safe_message_url(link) {
        thread::spawn(move || {
            let accepted = rfd::MessageDialog::new()
                .set_title("Open external link?")
                .set_description(format!("The message links to:\n\n{url}"))
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show()
                == rfd::MessageDialogResult::Ok;
            if accepted {
                let _ = webbrowser::open(&url);
            }
        });
    }
}

fn safe_message_url(link: &str) -> Option<String> {
    if link.len() > 2048 || link.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(link).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.into())
}

fn finish_composer_submission(
    ui: &AppWindow,
    operation: i32,
    mode: i32,
    target_id: &str,
    submitted: &str,
    markdown: bool,
    saved_draft: &str,
) {
    if ui.get_composer_operation() != operation {
        return;
    }
    ui.set_composer_busy(false);
    if ui.get_composer_mode() != mode
        || ui.get_composer_target_id().as_str() != target_id
        || ui.get_composer_text().as_str() != submitted
        || ui.get_composer_markdown() != markdown
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
        ui.set_composer_markdown(ui.get_composer_saved_markdown());
        ui.set_composer_saved_markdown(false);
    }
}

fn reset_attachment_view(ui: &AppWindow) {
    ui.invoke_close_image_preview();
    // An unfinished save keeps downloading in the account's transfer center;
    // leaving the account stops it. Bumping the operation below keeps its
    // late notice out of the next room.
    // Uploads continue in their room after navigation and staged files stay
    // with it. The workbench shows the next room's queue once the active
    // account or room changes; nothing of this one remains meanwhile.
    ui.set_attachment_items(ModelRc::new(VecModel::<AttachmentItem>::default()));
    ui.set_attachment_ready_count(0);
    ui.set_attachment_can_add(false);
    ui.set_attachment_status("".into());
    ui.set_download_operation(ui.get_download_operation().wrapping_add(1));
    ui.set_downloading_event_id("".into());
    ui.set_transfer_notice("".into());
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
                if index % 3 == 0 { "Casey" } else { "Morgan" },
                if index % 3 == 0 { "CY" } else { "MR" },
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

/// A validated `#RRGGBB` or `#RRGGBBAA` color, alpha last. Validation has
/// already rejected anything else; an unexpected value paints black.
fn color_from_hex(value: &str) -> Color {
    let [red, green, blue, alpha] =
        morpheus_config::registry::parse_hex_color(value).unwrap_or([0, 0, 0, 0xff]);
    Color::from_argb_u8(alpha, red, green, blue)
}

fn apply_room_sort(ui: &AppWindow) {
    navigation::apply_room_order(ui);
}

fn apply_notification_settings(ui: &AppWindow, config: &Config) {
    ui.set_notifications_enabled(config.notifications.enabled);
    notification_center::apply_config(ui, config);
    ui.set_notification_sounds(config.notifications.sounds);
    ui.set_notification_show_preview(config.notifications.show_message_preview);
    ui.set_notification_highlight_words(ModelRc::new(VecModel::from(
        config
            .notifications
            .highlight_words
            .iter()
            .map(|word| SharedString::from(word.as_str()))
            .collect::<Vec<_>>(),
    )));
}

fn apply_config(ui: &AppWindow, config: &Config, theme: Option<&ThemePack>) {
    // While the theme studio drafts look changes, they stay shown over the
    // accepted configuration (see theme_studio).
    if !theme_studio::show_over_accepted(ui, config, theme) {
        apply_appearance(ui, config, theme, true);
    }
    apply_behavior(ui, config);
    style_profiles::config_applied(ui);
}

/// Accepted behavioral settings always take effect, including while a style
/// preview owns the appearance. Preview/cancel never applies these settings.
fn apply_behavior(ui: &AppWindow, config: &Config) {
    shortcuts::apply_config(ui, config);
    audio_devices::apply_config(ui, config);
    image_previews::apply_config(ui, config);
    room_activity::apply_config(ui, config);
    apply_notification_settings(ui, config);
}

/// Everything of `config` that shows the look of the window, and nothing that
/// changes behavior such as shortcuts, devices or notifications. A style
/// profile preview applies only this, with `project_editors` false so the
/// Settings editors and their undo steps stay on the accepted configuration.
fn apply_appearance(
    ui: &AppWindow,
    config: &Config,
    theme: Option<&ThemePack>,
    project_editors: bool,
) {
    appearance::apply_config(ui, config);
    appearance::apply_theme(ui, config, theme);
    if project_editors {
        // After the theme, so the workspace reads the colors now shown.
        appearance_editor::project(ui, config, theme);
        metric_editor::project(ui, config);
        sender_palette_editor::project(ui, config, theme);
    }
    // The theme mode follows the window's active account: its configured
    // override, else the global appearance. Only this accepted configuration
    // replaces the remembered preferences.
    appearance::remember_theme_preferences(config, theme);
    appearance::apply_theme_mode(ui);
    ui.set_font_scale_percent(config.appearance.scale_percent.into());
    let density_index = |density: Density| match density {
        Density::Compact => 0,
        Density::Comfortable => 1,
        Density::Spacious => 2,
    };
    ui.set_density_mode(density_index(config.appearance.density));
    // Omitted pane densities follow the global density.
    let layout = &config.layout;
    ui.set_room_density_mode(density_index(
        layout.room_density.unwrap_or(config.appearance.density),
    ));
    ui.set_message_density_mode(density_index(
        layout.message_density.unwrap_or(config.appearance.density),
    ));
    ui.set_details_side(match layout.details_side {
        morpheus_config::DetailsSide::Right => 0,
        morpheus_config::DetailsSide::Left => 1,
    });
    workspace_layout::apply_config(ui, config);
    ui.set_direct_messages_first(layout.direct_messages_first);
    ui.set_room_density_choice(layout.room_density.map_or(-1, density_index));
    ui.set_message_density_choice(layout.message_density.map_or(-1, density_index));
    ui.set_sort_unread_first(config.layout.sort_unread_first);
    apply_room_sort(ui);
    ui.set_room_list_width(config.layout.room_list_width.into());
    ui.set_details_width(config.layout.details_width.into());
    ui.set_show_side_panel(config.layout.show_member_panel);
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
    // A previewed style profile stays while the accepted look is unchanged,
    // as when the watcher re-reads this window's own save; any other change
    // ends the preview here, before it is applied.
    if style_profiles::keeps_preview(ui, loaded) {
        apply_behavior(ui, &loaded.config);
        style_profiles::config_applied(ui);
    } else {
        apply_config(ui, &loaded.config, loaded.theme.as_ref());
    }
    ui.set_config_error("".into());
}

fn save_config_change(
    ui: &AppWindow,
    store: &ConfigStore,
    change: impl FnOnce(&mut Config),
) -> Option<Config> {
    save_config_edit(ui, store, |config| {
        change(config);
        true
    })
}

/// `save_config_change` for a change decided on the file as re-read: when
/// `change` returns false, nothing is written or applied and `None` is
/// returned without an error, so the caller tells a declined change from a
/// failed save by what its closure recorded. Look changes made in Settings,
/// or while the theme studio holds a draft, join that draft instead of the
/// file; other settings are written as before.
fn save_config_edit(
    ui: &AppWindow,
    store: &ConfigStore,
    change: impl FnOnce(&mut Config) -> bool,
) -> Option<Config> {
    if theme_studio::routes(ui) {
        return theme_studio::save(ui, store, change);
    }
    write_config_edit(ui, store, change)
}

/// `save_config_edit` straight to the file, for a change that is itself an
/// explicit apply of a whole look (a style profile). It is never drafted.
fn write_config_edit(
    ui: &AppWindow,
    store: &ConfigStore,
    change: impl FnOnce(&mut Config) -> bool,
) -> Option<Config> {
    // Read from disk first so an invalid external edit is never overwritten by a UI action.
    let result = (|| {
        let (mut config, revision) = store.load_revision()?;
        let before = style_profiles::previewing().then(|| config.clone());
        if !change(&mut config) {
            return Ok(None);
        }
        // A Settings change ends a style profile preview first. One that
        // would change the look may have been computed from the preview,
        // so it is refused rather than saved.
        if let Some(before) = before
            && !style_profiles::end_preview_for_save(ui, &before, &config)
        {
            return Ok(None);
        }
        // Written only over the revision the change was decided on. An edit
        // found in between is kept and reported instead of replaced; the
        // morpheus-config commit module states the remaining window.
        store.commit(&config, &revision)?;
        Ok::<_, morpheus_config::CommitError>(Some(config))
    })();
    match result {
        Ok(None) => None,
        Ok(Some(config)) => {
            // A missing or malformed theme should leave the last valid
            // appearance active, but it must not hold back notification
            // privacy changes from a successfully saved config.
            apply_notification_settings(ui, &config);
            shortcuts::apply_config(ui, &config);
            audio_devices::apply_config(ui, &config);
            image_previews::apply_config(ui, &config);
            room_activity::apply_config(ui, &config);
            match store.load_theme(&config) {
                Ok(theme) => apply_loaded_config(
                    ui,
                    &LoadedConfig {
                        config: config.clone(),
                        theme,
                    },
                ),
                Err(error) => ui.set_config_error(error.to_string().into()),
            }
            Some(config)
        }
        Err(error) => {
            ui.set_config_error(error.to_string().into());
            None
        }
    }
}

fn save_notification_config_change(
    ui: &AppWindow,
    store: &ConfigStore,
    notifications: &DesktopNotifications,
    voice_spaces: &VoiceSpaces,
    change: impl FnOnce(&mut Config),
) -> bool {
    if let Some(config) = save_config_change(ui, store, change) {
        // Apply each successful in-app privacy change synchronously. Rapid
        // off/on edits may coalesce in the 700 ms file watcher, but must still
        // revoke pending incoming offers before this callback returns.
        voice_spaces.apply_config(&config);
        notifications.apply_config(&config);
        true
    } else {
        false
    }
}

const MAX_HIGHLIGHT_WORD_CHARS: usize = 64;
const MAX_HIGHLIGHT_WORDS: usize = 32;

fn normalize_highlight_word(input: &str) -> Result<String, &'static str> {
    if input.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    }) {
        return Err("Highlight text cannot contain control or direction characters.");
    }
    let word = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if word.is_empty() {
        return Err("Enter a word or phrase to highlight.");
    }
    if word.chars().count() > MAX_HIGHLIGHT_WORD_CHARS {
        return Err("A highlight can be at most 64 characters.");
    }
    Ok(word)
}

fn show_login_error(ui: &slint::Weak<AppWindow>, message: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_login_loading(false);
        ui.set_login_error(message.into());
    });
}

fn directory_plain_text(value: &str, max_chars: usize) -> String {
    let mut text = String::new();
    for character in value.chars().take(max_chars) {
        text.push(
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
            {
                ' '
            } else {
                character
            },
        );
    }
    text.trim().to_owned()
}

/// Cancel message search and clear its results (see message_search).
fn reset_room_search(ui: &AppWindow) {
    message_search::reset(ui);
}

fn public_room_item(room: PublicRoomInfo, route_server: &str) -> PublicRoomItem {
    let id = room.room_id;
    let alias = room.canonical_alias.unwrap_or_default();
    let title = room
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| if alias.is_empty() { &id } else { &alias });
    let title = directory_plain_text(title, 120);
    let title = if title.is_empty() {
        directory_plain_text(if alias.is_empty() { &id } else { &alias }, 120)
    } else {
        title
    };
    PublicRoomItem {
        id: id.into(),
        title: title.into(),
        alias: directory_plain_text(&alias, 160).into(),
        canonical_alias: alias.into(),
        topic: directory_plain_text(room.topic.as_deref().unwrap_or(""), 240).into(),
        members: format!("{} members", room.joined_members).into(),
        is_space: room.is_space,
        route_server: route_server.into(),
    }
}

fn matrix_id_server(id: &str) -> Option<&str> {
    let id = id.strip_prefix('!').or_else(|| id.strip_prefix('#'))?;
    id.split_once(':')
        .map(|(_, server)| server)
        .filter(|server| !server.is_empty())
}

type JoinSlot = Arc<Mutex<Option<(i32, live::JoinAsked)>>>;

/// The real UI withdrawal routes, shared by startup and the generated
/// window regression. They hold only operation metadata, never a client.
fn register_join_cancellation(ui: &AppWindow, room_join: JoinSlot, directory_join: JoinSlot) {
    let weak = ui.as_weak();
    ui.on_cancel_discovery_join(move || {
        let taken = directory_join
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some((_, asked)) = taken {
            asked.withdraw();
        }
        if let Some(window) = weak.upgrade() {
            window.set_directory_join_operation(
                window.get_directory_join_operation().wrapping_add(1),
            );
            window.set_directory_join_busy(false);
            window.set_directory_joining_room_id("".into());
        }
    });
    let weak = ui.as_weak();
    ui.on_cancel_room_join(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !window.get_room_action_busy() || window.get_room_action_mode() != 1 {
            return;
        }
        let taken = room_join
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some((_, asked)) = taken {
            asked.withdraw();
        }
        window.set_room_action_operation(window.get_room_action_operation().wrapping_add(1));
        window.set_room_action_busy(false);
        window.set_room_action_error("".into());
    });
}

fn room_join_target(
    input: &str,
    canonical_alias: &str,
    route_server: &str,
) -> Result<MatrixLinkTarget, String> {
    if input.trim().is_empty() {
        return Err("Enter a room ID, alias, or Matrix link".into());
    }
    let mut target = parse_matrix_link_target(input).map_err(|error| error.to_string())?;
    match &mut target {
        MatrixLinkTarget::Room {
            room_or_alias_id,
            via_servers,
            ..
        } => {
            let add_server = |server: &str, via_servers: &mut Vec<String>| {
                if !server.is_empty() && !via_servers.iter().any(|known| known == server) {
                    via_servers.push(server.to_owned());
                }
            };
            let had_link_routes = !via_servers.is_empty();
            add_server(route_server, via_servers);
            if let Some(server) = matrix_id_server(canonical_alias) {
                add_server(server, via_servers);
            }
            if !had_link_routes {
                if let Some(server) = matrix_id_server(room_or_alias_id) {
                    add_server(server, via_servers);
                }
            }
            Ok(target)
        }
        MatrixLinkTarget::User { .. } => {
            Err("That link identifies a user. Enter a room link or alias.".into())
        }
    }
}

// The instrumented acceptance build measures the optimized profile only.
#[cfg(all(morpheus_timeline_acceptance, debug_assertions))]
compile_error!("The timeline acceptance build requires the release profile");

/// Development-only account and config root for disposable desktop QA. The
/// normal application paths are unchanged when this variable is absent.
#[cfg_attr(morpheus_timeline_acceptance, allow(dead_code))]
fn qa_profile_root() -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
    {
        let Some(value) = std::env::var_os("MORPHEUS_QA_PROFILE_DIR") else {
            return Ok(None);
        };
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err("MORPHEUS_QA_PROFILE_DIR must be an absolute path".into());
        }
        Ok(Some(path))
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(None)
    }
}

#[cfg(feature = "native-calls")]
fn call_status_label(status: &CallStatus) -> String {
    match status {
        CallStatus::Idle => "Select a room, then open or join its audio call".into(),
        CallStatus::OpeningSlot { .. } => "Opening MatrixRTC room call…".into(),
        CallStatus::Starting { .. } => "Joining MatrixRTC audio…".into(),
        CallStatus::Joined { .. } => "Room call joined".into(),
        CallStatus::Reconnecting { interruption, .. } => interruption.label(),
        CallStatus::Ending { .. } => "Leaving call…".into(),
        CallStatus::CleanupFailed { reason, .. } => {
            format!(
                "Call cleanup is incomplete: {}. Review before continuing.",
                directory_plain_text(reason, 180)
            )
        }
        CallStatus::Failed { reason, .. } => {
            format!("Call failed: {}", directory_plain_text(reason, 180))
        }
        CallStatus::Ended { reason, .. } => {
            format!("Call ended: {}", directory_plain_text(reason, 180))
        }
    }
}

#[cfg(feature = "native-calls")]
fn call_room_id(status: &CallStatus) -> Option<&str> {
    match status {
        CallStatus::OpeningSlot { room_id, .. }
        | CallStatus::Starting { room_id, .. }
        | CallStatus::Ending { room_id, .. }
        | CallStatus::CleanupFailed { room_id, .. } => Some(room_id),
        CallStatus::Joined { call, .. } | CallStatus::Reconnecting { call, .. } => {
            Some(&call.room_id)
        }
        CallStatus::Idle | CallStatus::Failed { .. } | CallStatus::Ended { .. } => None,
    }
}

#[cfg(feature = "native-calls")]
fn call_room_display(
    room_id: &str,
    previous_room_id: &str,
    previous_display: &str,
    active_room_id: &str,
    active_room_name: &str,
) -> String {
    if room_id.is_empty() {
        String::new()
    } else if previous_room_id == room_id && !previous_display.is_empty() {
        previous_display.to_owned()
    } else if active_room_id == room_id {
        directory_plain_text(active_room_name, 140)
    } else {
        directory_plain_text(room_id, 260)
    }
}

#[cfg(feature = "native-calls")]
fn clear_native_capture(ui: &AppWindow) {
    ui.set_native_camera_active(false);
    ui.set_native_screen_active(false);
    ui.set_native_camera_busy(false);
    ui.set_native_screen_busy(false);
    ui.set_native_screen_selecting(false);
    ui.set_native_camera_stopping(false);
    ui.set_native_screen_stopping(false);
    ui.set_native_camera_source(SharedString::default());
    ui.set_native_screen_source(SharedString::default());
    ui.set_native_capture_error(SharedString::default());
    ui.set_native_camera_error(SharedString::default());
    ui.set_native_screen_error(SharedString::default());
    ui.set_native_capture_picker_open(false);
    ui.set_native_capture_sources(ModelRc::new(VecModel::from(
        Vec::<CallCaptureSourceItem>::new(),
    )));
    ui.set_native_capture_source_loading(false);
    ui.set_native_capture_source_error(SharedString::default());
}

#[cfg(feature = "native-calls")]
fn safe_capture_label(label: &str) -> String {
    let visible = sanitized_capture_text(label, 80);
    if visible.is_empty() {
        "Unnamed source".into()
    } else {
        visible
    }
}

#[cfg(feature = "native-calls")]
fn safe_capture_error(error: &str) -> String {
    let visible = sanitized_capture_text(error, 240);
    if visible.is_empty() {
        "Unknown capture error".into()
    } else {
        visible
    }
}

#[cfg(feature = "native-calls")]
fn sanitized_capture_text(text: &str, limit: usize) -> String {
    let visible: String = text
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}' | '\u{feff}'
                )
        })
        .take(limit)
        .collect();
    visible.trim().into()
}

#[cfg(feature = "native-calls")]
fn capture_sources(picker_kind: i32) -> Result<Vec<(i32, String, String)>, String> {
    if picker_kind == 0 {
        return morpheus_rtc::CameraCapture::camera_sources()
            .map(|sources| {
                sources
                    .into_iter()
                    .map(|(id, name)| (0, id, safe_capture_label(&name)))
                    .collect()
            })
            .map_err(|error| safe_capture_label(&error.to_string()));
    }
    if picker_kind != 1 {
        return Err("Unknown capture source type".into());
    }
    let displays = morpheus_rtc::ScreenCapture::display_sources();
    let windows = morpheus_rtc::ScreenCapture::window_sources();
    let mut sources = Vec::new();
    if let Ok(displays) = &displays {
        sources.extend(
            displays
                .iter()
                .map(|(id, name)| (1, id.clone(), safe_capture_label(name))),
        );
    }
    if let Ok(windows) = &windows {
        sources.extend(
            windows
                .iter()
                .map(|(id, name)| (2, id.clone(), safe_capture_label(name))),
        );
    }
    if sources.is_empty() {
        let details = displays.err().or_else(|| windows.err());
        Err(details
            .map(|error| safe_capture_label(&error.to_string()))
            .unwrap_or_else(|| "No displays or windows are available to share".into()))
    } else {
        Ok(sources)
    }
}

#[cfg(feature = "native-calls")]
fn apply_capture_status(ui: &AppWindow, status: LocalVideoStatus) {
    let camera_error = match &status.camera.phase {
        CapturePhase::Failed(error) => Some(error.as_str()),
        _ => None,
    };
    let screen_error = match &status.screen.phase {
        CapturePhase::Failed(error) => Some(error.as_str()),
        _ => None,
    };
    ui.set_native_camera_active(matches!(&status.camera.phase, CapturePhase::Active));
    ui.set_native_screen_active(matches!(&status.screen.phase, CapturePhase::Active));
    ui.set_native_camera_busy(matches!(
        &status.camera.phase,
        CapturePhase::Starting | CapturePhase::Stopping
    ));
    ui.set_native_screen_busy(matches!(
        &status.screen.phase,
        CapturePhase::Selecting | CapturePhase::Starting | CapturePhase::Stopping
    ));
    ui.set_native_screen_selecting(matches!(&status.screen.phase, CapturePhase::Selecting));
    ui.set_native_camera_stopping(matches!(&status.camera.phase, CapturePhase::Stopping));
    ui.set_native_screen_stopping(matches!(&status.screen.phase, CapturePhase::Stopping));
    ui.set_native_camera_source(status.camera.source.into());
    ui.set_native_screen_source(status.screen.source.into());
    ui.set_native_camera_error(
        camera_error
            .map(safe_capture_error)
            .unwrap_or_default()
            .into(),
    );
    ui.set_native_screen_error(
        screen_error
            .map(safe_capture_error)
            .unwrap_or_default()
            .into(),
    );
}

#[cfg(feature = "native-calls")]
fn ask_to_leave_call(ui: &slint::Weak<AppWindow>, calls: &CallController, action: i32) -> bool {
    let status = calls.status();
    if !status.blocks_account_switch() {
        return false;
    }
    if let Some(ui) = ui.upgrade() {
        matrix_links::clear(&ui);
        thread_browser::withdraw(&ui);
        ui.set_native_leave_unconfirmed(matches!(status, CallStatus::CleanupFailed { .. }));
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
        -3 => ui.invoke_continue_log_out(),
        index if index >= 0 => ui.invoke_switch_account(index),
        _ => {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `config validate` and `config schema` run first: before the screenshot
    // preview, the updater, the startup session, the window, the account
    // store, the credential vault, sync and media devices. They write nothing.
    if let Some(status) = config_cli::early_dispatch() {
        std::process::exit(status);
    }
    // An instrumented acceptance build refuses to start without a valid run
    // file, so it never runs as an ordinary app. See tests/TIMELINE_10K.md.
    #[cfg(morpheus_timeline_acceptance)]
    let mut acceptance = timeline_acceptance::Acceptance::from_environment()?;
    // Screenshot generation uses only the in-memory offline preview. Dispatch
    // before updater, configuration, credential vault or account initialization.
    #[cfg(debug_assertions)]
    if preview_capture::early_dispatch()? {
        return Ok(());
    }
    if updater::early_dispatch()? {
        return Ok(());
    }
    // Declared first and dropped last, after runtimes/account workers stop.
    let update_session = Arc::new(Mutex::new(updater::StartupSession::begin()?));
    if update_session
        .lock()
        .map_err(|_| updater::UpdateError::RecoveryRequired)?
        .restarted
    {
        return Ok(());
    }
    #[cfg(feature = "native-calls")]
    morpheus_rtc::install_crypto_provider();
    let ui = AppWindow::new()?;
    ui.set_login_loading(true);
    ui.show()?;
    #[cfg(morpheus_timeline_acceptance)]
    acceptance.mark_window_shown();
    update_session
        .lock()
        .map_err(|_| updater::UpdateError::RecoveryRequired)?
        .acknowledge_before_store()?;
    #[cfg(not(morpheus_timeline_acceptance))]
    let qa_profile = qa_profile_root()?;
    // The run's generated, isolated profile replaces the standard app paths.
    #[cfg(morpheus_timeline_acceptance)]
    let qa_profile = Some(acceptance.profile_root().to_path_buf());
    let data_dir = if let Some(root) = &qa_profile {
        root.join("data")
    } else {
        ProjectDirs::from("io.github", "brk275", "Morpheus")
            .ok_or("No application data directory is available")?
            .data_dir()
            .to_path_buf()
    };
    #[cfg(not(morpheus_timeline_acceptance))]
    let manager = match AccountManager::open(data_dir) {
        Ok(manager) => Arc::new(manager),
        Err(error) => {
            // No account worker, sign-in callback or preview is initialized
            // on this path. Keep the saved-account protection fail-closed.
            ui.hide()?;
            startup_error::show(&error)?;
            return Err("Saved-account startup was blocked".into());
        }
    };
    // A sliding run keeps the shipping automatic policy; a classic run forces
    // the same classic /sync loop that policy falls back to.
    #[cfg(morpheus_timeline_acceptance)]
    let manager = Arc::new(AccountManager::open_with_sync_policy(
        data_dir,
        acceptance.sync_policy(),
    )?);
    let store = if let Some(root) = &qa_profile {
        ConfigStore::at(root.join("config.toml"))
    } else {
        ConfigStore::standard()?
    };
    let voice_spaces = Arc::new(VoiceSpaces::new(store.clone()));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Subscribed before the startup restore below can start a sync.
    #[cfg(morpheus_timeline_acceptance)]
    acceptance.observe_sync(&manager, runtime.handle());
    let live = LiveUi::new(
        manager.clone(),
        runtime.handle().clone(),
        ui.as_weak(),
        voice_spaces.clone(),
    );
    security::register(&ui, live.clone(), runtime.handle().clone());
    let image_previews =
        image_previews::ImagePreviews::register(&ui, live.clone(), store.clone(), runtime.handle());
    let room_settings =
        room_settings::RoomSettingsUi::register(&ui, live.clone(), runtime.handle());
    let room_permissions = room_permissions::RoomPermissionsUi::register(
        &ui,
        live.clone(),
        runtime.handle(),
        manager.clone(),
    );
    let pinned_messages =
        pinned_messages::PinnedMessagesUi::register(&ui, live.clone(), runtime.handle());
    let room_media = room_media_browser::RoomMediaBrowser::register(
        &ui,
        live.clone(),
        runtime.handle(),
        manager.clone(),
    );
    let room_activity =
        room_activity::RoomActivityUi::register(&ui, live.clone(), store.clone(), runtime.handle());
    let device_sessions =
        device_sessions::DeviceSessionsUi::register(&ui, live.clone(), runtime.handle());
    let member_privacy =
        member_privacy::MemberPrivacyUi::register(&ui, live.clone(), runtime.handle());
    ui.set_native_call_enabled(cfg!(feature = "native-calls"));
    ui.set_native_call_status("Select a room to check call availability".into());
    let update_activity = updater::UpdateActivity::default();
    let update_accounts = manager.clone();
    let updates = update_controller::UpdateController::new(
        runtime.handle().clone(),
        update_activity.clone(),
        update_session.clone(),
        live.security_gate.clone(),
        Arc::new(move || update_accounts.account_access_busy()),
    );
    let update_ui_task = updates.bind_ui(&ui);
    #[cfg(feature = "native-calls")]
    let calls = Arc::new(CallController::new(
        manager.clone(),
        update_activity.clone(),
    ));
    #[cfg(feature = "native-calls")]
    let access_calls = calls.clone();
    let account_access = account_access::AccountAccessUi::register(
        &ui,
        live.clone(),
        runtime.handle().clone(),
        move || {
            #[cfg(feature = "native-calls")]
            {
                !access_calls.status().blocks_account_switch()
            }
            #[cfg(not(feature = "native-calls"))]
            {
                true
            }
        },
    );
    #[cfg(feature = "native-calls")]
    let mut incoming_calls =
        incoming_ring::ui::IncomingUi::bind(&ui, runtime.handle(), live.clone(), calls.clone());
    // Leave never tears down an active or ringing call of this account.
    #[cfg(feature = "native-calls")]
    let leave_fence: room_leave::CallFence = {
        let calls = calls.clone();
        let ringing = incoming_calls.ringing_in();
        Arc::new(move |account: &str, room: &str| {
            let active = match calls.status() {
                CallStatus::OpeningSlot {
                    account_id,
                    room_id,
                }
                | CallStatus::Starting {
                    account_id,
                    room_id,
                }
                | CallStatus::Ending {
                    account_id,
                    room_id,
                }
                | CallStatus::CleanupFailed {
                    account_id,
                    room_id,
                    ..
                } => account_id == account && room_id == room,
                CallStatus::Joined { account_id, call }
                | CallStatus::Reconnecting {
                    account_id, call, ..
                } => account_id == account && call.room_id == room,
                _ => false,
            };
            room_leave::RoomCall::of(active, ringing(account, room))
        })
    };
    #[cfg(not(feature = "native-calls"))]
    let leave_fence: room_leave::CallFence =
        Arc::new(|_: &str, _: &str| room_leave::RoomCall::of(false, false));
    let room_leave =
        room_leave::RoomLeaveUi::register(&ui, live.clone(), runtime.handle(), leave_fence);
    #[cfg(feature = "native-calls")]
    let voice_presence =
        voice_rosters::VoiceRosterUi::register(&ui, live.clone(), runtime.handle());
    #[cfg(feature = "native-calls")]
    let applied_audio_token = Arc::new(AtomicU64::new(0));
    #[cfg(feature = "native-calls")]
    {
        let calls = calls.clone();
        let weak = ui.as_weak();
        runtime.spawn(async move {
            let mut status = calls.subscribe_status();
            loop {
                let current = status.borrow_and_update().clone();
                let connected = current.blocks_account_switch();
                let joined = current.controls_available();
                let leave_unconfirmed = matches!(current, CallStatus::CleanupFailed { .. });
                let label = call_status_label(&current);
                let room_id = call_room_id(&current).unwrap_or("").to_owned();
                let calls_for_ui = calls.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if calls_for_ui.status() != current {
                        return;
                    }
                    let room_display = call_room_display(
                        &room_id,
                        ui.get_native_call_room_id().as_str(),
                        ui.get_native_call_room_display().as_str(),
                        ui.get_active_room_id().as_str(),
                        ui.get_active_room_name().as_str(),
                    );
                    ui.set_call_connected(connected);
                    ui.set_native_call_joined(joined);
                    ui.set_native_leave_unconfirmed(leave_unconfirmed);
                    ui.set_native_call_status(label.into());
                    ui.set_native_call_room_id(room_id.into());
                    ui.set_native_call_room_display(room_display.into());
                });
                if status.changed().await.is_err() {
                    break;
                }
            }
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let calls = calls.clone();
        let weak = ui.as_weak();
        runtime.spawn(async move {
            let mut invitation = calls.subscribe_invitation();
            loop {
                let update = *invitation.borrow_and_update();
                let calls_for_ui = calls.clone();
                let closed = weak
                    .upgrade_in_event_loop(move |ui| {
                        // A queued completion cannot overwrite a later join or
                        // restore invitation text after Leave cleared its token.
                        if calls_for_ui.invitation() != update {
                            return;
                        }
                        let text = if update.token == calls_for_ui.video_token() {
                            update.text
                        } else {
                            ""
                        };
                        ui.set_native_invitation_status(text.into());
                    })
                    .is_err();
                if closed || invitation.changed().await.is_err() {
                    break;
                }
            }
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let calls = calls.clone();
        let weak = ui.as_weak();
        let applied_token = applied_audio_token.clone();
        runtime.spawn(async move {
            let mut audio = calls.subscribe_audio();
            loop {
                let update = audio.borrow_and_update().clone();
                let calls_for_ui = calls.clone();
                let applied_token = applied_token.clone();
                let closed = weak
                    .upgrade_in_event_loop(move |ui| {
                        let current = calls_for_ui.audio_token();
                        if update.token() != current {
                            if applied_token.load(Ordering::SeqCst) != current {
                                ui.set_native_audio_ready(false);
                                ui.set_native_audio_failed(false);
                                ui.set_native_microphone_muted(true);
                                ui.set_native_deafened(false);
                                applied_token.store(current, Ordering::SeqCst);
                            }
                            return;
                        }
                        match update {
                            AudioUpdate::Unavailable { .. } => {
                                ui.set_native_audio_ready(false);
                                ui.set_native_audio_failed(false);
                                ui.set_native_microphone_muted(true);
                                ui.set_native_deafened(false);
                            }
                            AudioUpdate::Failed { reason, .. } => {
                                ui.set_native_audio_error(reason.message().into());
                                ui.set_native_audio_ready(false);
                                ui.set_native_audio_failed(true);
                                ui.set_native_microphone_muted(true);
                                ui.set_native_deafened(false);
                            }
                            AudioUpdate::Applied { state, .. } => {
                                ui.set_native_audio_ready(true);
                                ui.set_native_audio_failed(false);
                                ui.set_native_microphone_muted(state.microphone_muted);
                                ui.set_native_deafened(state.deafened);
                            }
                        }
                        applied_token.store(current, Ordering::SeqCst);
                    })
                    .is_err();
                if closed || audio.changed().await.is_err() {
                    break;
                }
            }
        });
    }
    #[cfg(feature = "native-calls")]
    call_stage::bind(&ui, runtime.handle(), calls.clone());
    // The call page names the devices in use and the last route change, only
    // for the call generation on screen. The watch holds only the latest
    // report, so one for another generation means none for this one yet.
    #[cfg(feature = "native-calls")]
    {
        let calls = calls.clone();
        let weak = ui.as_weak();
        runtime.spawn(async move {
            let mut routes = calls.subscribe_audio_route();
            loop {
                let update = routes.borrow_and_update().clone();
                let calls_for_ui = calls.clone();
                let closed = weak
                    .upgrade_in_event_loop(move |ui| {
                        if update.token == 0 || update.token != calls_for_ui.video_token() {
                            ui.set_native_audio_route("".into());
                            ui.set_native_audio_route_notice("".into());
                            ui.set_native_audio_degraded(false);
                            return;
                        }
                        ui.set_native_audio_degraded(route_degraded(&update.route));
                        ui.set_native_audio_route(route_summary(&update.route).into());
                        ui.set_native_audio_route_notice(
                            update.route.notice.map(route_notice).unwrap_or("").into(),
                        );
                    })
                    .is_err();
                if closed || routes.changed().await.is_err() {
                    break;
                }
            }
        });
    }
    // Use for this call: the saved catalog IDs (never a label or a display
    // name) switch the call generation on screen, on this explicit click only.
    // The reply writes the notice only while that call and this click are
    // still current and nothing else has replaced the pending text.
    #[cfg(feature = "native-calls")]
    {
        const SWITCHING: &str = "Switching this call's audio…";
        let calls = calls.clone();
        let weak = ui.as_weak();
        let handle = runtime.handle().clone();
        let clicks = Arc::new(AtomicU64::new(0));
        ui.on_apply_audio_route_to_call(move || {
            let Some(ui) = weak.upgrade() else { return };
            if !ui.get_audio_devices_enabled() || ui.get_audio_devices_busy() {
                return;
            }
            let click = clicks.fetch_add(1, Ordering::SeqCst) + 1;
            // The call itself, not only its generation: a later reply may only
            // claim a rejoin of this same call.
            let call = calls.call_token();
            let token = calls.video_token();
            if token == 0 || call == 0 || !ui.get_native_call_joined() {
                ui.set_audio_device_notice(
                    "No call is in progress, so nothing was switched.".into(),
                );
                return;
            }
            let selection = audio_devices::selection(&ui);
            ui.set_audio_device_notice(SWITCHING.into());
            let calls = calls.clone();
            let weak = weak.clone();
            let clicks = clicks.clone();
            handle.spawn(async move {
                let result = calls.set_audio_route(token, selection).await;
                let accepted = result.is_ok();
                let text = match result {
                    Ok(()) => "Switch requested. The call page shows which devices are in use.",
                    Err(CallControllerError::AccountChanged) => {
                        "The account changed, so this call's audio was not switched."
                    }
                    // Nothing was queued and nothing is retried.
                    Err(CallControllerError::TimedOut(_)) => {
                        "Switching took too long, so this call's audio was not changed. Choose Use for this call again to retry."
                    }
                    Err(_) => "This call cannot switch audio right now, so nothing was changed.",
                };
                let calls_for_ui = calls.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if clicks.load(Ordering::SeqCst) != click
                        || ui.get_audio_device_notice().as_str() != SWITCHING
                    {
                        return;
                    }
                    // The pending text always resolves, and once the call on
                    // screen changed, only to what is still true of this call.
                    let scope = route_reply_scope(
                        token,
                        call,
                        calls_for_ui.video_token(),
                        calls_for_ui.call_token(),
                    );
                    let text = route_reply_notice(scope, accepted, text);
                    ui.set_audio_device_notice(text.into());
                });
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let calls = calls.clone();
        let weak = ui.as_weak();
        let applied_token = Arc::new(AtomicU64::new(0));
        runtime.spawn(async move {
            let mut capture = calls.subscribe_local_video();
            loop {
                let update = capture.borrow_and_update().clone();
                let calls_for_ui = calls.clone();
                let applied_token = applied_token.clone();
                let closed = weak
                    .upgrade_in_event_loop(move |ui| {
                        let current = calls_for_ui.video_token();
                        if update.token != current {
                            if applied_token.load(Ordering::SeqCst) != current {
                                clear_native_capture(&ui);
                                applied_token.store(current, Ordering::SeqCst);
                            }
                            return;
                        }
                        if update.token == 0 {
                            clear_native_capture(&ui);
                        } else {
                            apply_capture_status(&ui, update);
                        }
                        applied_token.store(current, Ordering::SeqCst);
                    })
                    .is_err();
                if closed || capture.changed().await.is_err() {
                    break;
                }
            }
        });
    }
    runtime.spawn(live.clone().watch_updates(ui.as_weak()));
    runtime.spawn(live.clone().watch_sync(ui.as_weak()));
    ui.set_config_path(store.path().display().to_string().into());
    let initial_config = match store.load_or_create() {
        Ok(config) => {
            voice_spaces.apply_config(&config);
            match store.load_theme(&config) {
                Ok(theme) => apply_config(&ui, &config, theme.as_ref()),
                Err(error) => {
                    apply_config(&ui, &config, None);
                    ui.set_config_error(error.to_string().into());
                }
            }
            config
        }
        Err(error) => {
            apply_config(&ui, &Config::default(), None);
            ui.set_config_error(error.to_string().into());
            Config::default()
        }
    };
    shortcuts::register(&ui, store.clone());
    audio_devices::register(&ui, store.clone(), runtime.handle());
    let notifications = DesktopNotifications::start(
        manager.clone(),
        initial_config,
        ui.as_weak(),
        runtime.handle(),
    );
    let _room_organizer = room_organization::register(&ui, live.clone(), runtime.handle().clone());
    let _notification_controls = notification_center::register(
        &ui,
        live.clone(),
        notifications.clone(),
        store.clone(),
        runtime.handle(),
    );

    let watch = store.watch_with_theme(Duration::from_millis(700));
    let weak = ui.as_weak();
    let watch_live = live.clone();
    let watch_notifications = notifications.clone();
    let watch_handle = runtime.handle().clone();
    let watch_store = store.clone();
    thread::spawn(move || {
        while watch.recv().is_ok() {
            // The watcher result can be older than a Settings save. Read the
            // latest file on the UI event loop, where Settings callbacks also
            // run, so an older notification policy cannot overwrite a newer
            // privacy choice. Theme errors must not block a valid policy.
            let current_store = watch_store.clone();
            let current_notifications = watch_notifications.clone();
            let current_live = watch_live.clone();
            let current_weak = weak.clone();
            let current_handle = watch_handle.clone();
            let closed = weak
                .upgrade_in_event_loop(move |ui| match current_store.load() {
                    Ok(config) => {
                        current_notifications.apply_config(&config);
                        apply_notification_settings(&ui, &config);
                        shortcuts::apply_config(&ui, &config);
                        audio_devices::apply_config(&ui, &config);
                        image_previews::apply_config(&ui, &config);
                        room_activity::apply_config(&ui, &config);
                        current_live.voice_spaces.apply_config(&config);
                        match current_store.load_theme(&config) {
                            Ok(theme) => apply_loaded_config(
                                &ui,
                                &LoadedConfig {
                                    config: config.clone(),
                                    theme,
                                },
                            ),
                            Err(error) => ui.set_config_error(error.to_string().into()),
                        }
                        let live = current_live.clone();
                        let weak = current_weak.clone();
                        current_handle.spawn(async move {
                            let _ = live.refresh(weak).await;
                        });
                    }
                    Err(error) => {
                        ui.set_config_error(error.to_string().into());
                    }
                })
                .is_err();
            if closed {
                break;
            }
        }
    });
    {
        let access = account_access.clone();
        runtime.spawn(async move { access.restore().await });
    }

    let state = Rc::new(RefCell::new(fixtures()));
    apply_state(&ui, &state.borrow());

    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let call_live = live.clone();
        let handle = runtime.handle().clone();
        ui.on_open_native_call_slot(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_live_session()
                || window.get_call_connected()
                || window.get_native_call_request_pending()
                || window.get_room_call_mode() == RoomCallMode::ElementCallLegacy.index()
            {
                return;
            }
            let account_id = window.get_active_account_local_id().to_string();
            let room_id = window.get_active_room_id().to_string();
            if account_id.is_empty() || room_id.is_empty() {
                return;
            }
            let Some(ring_request) = calls.reserve_ringing_request() else {
                return;
            };
            window.set_native_call_request_pending(true);
            let calls = calls.clone();
            let call_live = call_live.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                let _transition = call_live.account_transition.lock().await;
                let result = calls.open_slot(&account_id, &room_id).await;
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    drop(ring_request);
                    ui.set_native_call_request_pending(false);
                    if ui.get_active_account_local_id().as_str() != account_id
                        || ui.get_active_room_id().as_str() != room_id
                        || ui.get_room_call_mode() == RoomCallMode::ElementCallLegacy.index()
                    {
                        return;
                    }
                    match result {
                        Ok(()) => ui.set_native_call_status(
                            "Room call opened · join audio when ready".into(),
                        ),
                        Err(error) => ui.set_session_error(error.to_string().into()),
                    }
                });
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let applied_audio_token = applied_audio_token.clone();
        let handle = runtime.handle().clone();
        ui.on_toggle_native_microphone(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_native_audio_ready() {
                return;
            }
            let token = applied_audio_token.load(Ordering::SeqCst);
            let muted = !window.get_native_microphone_muted();
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                if let Err(error) = calls.set_microphone_muted(token, muted).await {
                    if !matches!(error, CallControllerError::StaleCall) {
                        show_error(&weak, error.to_string());
                    }
                }
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let handle = runtime.handle().clone();
        let listing = Arc::new(AtomicBool::new(false));
        ui.on_refresh_native_capture_sources(move |picker_kind| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_live_session()
                || !window.get_native_call_joined()
                || !window.get_native_capture_picker_open()
                || !matches!(picker_kind, 0 | 1)
            {
                return;
            }
            if listing.swap(true, Ordering::AcqRel) {
                window.set_native_capture_source_error("Device discovery is still running".into());
                return;
            }
            let token = calls.video_token();
            window.set_native_capture_source_loading(true);
            window.set_native_capture_source_error(SharedString::default());
            window.set_native_capture_sources(ModelRc::new(VecModel::from(Vec::<
                CallCaptureSourceItem,
            >::new())));
            let listing = listing.clone();
            let weak = weak.clone();
            let calls = calls.clone();
            handle.spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(12),
                    tokio::task::spawn_blocking(move || {
                        let result = capture_sources(picker_kind);
                        listing.store(false, Ordering::Release);
                        result
                    }),
                )
                .await;
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if calls.video_token() != token
                        || !ui.get_native_call_joined()
                        || !ui.get_native_capture_picker_open()
                        || ui.get_native_capture_picker_kind() != picker_kind
                    {
                        return;
                    }
                    ui.set_native_capture_source_loading(false);
                    match result {
                        Ok(Ok(Ok(sources))) => {
                            let rows: Vec<_> = sources
                                .into_iter()
                                .map(|(kind, id, name)| CallCaptureSourceItem {
                                    kind,
                                    id: id.into(),
                                    name: name.into(),
                                })
                                .collect();
                            if rows.is_empty() {
                                ui.set_native_capture_source_error(
                                    "No sources are available".into(),
                                );
                            }
                            ui.set_native_capture_sources(ModelRc::new(VecModel::from(rows)));
                        }
                        Ok(Ok(Err(error))) => ui.set_native_capture_source_error(error.into()),
                        Ok(Err(_)) => ui.set_native_capture_source_error(
                            "Device discovery stopped unexpectedly".into(),
                        ),
                        Err(_) => ui.set_native_capture_source_error(
                            "Device discovery is still running in the OS; wait before refreshing"
                                .into(),
                        ),
                    }
                });
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let handle = runtime.handle().clone();
        ui.on_start_native_capture(move |kind, id| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_live_session() || !window.get_native_call_joined() {
                return;
            }
            let id = id.to_string();
            let model = window.get_native_capture_sources();
            let selected = (0..model.row_count())
                .filter_map(|index| model.row_data(index))
                .find(|source| source.kind == kind && source.id.as_str() == id);
            let Some(selected) = selected else {
                window
                    .set_native_capture_error("Capture source changed; refresh the picker".into());
                return;
            };
            let source = match kind {
                0 => CaptureSource::Camera(id),
                1 => CaptureSource::Display(id),
                2 => CaptureSource::Window(id),
                _ => return,
            };
            let token = calls.video_token();
            if token == 0 {
                return;
            }
            let label = safe_capture_label(selected.name.as_str());
            window.set_native_capture_error(SharedString::default());
            if kind == 0 {
                window.set_native_camera_busy(true);
            } else {
                window.set_native_screen_busy(true);
            }
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                if let Err(error) = calls.start_video_capture(token, source, label).await {
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        if calls.video_token() == token {
                            if kind == 0 {
                                ui.set_native_camera_busy(false);
                            } else {
                                ui.set_native_screen_busy(false);
                            }
                            ui.set_native_capture_error(
                                safe_capture_label(&error.to_string()).into(),
                            );
                        }
                    });
                }
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let handle = runtime.handle().clone();
        ui.on_stop_native_capture(move |kind| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_live_session() || !window.get_native_call_joined() {
                return;
            }
            let capture_kind = match kind {
                0 => CaptureKind::Camera,
                1 => CaptureKind::Screen,
                _ => return,
            };
            let token = calls.video_token();
            if token == 0 {
                return;
            }
            window.set_native_capture_error(SharedString::default());
            if kind == 0 {
                window.set_native_camera_busy(true);
            } else {
                window.set_native_screen_busy(true);
            }
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                if let Err(error) = calls.stop_video_capture(token, capture_kind).await {
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        if calls.video_token() == token {
                            if kind == 0 {
                                ui.set_native_camera_busy(false);
                            } else {
                                ui.set_native_screen_busy(false);
                            }
                            ui.set_native_capture_error(
                                safe_capture_label(&error.to_string()).into(),
                            );
                        }
                    });
                }
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let applied_audio_token = applied_audio_token.clone();
        let handle = runtime.handle().clone();
        ui.on_toggle_native_deafen(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_native_audio_ready() {
                return;
            }
            let token = applied_audio_token.load(Ordering::SeqCst);
            let deafened = !window.get_native_deafened();
            let calls = calls.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                if let Err(error) = calls.set_deafened(token, deafened).await {
                    if !matches!(error, CallControllerError::StaleCall) {
                        show_error(&weak, error.to_string());
                    }
                }
            });
        });
    }
    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let calls = calls.clone();
        let call_live = live.clone();
        let handle = runtime.handle().clone();
        let start_call = move |intent: CallStartIntent| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !window.get_live_session()
                || window.get_call_connected()
                || window.get_native_call_request_pending()
            {
                return;
            }
            let account_id = window.get_active_account_local_id().to_string();
            let room_id = window.get_active_room_id().to_string();
            if account_id.is_empty() || room_id.is_empty() {
                return;
            }
            let Some(selection) = call_live.manager.account_selection(&account_id) else {
                return;
            };
            // Read the account-scoped checked preferences, not a potentially
            // queued UI snapshot. The worker owns this immutable choice.
            let mode =
                CallJoinMode::from(call_live.voice_spaces.room_call_mode(&account_id, &room_id));
            if intent == CallStartIntent::Notify
                && (mode == CallJoinMode::ElementCallRoomState
                    || window.get_active_voice_local()
                    || window.get_active_voice_shared())
            {
                return;
            }
            let audio_selection = audio_devices::selection(&window);
            let Some(ring_request) = calls.reserve_ringing_request() else {
                return;
            };
            window.set_native_call_request_pending(true);
            let calls = calls.clone();
            let call_live = call_live.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                let _transition = call_live.account_transition.lock().await;
                let local_voice = call_live
                    .voice_spaces
                    .local_rooms(&account_id)
                    .contains(&room_id);
                // A live preference edit made while this request waited must
                // not turn voice-space entry into an invitation.
                let intent = if local_voice {
                    CallStartIntent::Quiet
                } else {
                    intent
                };
                let result = calls
                    .start_audio(
                        selection.clone(),
                        &room_id,
                        mode,
                        audio_selection,
                        intent,
                        call_live.voice_spaces.clone(),
                    )
                    .await;
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    drop(ring_request);
                    ui.set_native_call_request_pending(false);
                    if selection.is_current()
                        && ui.get_active_account_local_id().as_str() == account_id
                        && ui.get_active_room_id().as_str() == room_id
                        && let Err(error) = result
                    {
                        ui.set_session_error(error.to_string().into());
                    }
                });
            });
        };
        let join_quietly = start_call.clone();
        ui.on_start_native_call(move || join_quietly(CallStartIntent::Quiet));
        ui.on_notify_native_call(move || start_call(CallStartIntent::Notify));
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
    let switch_access = account_access.clone();
    #[cfg(feature = "native-calls")]
    let switch_calls = calls.clone();
    ui.on_switch_account(move |index| {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_cancel_room_settings();
            ui.invoke_close_image_preview();
            space_explorer::clear(&ui);
            space_management::clear(&ui);
            room_addresses::clear(&ui);
            matrix_links::clear(&ui);
            thread_browser::withdraw(&ui);
        }
        if switch_live.is_live.load(Ordering::Acquire) {
            if let Some(window) = weak.upgrade()
                && security::block_account_change(&window, &switch_live.security_gate)
            {
                return;
            }
            #[cfg(feature = "native-calls")]
            if ask_to_leave_call(&weak, &switch_calls, index) {
                return;
            }
            if let Some(window) = weak.upgrade() {
                if let Some(account) = window
                    .get_account_options()
                    .iter()
                    .find(|a| a.index == index)
                {
                    if !account.local_id.is_empty() {
                        switch_access.request(account.local_id.to_string());
                    }
                }
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
        if let Some(ui) = weak.upgrade() {
            room_addresses::clear(&ui);
            ui.invoke_cancel_room_settings();
            ui.invoke_close_image_preview();
        }
        if room_live.is_live.load(Ordering::Acquire) {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let account_id = window.get_active_account_local_id().to_string();
            if account_id.is_empty() {
                return;
            }
            window.invoke_cancel_room_settings();
            let navigation_epoch = room_live.invalidate_navigation();
            message_search::room_changed(&window, false);
            let live = room_live.clone();
            let ui = weak.clone();
            room_handle.spawn(async move {
                if let Err(error) = live
                    .select_room_for(
                        &account_id,
                        room_id.to_string(),
                        navigation_epoch,
                        ui.clone(),
                    )
                    .await
                {
                    if live.navigation_is_current(navigation_epoch) {
                        let _ = ui.upgrade_in_event_loop(move |window| {
                            if window.get_active_account_local_id().as_str() == account_id {
                                window.set_session_error(error.to_string().into());
                            }
                        });
                    }
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
    let voice_live = live.clone();
    let voice_handle = runtime.handle().clone();
    #[cfg(feature = "native-calls")]
    let voice_calls = calls.clone();
    ui.on_open_voice_space(move |room_id| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !voice_live.is_live.load(Ordering::Acquire) {
            window.invoke_open_room(room_id);
            matrix_links::clear(&window);
            thread_browser::withdraw(&window);
            window.set_page(2);
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = room_id.to_string();
        if account_id.is_empty() || room_id.is_empty() {
            return;
        }
        #[cfg(feature = "native-calls")]
        {
            let status = voice_calls.status();
            if status.blocks_account_switch() && call_room_id(&status) != Some(room_id.as_str()) {
                window.set_session_error(
                    "Leave the current call before entering another voice space".into(),
                );
                return;
            }
        }
        window.invoke_cancel_room_settings();
        let navigation_epoch = voice_live.invalidate_navigation();
        message_search::room_changed(&window, false);
        let live = voice_live.clone();
        let weak = weak.clone();
        #[cfg(feature = "native-calls")]
        let calls = voice_calls.clone();
        voice_handle.spawn(async move {
            let result = live
                .select_room_for(&account_id, room_id.clone(), navigation_epoch, weak.clone())
                .await;
            match result {
                Ok(true) => {
                    let _ = weak.upgrade_in_event_loop(move |window| {
                        if window.get_active_account_local_id().as_str() != account_id
                            || window.get_active_room_id().as_str() != room_id
                            || !live.navigation_is_current(navigation_epoch)
                        {
                            return;
                        }
                        matrix_links::clear(&window);
                        thread_browser::withdraw(&window);
                        window.set_page(2);
                        #[cfg(feature = "native-calls")]
                        if !calls.status().blocks_account_switch() {
                            // Joining publishes membership in the room slot;
                            // it does not ring every room member.
                            window.invoke_start_native_call();
                        }
                    });
                }
                Ok(false) => {}
                Err(error) if live.navigation_is_current(navigation_epoch) => {
                    show_error(&weak, error.to_string());
                }
                Err(_) => {}
            }
        });
    });

    let weak = ui.as_weak();
    let local_voice_live = live.clone();
    let local_voice_handle = runtime.handle().clone();
    ui.on_set_local_voice_space(move |enabled| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !local_voice_live.is_live.load(Ordering::Acquire) || window.get_voice_designation_busy()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = window.get_active_room_id().to_string();
        if account_id.is_empty() || room_id.is_empty() {
            return;
        }
        match local_voice_live
            .voice_spaces
            .set_local(&account_id, &room_id, enabled)
        {
            Ok(_) => {
                window.set_active_voice_local(enabled);
                window.set_voice_designation_error("".into());
                window.set_voice_designation_notice(if enabled {
                    "Marked as a voice space for this account only".into()
                } else {
                    "Local voice-space label removed".into()
                });
                let live = local_voice_live.clone();
                let ui = weak.clone();
                local_voice_handle.spawn(async move {
                    let _ = live.refresh(ui).await;
                });
            }
            Err(error) => window.set_voice_designation_error(error.to_string().into()),
        }
    });

    let weak = ui.as_weak();
    let mode_live = live.clone();
    #[cfg(feature = "native-calls")]
    let mode_calls = calls.clone();
    ui.on_choose_room_call_mode(move |index| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !mode_live.is_live.load(Ordering::Acquire)
            || window.get_call_connected()
            || window.get_native_call_request_pending()
        {
            return;
        }
        #[cfg(feature = "native-calls")]
        if mode_calls.status().blocks_account_switch() {
            return;
        }
        let Some(mode) = RoomCallMode::from_index(index) else {
            return;
        };
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = window.get_active_room_id().to_string();
        if account_id.is_empty() || room_id.is_empty() {
            return;
        }
        match mode_live
            .voice_spaces
            .set_room_call_mode(&account_id, &room_id, mode)
        {
            Ok(()) => window.set_room_call_mode(mode.index()),
            Err(error) => window.set_session_error(
                directory_plain_text(&format!("Could not save room call mode: {error}"), 240)
                    .into(),
            ),
        }
    });

    #[cfg(feature = "native-calls")]
    {
        let weak = ui.as_weak();
        let shared_voice_live = live.clone();
        let shared_voice_handle = runtime.handle().clone();
        ui.on_set_shared_voice_space(move |enabled| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !shared_voice_live.is_live.load(Ordering::Acquire)
                || window.get_voice_designation_busy()
            {
                return;
            }
            let account_id = window.get_active_account_local_id().to_string();
            let room_id = window.get_active_room_id().to_string();
            if account_id.is_empty() || room_id.is_empty() {
                return;
            }
            let operation = window.get_voice_designation_operation().wrapping_add(1);
            window.set_voice_designation_operation(operation);
            window.set_voice_designation_busy(true);
            window.set_voice_designation_error("".into());
            window.set_voice_designation_notice("".into());
            let live = shared_voice_live.clone();
            let weak = weak.clone();
            shared_voice_handle.spawn(async move {
                let transition = live.account_transition.lock().await;
                let result = if live.is_selected_room_for(&account_id, &room_id).await {
                    live.voice_spaces
                        .set_shared(&live.manager, &account_id, &room_id, enabled)
                        .await
                } else {
                    Err("The selected Matrix room changed".to_owned())
                };
                drop(transition);
                let refresh = result.is_ok();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if window.get_active_account_local_id().as_str() != account_id
                        || window.get_active_room_id().as_str() != room_id
                        || window.get_voice_designation_operation() != operation
                    {
                        return;
                    }
                    window.set_voice_designation_busy(false);
                    match result {
                        Ok(()) => {
                            window.set_active_voice_shared(enabled);
                            window.set_voice_designation_notice(if enabled {
                                "Shared voice-space label saved for room members".into()
                            } else {
                                "Shared voice-space label removed".into()
                            });
                        }
                        Err(error) => window.set_voice_designation_error(error.into()),
                    }
                });
                if refresh {
                    let _ = live.refresh(weak).await;
                }
            });
        });
    }

    let weak = ui.as_weak();
    let history_live = live.clone();
    let history_handle = runtime.handle().clone();
    ui.on_load_older_messages(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !window.get_live_session()
            || window.get_history_loading()
            || window.get_history_at_start()
        {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = window.get_active_room_id().to_string();
        if account_id.is_empty() || room_id.is_empty() {
            return;
        }
        // Bind the click before spawning: account IDs alone cannot detect
        // switching away and back before the worker first polls.
        let Some(selection) = history_live.manager.account_selection(&account_id) else {
            return;
        };
        // A conversation focused on an older message pages exactly the view
        // shown at this click, never a view loaded or shown since.
        let focused = if window.get_conversation_focused() {
            let Some(view) = history_live.focused_view(&room_id) else {
                return;
            };
            Some(view)
        } else {
            None
        };
        let operation = window.get_history_operation().wrapping_add(1);
        window.set_history_operation(operation);
        window.set_history_loading(true);
        window.set_history_error("".into());
        let live = history_live.clone();
        let weak = weak.clone();
        history_handle.spawn(async move {
            let manager = &live.manager;
            let result = if let Some(view) = &focused {
                manager
                    .paginate_focused_selected(&selection, view, true, 50)
                    .await
            } else {
                manager
                    .paginate_backwards_selected(&selection, &room_id, 50)
                    .await
            };
            let _ = weak.upgrade_in_event_loop(move |window| {
                if !selection.is_current()
                    || window.get_active_account_local_id().as_str() != account_id
                    || window.get_active_room_id().as_str() != room_id
                    || window.get_history_operation() != operation
                {
                    return;
                }
                window.set_history_loading(false);
                match result {
                    Ok(start_reached) => window.set_history_at_start(start_reached),
                    Err(_) => window.set_history_error(
                        "Could not load earlier history. Check sync and retry.".into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let members_live = live.clone();
    let members_handle = runtime.handle().clone();
    ui.on_refresh_room_members(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !members_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        let room_id = window.get_active_room_id().to_string();
        if account_id.is_empty() || room_id.is_empty() {
            return;
        }
        let live = members_live.clone();
        let ui = weak.clone();
        members_handle.spawn(async move {
            live.refresh_members_for(account_id, room_id, ui, true)
                .await;
        });
    });

    // Member actions and message redaction, each owned by what it was
    // confirmed in (moderation_ui); the room name and topic of the shared
    // admin callback stay with the room settings form.
    let room_text = room_settings.clone();
    let moderation = moderation_ui::ModerationUi::register(
        &ui,
        live.clone(),
        runtime.handle(),
        manager.clone(),
        move |window, kind, value| room_text.save_text(window, kind, value),
    );

    let weak = ui.as_weak();
    ui.on_open_discovery(move || {
        if let Some(window) = weak.upgrade() {
            if window.get_live_session() {
                window.invoke_query_public_rooms(false);
            }
        }
    });

    message_search::register(&ui, live.clone(), runtime.handle().clone());
    space_explorer::register(&ui, live.clone(), runtime.handle().clone());
    matrix_links::register(&ui, live.clone(), runtime.handle().clone());
    space_management::register(&ui, live.clone(), runtime.handle().clone());
    room_addresses::register(&ui, live.clone(), runtime.handle().clone());
    thread_history::register(&ui, live.clone(), runtime.handle().clone());
    thread_browser::register(&ui, live.clone(), runtime.handle().clone());
    account_identity::register(&ui, live.clone(), runtime.handle().clone());
    people_directory::register(&ui, live.clone(), runtime.handle().clone());

    let weak = ui.as_weak();
    let directory_live = live.clone();
    let directory_handle = runtime.handle().clone();
    ui.on_query_public_rooms(move |append| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !directory_live.is_live.load(Ordering::Acquire) {
            window.set_directory_error("Sign in to search public rooms".into());
            return;
        }
        if window.get_directory_loading() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            window.set_directory_error("Choose a Matrix account first".into());
            return;
        }
        let since = if append {
            let next = window.get_directory_next_batch().to_string();
            if next.is_empty() {
                return;
            }
            Some(next)
        } else {
            window
                .set_directory_results(ModelRc::new(VecModel::from(Vec::<PublicRoomItem>::new())));
            window.set_directory_next_batch("".into());
            window.set_directory_searched(false);
            None
        };
        let search = window.get_directory_search().trim().to_owned();
        let server = window.get_directory_server().trim().to_owned();
        let route_server = server.clone();
        let query = PublicRoomsQuery {
            search_term: (!search.is_empty()).then_some(search),
            server: (!server.is_empty()).then_some(server),
            since,
            limit: 25,
        };
        let operation = window.get_directory_operation().wrapping_add(1);
        window.set_directory_operation(operation);
        window.set_directory_error("".into());
        window.set_directory_loading(true);
        let manager = directory_live.manager.clone();
        let weak = weak.clone();
        directory_handle.spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                manager.public_rooms_for(&account_id, query),
            )
            .await
            .map_err(|_| "Public room search timed out".to_owned())
            .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = weak.upgrade_in_event_loop(move |window| {
                if window.get_active_account_local_id().as_str() != account_id
                    || window.get_directory_operation() != operation
                {
                    return;
                }
                window.set_directory_loading(false);
                window.set_directory_searched(true);
                match result {
                    Ok(page) => {
                        let mut rooms = if append {
                            window.get_directory_results().iter().collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };
                        let mut seen = rooms
                            .iter()
                            .map(|room| room.id.to_string())
                            .collect::<HashSet<_>>();
                        for room in page.rooms {
                            let item = public_room_item(room, &route_server);
                            if seen.insert(item.id.to_string()) {
                                rooms.push(item);
                            }
                        }
                        window.set_directory_results(ModelRc::new(VecModel::from(rooms)));
                        window.set_directory_next_batch(page.next_batch.unwrap_or_default().into());
                    }
                    Err(error) => window.set_directory_error(error.into()),
                }
            });
        });
    });

    // A directory join has no Cancel button, but leaving its page or
    // replacing it with a protected modal still withdraws this exact owner.
    let directory_join: JoinSlot = Arc::default();
    let room_join: JoinSlot = Arc::default();
    register_join_cancellation(&ui, room_join.clone(), directory_join.clone());
    let weak = ui.as_weak();
    let directory_live = live.clone();
    let directory_handle = runtime.handle().clone();
    let directory_slot = directory_join.clone();
    ui.on_join_discovery_target(
        move |input, canonical_alias, route_server, _display_name, _is_space| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if !directory_live.is_live.load(Ordering::Acquire) {
                window.set_directory_join_error("Sign in to join a Matrix room".into());
                return;
            }
            if window.get_directory_join_busy() {
                return;
            }
            if !window.get_discovery_context_allowed()
                || window.get_page() != 3
                || window.get_room_actions_open()
                || space_explorer::is_open(&window)
                || directory_live.security_gate.blocked()
            {
                return;
            }
            // A complete Matrix user ID opens Start a conversation for that
            // person; nothing is joined or created from here.
            if let Some(user) = morpheus_matrix::exact_user_id(&input) {
                window.invoke_open_people(user.into());
                return;
            }
            let account_id = window.get_active_account_local_id().to_string();
            // The exact account shown at the click: an account switch before
            // the request is sent cancels it, and a switch back never revives it.
            let Some(selection) = directory_live
                .manager
                .account_selection(&account_id)
                .filter(morpheus_matrix::AccountSelection::is_current)
            else {
                window.set_directory_join_error("Choose a Matrix account first".into());
                return;
            };
            let target = match room_join_target(&input, &canonical_alias, &route_server) {
                Ok(target) => target,
                Err(error) => {
                    window.set_directory_join_error(error.into());
                    return;
                }
            };
            // The navigation and security generations at this click.
            let asked = directory_live.join_asked();
            let operation = window.get_directory_join_operation().wrapping_add(1);
            let previous = directory_slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .replace((operation, asked.clone()));
            if let Some((_, previous)) = previous {
                previous.withdraw();
            }
            window.set_directory_join_operation(operation);
            window.set_directory_join_error("".into());
            window.set_directory_join_busy(true);
            window.set_directory_joining_room_id(input.clone());
            // Whether it is a Space is read from sync once it is joined, not
            // from the directory's display fields.
            let live = directory_live.clone();
            let weak = weak.clone();
            let directory_slot = directory_slot.clone();
            directory_handle.spawn(async move {
                // This caller bound includes SDK metadata setup as well as
                // the final POST. Its classification follows SDK admission,
                // which cannot establish that bytes reached the wire.
                let result = tokio::time::timeout(
                    Duration::from_secs(60),
                    live.join_target_for(&selection, &target, &asked, weak.clone()),
                )
                .await
                .map_err(|_| asked.expired().to_string())
                .and_then(|result| result.map(|_| ()).map_err(|error| error.to_string()));
                let _ = weak.upgrade_in_event_loop(move |window| {
                    let mut slot = directory_slot
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if slot.as_ref().is_some_and(|(owner, _)| *owner == operation) {
                        *slot = None;
                    }
                    drop(slot);
                    if window.get_active_account_local_id().as_str() != account_id
                        || window.get_directory_join_operation() != operation
                    {
                        return;
                    }
                    window.set_directory_join_busy(false);
                    window.set_directory_joining_room_id("".into());
                    match result {
                        Ok(()) => {
                            window.set_directory_join_input("".into());
                            if window.get_page() == 3 {
                                window.set_page(0);
                            }
                        }
                        Err(error) => window.set_directory_join_error(error.into()),
                    }
                });
            });
        },
    );

    let weak = ui.as_weak();
    let action_live = live.clone();
    let action_handle = runtime.handle().clone();
    let join_slot = room_join.clone();
    ui.on_submit_room_action(move |mode, input| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !action_live.is_live.load(Ordering::Acquire) {
            window.set_room_action_error("Sign in to a Matrix account first".into());
            return;
        }
        if window.get_room_action_busy() {
            return;
        }
        let action = match mode {
            0 => Some(RoomAction::CreatePrivate),
            1 => None,
            2 => Some(RoomAction::CreateSpace),
            _ => return,
        };
        if action.is_none()
            && (!window.get_discovery_context_allowed()
                || !window.get_room_actions_open()
                || space_explorer::is_open(&window)
                || action_live.security_gate.blocked())
        {
            return;
        }
        // Joining by a user's Matrix ID means talking to them: open Start a
        // conversation for that exact person instead of failing.
        if action.is_none()
            && let Some(user) = morpheus_matrix::exact_user_id(&input)
        {
            window.set_room_actions_open(false);
            window.invoke_open_people(user.into());
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            window.set_room_action_error("Choose a Matrix account first".into());
            return;
        }
        let operation = window.get_room_action_operation().wrapping_add(1);
        window.set_room_action_operation(operation);
        window.set_room_action_busy(true);
        window.set_room_action_error("".into());
        let live = action_live.clone();
        let weak = weak.clone();
        let submitted = input.to_string();
        // A join is bound to the exact account shown at the click, and to
        // the navigation and security generations at it.
        let selection = action_live
            .manager
            .account_selection(&account_id)
            .filter(morpheus_matrix::AccountSelection::is_current);
        let asked = action.is_none().then(|| action_live.join_asked());
        if let Some(asked) = &asked {
            *join_slot.lock().unwrap_or_else(|error| error.into_inner()) =
                Some((operation, asked.clone()));
        }
        let join_slot = join_slot.clone();
        action_handle.spawn(async move {
            let result = match (action, asked) {
                (Some(action), _) => live
                    .run_room_action(&account_id, action, &submitted, weak.clone())
                    .await
                    .map_err(|error| error.to_string()),
                (None, asked) => match (room_join_target(&submitted, "", ""), selection, asked) {
                    (Ok(target), Some(selection), Some(asked)) => live
                        .join_target_for(&selection, &target, &asked, weak.clone())
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    (Ok(_), _, _) => Err(morpheus_matrix::JoinError::AccountChanged.to_string()),
                    (Err(error), _, _) => Err(error),
                },
            };
            let _ = weak.upgrade_in_event_loop(move |window| {
                if window.get_active_account_local_id().as_str() != account_id
                    || window.get_room_action_operation() != operation
                {
                    return;
                }
                let mut slot = join_slot.lock().unwrap_or_else(|error| error.into_inner());
                if slot.as_ref().is_some_and(|(owner, _)| *owner == operation) {
                    *slot = None;
                }
                drop(slot);
                window.set_room_action_busy(false);
                match result {
                    Ok(()) => {
                        window.set_room_actions_open(false);
                        window.set_room_action_input("".into());
                        window.set_page(0);
                    }
                    Err(error) => window.set_room_action_error(error.into()),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let invite_live = live.clone();
    let invite_handle = runtime.handle().clone();
    ui.on_respond_invite(move |room_id, accept| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !invite_live.is_live.load(Ordering::Acquire) {
            window.set_invite_error("Sign in to respond to invitations".into());
            return;
        }
        if window.get_room_action_busy() {
            return;
        }
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        let operation = window.get_room_action_operation().wrapping_add(1);
        window.set_room_action_operation(operation);
        window.set_room_action_busy(true);
        window.set_invite_error("".into());
        let live = invite_live.clone();
        let weak = weak.clone();
        invite_handle.spawn(async move {
            let result = live
                .respond_to_invite(&account_id, &room_id, accept, weak.clone())
                .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if window.get_active_account_local_id().as_str() != account_id
                    || window.get_room_action_operation() != operation
                {
                    return;
                }
                window.set_room_action_busy(false);
                if let Err(error) = result {
                    window.set_invite_error(error.to_string().into());
                }
            });
        });
    });

    let weak = ui.as_weak();
    let space_live = live.clone();
    let space_handle = runtime.handle().clone();
    ui.on_select_space(move |space_id| {
        if !space_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(window) = weak.upgrade() else {
            return;
        };
        let account_id = window.get_active_account_local_id().to_string();
        if account_id.is_empty() {
            return;
        }
        window.invoke_cancel_room_settings();
        let navigation_epoch = space_live.invalidate_navigation();
        let live = space_live.clone();
        let ui = weak.clone();
        space_handle.spawn(async move {
            if let Err(error) = live
                .select_space_for(
                    &account_id,
                    space_id.to_string(),
                    navigation_epoch,
                    ui.clone(),
                )
                .await
            {
                if live.navigation_is_current(navigation_epoch) {
                    let _ = ui.upgrade_in_event_loop(move |window| {
                        if window.get_active_account_local_id().as_str() == account_id {
                            window.set_session_error(error.to_string().into());
                        }
                    });
                }
            }
        });
    });

    let composer_ui =
        composer_mentions::ui::ComposerUi::install(&ui, live.clone(), runtime.handle().clone());
    // Callbacks own the controller; picker state ends with its selection.
    let _stickers = stickers::register(&ui, live.clone(), runtime.handle().clone());
    polls::register(&ui, live.clone(), runtime.handle().clone());

    // Its callbacks hold the attachment workbench for the window's lifetime.
    let _attachments =
        attachment_workbench::Workbench::register(&ui, live.clone(), runtime.handle().clone());

    let weak = ui.as_weak();
    let send_state = state.clone();
    let send_live = live.clone();
    let send_handle = runtime.handle().clone();
    let send_composers = composer_ui.clone();
    let link_weak = ui.as_weak();
    let link_live = live.clone();
    let link_handle = runtime.handle().clone();
    ui.on_open_message_link(move |link| {
        // A Matrix link opens inside Morpheus and never reaches the browser
        // by itself; any other link keeps the external-link confirmation.
        if let Some(window) = link_weak.upgrade()
            && matrix_links::open(&window, &link_live, &link_handle, link.as_str())
        {
            return;
        }
        confirm_external_link(link.as_str());
    });
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
        let markdown = ui.get_composer_markdown();
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
            // A sent message appears in the live conversation.
            ui.invoke_return_to_live();
            send_composers.send(&ui, &send_live, &send_handle, false);
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
                    markdown,
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
                let error_account_id = account_id.clone();
                let error_room_id = room_id.clone();
                if let Err(error) = live
                    .toggle_reaction(account_id, room_id, event_id, reaction)
                    .await
                {
                    let message = directory_plain_text(&error.to_string(), 240);
                    let _ = weak.upgrade_in_event_loop(move |window| {
                        if window.get_active_account_local_id().as_str() == error_account_id
                            && window.get_active_room_id().as_str() == error_room_id
                        {
                            window.set_session_error(message.into());
                        }
                    });
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
            let draft = thread_live.composed_draft(&account_id, &room_id, Some(&event_id));
            ui.set_thread_composer_text(draft.body.into());
            ui.set_thread_markdown(draft.format == TextFormat::Markdown);
            let summary = thread_live.pending_summary(&account_id, &room_id, Some(&event_id));
            let items = thread_live.pending_items(&account_id, &room_id, Some(&event_id));
            draft_recovery::publish_thread(&ui, &summary, &items);
            if summary.unconfirmed {
                ui.set_transfer_notice(
                    "A reply here may not have finished sending before Morpheus closed. Check the thread, then choose.".into(),
                );
            } else if summary.quarantined || summary.read_only {
                ui.set_transfer_notice(
                    "Saved send status for this thread needs review before replying.".into(),
                );
            } else if summary.count > 0 {
                ui.set_transfer_notice(
                    "A thread reply is still waiting to send; it may still send".into(),
                );
            }
            let live = thread_live.clone();
            let generation = live.begin_thread_request();
            let weak = weak.clone();
            thread_handle.spawn(async move {
                let error_account_id = account_id.clone();
                let error_room_id = room_id.clone();
                let error_live = live.clone();
                if let Err(error) = live
                    .open_thread(account_id, room_id, event_id, generation, weak.clone())
                    .await
                {
                    let message = directory_plain_text(&error.to_string(), 240);
                    let _ = weak.upgrade_in_event_loop(move |window| {
                        if error_live.thread_request_is_current(generation)
                            && window.get_active_account_local_id().as_str() == error_account_id
                            && window.get_active_room_id().as_str() == error_room_id
                        {
                            window.set_session_error(message.into());
                        }
                    });
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

    // Saved-send controls act on the exact record, selection and edit
    // revision the user clicked on, all captured synchronously here; every
    // later step re-checks them before changing anything.
    let weak = ui.as_weak();
    let pending_live = live.clone();
    let pending_handle = runtime.handle().clone();
    ui.on_cancel_pending_send(move |threaded, key| {
        let Some(ui) = weak.upgrade() else { return };
        if !pending_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(nonce) = Generation::from_key(&key) else {
            return;
        };
        let Some(selection) = pending_live.account_selection(&scope.account) else {
            return;
        };
        let Some(target) = pending_live.request_target(&scope, &nonce) else {
            return;
        };
        let live = pending_live.clone();
        let weak = weak.clone();
        pending_handle.spawn(async move {
            live.cancel_pending_send(selection, scope, target, weak)
                .await
        });
    });
    let weak = ui.as_weak();
    let retry_live = live.clone();
    let retry_handle = runtime.handle().clone();
    ui.on_retry_pending_send(move |threaded, key| {
        let Some(ui) = weak.upgrade() else { return };
        if !retry_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(nonce) = Generation::from_key(&key) else {
            return;
        };
        let Some(selection) = retry_live.account_selection(&scope.account) else {
            return;
        };
        let Some(target) = retry_live.request_target(&scope, &nonce) else {
            return;
        };
        let live = retry_live.clone();
        let weak = weak.clone();
        retry_handle.spawn(async move {
            live.retry_pending_send(selection, scope, target, weak)
                .await
        });
    });
    let weak = ui.as_weak();
    let review_live = live.clone();
    let review_handle = runtime.handle().clone();
    ui.on_review_unconfirmed_send(move |threaded, key, sent| {
        let Some(ui) = weak.upgrade() else { return };
        if !review_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(nonce) = Generation::from_key(&key) else {
            return;
        };
        let Some(selection) = review_live.account_selection(&scope.account) else {
            return;
        };
        let live = review_live.clone();
        let weak = weak.clone();
        review_handle.spawn(async move {
            live.review_unconfirmed_send(selection, scope, nonce, sent, weak)
                .await
        });
    });
    let weak = ui.as_weak();
    let restore_live = live.clone();
    let restore_handle = runtime.handle().clone();
    ui.on_restore_record_text(move |threaded, key| {
        let Some(ui) = weak.upgrade() else { return };
        if !restore_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(nonce) = Generation::from_key(&key) else {
            return;
        };
        let Some(selection) = restore_live.account_selection(&scope.account) else {
            return;
        };
        let empty = if threaded {
            ui.get_thread_composer_text().is_empty()
        } else {
            ui.get_composer_text().is_empty()
        };
        let Some(revision) =
            restore_live.scoped_revision(&scope.account, &scope.room, scope.thread())
        else {
            return;
        };
        if !empty {
            ui.set_transfer_notice(
                "Clear the composer first; saved text never replaces what you typed.".into(),
            );
            return;
        }
        let live = restore_live.clone();
        let weak = weak.clone();
        restore_handle.spawn(async move {
            live.restore_record_text(selection, scope, nonce, revision, weak)
                .await
        });
    });
    let weak = ui.as_weak();
    let copy_live = live.clone();
    ui.on_copy_record_text(move |threaded, key| {
        let Some(ui) = weak.upgrade() else { return };
        if !copy_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(nonce) = Generation::from_key(&key) else {
            return;
        };
        let Some(text) = copy_live.record_text(&scope, &nonce) else {
            return;
        };
        let copied = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text.body))
            .is_ok();
        ui.set_transfer_notice(
            if copied {
                "Message text copied to the system clipboard."
            } else {
                "Could not access the clipboard. Use Restore text with an empty composer instead."
            }
            .into(),
        );
    });
    let weak = ui.as_weak();
    let discard_live = live.clone();
    let discard_handle = runtime.handle().clone();
    ui.on_discard_record_text(move |threaded, key| {
        let Some(ui) = weak.upgrade() else { return };
        if !discard_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(nonce) = Generation::from_key(&key) else {
            return;
        };
        let Some(selection) = discard_live.account_selection(&scope.account) else {
            return;
        };
        let live = discard_live.clone();
        let weak = weak.clone();
        discard_handle
            .spawn(async move { live.discard_kept_text(selection, scope, nonce, weak).await });
    });
    let weak = ui.as_weak();
    let unreadable_live = live.clone();
    let unreadable_handle = runtime.handle().clone();
    ui.on_discard_unreadable_status(move |threaded| {
        let Some(ui) = weak.upgrade() else { return };
        if !unreadable_live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(scope) = draft_recovery::current_scope(&ui, threaded) else {
            return;
        };
        let Some(selection) = unreadable_live.account_selection(&scope.account) else {
            return;
        };
        let live = unreadable_live.clone();
        let weak = weak.clone();
        unreadable_handle
            .spawn(async move { live.discard_unreadable_status(selection, scope, weak).await });
    });

    let weak = ui.as_weak();
    let thread_send_state = state.clone();
    let thread_send_live = live.clone();
    let thread_send_handle = runtime.handle().clone();
    let thread_send_composers = composer_ui.clone();
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
            thread_send_composers.send(&ui, &thread_send_live, &thread_send_handle, true);
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
    let add_security_gate = live.security_gate.clone();
    #[cfg(feature = "native-calls")]
    let add_calls = calls.clone();
    ui.on_add_account(move || {
        if weak.upgrade().is_some_and(|ui| room_leave::is_open(&ui)) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            room_addresses::clear(&ui);
        }
        if let Some(window) = weak.upgrade()
            && security::block_account_change(&window, &add_security_gate)
        {
            return;
        }
        #[cfg(feature = "native-calls")]
        if ask_to_leave_call(&weak, &add_calls, -2) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            matrix_links::clear(&ui);
            thread_browser::withdraw(&ui);
            ui.set_login_error("".into());
            ui.set_show_sign_in(true);
        }
    });

    let weak = ui.as_weak();
    let preview_live = live.clone();
    let preview_handle = runtime.handle().clone();
    let preview_state = state.clone();
    #[cfg(feature = "native-calls")]
    let preview_calls = calls.clone();
    let ready_ui = ui.as_weak();
    ui.on_preview_demo_ready(move || {
        if let Some(ui) = ready_ui.upgrade() {
            ui.set_login_error("".into());
            ui.set_session_error("".into());
            ui.set_login_loading(false);
            ui.set_show_sign_in(false);
            ui.set_composer_text("".into());
            apply_state(&ui, &preview_state.borrow());
        }
    });
    ui.on_preview_demo(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_login_loading() {
                return;
            }
            if security::block_account_change(&ui, &preview_live.security_gate) {
                return;
            }
            ui.set_login_loading(true);
            let live = preview_live.clone();
            let weak = weak.clone();
            #[cfg(feature = "native-calls")]
            let calls = preview_calls.clone();
            preview_handle.spawn(async move {
                let _transition = live.account_transition.lock().await;
                #[cfg(feature = "native-calls")]
                if calls.status().blocks_account_switch() {
                    show_login_error(
                        &weak,
                        "Leave the active call before opening the preview.".into(),
                    );
                    return;
                }
                let Some(security) = live.security_gate.try_account_access() else {
                    show_login_error(
                        &weak,
                        "Finish the current security operation before opening the preview.".into(),
                    );
                    return;
                };
                if let Err(error) = live.flush_drafts().await {
                    show_login_error(&weak, format!("Could not save current draft: {error}"));
                    return;
                }
                live.clear_feed().await;
                live.manager.stop().await;
                live.account_detached(weak.clone()).await;
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    // Retain the reservation through publication of the preview
                    // so a queued activation cannot be overwritten by this view.
                    let _security = security;
                    ui.invoke_preview_demo_ready();
                });
            });
        }
    });

    let weak = ui.as_weak();
    let login_live = live.clone();
    let login_handle = runtime.handle().clone();
    let login_access = account_access.clone();
    #[cfg(feature = "native-calls")]
    let login_calls = calls.clone();
    ui.on_submit_login(move |homeserver, username, password| {
        if let Some(window) = weak.upgrade()
            && security::block_account_change(&window, &login_live.security_gate)
        {
            window.set_login_loading(false);
            return;
        }
        if let Some(window) = weak.upgrade() {
            window.invoke_cancel_room_settings();
        }
        login_live.invalidate_navigation();
        let live = login_live.clone();
        let security_gate = login_live.security_gate.clone();
        let ui = weak.clone();
        #[cfg(feature = "native-calls")]
        let calls = login_calls.clone();
        let access = login_access.clone();
        login_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if security_gate.blocked() {
                show_login_error(
                    &ui,
                    "Finish the current security operation or save its recovery key first".into(),
                );
                return;
            }
            #[cfg(feature = "native-calls")]
            if calls.status().blocks_account_switch() {
                show_login_error(
                    &ui,
                    "Leave the active call before signing in to another account".into(),
                );
                return;
            }
            let Some(_security) = security_gate.try_account_access() else {
                show_login_error(
                    &ui,
                    "Finish the current security operation before signing in.".into(),
                );
                return;
            };
            if let Err(error) = live.flush_drafts().await {
                show_login_error(&ui, format!("Could not save current draft: {error}"));
                return;
            }
            match live
                .manager
                .add_password_account(&homeserver, &username, &password)
                .await
            {
                Ok(account) => {
                    access.refresh_accounts().await;
                    if live
                        .manager
                        .active_account()
                        .await
                        .as_ref()
                        .map(|a| a.id.as_str())
                        != Some(account.id.as_str())
                    {
                        access.request(account.id);
                        let _ = ui.upgrade_in_event_loop(|window| {
                            window.set_login_loading(false);
                            window.set_login_error(
                                "Account saved. Enter your app password to open it.".into(),
                            );
                        });
                        return;
                    }
                    if let Err(error) = live.account_activated(ui.clone()).await {
                        show_login_error(&ui, error.to_string());
                    } else {
                        let _ = ui.upgrade_in_event_loop(|window| {
                            window.set_login_loading(false);
                            window.set_login_error("".into());
                            window.set_show_sign_in(false);
                        });
                    }
                }
                Err(error) => {
                    access.refresh_accounts().await;
                    let mut message = error.to_string();
                    if !live.manager.app_password_status().await.pending_removals.is_empty() {
                        message.push_str(" Some saved accounts need cleanup. Open Manage accounts and choose Retry removal.");
                    }
                    show_login_error(&ui, message);
                }
            }
        });
    });

    let weak = ui.as_weak();
    let sso_live = live.clone();
    let sso_handle = runtime.handle().clone();
    let sso_access = account_access.clone();
    #[cfg(feature = "native-calls")]
    let sso_calls = calls.clone();
    ui.on_submit_sso(move |homeserver| {
        if let Some(window) = weak.upgrade()
            && security::block_account_change(&window, &sso_live.security_gate)
        {
            window.set_login_loading(false);
            return;
        }
        if let Some(window) = weak.upgrade() {
            window.invoke_cancel_room_settings();
        }
        sso_live.invalidate_navigation();
        let live = sso_live.clone();
        let security_gate = sso_live.security_gate.clone();
        let ui = weak.clone();
        #[cfg(feature = "native-calls")]
        let calls = sso_calls.clone();
        let access = sso_access.clone();
        sso_handle.spawn(async move {
            let _transition = live.account_transition.lock().await;
            if security_gate.blocked() {
                show_login_error(
                    &ui,
                    "Finish the current security operation or save its recovery key first".into(),
                );
                return;
            }
            #[cfg(feature = "native-calls")]
            if calls.status().blocks_account_switch() {
                show_login_error(
                    &ui,
                    "Leave the active call before signing in to another account".into(),
                );
                return;
            }
            let Some(_security) = security_gate.try_account_access() else {
                show_login_error(
                    &ui,
                    "Finish the current security operation before signing in.".into(),
                );
                return;
            };
            if let Err(error) = live.flush_drafts().await {
                show_login_error(&ui, format!("Could not save current draft: {error}"));
                return;
            }
            match sso::sign_in(live.manager.clone(), homeserver.to_string()).await {
                Ok(account) => {
                    access.refresh_accounts().await;
                    if live
                        .manager
                        .active_account()
                        .await
                        .as_ref()
                        .map(|a| a.id.as_str())
                        != Some(account.id.as_str())
                    {
                        access.request(account.id);
                        let _ = ui.upgrade_in_event_loop(|window| {
                            window.set_login_loading(false);
                            window.set_login_error(
                                "Account saved. Enter your app password to open it.".into(),
                            );
                        });
                        return;
                    }
                    if let Err(error) = live.account_activated(ui.clone()).await {
                        show_login_error(&ui, error.to_string());
                    } else {
                        let _ = ui.upgrade_in_event_loop(|window| {
                            window.set_login_loading(false);
                            window.set_login_error("".into());
                            window.set_show_sign_in(false);
                        });
                    }
                }
                Err(error) => {
                    access.refresh_accounts().await;
                    let mut message = error;
                    if !live.manager.app_password_status().await.pending_removals.is_empty() {
                        message.push_str(" Some saved accounts need cleanup. Open Manage accounts and choose Retry removal.");
                    }
                    show_login_error(&ui, message);
                }
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

    // Bind logout at the first click, including while a leave-call dialog is
    // open. A resumed intent never captures whichever account is current later.
    let pending_logout = Rc::new(RefCell::new(None::<morpheus_matrix::AccountSelection>));
    let weak = ui.as_weak();
    let logout_live = live.clone();
    let logout_handle = runtime.handle().clone();
    let logout_access = account_access.clone();
    #[cfg(feature = "native-calls")]
    let logout_calls = calls.clone();
    let perform_logout = Rc::new(move |selection: morpheus_matrix::AccountSelection| {
        logout_live.invalidate_navigation();
        let live = logout_live.clone();
        let security_gate = logout_live.security_gate.clone();
        let ui = weak.clone();
        #[cfg(feature = "native-calls")]
        let calls = logout_calls.clone();
        let access = logout_access.clone();
        logout_handle.spawn(async move {
            let transition = live.account_transition.lock().await;
            if security_gate.blocked() {
                let _ = ui.upgrade_in_event_loop(move |window| {
                    security::block_account_change(&window, &security_gate);
                });
                return;
            }
            #[cfg(feature = "native-calls")]
            if calls.status().blocks_account_switch() {
                // A new call began after the click or confirmation. Do not
                // reopen a dialog and accidentally replace its captured intent.
                show_error(&ui, "Leave the active call before logging out.".into());
                return;
            }
            let Some(_security) = security_gate.try_account_access() else { return; };
            let account_id = selection.account_id().to_owned();
            // The core checks selection identity atomically before any logout
            // effect. A queued A -> B (including A -> B -> A) cannot retarget it.
            let result = live.manager.logout_selected_account(&selection).await;
            if matches!(&result, Err(morpheus_matrix::MatrixCoreError::SelectedAccountChanged)) {
                show_error(&ui, "The account changed. Open its menu and request logout again.".into());
                return;
            }
            if live.manager.active_account().await.is_none() {
                live.account_detached(ui.clone()).await;
            }
            access.refresh_accounts().await;
            drop(transition);
            match result {
                Ok(result) => {
                    room_addresses::forget_account(&account_id);
                    if let Err(error) = live.voice_spaces.clear_logged_out_account(&account_id) {
                        show_error(&ui, format!("Could not remove local voice labels: {error}"));
                    }
                    live.voice_spaces.forget_account(&account_id).await;
                    live.forget_account_drafts(&account_id);
                    room_permissions::forget_account(&account_id);
                    room_leave::forget_account(&account_id);
                    room_settings::forget_access_account(&account_id);
                    space_management::forget_account(&account_id);
                    live.forget_removed_account_joins(&account_id);
                    live.clear_feed().await;
                    live.account_detached(ui.clone()).await;
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        ui.set_account_access_notice(if result.remote_logout_succeeded {
                            "Logged out and removed from this app."
                        } else {
                            "Removed locally, but homeserver logout was not confirmed. Revoke this device from another client."
                        }.into());
                    });
                }
                Err(error) => show_error(&ui, error.to_string()),
            }
        });
    });
    let weak = ui.as_weak();
    let logout_live = live.clone();
    let logout_pending = pending_logout.clone();
    let logout_perform = perform_logout.clone();
    #[cfg(feature = "native-calls")]
    let logout_calls = calls.clone();
    ui.on_log_out(move || {
        // A fresh click supersedes any dismissed/old confirmation.
        logout_pending.borrow_mut().take();
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.invoke_cancel_room_settings();
        window.invoke_close_image_preview();
        if !logout_live.is_live.load(Ordering::Acquire) {
            if window.get_call_connected() {
                window.set_pending_account_index(-3);
                window.set_leave_call_dialog(true);
            } else {
                window.set_show_sign_in(true);
            }
            return;
        }
        if security::block_account_change(&window, &logout_live.security_gate) {
            return;
        }
        let visible_id = window.get_active_account_local_id();
        let Some(selection) = logout_live.manager.account_selection(visible_id.as_str()) else {
            show_error(
                &weak,
                "The account changed. Open its menu and request logout again.".into(),
            );
            return;
        };
        #[cfg(feature = "native-calls")]
        if ask_to_leave_call(&weak, &logout_calls, -3) {
            *logout_pending.borrow_mut() = Some(selection);
            return;
        }
        logout_perform(selection);
    });
    let weak = ui.as_weak();
    let resume_live = live.clone();
    let resume_pending = pending_logout.clone();
    ui.on_continue_log_out(move || {
        let selection = resume_pending.borrow_mut().take();
        if !resume_live.is_live.load(Ordering::Acquire) {
            // The offline call preview has no Matrix account or destructive work.
            if let Some(window) = weak.upgrade() {
                window.set_show_sign_in(true);
            }
            return;
        }
        let Some(selection) = selection else {
            show_error(
                &weak,
                "The account changed. Open its menu and request logout again.".into(),
            );
            return;
        };
        perform_logout(selection);
    });
    ui.on_cancel_pending_log_out(move || {
        pending_logout.borrow_mut().take();
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
                let result = if matches!(calls.status(), CallStatus::CleanupFailed { .. }) {
                    calls.abandon_failed_leave().await
                } else {
                    calls.end_call().await
                }
                .map_err(|error| error.to_string());
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
    appearance::install_callbacks(&ui, &store);
    workspace_layout::register_live(&ui, &store, &live);
    appearance_editor::register(&ui, &store);
    metric_editor::register(&ui, &store);
    sender_palette_editor::register(&ui, &store);
    style_profiles::register(&ui, &store);
    theme_studio::register(&ui, &store);
    appearance::register_room_hook(&ui);
    settings_search::register(&ui);
    ui.on_choose_theme(move |index| {
        if let Some(ui) = weak.upgrade() {
            let theme = match index {
                0 => ThemeMode::Dark,
                1 => ThemeMode::Light,
                2 => ThemeMode::HighContrast,
                3 => ThemeMode::System,
                _ => return,
            };
            let _ = save_config_change(&ui, &setting_store, |config| {
                config.appearance.theme = theme;
                config.appearance.theme_pack = None;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_change_font_scale(move |percent| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &setting_store, |config| {
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
            let _ = save_config_change(&ui, &setting_store, |config| {
                config.appearance.density = density;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_set_sort_unread_first(move |sort_unread_first| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &setting_store, |config| {
                config.layout.sort_unread_first = sort_unread_first;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_set_side_panel(move |visible| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &setting_store, |config| {
                config.layout.show_member_panel = visible;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_change_room_list_width(move |width| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &setting_store, |config| {
                config.layout.room_list_width = width.clamp(180, 480) as u16;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    ui.on_change_details_width(move |width| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &setting_store, |config| {
                config.layout.details_width = width.clamp(200, 520) as u16;
            });
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    let setting_notifications = notifications.clone();
    let setting_voice_spaces = voice_spaces.clone();
    ui.on_set_notifications_enabled(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            save_notification_config_change(
                &ui,
                &setting_store,
                &setting_notifications,
                &setting_voice_spaces,
                |config| {
                    config.notifications.enabled = enabled;
                },
            );
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    let setting_notifications = notifications.clone();
    let setting_voice_spaces = voice_spaces.clone();
    ui.on_set_notification_sounds(move |sounds| {
        if let Some(ui) = weak.upgrade() {
            save_notification_config_change(
                &ui,
                &setting_store,
                &setting_notifications,
                &setting_voice_spaces,
                |config| {
                    config.notifications.sounds = sounds;
                },
            );
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    let setting_notifications = notifications.clone();
    let setting_voice_spaces = voice_spaces.clone();
    ui.on_set_notification_show_preview(move |show_preview| {
        if let Some(ui) = weak.upgrade() {
            save_notification_config_change(
                &ui,
                &setting_store,
                &setting_notifications,
                &setting_voice_spaces,
                |config| {
                    config.notifications.show_message_preview = show_preview;
                },
            );
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    let setting_notifications = notifications.clone();
    let setting_voice_spaces = voice_spaces.clone();
    ui.on_add_highlight_word(move |input| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let word = match normalize_highlight_word(input.as_str()) {
            Ok(word) => word,
            Err(error) => {
                ui.set_notification_highlight_error(error.into());
                return;
            }
        };
        let listed = ui.get_notification_highlight_words();
        if listed.row_count() >= MAX_HIGHLIGHT_WORDS {
            ui.set_notification_highlight_error("Add at most 32 highlights.".into());
            return;
        }
        if listed
            .iter()
            .any(|existing| existing.as_str().to_lowercase() == word.to_lowercase())
        {
            ui.set_notification_highlight_error("That highlight is already in the list.".into());
            return;
        }
        let saved = save_notification_config_change(
            &ui,
            &setting_store,
            &setting_notifications,
            &setting_voice_spaces,
            |config| {
                if config.notifications.highlight_words.len() < MAX_HIGHLIGHT_WORDS
                    && !config
                        .notifications
                        .highlight_words
                        .iter()
                        .any(|existing| existing.to_lowercase() == word.to_lowercase())
                {
                    config.notifications.highlight_words.push(word.clone());
                }
            },
        );
        if saved {
            if ui
                .get_notification_highlight_words()
                .iter()
                .any(|existing| existing.as_str().to_lowercase() == word.to_lowercase())
            {
                ui.set_notification_highlight_input("".into());
                ui.set_notification_highlight_error("".into());
            } else {
                ui.set_notification_highlight_error("Add at most 32 highlights.".into());
            }
        }
    });

    let weak = ui.as_weak();
    let setting_store = store.clone();
    let setting_notifications = notifications.clone();
    let setting_voice_spaces = voice_spaces.clone();
    ui.on_remove_highlight_word(move |word| {
        if let Some(ui) = weak.upgrade() {
            let saved = save_notification_config_change(
                &ui,
                &setting_store,
                &setting_notifications,
                &setting_voice_spaces,
                |config| {
                    config
                        .notifications
                        .highlight_words
                        .retain(|existing| existing != word.as_str());
                },
            );
            if saved {
                ui.set_notification_highlight_error("".into());
            }
        }
    });

    #[cfg(morpheus_timeline_acceptance)]
    let acceptance = acceptance.start(&ui, live.clone(), runtime.handle().clone())?;
    let ui_result = ui.run();
    device_sessions.shutdown(&ui);
    member_privacy.shutdown(&ui);
    pinned_messages.shutdown(&ui);
    room_media.shutdown(&ui);
    room_activity.shutdown(&ui);
    room_permissions.shutdown(&ui);
    room_settings.shutdown(&ui);
    room_leave.shutdown(&ui);
    moderation.shutdown();
    image_previews.shutdown(&ui);
    #[cfg(feature = "native-calls")]
    voice_presence.shutdown(&ui);
    #[cfg(feature = "native-calls")]
    incoming_calls.shutdown();
    notifications.shutdown();
    update_ui_task.abort();
    let update_result = runtime.block_on(async {
        let _ = update_ui_task.await;
        // A cancelled download may still be waiting for its bounded network
        // timeout. Stop call capture concurrently, never after that wait.
        #[cfg(feature = "native-calls")]
        {
            let (update_result, call_result, ()) = tokio::join!(
                updates.shutdown(),
                calls.end_call(),
                incoming_calls.finish()
            );
            if let Err(error) = call_result {
                eprintln!("Could not leave native call during shutdown: {error}");
            }
            update_result
        }
        #[cfg(not(feature = "native-calls"))]
        updates.shutdown().await
    });
    let draft_result = runtime.block_on(live.flush_drafts());
    runtime.block_on(manager.stop());
    if let Err(error) = update_result {
        // Static diagnostic only; all other app shutdown work still completes.
        eprintln!("Could not finish update shutdown safely: {error}");
    }
    ui_result?;
    if let Err(error) = draft_result {
        eprintln!("Could not save drafts during shutdown: {error}");
        return Err(Box::new(error));
    }
    #[cfg(morpheus_timeline_acceptance)]
    acceptance.finish()?;
    Ok(())
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn highlight_editor_normalizes_input_and_rejects_hidden_controls() {
        assert_eq!(
            normalize_highlight_word("  release   candidate  "),
            Ok("release candidate".into())
        );
        assert_eq!(normalize_highlight_word(" café "), Ok("café".into()));
        assert!(normalize_highlight_word("   ").is_err());
        assert!(normalize_highlight_word("urgent\nnoise").is_err());
        assert!(normalize_highlight_word("safe\u{202e}evil").is_err());
        assert!(normalize_highlight_word("zero\u{200b}width").is_err());
        assert!(normalize_highlight_word(&"x".repeat(65)).is_err());
    }

    #[test]
    fn timeline_link_handler_accepts_only_browser_urls() {
        assert_eq!(
            safe_message_url("https://example.org/path"),
            Some("https://example.org/path".into())
        );
        assert_eq!(safe_message_url("javascript:alert(1)"), None);
        assert_eq!(safe_message_url("https://user:secret@example.org/"), None);
        assert_eq!(safe_message_url("https://example.org/\nInjected"), None);
    }

    #[cfg(feature = "native-calls")]
    #[test]
    fn call_stage_uses_joined_room_even_after_chat_navigation() {
        let call = morpheus_rtc::CallInfo {
            room_id: "!room-a:example.org".into(),
            slot_id: "m.call#room".into(),
            membership_id: "membership".into(),
            local_identity: "alice".into(),
        };
        let status = CallStatus::Joined {
            account_id: "account-a".into(),
            call,
        };
        let selected_chat_room = "!room-b:example.org";
        assert_ne!(call_room_id(&status), Some(selected_chat_room));
        assert_eq!(call_room_id(&status), Some("!room-a:example.org"));
        assert_eq!(
            call_room_display(
                "!room-a:example.org",
                "",
                "",
                "!room-a:example.org",
                "Morpheus stage QA"
            ),
            "Morpheus stage QA"
        );
        assert_eq!(
            call_room_display(
                "!room-a:example.org",
                "!room-a:example.org",
                "Morpheus stage QA",
                selected_chat_room,
                "Other room"
            ),
            "Morpheus stage QA"
        );
    }

    #[test]
    fn join_target_preserves_link_routes_and_rejects_user_links() {
        let target = room_join_target(
            "https://matrix.to/#/%23lobby:example.org?via=one.org",
            "",
            "two.org",
        )
        .unwrap();
        assert_eq!(
            target,
            MatrixLinkTarget::Room {
                room_or_alias_id: "#lobby:example.org".into(),
                event_id: None,
                via_servers: vec!["one.org".into(), "two.org".into()],
            }
        );
        assert!(room_join_target("@alice:example.org", "", "").is_err());
        assert_eq!(
            room_join_target("!room:example.org:8448", "#alias:alias.org", "").unwrap(),
            MatrixLinkTarget::Room {
                room_or_alias_id: "!room:example.org:8448".into(),
                event_id: None,
                via_servers: vec!["alias.org".into(), "example.org:8448".into()],
            }
        );
    }

    #[test]
    fn directory_metadata_is_bounded_plain_text() {
        assert_eq!(directory_plain_text("Room\nName", 32), "Room Name");
        assert_eq!(
            directory_plain_text("safe\u{202e}evil\u{200b}text", 32),
            "safe evil text"
        );
        assert_eq!(directory_plain_text(&"a".repeat(500), 120).len(), 120);
    }
}
