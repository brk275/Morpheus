//! Keep the importable style profile examples valid against the real format.

use morpheus_config::BuiltInStylePreset;
use morpheus_config::style_profiles::StyleProfile;

const EXAMPLES: [(&str, &str); 3] = [
    ("harbor.toml", include_str!("../../../examples/style-profiles/harbor.toml")),
    ("lantern.toml", include_str!("../../../examples/style-profiles/lantern.toml")),
    ("ledger.toml", include_str!("../../../examples/style-profiles/ledger.toml")),
];

/// Every example is a valid profile that needs no extra theme file, changes
/// the look, and survives Morpheus writing it back.
#[test]
fn style_profile_examples_are_valid_importable_profiles() {
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/style-profiles");
    let mut listed: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    listed.sort();
    let checked: Vec<&str> = EXAMPLES.iter().map(|(name, _)| *name).collect();
    assert_eq!(listed, checked, "every example profile is checked here");

    for (file, raw) in EXAMPLES {
        let profile =
            StyleProfile::parse(raw).unwrap_or_else(|error| panic!("{file} is invalid: {error}"));
        if let Some(pack) = profile.appearance.theme_pack.as_deref() {
            assert!(
                BuiltInStylePreset::ALL
                    .into_iter()
                    .any(|preset| preset.theme_name() == pack),
                "{file} needs an extra theme file"
            );
        }
        assert_ne!(profile, StyleProfile::named(&profile.name), "{file} changes nothing");
        let written = profile.to_toml().unwrap();
        assert_eq!(StyleProfile::parse(&written).unwrap(), profile, "{file}");
    }
}
