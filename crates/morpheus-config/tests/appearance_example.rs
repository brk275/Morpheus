//! Keep the copyable editing examples valid against the actual config schema.

use morpheus_config::{BuiltInStylePreset, Config, registry};

const STYLES: [(&str, &str); 4] = [
    (
        "beacon.toml",
        include_str!("../../../examples/appearance/styles/beacon.toml"),
    ),
    (
        "harbor.toml",
        include_str!("../../../examples/appearance/styles/harbor.toml"),
    ),
    (
        "lantern.toml",
        include_str!("../../../examples/appearance/styles/lantern.toml"),
    ),
    (
        "ledger.toml",
        include_str!("../../../examples/appearance/styles/ledger.toml"),
    ),
];

fn embedded_theme(selected: &str) -> bool {
    BuiltInStylePreset::ALL
        .into_iter()
        .any(|preset| preset.theme_name() == selected && preset.theme().is_ok())
}

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

/// Every style example is a valid sparse configuration that needs no extra
/// theme file and changes at least one registry metric, so none is inert.
#[test]
fn style_examples_are_valid_and_use_registry_metrics() {
    let directory = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/appearance/styles"
    );
    let mut listed: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    listed.sort();
    let checked: Vec<&str> = STYLES.iter().map(|(name, _)| *name).collect();
    assert_eq!(listed, checked, "every style example is checked here");

    let defaults = Config::default();
    for (name, text) in STYLES {
        let config: Config =
            toml::from_str(text).unwrap_or_else(|error| panic!("{name} does not parse: {error}"));
        config
            .validate()
            .unwrap_or_else(|error| panic!("{name} is invalid: {error}"));
        if let Some(selected) = config.appearance.theme_pack.as_deref() {
            assert!(embedded_theme(selected), "{name} needs an extra theme file");
        }
        assert_eq!(
            config.shortcuts, defaults.shortcuts,
            "{name} changes shortcuts"
        );
        assert!(
            config.account_overrides.is_empty(),
            "{name} names an account"
        );
        assert!(
            registry::metric_fields()
                .any(|(field, _)| config.metric(field.key) != defaults.metric(field.key)),
            "{name} changes no registry metric"
        );
    }
}
