//! Per-room appearance: global -> account -> room precedence, inheritance of
//! what a level omits, reset of one level only, and bounded, validated,
//! backward-compatible storage.

use std::collections::BTreeMap;

use morpheus_config::{Config, MAX_ROOM_APPEARANCES, RoomAppearance, ThemeMode};

const ROOM: &str = "!harbor:example.org";
const OTHER_ROOM: &str = "!quay:example.org";

fn parse(raw: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(raw).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

fn layered() -> Config {
    parse(&format!(
        "schema_version = 1\n\
         [appearance]\ntheme = \"system\"\n\
         [colors]\nlink = \"#111111\"\naccent = \"#121212\"\n\
         [account_overrides.local-a]\ntheme = \"light\"\n\
         [account_overrides.local-a.colors]\nlink = \"#222222\"\n\
         [account_overrides.local-a.room_appearance.\"{ROOM}\"]\ntheme = \"high_contrast\"\n\
         [account_overrides.local-a.room_appearance.\"{ROOM}\".colors]\nlink = \"#333333\"\n"
    ))
    .unwrap()
}

#[test]
fn the_most_specific_level_wins_and_omitted_values_inherit() {
    let config = layered();
    let room = config.colors_for_room(Some("local-a"), Some(ROOM));
    assert_eq!(room["link"], "#333333");
    // The room does not set `accent`: it inherits the global value.
    assert_eq!(room["accent"], "#121212");
    assert_eq!(
        config.theme_for_room(Some("local-a"), Some(ROOM)),
        ThemeMode::HighContrast
    );
    // Another room of the account inherits the account's values.
    let other = config.colors_for_room(Some("local-a"), Some(OTHER_ROOM));
    assert_eq!(other["link"], "#222222");
    assert_eq!(
        config.theme_for_room(Some("local-a"), Some(OTHER_ROOM)),
        ThemeMode::Light
    );
    // The same room ID in another account, and no account, use global.
    assert_eq!(
        config.colors_for_room(Some("local-b"), Some(ROOM))["link"],
        "#111111"
    );
    assert_eq!(
        config.theme_for_room(Some("local-b"), Some(ROOM)),
        ThemeMode::System
    );
    assert_eq!(config.theme_for_room(None, Some(ROOM)), ThemeMode::System);
    assert_eq!(
        config.colors_for_room(Some(""), Some(ROOM))["link"],
        "#111111"
    );
}

#[test]
fn resetting_a_room_removes_only_that_room() {
    let mut config = layered();
    config
        .account_overrides
        .get_mut("local-a")
        .unwrap()
        .room_appearance
        .insert(
            OTHER_ROOM.into(),
            RoomAppearance {
                theme: Some(ThemeMode::Dark),
                colors: BTreeMap::new(),
            },
        );
    assert!(config.clear_room_appearance("local-a", ROOM));
    assert!(!config.clear_room_appearance("local-a", ROOM));
    assert_eq!(
        config.colors_for_room(Some("local-a"), Some(ROOM))["link"],
        "#222222"
    );
    assert_eq!(
        config.theme_for_room(Some("local-a"), Some(OTHER_ROOM)),
        ThemeMode::Dark
    );
    // The account's own settings are untouched.
    assert_eq!(
        config.account_overrides["local-a"].theme,
        Some(ThemeMode::Light)
    );
    // Clearing the account's appearance removes its rooms too.
    assert!(config.clear_account_appearance("local-a"));
    assert!(
        config
            .room_appearance(Some("local-a"), Some(OTHER_ROOM))
            .is_none()
    );
}

#[test]
fn room_entries_are_bounded_and_validated_without_echoing_ids() {
    let mut config = layered();
    let rooms = &mut config
        .account_overrides
        .get_mut("local-a")
        .unwrap()
        .room_appearance;
    for index in 0..MAX_ROOM_APPEARANCES {
        rooms.insert(
            format!("!room{index}:example.org"),
            RoomAppearance {
                theme: Some(ThemeMode::Dark),
                colors: BTreeMap::new(),
            },
        );
    }
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("room_appearance"), "{error}");
    assert!(
        !error.contains("example.org") && !error.contains("local-a"),
        "{error}"
    );

    for bad in [
        // Not a room ID.
        "[account_overrides.local-a.room_appearance.\"general\"]\ntheme = \"dark\"\n".to_owned(),
        // Unknown token and invalid color.
        format!(
            "[account_overrides.local-a.room_appearance.\"{ROOM}\".colors]\nscript = \"#123456\"\n"
        ),
        format!("[account_overrides.local-a.room_appearance.\"{ROOM}\".colors]\nlink = \"red\"\n"),
        // No other keys: no credentials, paths or expressions.
        format!("[account_overrides.local-a.room_appearance.\"{ROOM}\"]\naccess_token = \"x\"\n"),
    ] {
        assert!(
            parse(&format!("schema_version = 1\n{bad}")).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn files_without_rooms_load_unchanged_and_rooms_round_trip() {
    let plain =
        parse("schema_version = 1\n[account_overrides.local-a]\ntheme = \"light\"\n").unwrap();
    assert!(
        plain.account_overrides["local-a"]
            .room_appearance
            .is_empty()
    );
    // An empty room table is not written.
    assert!(!toml::to_string(&plain).unwrap().contains("room_appearance"));

    let config = layered();
    let written = toml::to_string_pretty(&config).unwrap();
    assert_eq!(parse(&written).unwrap(), config);
}

#[test]
fn avatar_slots_inherit_by_account_and_room_and_round_trip() {
    for id in ["avatar_1", "avatar_2", "avatar_3", "avatar_4", "avatar_5"] {
        let mut config = parse(&format!(
            "schema_version = 1\n[colors]\n{id} = '#111111'\n\
             [account_overrides.local-a.colors]\n{id} = '#22222280'\n\
             [account_overrides.local-a.room_appearance.\"{ROOM}\".colors]\n{id} = '#33333300'\n"
        ))
        .unwrap();
        assert_eq!(
            config.colors_for_room(Some("local-a"), Some(ROOM))[id],
            "#33333300"
        );
        assert_eq!(
            config.colors_for_room(Some("local-a"), Some(OTHER_ROOM))[id],
            "#22222280"
        );
        assert_eq!(
            config.colors_for_room(Some("local-b"), Some(ROOM))[id],
            "#111111"
        );
        assert_eq!(
            config.colors_for_room(None, Some(ROOM))[id],
            "#111111"
        );
        assert_eq!(
            parse(&toml::to_string_pretty(&config).unwrap()).unwrap(),
            config
        );
        assert!(config.clear_room_appearance("local-a", ROOM));
        assert_eq!(
            config.colors_for_room(Some("local-a"), Some(ROOM))[id],
            "#22222280"
        );
        assert!(config.clear_account_appearance("local-a"));
        assert_eq!(
            config.colors_for_room(Some("local-a"), Some(ROOM))[id],
            "#111111"
        );
    }
}
