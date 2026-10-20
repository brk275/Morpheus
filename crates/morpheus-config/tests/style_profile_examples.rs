//! Keep the importable style profile examples valid against the real format.

use morpheus_config::BuiltInStylePreset;
use morpheus_config::style_profiles::StyleProfile;

const EXAMPLES: [(&str, &str); 4] = [
    (
        "harbor.toml",
        include_str!("../../../examples/style-profiles/harbor.toml"),
    ),
    (
        "lantern.toml",
        include_str!("../../../examples/style-profiles/lantern.toml"),
    ),
    (
        "ledger.toml",
        include_str!("../../../examples/style-profiles/ledger.toml"),
    ),
    (
        "meadow.toml",
        include_str!("../../../examples/style-profiles/meadow.toml"),
    ),
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
        assert_ne!(
            profile,
            StyleProfile::named(&profile.name),
            "{file} changes nothing"
        );
        let written = profile.to_toml().unwrap();
        assert_eq!(StyleProfile::parse(&written).unwrap(), profile, "{file}");
    }
}

/// Theme packs carry appearance only: audio and other media devices,
/// experimental switches and account data are refused on import, and an
/// oversized file is refused before it is parsed. No message repeats the
/// rejected value.
#[test]
fn imports_refuse_media_audio_accounts_and_oversized_files() {
    use morpheus_config::style_profiles::{MAX_PROFILE_BYTES, import_profile};

    for raw in [
        "profile_version = 1\nname = \"A\"\n[media]\nmicrophone_device = \"secret-mic\"\n",
        "profile_version = 1\nname = \"A\"\n[media]\nspeaker_device = \"secret-speaker\"\n",
        "profile_version = 1\nname = \"A\"\n[media]\nautoplay_animated_media = true\n",
        "profile_version = 1\nname = \"A\"\n[experimental]\nbackground_account_sync = true\n",
        "profile_version = 1\nname = \"A\"\n[account_overrides.secret]\ntheme = \"dark\"\n",
    ] {
        let error = StyleProfile::parse(raw).expect_err(raw);
        assert_eq!(error.code(), "invalid-profile", "{raw}");
        assert!(!error.to_string().contains("secret"), "{raw}");
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.toml");
    let mut raw = String::from("profile_version = 1\nname = \"Large\"\n");
    while raw.len() as u64 <= MAX_PROFILE_BYTES {
        raw.push_str("# padding padding padding padding padding padding padding\n");
    }
    std::fs::write(&path, raw).unwrap();
    let error = import_profile(&path).expect_err("oversized import");
    assert_eq!(error.code(), "file-too-large");
}
