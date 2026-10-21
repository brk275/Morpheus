//! Keep the importable theme pack examples valid and in step with the folder.

use morpheus_config::style_profiles::{pack_name_from, parse_theme_pack, theme_pack_toml};

const EXAMPLES: [(&str, &str); 2] = [
    (
        "ember.toml",
        include_str!("../../../examples/theme-packs/ember.toml"),
    ),
    (
        "tidepool.toml",
        include_str!("../../../examples/theme-packs/tidepool.toml"),
    ),
];

#[test]
fn theme_pack_examples_are_valid_importable_packs() {
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/theme-packs");
    let mut listed: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    listed.sort();
    let checked: Vec<&str> = EXAMPLES.iter().map(|(name, _)| *name).collect();
    assert_eq!(listed, checked, "every example pack is checked here");

    for (file, raw) in EXAMPLES {
        let pack = parse_theme_pack(raw).unwrap_or_else(|error| panic!("{file}: {error}"));
        // Installed under a plain name derived from its own display name.
        let name = pack_name_from(&pack.name).unwrap();
        assert_eq!(format!("{name}.toml"), file);
        // Morpheus writes it back to an equal pack.
        let written = theme_pack_toml(&pack).unwrap();
        assert_eq!(parse_theme_pack(&written).unwrap(), pack, "{file}");
    }
}
