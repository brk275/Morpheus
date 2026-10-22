//! Actual parser/validator and registry contracts for editable hover timing.
use morpheus_config::{BuiltInStylePreset, Config, registry};

#[test]
fn hover_timing_defaults_and_round_trips_without_losing_reduced_motion() {
    let omitted: Config = toml::from_str("[appearance]\nreduce_motion = true\n").unwrap();
    assert_eq!(omitted.appearance.hover_duration_ms, 90);
    assert!(omitted.appearance.reduce_motion);
    for value in [0, 1, 90, 120, 1000] {
        let config: Config = toml::from_str(&format!(
            "[appearance]\nhover_duration_ms = {value}\nreduce_motion = true\n"
        ))
        .unwrap();
        config.validate().unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.appearance.hover_duration_ms, value);
        assert!(restored.appearance.reduce_motion);
        for preset in BuiltInStylePreset::ALL {
            let mut themed = restored.clone();
            themed.apply_style_preset(preset);
            themed.validate().unwrap();
            assert_eq!(themed.appearance.hover_duration_ms, value);
            assert!(themed.appearance.reduce_motion);
        }
    }
}

#[test]
fn hover_timing_rejects_unbounded_values_and_is_discoverable() {
    let too_slow: Config = toml::from_str("[appearance]\nhover_duration_ms = 1001\n").unwrap();
    assert!(too_slow.validate().is_err());
    for invalid in ["-1", "65536", "1.5", "\"fast\""] {
        assert!(
            toml::from_str::<Config>(&format!("[appearance]\nhover_duration_ms = {invalid}\n"))
                .is_err()
        );
    }
    assert!(
        registry::FIELDS
            .iter()
            .any(|field| field.key == "appearance.hover_duration_ms")
    );
    assert!(
        registry::search_settings("animation speed", |_| true)
            .iter()
            .any(|target| target.id == "appearance.hover_duration")
    );
}
