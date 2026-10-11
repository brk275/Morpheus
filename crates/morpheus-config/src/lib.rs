//! Versioned, non-secret configuration for the Morpheus desktop client.

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not locate the operating-system configuration directory")]
    MissingConfigDirectory,
    #[error("configuration file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML configuration: {0}")]
    Decode(#[from] toml::de::Error),
    #[error("could not encode TOML configuration: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("unsupported configuration schema version {0}")]
    Version(u32),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("invalid theme pack: {0}")]
    InvalidTheme(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub appearance: Appearance,
    pub layout: Layout,
    pub notifications: Notifications,
    pub media: Media,
    pub shortcuts: BTreeMap<String, String>,
    pub experimental: Experimental,
    pub account_overrides: BTreeMap<String, AccountOverrides>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            appearance: Appearance::default(),
            layout: Layout::default(),
            notifications: Notifications::default(),
            media: Media::default(),
            shortcuts: default_shortcuts(),
            experimental: Experimental::default(),
            account_overrides: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::Version(self.schema_version));
        }
        if !(75..=200).contains(&self.appearance.scale_percent) {
            return Err(ConfigError::Invalid(
                "appearance.scale_percent must be between 75 and 200".into(),
            ));
        }
        if !(180..=480).contains(&self.layout.room_list_width) {
            return Err(ConfigError::Invalid(
                "layout.room_list_width must be between 180 and 480".into(),
            ));
        }
        if !(200..=520).contains(&self.layout.details_width) {
            return Err(ConfigError::Invalid(
                "layout.details_width must be between 200 and 520".into(),
            ));
        }
        if self.appearance.font_family.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "appearance.font_family cannot be empty".into(),
            ));
        }
        if let Some(name) = &self.appearance.theme_pack
            && (name.is_empty()
                || !name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                }))
        {
            return Err(ConfigError::Invalid(
                "appearance.theme_pack must be a theme name, not a path".into(),
            ));
        }
        validate_shortcuts(&self.shortcuts)?;
        for overrides in self.account_overrides.values() {
            for room_id in &overrides.local_voice_rooms {
                if !room_id.starts_with('!') || !room_id.contains(':') {
                    return Err(ConfigError::Invalid(
                        "account_overrides.local_voice_rooms must contain Matrix room IDs".into(),
                    ));
                }
            }
            for room_id in &overrides.legacy_element_call_rooms {
                if !room_id.starts_with('!') || !room_id.contains(':') {
                    return Err(ConfigError::Invalid(
                        "account_overrides.legacy_element_call_rooms must contain Matrix room IDs"
                            .into(),
                    ));
                }
            }
            for room_id in &overrides.element_call_matrix_2_0_rooms {
                if !room_id.starts_with('!') || !room_id.contains(':') {
                    return Err(ConfigError::Invalid(
                        "account_overrides.element_call_matrix_2_0_rooms must contain Matrix room IDs".into(),
                    ));
                }
                if overrides.legacy_element_call_rooms.contains(room_id) {
                    return Err(ConfigError::Invalid(
                        "a room cannot use both Element Call compatibility modes".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Local voice-space presentation is scoped to one signed-in account.
    /// This is a UI preference; it never changes Matrix room state.
    pub fn is_local_voice_room(&self, account_id: &str, room_id: &str) -> bool {
        self.account_overrides
            .get(account_id)
            .is_some_and(|overrides| overrides.local_voice_rooms.iter().any(|id| id == room_id))
    }

    /// Designate a room as a voice space when the user cannot change its
    /// shared room state. Returns whether the preference actually changed.
    pub fn set_local_voice_room(
        &mut self,
        account_id: &str,
        room_id: &str,
        enabled: bool,
    ) -> Result<bool, ConfigError> {
        if account_id.is_empty() || !room_id.starts_with('!') || !room_id.contains(':') {
            return Err(ConfigError::Invalid(
                "local voice spaces require an account and Matrix room ID".into(),
            ));
        }
        if enabled {
            let rooms = &mut self
                .account_overrides
                .entry(account_id.to_owned())
                .or_default()
                .local_voice_rooms;
            if rooms.iter().any(|id| id == room_id) {
                return Ok(false);
            }
            rooms.push(room_id.to_owned());
            rooms.sort();
            return Ok(true);
        }
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return Ok(false);
        };
        let before = overrides.local_voice_rooms.len();
        overrides.local_voice_rooms.retain(|id| id != room_id);
        let changed = before != overrides.local_voice_rooms.len();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        Ok(changed)
    }

    /// Remove local labels when an account is removed, preserving its other
    /// settings if any remain.
    pub fn clear_local_voice_rooms(&mut self, account_id: &str) -> bool {
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return false;
        };
        let changed = !overrides.local_voice_rooms.is_empty();
        overrides.local_voice_rooms.clear();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        changed
    }

    /// Legacy Element Call room-state membership is an explicit local choice
    /// for one account and room. Standard MatrixRTC remains the default.
    pub fn is_legacy_element_call_room(&self, account_id: &str, room_id: &str) -> bool {
        self.account_overrides
            .get(account_id)
            .is_some_and(|overrides| {
                overrides
                    .legacy_element_call_rooms
                    .iter()
                    .any(|id| id == room_id)
            })
    }

    pub fn room_call_mode(&self, account_id: &str, room_id: &str) -> RoomCallMode {
        let Some(overrides) = self.account_overrides.get(account_id) else {
            return RoomCallMode::Standard;
        };
        if overrides
            .element_call_matrix_2_0_rooms
            .iter()
            .any(|id| id == room_id)
        {
            RoomCallMode::ElementCallMatrix2
        } else if self.is_legacy_element_call_room(account_id, room_id) {
            RoomCallMode::ElementCallLegacy
        } else {
            RoomCallMode::Standard
        }
    }

    /// One explicit local choice. Changing it does not change room state or an
    /// already-created call. The existing legacy list remains schema-compatible.
    pub fn set_room_call_mode(
        &mut self,
        account_id: &str,
        room_id: &str,
        mode: RoomCallMode,
    ) -> Result<bool, ConfigError> {
        if account_id.is_empty() || !room_id.starts_with('!') || !room_id.contains(':') {
            return Err(ConfigError::Invalid(
                "call mode requires an account and Matrix room ID".into(),
            ));
        }
        self.validate()?;
        if self.room_call_mode(account_id, room_id) == mode {
            return Ok(false);
        }
        let overrides = self
            .account_overrides
            .entry(account_id.to_owned())
            .or_default();
        overrides
            .legacy_element_call_rooms
            .retain(|id| id != room_id);
        overrides
            .element_call_matrix_2_0_rooms
            .retain(|id| id != room_id);
        let rooms = match mode {
            RoomCallMode::Standard => None,
            RoomCallMode::ElementCallMatrix2 => Some(&mut overrides.element_call_matrix_2_0_rooms),
            RoomCallMode::ElementCallLegacy => Some(&mut overrides.legacy_element_call_rooms),
        };
        if let Some(rooms) = rooms {
            rooms.push(room_id.to_owned());
            rooms.sort();
        }
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        Ok(true)
    }

    pub fn clear_room_call_modes(&mut self, account_id: &str) -> bool {
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return false;
        };
        let changed = !overrides.legacy_element_call_rooms.is_empty()
            || !overrides.element_call_matrix_2_0_rooms.is_empty();
        overrides.legacy_element_call_rooms.clear();
        overrides.element_call_matrix_2_0_rooms.clear();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        changed
    }

    pub fn set_legacy_element_call_room(
        &mut self,
        account_id: &str,
        room_id: &str,
        enabled: bool,
    ) -> Result<bool, ConfigError> {
        if enabled {
            self.set_room_call_mode(account_id, room_id, RoomCallMode::ElementCallLegacy)
        } else if self.is_legacy_element_call_room(account_id, room_id) {
            self.set_room_call_mode(account_id, room_id, RoomCallMode::Standard)
        } else {
            Ok(false)
        }
    }

    pub fn clear_legacy_element_call_rooms(&mut self, account_id: &str) -> bool {
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return false;
        };
        let changed = !overrides.legacy_element_call_rooms.is_empty();
        overrides.legacy_element_call_rooms.clear();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        changed
    }

    /// Local desktop notifications honor the global switch and the selected
    /// account's override. Matrix room push rules are evaluated separately by
    /// the SDK before a notification reaches the desktop presenter.
    pub fn notifications_enabled_for(&self, account_id: &str) -> bool {
        self.notifications.enabled
            && self
                .account_overrides
                .get(account_id)
                .and_then(|overrides| overrides.notifications_enabled)
                .unwrap_or(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    HighContrast,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
    Spacious,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub theme: ThemeMode,
    /// A filename in the themes directory without path separators.
    pub theme_pack: Option<String>,
    pub font_family: String,
    pub density: Density,
    pub scale_percent: u16,
    pub reduce_motion: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            theme_pack: None,
            font_family: "Inter".into(),
            density: Density::Comfortable,
            scale_percent: 100,
            reduce_motion: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub room_list_width: u16,
    pub details_width: u16,
    pub show_member_panel: bool,
    pub show_space_labels: bool,
    pub group_direct_messages: bool,
    pub sort_unread_first: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            room_list_width: 260,
            details_width: 300,
            show_member_panel: true,
            show_space_labels: false,
            group_direct_messages: true,
            sort_unread_first: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
    pub sounds: bool,
    pub show_message_preview: bool,
    pub highlight_words: Vec<String>,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            sounds: true,
            show_message_preview: false,
            highlight_words: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Media {
    pub microphone_device: Option<String>,
    pub speaker_device: Option<String>,
    pub camera_device: Option<String>,
    pub noise_suppression: bool,
    pub autoplay_animated_media: bool,
    pub previews_in_encrypted_rooms: bool,
}

impl Default for Media {
    fn default() -> Self {
        Self {
            microphone_device: None,
            speaker_device: None,
            camera_device: None,
            noise_suppression: true,
            autoplay_animated_media: false,
            previews_in_encrypted_rooms: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Experimental {
    /// Inactive accounts otherwise stop network sync after an account switch.
    pub background_account_sync: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountOverrides {
    pub notifications_enabled: Option<bool>,
    pub theme: Option<ThemeMode>,
    /// Rooms shown as voice spaces locally when the user cannot set room state.
    pub local_voice_rooms: Vec<String>,
    /// Account-local rooms that use Element Call's older room-state dialect.
    pub legacy_element_call_rooms: Vec<String>,
    /// Account-local rooms explicitly using Element Call's sticky 2025 dialect.
    pub element_call_matrix_2_0_rooms: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoomCallMode {
    #[default]
    Standard,
    ElementCallMatrix2,
    ElementCallLegacy,
}

impl RoomCallMode {
    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Standard),
            1 => Some(Self::ElementCallMatrix2),
            2 => Some(Self::ElementCallLegacy),
            _ => None,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::Standard => 0,
            Self::ElementCallMatrix2 => 1,
            Self::ElementCallLegacy => 2,
        }
    }
}

pub fn default_shortcuts() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("quick_switcher".into(), "Mod+K".into()),
        ("search".into(), "Mod+F".into()),
        ("next_unread".into(), "Alt+Down".into()),
        ("previous_unread".into(), "Alt+Up".into()),
    ])
}

/// Supported commands, shared by the TOML validator and the desktop dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    QuickSwitcher,
    Search,
    NextUnread,
    PreviousUnread,
}

impl ShortcutAction {
    pub const ALL: [Self; 4] = [
        Self::QuickSwitcher,
        Self::Search,
        Self::NextUnread,
        Self::PreviousUnread,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::QuickSwitcher => "quick_switcher",
            Self::Search => "search",
            Self::NextUnread => "next_unread",
            Self::PreviousUnread => "previous_unread",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutKey {
    Character(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
}

/// Physical modifiers. The Slint adapter accounts for its macOS Ctrl/Meta remapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShortcutModifiers {
    pub control: bool,
    pub meta: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    primary: bool,
    modifiers: ShortcutModifiers,
    pub key: ShortcutKey,
}

impl Shortcut {
    /// A combination contains modifiers followed by one key. `Mod` is Command
    /// on macOS and Control elsewhere. Missing actions are disabled; empty or
    /// malformed values are errors, so a typo cannot silently disable a binding.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let invalid = |reason: &str| ConfigError::Invalid(format!("shortcut {value:?}: {reason}"));
        if value.len() > 64 || !value.is_ascii() || value.chars().any(char::is_control) {
            return Err(invalid("use a short ASCII key combination"));
        }
        let parts: Vec<_> = value.split('+').map(str::trim).collect();
        let (key, modifiers) = parts.split_last().ok_or_else(|| invalid("missing key"))?;
        let key_name = key.to_ascii_uppercase();
        let key = match key_name.as_str() {
            "UP" => ShortcutKey::Up,
            "DOWN" => ShortcutKey::Down,
            "LEFT" => ShortcutKey::Left,
            "RIGHT" => ShortcutKey::Right,
            "HOME" => ShortcutKey::Home,
            "END" => ShortcutKey::End,
            "PAGEUP" => ShortcutKey::PageUp,
            "PAGEDOWN" => ShortcutKey::PageDown,
            name if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() => {
                ShortcutKey::Character(name.as_bytes()[0] as char)
            }
            name if (1..=12).any(|number| name == format!("F{number}")) => {
                ShortcutKey::Function(name[1..].parse().expect("validated function key"))
            }
            _ => {
                return Err(invalid(
                    "key must be A–Z, 0–9, an arrow, Home, End, PageUp, PageDown, or F1–F12",
                ));
            }
        };
        let mut shortcut = Self {
            primary: false,
            modifiers: ShortcutModifiers::default(),
            key,
        };
        for part in modifiers {
            let flag = match part.to_ascii_lowercase().as_str() {
                "mod" => &mut shortcut.primary,
                "ctrl" => &mut shortcut.modifiers.control,
                "meta" => &mut shortcut.modifiers.meta,
                "alt" => &mut shortcut.modifiers.alt,
                "shift" => &mut shortcut.modifiers.shift,
                _ => return Err(invalid("modifiers are Mod, Ctrl, Meta, Alt, and Shift")),
            };
            if *flag {
                return Err(invalid("a modifier occurs twice"));
            }
            *flag = true;
        }
        if shortcut.primary && (shortcut.modifiers.control || shortcut.modifiers.meta) {
            return Err(invalid("Mod cannot be combined with Ctrl or Meta"));
        }
        if !(shortcut.primary
            || shortcut.modifiers.control
            || shortcut.modifiers.meta
            || shortcut.modifiers.alt)
        {
            return Err(invalid(
                "include Mod, Ctrl, Meta, or Alt; typing keys are reserved",
            ));
        }
        if let ShortcutKey::Character(character) = shortcut.key {
            // Slint exposes the logical text (Shift+1 is "!" on a US layout),
            // not a physical scan code. A digit spelling cannot portably match.
            if character.is_ascii_digit() && shortcut.modifiers.shift {
                return Err(invalid(
                    "Shift with a digit is layout-dependent; use a letter or function key",
                ));
            }
            // Alt letters can be text/dead keys on macOS; Ctrl+Alt is AltGr on
            // several Windows/Linux layouts. Keep those combinations for typing.
            if shortcut.modifiers.alt {
                return Err(invalid(
                    "Alt with a letter or digit is reserved for text input",
                ));
            }
            if matches!(
                character,
                'A' | 'C' | 'V' | 'X' | 'Y' | 'Z' | 'Q' | 'W' | 'H' | 'M'
            ) {
                return Err(invalid(
                    "this key combination is reserved for editing or window controls",
                ));
            }
        }
        if shortcut.modifiers.alt && shortcut.key == ShortcutKey::Function(4) {
            return Err(invalid("Alt+F4 is reserved for closing the window"));
        }
        Ok(shortcut)
    }

    pub fn modifiers(&self, macos: bool) -> ShortcutModifiers {
        let mut modifiers = self.modifiers;
        if self.primary {
            if macos {
                modifiers.meta = true;
            } else {
                modifiers.control = true;
            }
        }
        modifiers
    }

    pub fn canonical(&self) -> String {
        let mut parts = Vec::new();
        for (enabled, name) in [
            (self.primary, "Mod"),
            (self.modifiers.control, "Ctrl"),
            (self.modifiers.meta, "Meta"),
            (self.modifiers.alt, "Alt"),
            (self.modifiers.shift, "Shift"),
        ] {
            if enabled {
                parts.push(name.to_owned());
            }
        }
        parts.push(match self.key {
            ShortcutKey::Character(character) => character.to_string(),
            ShortcutKey::Up => "Up".into(),
            ShortcutKey::Down => "Down".into(),
            ShortcutKey::Left => "Left".into(),
            ShortcutKey::Right => "Right".into(),
            ShortcutKey::Home => "Home".into(),
            ShortcutKey::End => "End".into(),
            ShortcutKey::PageUp => "PageUp".into(),
            ShortcutKey::PageDown => "PageDown".into(),
            ShortcutKey::Function(number) => format!("F{number}"),
        });
        parts.join("+")
    }
}

pub fn validate_shortcuts(values: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    let mut bindings: Vec<(&str, Shortcut)> = Vec::new();
    for (name, value) in values {
        if ShortcutAction::parse(name).is_none() {
            return Err(ConfigError::Invalid(format!(
                "unknown shortcut action {name:?}"
            )));
        }
        let shortcut = Shortcut::parse(value)?;
        for (other_name, other) in &bindings {
            // A config is portable: reject an alias conflict on either platform,
            // even when the file is currently being edited on the other one.
            if other.key == shortcut.key
                && [false, true]
                    .into_iter()
                    .any(|macos| other.modifiers(macos) == shortcut.modifiers(macos))
            {
                return Err(ConfigError::Invalid(format!(
                    "shortcuts {other_name} and {name} conflict"
                )));
            }
        }
        bindings.push((name, shortcut));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn standard() -> Result<Self, ConfigError> {
        let dirs = ProjectDirs::from("io.github", "brk275", "Morpheus")
            .ok_or(ConfigError::MissingConfigDirectory)?;
        Ok(Self::at(dirs.config_dir().join("config.toml")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn themes_dir(&self) -> Result<PathBuf, ConfigError> {
        Ok(self
            .path
            .parent()
            .ok_or_else(|| {
                ConfigError::Invalid("configuration path has no parent directory".into())
            })?
            .join("themes"))
    }

    pub fn load_theme(&self, config: &Config) -> Result<Option<ThemePack>, ConfigError> {
        let Some(name) = config.appearance.theme_pack.as_deref() else {
            return Ok(None);
        };
        config.validate()?;
        let path = self.themes_dir()?.join(format!("{name}.toml"));
        let raw = fs::read_to_string(path)?;
        let theme: ThemePack = toml::from_str(&raw)?;
        theme.validate()?;
        Ok(Some(theme))
    }

    pub fn load_or_create(&self) -> Result<Config, ConfigError> {
        if !self.path.exists() {
            let config = Config::default();
            self.save(&config)?;
            return Ok(config);
        }
        self.load()
    }

    pub fn load(&self) -> Result<Config, ConfigError> {
        let raw = fs::read_to_string(&self.path)?;
        let config: Config = toml::from_str(&raw)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        config.validate()?;
        let parent = self.path.parent().ok_or_else(|| {
            ConfigError::Invalid("configuration path has no parent directory".into())
        })?;
        fs::create_dir_all(parent)?;
        let raw = toml::to_string_pretty(config)?;
        // A unique file in the same directory makes concurrent saves safe and
        // lets persist replace the destination atomically on each platform.
        // If replacement fails, the previous valid config stays in place.
        let mut staging = tempfile::NamedTempFile::new_in(parent)?;
        staging.write_all(raw.as_bytes())?;
        staging.as_file().sync_all()?;
        staging
            .persist(&self.path)
            .map_err(|error| ConfigError::Io(error.error))?;
        Ok(())
    }

    /// Poll the user-editable TOML file. Invalid edits yield an error; consumers
    /// keep the last valid `Config` until a later valid edit arrives.
    pub fn watch(&self, interval: Duration) -> Receiver<Result<Config, ConfigError>> {
        let (sender, receiver) = mpsc::channel();
        let path = self.path.clone();
        thread::spawn(move || {
            let mut last_raw: Option<String> = None;
            loop {
                thread::sleep(interval);
                let Ok(raw) = fs::read_to_string(&path) else {
                    continue;
                };
                if last_raw.as_ref() == Some(&raw) {
                    continue;
                }
                last_raw = Some(raw);
                let result = ConfigStore::at(&path).load();
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        receiver
    }

    /// Watch the settings file and its selected theme pack as a single unit.
    /// Callers only apply `Ok` values so a broken external edit leaves the
    /// last valid appearance in place while its error can be shown in the UI.
    pub fn watch_with_theme(
        &self,
        interval: Duration,
    ) -> Receiver<Result<LoadedConfig, ConfigError>> {
        let (sender, receiver) = mpsc::channel();
        let store = self.clone();
        thread::spawn(move || {
            let mut last_signature: Option<(
                Result<String, String>,
                Option<Result<String, String>>,
            )> = None;
            loop {
                thread::sleep(interval);
                let config_raw =
                    fs::read_to_string(store.path()).map_err(|error| error.to_string());
                let config = config_raw
                    .as_ref()
                    .ok()
                    .map(String::as_str)
                    .map(toml::from_str::<Config>)
                    .transpose();
                let theme_raw = config
                    .as_ref()
                    .ok()
                    .and_then(|config| config.as_ref())
                    .and_then(|config| config.validate().ok().map(|_| config))
                    .and_then(|config| config.appearance.theme_pack.as_ref())
                    .and_then(|name| {
                        store.themes_dir().ok().map(|directory| {
                            fs::read_to_string(directory.join(format!("{name}.toml")))
                                .map_err(|error| error.to_string())
                        })
                    });
                let signature = (config_raw, theme_raw);
                if last_signature.as_ref() == Some(&signature) {
                    continue;
                }
                last_signature = Some(signature);
                let result = store.load().and_then(|config| {
                    let theme = store.load_theme(&config)?;
                    Ok(LoadedConfig { config, theme })
                });
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        receiver
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: Config,
    pub theme: Option<ThemePack>,
}

/// A declarative theme pack. It cannot execute code or read arbitrary files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemePack {
    pub schema_version: u32,
    pub name: String,
    pub colors: ThemeColors,
}

impl ThemePack {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::Version(self.schema_version));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 80 {
            return Err(ConfigError::InvalidTheme(
                "name must contain between 1 and 80 characters".into(),
            ));
        }
        for (label, value) in self.colors.entries() {
            if value.len() != 7
                || !value.starts_with('#')
                || !value[1..]
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(ConfigError::InvalidTheme(format!(
                    "colors.{label} must be a #RRGGBB color"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeColors {
    pub rail: String,
    pub sidebar: String,
    pub canvas: String,
    pub panel: String,
    pub elevated: String,
    pub hover: String,
    pub ink: String,
    pub muted: String,
    pub divider: String,
    pub accent: String,
    pub accent_ink: String,
}

impl ThemeColors {
    fn entries(&self) -> [(&'static str, &str); 11] {
        [
            ("rail", &self.rail),
            ("sidebar", &self.sidebar),
            ("canvas", &self.canvas),
            ("panel", &self.panel),
            ("elevated", &self.elevated),
            ("hover", &self.hover),
            ("ink", &self.ink),
            ("muted", &self.muted),
            ("divider", &self.divider),
            ("accent", &self.accent),
            ("accent_ink", &self.accent_ink),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_parser_normalizes_keys_and_resolves_platform_modifiers() {
        let shortcut = Shortcut::parse(" shift + mOd + k ").unwrap();
        assert_eq!(shortcut.canonical(), "Mod+Shift+K");
        assert_eq!(shortcut.key, ShortcutKey::Character('K'));
        assert_eq!(
            shortcut.modifiers(false),
            ShortcutModifiers {
                control: true,
                shift: true,
                ..Default::default()
            }
        );
        assert_eq!(
            shortcut.modifiers(true),
            ShortcutModifiers {
                meta: true,
                shift: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Shortcut::parse("alt+pagedown").unwrap().canonical(),
            "Alt+PageDown"
        );
        assert_eq!(Shortcut::parse("Ctrl+F12").unwrap().canonical(), "Ctrl+F12");
        validate_shortcuts(&default_shortcuts()).unwrap();
    }

    #[test]
    fn shortcut_validation_preserves_typing_modal_keys_and_window_controls() {
        for invalid in [
            "",
            "K",
            "Shift+K",
            "Mod+",
            "Mod+K+F",
            "Mod+Mod+K",
            "Mod+Ctrl+K",
            "Command+K",
            "Mod+F13",
            "Mod+Escape",
            "Alt+Tab",
            "Mod+Enter",
            "Alt+A",
            "Ctrl+Alt+2",
            "Mod+C",
            "Ctrl+Shift+Z",
            "Mod+Shift+1",
            "Meta+Shift+0",
            "Meta+W",
            "Alt+F4",
            "Mod+Ｋ",
            "Mod+K\n",
        ] {
            assert!(Shortcut::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn shortcuts_reject_unknown_actions_and_conflicts_on_either_platform() {
        let mut config = Config::default();
        config
            .shortcuts
            .insert("run_command".into(), "Mod+R".into());
        assert!(config.validate().is_err());
        config.shortcuts.remove("run_command");
        for conflict in ["mod+k", "Ctrl+K", "Meta+K"] {
            config.shortcuts.insert("search".into(), conflict.into());
            assert!(config.validate().is_err(), "accepted conflict {conflict}");
        }
        config
            .shortcuts
            .insert("search".into(), "Mod+Shift+K".into());
        config.validate().unwrap();
        config.shortcuts.clear();
        config.validate().unwrap(); // A deliberately empty table disables all commands.
    }

    #[test]
    fn invalid_shortcut_save_preserves_the_last_valid_file_and_reload_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        let original = fs::read(store.path()).unwrap();
        config.shortcuts.insert("search".into(), "Mod+K".into());
        assert!(store.save(&config).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), original);
        config.shortcuts.insert("search".into(), "Mod+J".into());
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap().shortcuts["search"], "Mod+J");
        fs::write(store.path(), "[shortcuts]\nsearch = 'Mod+Tab'\n").unwrap();
        assert!(store.load().is_err());
        // Validation never repairs/overwrites an external malformed edit.
        assert!(
            fs::read_to_string(store.path())
                .unwrap()
                .contains("Mod+Tab")
        );
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), config);
    }

    #[test]
    fn creates_and_round_trips_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(dir.path().join("config.toml"));
        let created = store.load_or_create().unwrap();
        assert_eq!(created, Config::default());
        assert_eq!(store.load().unwrap(), created);
        let mut updated = created;
        updated.layout.room_list_width = 320;
        store.save(&updated).unwrap();
        assert_eq!(store.load().unwrap(), updated);
    }

    #[test]
    fn rejects_invalid_edit_without_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(dir.path().join("config.toml"));
        store.save(&Config::default()).unwrap();
        fs::write(store.path(), "schema_version = 99\n").unwrap();
        assert!(matches!(store.load(), Err(ConfigError::Version(99))));
        assert_eq!(
            fs::read_to_string(store.path()).unwrap(),
            "schema_version = 99\n"
        );
    }

    #[test]
    fn validates_layout_and_scale() {
        let mut config = Config::default();
        config.layout.room_list_width = 100;
        assert!(config.validate().is_err());
        config.layout.room_list_width = 260;
        config.appearance.scale_percent = 250;
        assert!(config.validate().is_err());
    }

    #[test]
    fn loads_declarative_theme_and_rejects_bad_color() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let theme_directory = store.themes_dir().unwrap();
        fs::create_dir_all(&theme_directory).unwrap();
        let mut config = Config::default();
        config.appearance.theme_pack = Some("aurora".into());
        let mut theme = ThemePack {
            schema_version: SCHEMA_VERSION,
            name: "Aurora".into(),
            colors: ThemeColors {
                rail: "#10131B".into(),
                sidebar: "#171B26".into(),
                canvas: "#1E2330".into(),
                panel: "#191E2A".into(),
                elevated: "#272D3B".into(),
                hover: "#32394B".into(),
                ink: "#FFFFFF".into(),
                muted: "#A7AFC1".into(),
                divider: "#303747".into(),
                accent: "#A88CFF".into(),
                accent_ink: "#FFFFFF".into(),
            },
        };
        fs::write(
            theme_directory.join("aurora.toml"),
            toml::to_string_pretty(&theme).unwrap(),
        )
        .unwrap();
        assert_eq!(store.load_theme(&config).unwrap(), Some(theme.clone()));
        theme.colors.accent = "violet".into();
        fs::write(
            theme_directory.join("aurora.toml"),
            toml::to_string_pretty(&theme).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.load_theme(&config),
            Err(ConfigError::InvalidTheme(_))
        ));
        config.appearance.theme_pack = Some("../escape".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn theme_watcher_reports_invalid_edit_after_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        config.appearance.theme_pack = Some("missing".into());
        store.save(&config).unwrap();

        let updates = store.watch_with_theme(Duration::from_millis(10));
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ConfigError::Io(_))
        ));

        let themes = store.themes_dir().unwrap();
        fs::create_dir_all(&themes).unwrap();
        fs::write(themes.join("missing.toml"), "not valid = [").unwrap();
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ConfigError::Decode(_))
        ));
    }

    #[test]
    fn local_voice_space_designation_is_account_scoped_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        let room = "!voice:example.org";
        assert!(config.set_local_voice_room("alice", room, true).unwrap());
        assert!(!config.set_local_voice_room("alice", room, true).unwrap());
        assert!(config.is_local_voice_room("alice", room));
        assert!(!config.is_local_voice_room("carol", room));
        store.save(&config).unwrap();
        assert!(store.load().unwrap().is_local_voice_room("alice", room));
        assert!(config.set_local_voice_room("alice", room, false).unwrap());
        assert!(!config.set_local_voice_room("alice", room, false).unwrap());
        assert!(!config.account_overrides.contains_key("alice"));
        assert!(config.set_local_voice_room("", room, true).is_err());
        assert!(
            config
                .set_local_voice_room("alice", "#alias:example.org", true)
                .is_err()
        );
        config.set_local_voice_room("alice", room, true).unwrap();
        assert!(config.clear_local_voice_rooms("alice"));
        assert!(!config.clear_local_voice_rooms("alice"));
        assert!(!config.account_overrides.contains_key("alice"));
        config.set_local_voice_room("alice", room, true).unwrap();
        config
            .account_overrides
            .get_mut("alice")
            .unwrap()
            .notifications_enabled = Some(false);
        assert!(config.clear_local_voice_rooms("alice"));
        assert_eq!(
            config
                .account_overrides
                .get("alice")
                .unwrap()
                .notifications_enabled,
            Some(false)
        );
    }

    #[test]
    fn desktop_notification_override_is_account_scoped() {
        let mut config = Config::default();
        assert!(config.notifications_enabled_for("alice"));
        config
            .account_overrides
            .entry("alice".into())
            .or_default()
            .notifications_enabled = Some(false);
        assert!(!config.notifications_enabled_for("alice"));
        assert!(config.notifications_enabled_for("bob"));
        config.notifications.enabled = false;
        assert!(!config.notifications_enabled_for("bob"));
    }

    #[test]
    fn legacy_element_call_choice_is_room_and_account_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        let room = "!legacy:example.org";
        let other_room = "!standard:example.org";
        assert!(!config.is_legacy_element_call_room("alice", room));
        assert!(
            config
                .set_legacy_element_call_room("alice", room, true)
                .unwrap()
        );
        assert!(
            !config
                .set_legacy_element_call_room("alice", room, true)
                .unwrap()
        );
        assert!(config.is_legacy_element_call_room("alice", room));
        assert!(!config.is_legacy_element_call_room("alice", other_room));
        assert!(!config.is_legacy_element_call_room("bob", room));
        store.save(&config).unwrap();
        assert!(
            store
                .load()
                .unwrap()
                .is_legacy_element_call_room("alice", room)
        );
        assert!(
            config
                .set_legacy_element_call_room("alice", room, false)
                .unwrap()
        );
        assert!(!config.is_legacy_element_call_room("alice", room));
        assert!(
            config
                .set_legacy_element_call_room("alice", room, true)
                .unwrap()
        );
        assert!(config.set_legacy_element_call_room("", room, true).is_err());
        assert!(
            config
                .set_legacy_element_call_room("alice", "#alias:example.org", true)
                .is_err()
        );
        config
            .account_overrides
            .get_mut("alice")
            .unwrap()
            .notifications_enabled = Some(false);
        assert!(config.clear_legacy_element_call_rooms("alice"));
        assert!(!config.is_legacy_element_call_room("alice", room));
        assert_eq!(
            config
                .account_overrides
                .get("alice")
                .unwrap()
                .notifications_enabled,
            Some(false)
        );
        assert!(!config.clear_legacy_element_call_rooms("alice"));
    }

    #[test]
    fn matrix_2_0_choice_preserves_legacy_configs_and_switches_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        fs::write(
            store.path(),
            "[account_overrides.alice]\nlegacy_element_call_rooms = [\"!old:example.org\"]\n",
        )
        .unwrap();
        let mut config = store.load().unwrap();
        assert_eq!(
            config.room_call_mode("alice", "!old:example.org"),
            RoomCallMode::ElementCallLegacy
        );
        let room = "!call:example.org";
        config
            .set_room_call_mode("alice", room, RoomCallMode::ElementCallMatrix2)
            .unwrap();
        let captured_join_mode = config.room_call_mode("alice", room);
        assert_eq!(captured_join_mode, RoomCallMode::ElementCallMatrix2);
        assert_eq!(config.room_call_mode("bob", room), RoomCallMode::Standard);
        assert_eq!(
            config.room_call_mode("alice", "!other:example.org"),
            RoomCallMode::Standard
        );
        store.save(&config).unwrap();
        let mut config = store.load().unwrap();
        assert_eq!(
            config.room_call_mode("alice", room),
            RoomCallMode::ElementCallMatrix2
        );
        assert_eq!(
            config.room_call_mode("alice", "!old:example.org"),
            RoomCallMode::ElementCallLegacy
        );
        config
            .set_room_call_mode("alice", room, RoomCallMode::ElementCallLegacy)
            .unwrap();
        assert!(
            !config.account_overrides["alice"]
                .element_call_matrix_2_0_rooms
                .contains(&room.to_owned())
        );
        config.validate().unwrap();
        config
            .set_room_call_mode("alice", room, RoomCallMode::Standard)
            .unwrap();
        assert_eq!(config.room_call_mode("alice", room), RoomCallMode::Standard);
        assert_eq!(captured_join_mode, RoomCallMode::ElementCallMatrix2);
        assert!(config.clear_room_call_modes("alice"));
        assert!(!config.account_overrides.contains_key("alice"));
    }

    #[test]
    fn conflicting_call_modes_cannot_replace_valid_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        let room = "!call:example.org";
        config
            .set_room_call_mode("alice", room, RoomCallMode::ElementCallMatrix2)
            .unwrap();
        store.save(&config).unwrap();
        let valid = fs::read(store.path()).unwrap();
        config
            .account_overrides
            .get_mut("alice")
            .unwrap()
            .legacy_element_call_rooms
            .push(room.into());
        assert!(config.validate().is_err());
        assert!(store.save(&config).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), valid);
        for index in [-1, 3, i32::MAX] {
            assert_eq!(RoomCallMode::from_index(index), None);
        }
        assert!(
            Config::default()
                .set_room_call_mode(
                    "alice",
                    "#alias:example.org",
                    RoomCallMode::ElementCallMatrix2
                )
                .is_err()
        );
        assert!(
            Config::default()
                .set_room_call_mode("", room, RoomCallMode::ElementCallMatrix2)
                .is_err()
        );
    }
}
