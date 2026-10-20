//! Original variations of the built-in starting points. Each one is a
//! built-in preset with a few ordinary settings changed, so it fills in the
//! same editable values and adds no hidden layer. Like the presets, a
//! variation never changes text size, Reduce motion or the focus ring, nor
//! accounts, notifications, shortcuts or devices. Choosing one in Settings
//! is drafted like any other look change: it shows at once and is saved
//! only by Apply.

use morpheus_config::{BuiltInStylePreset, Config, Density, ThemeMode};

pub(crate) struct Variation {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    base: BuiltInStylePreset,
    adjust: fn(&mut Config),
}

impl Variation {
    pub(crate) fn apply(&self, config: &mut Config) {
        config.apply_style_preset(self.base);
        (self.adjust)(config);
    }
}

/// In the order Settings lists them.
pub(crate) const VARIATIONS: [Variation; 5] = [
    Variation {
        name: "Nocturne Compact",
        description: "Charcoal · tighter rows and margins · narrower panes",
        base: BuiltInStylePreset::Nocturne,
        adjust: |config| {
            config.appearance.density = Density::Compact;
            config.geometry.corner_radius = 6;
            config.geometry.message_gap = 4;
            config.geometry.content_padding = 14;
            config.layout.room_list_width = 230;
            config.layout.details_width = 250;
        },
    },
    Variation {
        name: "Nocturne Large Print",
        description: "Charcoal · large type and icons · roomy controls",
        base: BuiltInStylePreset::Nocturne,
        adjust: |config| {
            config.typography.message_size = 18;
            config.typography.sidebar_size = 15;
            config.typography.sender_size = Some(17);
            config.typography.timestamp_size = Some(13);
            config.typography.composer_size = Some(18);
            config.geometry.icon_size = 22;
            config.geometry.control_padding = 14;
            config.geometry.input_padding = 12;
            config.geometry.rail_width = 72;
            config.layout.room_list_width = 300;
            config.layout.details_width = 320;
        },
    },
    Variation {
        name: "Terminal Soft",
        description: "Monospaced columns · rounded edges · small avatars",
        base: BuiltInStylePreset::Terminal,
        adjust: |config| {
            config.appearance.density = Density::Comfortable;
            config.geometry.corner_radius = 6;
            config.geometry.message_gap = 6;
            config.geometry.avatar_size = Some(24);
            config.chat.show_avatars = true;
        },
    },
    Variation {
        name: "Paper Wide",
        description: "Warm paper · serif · wide margins for long reading",
        base: BuiltInStylePreset::Paper,
        adjust: |config| {
            config.typography.message_size = 17;
            config.geometry.content_padding = 48;
            config.geometry.message_gap = 18;
            config.layout.room_list_width = 220;
        },
    },
    Variation {
        name: "Contrast Reading",
        description: "Built-in high-contrast colors · heavier outlines · larger type",
        base: BuiltInStylePreset::Nocturne,
        adjust: |config| {
            // The built-in high-contrast palette, not a pack's colors.
            config.appearance.theme = ThemeMode::HighContrast;
            config.appearance.theme_pack = None;
            config.typography.message_size = 17;
            config.typography.sidebar_size = 15;
            config.typography.timestamp_size = Some(13);
            config.geometry.border_width = 2;
            config.geometry.icon_size = 20;
            config.geometry.message_gap = 12;
        },
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variation_is_valid_and_keeps_the_accessibility_choices() {
        let mut person = Config::default();
        person.appearance.scale_percent = 150;
        person.appearance.reduce_motion = true;
        person.geometry.focus_width = 4;
        person.notifications.sounds = !person.notifications.sounds;
        let names: std::collections::BTreeSet<_> =
            VARIATIONS.iter().map(|variation| variation.name).collect();
        assert_eq!(names.len(), VARIATIONS.len());
        for variation in &VARIATIONS {
            let mut config = person.clone();
            variation.apply(&mut config);
            config
                .validate()
                .unwrap_or_else(|error| panic!("{}: {error}", variation.name));
            assert_eq!(config.appearance.scale_percent, 150, "{}", variation.name);
            assert!(config.appearance.reduce_motion, "{}", variation.name);
            assert_eq!(config.geometry.focus_width, 4, "{}", variation.name);
            assert_eq!(
                config.notifications, person.notifications,
                "{}",
                variation.name
            );
            // A variation differs from its base preset.
            let mut base = person.clone();
            base.apply_style_preset(variation.base);
            assert_ne!(config, base, "{}", variation.name);
        }
    }
}
