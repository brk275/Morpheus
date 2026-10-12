//! Declarative appearance bridge shared by live sessions and offline previews.

use morpheus_config::{BuiltInStylePreset, Config, ConfigStore, MessageLayout};
use slint::ComponentHandle;

use super::{AppWindow, StyleTokens, save_config_change};

pub(super) fn apply_config(ui: &AppWindow, config: &Config) {
    ui.set_message_font_family(config.typography.message_family(&config.appearance).into());
    ui.set_monospace_font_family(config.typography.monospace_font_family.as_str().into());
    ui.set_message_font_size(config.typography.message_size.into());
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
        if !(0..=5).contains(&field) {
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
        if !(0..=2).contains(&field) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let _ = save_config_change(&ui, &store_for_flag, |config| match field {
                0 => config.chat.show_avatars = value,
                1 => config.chat.show_timestamps = value,
                2 => config.chat.color_senders = value,
                _ => unreachable!("validated appearance flag"),
            });
        }
    });
}
