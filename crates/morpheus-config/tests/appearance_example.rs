//! Keep the copyable editing example valid against the actual config schema.

use morpheus_config::{BuiltInStylePreset, Config};

#[test]
fn sparse_appearance_example_uses_supported_settings() {
    let config: Config =
        toml::from_str(include_str!("../../../examples/appearance/config.toml")).unwrap();
    config.validate().unwrap();
    let selected = config.appearance.theme_pack.as_deref().unwrap();
    let preset = BuiltInStylePreset::ALL
        .into_iter()
        .find(|preset| preset.theme_name() == selected)
        .expect("the example selects an embedded theme, without extra files");
    preset.theme().unwrap();
}
