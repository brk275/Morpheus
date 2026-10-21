//! Per-control icon colors: four optional state tokens that follow `icon`
//! until a theme pack or `config.toml` sets them.

use morpheus_config::registry::{self, Fallback, TokenSource};
use morpheus_config::{Config, ThemePack, ThemeStates};

const CONTROL_ICONS: [&str; 4] = [
    "icon_navigation",
    "icon_header",
    "icon_composer",
    "icon_message",
];

#[test]
fn control_icons_are_state_tokens_that_follow_icon() {
    for id in CONTROL_ICONS {
        let token = registry::color_token(id).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(token.source, TokenSource::State, "{id}");
        assert_eq!(token.fallback, Fallback::Token("icon"), "{id}");
        assert!(
            ThemeStates::default()
                .entries()
                .iter()
                .any(|(name, _)| *name == id),
            "{id}"
        );
    }
}

#[test]
fn control_icons_validate_in_config_and_theme_packs() {
    let config: Config = toml::from_str(
        "schema_version = 1\n[colors]\nicon_header = '#AABBCC'\nicon_message = '#11223344'\n",
    )
    .unwrap();
    config.validate().unwrap();
    assert_eq!(
        config.color_override("icon_header"),
        registry::parse_hex_color("#AABBCC")
    );
    let invalid: Config =
        toml::from_str("schema_version = 1\n[colors]\nicon_composer = 'teal'\n").unwrap();
    assert!(invalid.validate().is_err());

    let base = morpheus_config::BuiltInStylePreset::Paper.theme().unwrap();
    let encoded = toml::to_string_pretty(&base).unwrap();
    let themed: ThemePack = toml::from_str(&format!(
        "{encoded}\n[states]\nicon_navigation = '#334455'\n"
    ))
    .unwrap();
    themed.validate().unwrap();
    assert_eq!(themed.states.get("icon_navigation"), Some("#334455"));
    assert_eq!(themed.states.get("icon_header"), None);
    // Older theme packs without these keys keep working.
    let older: ThemePack = toml::from_str(&encoded).unwrap();
    older.validate().unwrap();
    assert!(older.states.is_empty());
}
