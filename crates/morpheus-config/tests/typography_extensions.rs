//! Line height and body weight: older files keep working, the new keys are
//! validated against their registry ranges, the metric editor can set and
//! clear them, and style profiles carry them.

use morpheus_config::style_profiles::StyleProfile;
use morpheus_config::{Config, MetricValue};

fn parse(raw: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(raw).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

#[test]
fn files_without_the_new_keys_keep_the_natural_look() {
    let config = parse("schema_version = 1\n[typography]\nmessage_size = 15\n").unwrap();
    assert_eq!(config.typography.line_height_percent, None);
    assert_eq!(config.typography.body_weight, None);
    assert_eq!(Config::default().typography.line_height_percent, None);
    assert_eq!(Config::default().typography.body_weight, None);
}

#[test]
fn the_new_keys_are_range_checked() {
    let accepted = parse(
        "schema_version = 1\n[typography]\nline_height_percent = 150\nbody_weight = 600\n",
    )
    .unwrap();
    assert_eq!(accepted.typography.line_height_percent, Some(150));
    assert_eq!(accepted.typography.body_weight, Some(600));
    for raw in [
        "[typography]\nline_height_percent = 99",
        "[typography]\nline_height_percent = 201",
        "[typography]\nbody_weight = 299",
        "[typography]\nbody_weight = 801",
        "[typography]\nline_height_percent = -1",
        "[typography]\nline_height = 150",
    ] {
        assert!(parse(&format!("schema_version = 1\n{raw}\n")).is_err(), "{raw}");
    }
}

#[test]
fn the_metric_editor_sets_and_clears_them() {
    let mut config = Config::default();
    for (key, value) in [
        ("typography.line_height_percent", 130),
        ("typography.body_weight", 500),
    ] {
        assert!(config.set_metric(key, MetricValue::Set(value)).unwrap());
        assert_eq!(config.metric(key), Some(MetricValue::Set(value)));
        assert!(config.set_metric(key, MetricValue::Set(9_000)).is_err());
        assert!(config.set_metric(key, MetricValue::Inherited).unwrap());
        assert_eq!(config.metric(key), Some(MetricValue::Inherited));
    }
    assert_eq!(config, Config::default());
}

#[test]
fn style_profiles_carry_them() {
    let profile = StyleProfile::parse(
        "profile_version = 1\nname = \"Airy\"\n[typography]\nline_height_percent = 140\nbody_weight = 300\n",
    )
    .unwrap();
    assert_eq!(profile.typography.line_height_percent, Some(140));
    assert_eq!(profile.typography.body_weight, Some(300));
    let written = profile.to_toml().unwrap();
    assert_eq!(StyleProfile::parse(&written).unwrap(), profile);
}
