//! Declarative appearance bridge shared by live sessions and offline previews.

use std::sync::{Mutex, PoisonError};

use morpheus_config::{
    BuiltInStylePreset, Config, ConfigStore, ConversationHeaderMode, MessageLayout, NavigationMode,
    NicknameAlignment, ThemeMode, ThemePack, TimestampFormat,
};
use slint::fontique_011::fontique;
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{
    AppWindow, BaseColorSet, ColorRole, StyleTokens, ThemeRoleSet, color_from_hex,
    save_config_change,
};

/// Theme-mode preferences of the last configuration this window accepted.
/// Only a successfully loaded configuration replaces them, so a rejected
/// external edit keeps the last valid choice. The account they apply to is
/// the window's active account ID, which the live controller publishes only
/// after a successful activation and clears whenever no account is active;
/// a wrong password, a cancelled or stale switch never changes it.
static THEME_SOURCE: Mutex<Option<Config>> = Mutex::new(None);

/// Record the last valid configuration's theme preferences.
pub(crate) fn remember_theme_preferences(config: &Config) {
    *THEME_SOURCE.lock().unwrap_or_else(PoisonError::into_inner) = Some(config.clone());
}

/// Show the theme mode configured for the window's active account, or the
/// global mode when that account has no override or no account is active.
/// The Settings picker shows the global choice separately, with a notice
/// while the active account overrides it.
pub(crate) fn apply_theme_mode(ui: &AppWindow) {
    let account = ui.get_active_account_local_id();
    let projection = {
        let source = THEME_SOURCE.lock().unwrap_or_else(PoisonError::into_inner);
        theme_projection_for(source.as_ref(), account.as_str())
    };
    ui.set_theme_mode(theme_mode_index(projection.effective));
    ui.set_global_theme_mode(theme_mode_index(projection.global));
    ui.set_account_theme_notice(projection.notice.into());
}

#[derive(Debug, PartialEq, Eq)]
struct ThemeModeProjection {
    /// The palette the window shows.
    effective: ThemeMode,
    /// The global `appearance.theme`, which the Settings picker edits.
    global: ThemeMode,
    /// Empty unless the active account overrides the global choice.
    notice: String,
}

fn theme_projection_for(source: Option<&Config>, account_id: &str) -> ThemeModeProjection {
    let default = Config::default();
    let config = source.unwrap_or(&default);
    let global = config.theme_for(None);
    let effective = config.theme_for(Some(account_id));
    let overridden = !account_id.is_empty()
        && config
            .account_overrides
            .get(account_id)
            .is_some_and(|overrides| overrides.theme.is_some());
    ThemeModeProjection {
        effective,
        global,
        notice: if overridden {
            format!(
                "This account overrides it with {} in config.toml.",
                theme_mode_name(effective)
            )
        } else {
            String::new()
        },
    }
}

fn resolve_theme_mode(source: Option<&Config>, account_id: &str) -> ThemeMode {
    theme_projection_for(source, account_id).effective
}

fn theme_mode_name(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Dark => "Dark",
        ThemeMode::Light => "Light",
        ThemeMode::HighContrast => "High contrast",
        ThemeMode::System => "System",
    }
}

/// The `theme-mode` index the window palette switches on.
pub(crate) fn theme_mode_index(mode: ThemeMode) -> i32 {
    match mode {
        ThemeMode::Dark => 0,
        ThemeMode::Light => 1,
        ThemeMode::HighContrast => 2,
        ThemeMode::System => 3,
    }
}

/// Apply a complete projection every time, including clearing omitted overrides.
/// Sender colors are a live UI model: existing message rows need no rebuilding.
/// A `[colors]` override in `config.toml` wins over the theme pack, which wins
/// over each control's built-in color.
pub(super) fn apply_theme(ui: &AppWindow, config: &Config, theme: Option<&ThemePack>) {
    let (roles, senders) = theme_projection(config, theme);
    let tokens = ui.global::<StyleTokens>();
    tokens.set_roles(roles);
    tokens.set_base(base_projection(config));
    ui.set_sender_palette(ModelRc::new(VecModel::from(senders)));
}

fn color_role(value: Option<&str>) -> ColorRole {
    value.map_or_else(ColorRole::default, |value| ColorRole {
        present: true,
        color: color_from_hex(value),
    })
}

fn base_projection(config: &Config) -> BaseColorSet {
    let base = |id: &str| color_role(config.colors.get(id).map(String::as_str));
    BaseColorSet {
        rail: base("rail"),
        sidebar: base("sidebar"),
        canvas: base("canvas"),
        panel: base("panel"),
        elevated: base("elevated"),
        hover: base("hover"),
        ink: base("ink"),
        muted: base("muted"),
        divider: base("divider"),
        accent: base("accent"),
        accent_ink: base("accent_ink"),
    }
}

fn theme_projection(
    config: &Config,
    theme: Option<&ThemePack>,
) -> (ThemeRoleSet, Vec<slint::Color>) {
    let role = |id: &str| {
        let value = config
            .colors
            .get(id)
            .map(String::as_str)
            .or_else(|| theme.and_then(|theme| theme.roles.get(id)));
        color_role(value)
    };
    let senders = theme
        .and_then(|theme| theme.senders.colors.as_deref())
        .unwrap_or_default()
        .iter()
        .map(|value| color_from_hex(value))
        .collect();
    (
        ThemeRoleSet {
            error: role("error"),
            error_surface: role("error_surface"),
            error_ink: role("error_ink"),
            success: role("success"),
            success_surface: role("success_surface"),
            success_ink: role("success_ink"),
            selection: role("selection"),
            selection_ink: role("selection_ink"),
            focus: role("focus"),
            link: role("link"),
            unread: role("unread"),
            unread_ink: role("unread_ink"),
            destructive: role("destructive"),
            destructive_ink: role("destructive_ink"),
            voice: role("voice"),
            timestamp: role("timestamp"),
            sender: role("sender"),
            message: role("message"),
            own_message: role("own_message"),
            own_surface: role("own_surface"),
            scrim: role("scrim"),
            shadow: role("shadow"),
            media_backdrop: role("media_backdrop"),
            presence_online: role("presence_online"),
            avatar_ink: role("avatar_ink"),
        },
        senders,
    )
}

/// Fixed status for the window transparency control. Support depends on the
/// window system; nothing here can observe whether a compositor blends it.
fn transparency_status(percent: u8) -> &'static str {
    if percent >= 100 {
        "Opaque. Lower the value to let the desktop show through the window's panes."
    } else if cfg!(target_os = "macos") {
        "Applied to the window as you change it. Not yet verified on this build."
    } else if cfg!(target_os = "windows") {
        "Requested from Windows. Whether the desktop shows through depends on desktop composition and the graphics driver. Not yet verified on this build."
    } else {
        "Requested from the window system. Wayland compositors, and X11 with a running compositing manager, can show the desktop; without one the panes may look black or stay opaque. Not yet verified on this build."
    }
}

/// Stable identity token, independent of message text, palette length and theme.
/// Use the existing avatar hash's polynomial with an explicit 64-bit width,
/// then retain 31 bits so Slint's signed model index is always nonnegative.
/// Avatar colors remain unchanged and are never used as the custom palette.
pub(super) fn sender_palette_index(sender: &str) -> i32 {
    let hash = sender.bytes().fold(0u64, |current, byte| {
        current.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    (hash & 0x7fff_ffff) as i32
}

/// Slint 1.18.1 treats font-family strings as named families, including generic
/// words. Resolve our documented generic names through the renderer's own
/// system-font collection before assigning them to Text, StyledText and inputs.
/// This runs on the UI thread after AppWindow creation. No fonts are downloaded
/// or registered, and the user's portable config string is never rewritten.
fn display_font_family(requested: &str) -> slint::SharedString {
    font_family_with_resolver(requested, |generic| {
        let mut collection = slint::fontique_011::shared_collection();
        let mut sources = fontique::SourceCache::default();
        let family_id = {
            let mut query = collection.query(&mut sources);
            query.set_families([fontique::QueryFamily::Generic(generic)]);
            let mut family_id = None;
            query.matches_with(|font| {
                family_id = Some(font.family.0);
                fontique::QueryStatus::Stop
            });
            family_id
        }?;
        collection.family_name(family_id).map(str::to_owned)
    })
    .into()
}

fn font_family_with_resolver(
    requested: &str,
    resolve: impl FnOnce(fontique::GenericFamily) -> Option<String>,
) -> String {
    let generic = match requested.trim().to_ascii_lowercase().as_str() {
        "monospace" => fontique::GenericFamily::Monospace,
        "serif" => fontique::GenericFamily::Serif,
        "sans-serif" => fontique::GenericFamily::SansSerif,
        // Named custom families pass through unchanged, even if not installed.
        _ => return requested.to_owned(),
    };
    // Preserve the existing renderer fallback if the OS has no usable match.
    // Do not cache misses: a later config/preset projection always resolves anew.
    resolve(generic)
        .filter(|family| !family.trim().is_empty())
        .unwrap_or_else(|| requested.to_owned())
}

#[cfg(test)]
thread_local! {
    static MOTION_PROBE: std::cell::Cell<(u32, bool, bool)> =
        const { std::cell::Cell::new((0, false, false)) };
}

/// Test-only facts: callback count, last requested value and save success.
/// No paths, errors, accounts or preference values from an owner profile.
#[cfg(test)]
pub(super) fn take_motion_probe() -> (u32, bool, bool) {
    MOTION_PROBE.with(|probe| probe.replace((0, false, false)))
}

pub(super) fn apply_config(ui: &AppWindow, config: &Config) {
    ui.global::<StyleTokens>()
        .set_reduced_motion(config.appearance.reduce_motion);
    ui.global::<StyleTokens>()
        .set_hover_duration_ms(config.appearance.hover_duration_ms.into());
    let opacity = config.window.opacity_percent;
    ui.global::<StyleTokens>()
        .set_window_opacity(f32::from(opacity) / 100.0);
    ui.set_window_opacity_percent(opacity.into());
    ui.set_window_transparency_status(transparency_status(opacity).into());
    // Move only a focused, disappearing navigation control before changing its
    // conditional subtree. Other input and modal focus remains untouched.
    ui.invoke_apply_navigation_mode(match config.layout.navigation {
        NavigationMode::Rail => 0,
        NavigationMode::Sidebar => 1,
    });
    ui.set_show_space_labels(config.layout.show_space_labels);
    ui.set_group_direct_messages(config.layout.group_direct_messages);
    ui.set_conversation_header_mode(match config.conversation.header {
        ConversationHeaderMode::Full => 0,
        ConversationHeaderMode::Compact => 1,
        ConversationHeaderMode::Minimal => 2,
    });
    ui.set_ui_font_family(display_font_family(&config.appearance.font_family));
    ui.set_message_font_family(display_font_family(
        config.typography.message_family(&config.appearance),
    ));
    ui.set_monospace_font_family(display_font_family(
        &config.typography.monospace_font_family,
    ));
    ui.set_message_font_size(config.typography.message_size.into());
    ui.set_sender_font_weight(config.typography.sender_weight.unwrap_or(0).into());
    ui.set_timestamp_format(match config.chat.timestamp_format {
        TimestampFormat::TwelveHour => 0,
        TimestampFormat::TwentyFourHour => 1,
        TimestampFormat::TwentyFourHourSeconds => 2,
    });
    ui.set_timestamp_brackets(config.chat.timestamp_brackets);
    ui.set_timestamp_width_px(config.chat.timestamp_width.into());
    ui.set_corner_radius_px(config.geometry.corner_radius.into());
    ui.set_message_gap_px(config.geometry.message_gap.into());
    ui.set_content_padding_px(config.geometry.content_padding.into());
    ui.set_rail_width_px(config.geometry.rail_width.into());
    ui.global::<StyleTokens>()
        .set_radius(config.geometry.corner_radius.into());
    ui.set_message_layout(match config.chat.layout {
        MessageLayout::Stacked => 0,
        MessageLayout::Compact => 1,
        MessageLayout::Columns => 2,
    });
    ui.set_show_message_avatars(config.chat.show_avatars);
    ui.set_show_message_timestamps(config.chat.show_timestamps);
    ui.set_color_message_senders(config.chat.color_senders);
    ui.set_nickname_width_px(config.chat.nickname_width.into());
    ui.set_nickname_brackets(config.chat.nickname_brackets);
    ui.set_nickname_alignment(match config.chat.nickname_alignment {
        NicknameAlignment::Left => 0,
        NicknameAlignment::Right => 1,
    });
    ui.set_appearance_ui_font_input(config.appearance.font_family.as_str().into());
    ui.set_appearance_message_font_input(
        config
            .typography
            .message_font_family
            .as_deref()
            .unwrap_or("")
            .into(),
    );
    ui.set_appearance_mono_font_input(config.typography.monospace_font_family.as_str().into());
}

pub(super) fn install_callbacks(ui: &AppWindow, store: &ConfigStore) {
    let weak = ui.as_weak();
    let store_for_motion = store.clone();
    ui.on_set_reduce_motion(move |reduced| {
        if let Some(ui) = weak.upgrade() {
            let _saved = save_config_change(&ui, &store_for_motion, |config| {
                config.appearance.reduce_motion = reduced;
            });
            #[cfg(test)]
            MOTION_PROBE.with(|probe| {
                let (count, _, _) = probe.get();
                probe.set((count.saturating_add(1), reduced, _saved.is_some()));
            });
        }
    });
    let weak = ui.as_weak();
    let store_for_navigation = store.clone();
    ui.on_choose_navigation_mode(move |index| {
        let mode = match index {
            0 => NavigationMode::Rail,
            1 => NavigationMode::Sidebar,
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_navigation, |config| {
                config.layout.navigation = mode;
            });
        }
    });
    let weak = ui.as_weak();
    let store_for_labels = store.clone();
    ui.on_set_show_space_labels(move |show| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_labels, |config| {
                config.layout.show_space_labels = show;
            });
        }
    });
    let weak = ui.as_weak();
    let store_for_grouping = store.clone();
    ui.on_set_group_direct_messages(move |group| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_grouping, |config| {
                config.layout.group_direct_messages = group;
            });
        }
    });
    let weak = ui.as_weak();
    let store_for_weight = store.clone();
    ui.on_set_sender_weight(move |value| {
        let weight = match value {
            0 => None,
            100..=900 => Some(value as u16),
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_weight, |config| {
                config.typography.sender_weight = weight;
            });
        }
    });
    let weak = ui.as_weak();
    let store_for_clock = store.clone();
    ui.on_choose_timestamp_format(move |index| {
        let format = match index {
            0 => TimestampFormat::TwelveHour,
            1 => TimestampFormat::TwentyFourHour,
            2 => TimestampFormat::TwentyFourHourSeconds,
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_clock, |config| {
                config.chat.timestamp_format = format;
            });
        }
    });
    let weak = ui.as_weak();
    let store_for_header = store.clone();
    ui.on_choose_conversation_header_mode(move |index| {
        let header = match index {
            0 => ConversationHeaderMode::Full,
            1 => ConversationHeaderMode::Compact,
            2 => ConversationHeaderMode::Minimal,
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_header, |config| {
                config.conversation.header = header;
            });
        }
    });

    let weak = ui.as_weak();
    let store_for_preset = store.clone();
    ui.on_choose_style_preset(move |index| {
        let Some(preset) = usize::try_from(index)
            .ok()
            .and_then(|index| BuiltInStylePreset::ALL.get(index))
            .copied()
        else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_preset, |config| {
                config.apply_style_preset(preset)
            });
        }
    });

    let weak = ui.as_weak();
    let store_for_fonts = store.clone();
    ui.on_set_style_fonts(move |interface, messages, monospace| {
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_fonts, |config| {
                config.appearance.font_family = interface.trim().to_owned();
                config.typography.message_font_family =
                    (!messages.trim().is_empty()).then(|| messages.trim().to_owned());
                config.typography.monospace_font_family = monospace.trim().to_owned();
            });
        }
    });

    let weak = ui.as_weak();
    let store_for_number = store.clone();
    ui.on_change_style_number(move |field, value| {
        if !(0..=7).contains(&field) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_number, |config| match field {
                0 => config.typography.message_size = value.clamp(10, 32) as u16,
                1 => config.geometry.corner_radius = value.clamp(0, 24) as u16,
                2 => config.geometry.message_gap = value.clamp(0, 32) as u16,
                3 => config.geometry.content_padding = value.clamp(0, 48) as u16,
                4 => config.geometry.rail_width = value.clamp(44, 100) as u16,
                5 => config.chat.nickname_width = value.clamp(64, 240) as u16,
                6 => config.chat.timestamp_width = value.clamp(48, 160) as u16,
                7 => config.appearance.hover_duration_ms = value.clamp(0, 1000) as u16,
                _ => unreachable!("validated appearance field"),
            });
        }
    });

    let weak = ui.as_weak();
    let store_for_layout = store.clone();
    ui.on_choose_message_layout(move |index| {
        let layout = match index {
            0 => MessageLayout::Stacked,
            1 => MessageLayout::Compact,
            2 => MessageLayout::Columns,
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            let _ =
                save_config_change(&ui, &store_for_layout, |config| config.chat.layout = layout);
        }
    });

    let weak = ui.as_weak();
    let store_for_flag = store.clone();
    ui.on_set_style_flag(move |field, value| {
        if !(0..=4).contains(&field) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_flag, |config| match field {
                0 => config.chat.show_avatars = value,
                1 => config.chat.show_timestamps = value,
                2 => config.chat.color_senders = value,
                3 => config.chat.timestamp_brackets = value,
                4 => config.chat.nickname_brackets = value,
                _ => unreachable!("validated appearance flag"),
            });
        }
    });

    let weak = ui.as_weak();
    let store_for_alignment = store.clone();
    ui.on_choose_nickname_alignment(move |index| {
        let alignment = match index {
            0 => NicknameAlignment::Left,
            1 => NicknameAlignment::Right,
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_alignment, |config| {
                config.chat.nickname_alignment = alignment;
            });
        }
    });
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn account_theme_mode_resolves_from_the_active_account_only() {
        let mut config = Config::default();
        config.appearance.theme = ThemeMode::System;
        config
            .account_overrides
            .entry("alice".into())
            .or_default()
            .theme = Some(ThemeMode::Light);
        // No accepted configuration yet: the built-in default, never a guess.
        assert_eq!(resolve_theme_mode(None, "alice"), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(Some(&config), "alice"), ThemeMode::Light);
        assert_eq!(resolve_theme_mode(Some(&config), "bob"), ThemeMode::System);
        assert_eq!(resolve_theme_mode(Some(&config), ""), ThemeMode::System);
        // The picker state is the global choice; the notice names the
        // override only while the active account has one.
        let alice = theme_projection_for(Some(&config), "alice");
        assert_eq!(alice.global, ThemeMode::System);
        assert_eq!(alice.effective, ThemeMode::Light);
        assert_eq!(
            alice.notice,
            "This account overrides it with Light in config.toml."
        );
        let bob = theme_projection_for(Some(&config), "bob");
        assert_eq!(bob.global, ThemeMode::System);
        assert_eq!(bob.effective, ThemeMode::System);
        assert!(bob.notice.is_empty());
        assert!(theme_projection_for(Some(&config), "").notice.is_empty());
        config.account_overrides.get_mut("alice").unwrap().theme = None;
        let alice = theme_projection_for(Some(&config), "alice");
        assert_eq!(alice.effective, ThemeMode::System);
        assert!(alice.notice.is_empty());
        assert_eq!(theme_mode_index(ThemeMode::Dark), 0);
        assert_eq!(theme_mode_index(ThemeMode::Light), 1);
        assert_eq!(theme_mode_index(ThemeMode::HighContrast), 2);
        assert_eq!(theme_mode_index(ThemeMode::System), 3);
    }

    #[test]
    fn omitted_roles_and_sender_palette_restore_legacy_bindings() {
        let (roles, senders) = theme_projection(&Config::default(), None);
        assert_eq!(roles, ThemeRoleSet::default());
        assert!(senders.is_empty());
        assert_eq!(base_projection(&Config::default()), BaseColorSet::default());
        assert!(!color_role(None).present);
        let black = color_role(Some("#000000"));
        assert!(black.present, "black is an explicit color, not an omission");
        assert_eq!(black.color, slint::Color::from_rgb_u8(0, 0, 0));
    }

    #[test]
    fn sparse_theme_replaces_roles_and_sender_colors_without_retaining_old_values() {
        let mut theme: ThemePack = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "name": "projection test",
            "colors": {
                "rail": "#010101", "sidebar": "#020202", "canvas": "#030303",
                "panel": "#040404", "elevated": "#050505", "hover": "#060606",
                "ink": "#070707", "muted": "#080808", "divider": "#090909",
                "accent": "#0A0A0A", "accent_ink": "#0B0B0B"
            },
            "roles": {"error": "#121314", "focus": "#212223", "selection_ink": "#313233",
                      "own_surface": "#616263", "timestamp": "#717273"},
            "senders": {"colors": ["#414243", "#515253"]}
        }))
        .unwrap();
        let (roles, senders) = theme_projection(&Config::default(), Some(&theme));
        assert_eq!(
            roles.error.color,
            slint::Color::from_rgb_u8(0x12, 0x13, 0x14)
        );
        assert!(roles.error.present && roles.focus.present && roles.selection_ink.present);
        assert!(!roles.success.present);
        // Message-surface roles project individually; omitted ones stay absent
        // so the rows keep their previous ink, muted and transparent colors.
        assert!(roles.own_surface.present && roles.timestamp.present);
        assert_eq!(
            roles.own_surface.color,
            slint::Color::from_rgb_u8(0x61, 0x62, 0x63)
        );
        assert!(!roles.sender.present && !roles.message.present && !roles.own_message.present);
        assert_eq!(
            senders,
            vec![
                slint::Color::from_rgb_u8(0x41, 0x42, 0x43),
                slint::Color::from_rgb_u8(0x51, 0x52, 0x53)
            ]
        );
        theme.roles = Default::default();
        theme.senders = Default::default();
        let (roles, senders) = theme_projection(&Config::default(), Some(&theme));
        assert_eq!(roles, ThemeRoleSet::default());
        assert!(senders.is_empty());
    }

    #[test]
    fn config_overrides_win_over_the_theme_and_accept_alpha() {
        let mut theme = morpheus_config::BuiltInStylePreset::Nocturne
            .theme()
            .unwrap();
        theme.roles.link = Some("#111111".into());
        let mut config = Config::default();
        config.colors.insert("link".into(), "#22222280".into());
        config.colors.insert("canvas".into(), "#33333300".into());
        config.colors.insert("scrim".into(), "#000000CC".into());
        let (roles, _) = theme_projection(&config, Some(&theme));
        assert!(roles.link.present);
        assert_eq!(
            roles.link.color,
            slint::Color::from_argb_u8(0x80, 0x22, 0x22, 0x22)
        );
        assert_eq!(roles.scrim.color, slint::Color::from_argb_u8(0xcc, 0, 0, 0));
        // Without the override the theme's role applies again.
        config.colors.remove("link");
        let (roles, _) = theme_projection(&config, Some(&theme));
        assert_eq!(
            roles.link.color,
            slint::Color::from_rgb_u8(0x11, 0x11, 0x11)
        );
        let base = base_projection(&config);
        assert!(base.canvas.present && !base.panel.present);
        assert_eq!(
            base.canvas.color,
            slint::Color::from_argb_u8(0, 0x33, 0x33, 0x33)
        );
        assert!(transparency_status(100).starts_with("Opaque"));
        assert!(transparency_status(40).contains("Not yet verified"));
    }

    #[test]
    fn sender_identity_indices_are_stable_for_full_matrix_ids() {
        let first = String::from("@casey:one.example.org");
        let copied = first.clone();
        assert_eq!(sender_palette_index(&first), sender_palette_index(&copied));
        assert_ne!(
            sender_palette_index(&first),
            sender_palette_index("@casey:two.example.org")
        );
        for identity in [
            "",
            "Casey",
            "@casey:one.example.org",
            "@雪:example.org",
            &"sender".repeat(4096),
        ] {
            let index = sender_palette_index(identity);
            assert!(index >= 0);
            for length in 1..=16 {
                let selected = index % length;
                assert!((0..length).contains(&selected));
            }
        }
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    #[test]
    fn generic_font_projection_uses_system_generic_categories() {
        for (requested, expected) in [
            ("monospace", fontique::GenericFamily::Monospace),
            ("SERIF", fontique::GenericFamily::Serif),
            (" sans-serif ", fontique::GenericFamily::SansSerif),
        ] {
            assert_eq!(
                font_family_with_resolver(requested, |generic| {
                    assert_eq!(generic, expected);
                    Some("Installed font family".to_owned())
                }),
                "Installed font family"
            );
        }
    }

    #[test]
    fn named_fonts_bypass_generic_lookup_and_empty_matches_keep_fallback() {
        for requested in ["Inter", "Monospace Custom", "Noto Sans 日本語", "system-ui"] {
            assert_eq!(
                font_family_with_resolver(requested, |_| panic!("named font must pass through")),
                requested
            );
        }
        for unavailable in [None, Some(String::new()), Some("  ".to_owned())] {
            assert_eq!(
                font_family_with_resolver("monospace", |_| unavailable),
                "monospace"
            );
        }
    }

    #[test]
    fn live_font_projection_does_not_retain_previous_generic_or_custom_choice() {
        assert_eq!(
            font_family_with_resolver("monospace", |_| Some("Fixed font".to_owned())),
            "Fixed font"
        );
        assert_eq!(font_family_with_resolver("serif", |_| None), "serif");
        assert_eq!(
            font_family_with_resolver("Custom Font", |_| panic!("custom family")),
            "Custom Font"
        );
        assert_eq!(
            font_family_with_resolver("monospace", |_| Some("New fixed font".to_owned())),
            "New fixed font"
        );
    }
}
