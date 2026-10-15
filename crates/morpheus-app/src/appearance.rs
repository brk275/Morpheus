//! Declarative appearance bridge shared by live sessions and offline previews.

use morpheus_config::{
    BuiltInStylePreset, Config, ConfigStore, ConversationHeaderMode, MessageLayout, NavigationMode,
    ThemePack, TimestampFormat,
};
use slint::fontique_011::fontique;
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{AppWindow, ColorRole, StyleTokens, ThemeRoleSet, color_from_hex, save_config_change};

/// Apply a complete projection every time, including clearing omitted overrides.
/// Sender colors are a live UI model: existing message rows need no rebuilding.
pub(super) fn apply_theme(ui: &AppWindow, theme: Option<&ThemePack>) {
    let (roles, senders) = theme_projection(theme);
    ui.global::<StyleTokens>().set_roles(roles);
    ui.set_sender_palette(ModelRc::new(VecModel::from(senders)));
}

fn color_role(value: Option<&str>) -> ColorRole {
    value.map_or_else(ColorRole::default, |value| ColorRole {
        present: true,
        color: color_from_hex(value),
    })
}

fn theme_projection(theme: Option<&ThemePack>) -> (ThemeRoleSet, Vec<slint::Color>) {
    let Some(theme) = theme else {
        return (ThemeRoleSet::default(), Vec::new());
    };
    let roles = &theme.roles;
    (
        ThemeRoleSet {
            error: color_role(roles.error.as_deref()),
            error_surface: color_role(roles.error_surface.as_deref()),
            error_ink: color_role(roles.error_ink.as_deref()),
            success: color_role(roles.success.as_deref()),
            success_surface: color_role(roles.success_surface.as_deref()),
            success_ink: color_role(roles.success_ink.as_deref()),
            selection: color_role(roles.selection.as_deref()),
            selection_ink: color_role(roles.selection_ink.as_deref()),
            focus: color_role(roles.focus.as_deref()),
            link: color_role(roles.link.as_deref()),
            unread: color_role(roles.unread.as_deref()),
            unread_ink: color_role(roles.unread_ink.as_deref()),
            destructive: color_role(roles.destructive.as_deref()),
            destructive_ink: color_role(roles.destructive_ink.as_deref()),
            voice: color_role(roles.voice.as_deref()),
        },
        theme
            .senders
            .colors
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|value| color_from_hex(value))
            .collect(),
    )
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

pub(super) fn apply_config(ui: &AppWindow, config: &Config) {
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
        if !(0..=6).contains(&field) {
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
        if !(0..=3).contains(&field) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_flag, |config| match field {
                0 => config.chat.show_avatars = value,
                1 => config.chat.show_timestamps = value,
                2 => config.chat.color_senders = value,
                3 => config.chat.timestamp_brackets = value,
                _ => unreachable!("validated appearance flag"),
            });
        }
    });
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn omitted_roles_and_sender_palette_restore_legacy_bindings() {
        let (roles, senders) = theme_projection(None);
        assert_eq!(roles, ThemeRoleSet::default());
        assert!(senders.is_empty());
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
            "roles": {"error": "#121314", "focus": "#212223", "selection_ink": "#313233"},
            "senders": {"colors": ["#414243", "#515253"]}
        }))
        .unwrap();
        let (roles, senders) = theme_projection(Some(&theme));
        assert_eq!(
            roles.error.color,
            slint::Color::from_rgb_u8(0x12, 0x13, 0x14)
        );
        assert!(roles.error.present && roles.focus.present && roles.selection_ink.present);
        assert!(!roles.success.present);
        assert_eq!(
            senders,
            vec![
                slint::Color::from_rgb_u8(0x41, 0x42, 0x43),
                slint::Color::from_rgb_u8(0x51, 0x52, 0x53)
            ]
        );
        theme.roles = Default::default();
        theme.senders = Default::default();
        let (roles, senders) = theme_projection(Some(&theme));
        assert_eq!(roles, ThemeRoleSet::default());
        assert!(senders.is_empty());
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
