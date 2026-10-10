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
        for (action, shortcut) in &self.shortcuts {
            if action.trim().is_empty() || shortcut.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "shortcuts must have non-empty actions and key combinations".into(),
                ));
            }
        }
        for overrides in self.account_overrides.values() {
            for room_id in &overrides.local_voice_rooms {
                if !room_id.starts_with('!') || !room_id.contains(':') {
                    return Err(ConfigError::Invalid(
                        "account_overrides.local_voice_rooms must contain Matrix room IDs".into(),
                    ));
                }
            }
        }
        Ok(())
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
}

fn default_shortcuts() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("quick_switcher".into(), "Mod+K".into()),
        ("search".into(), "Mod+F".into()),
        ("next_unread".into(), "Alt+Down".into()),
        ("previous_unread".into(), "Alt+Up".into()),
    ])
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
            let mut last_signature: Option<(Option<String>, Option<String>)> = None;
            loop {
                thread::sleep(interval);
                let config_raw = fs::read_to_string(store.path()).ok();
                let config = config_raw
                    .as_deref()
                    .map(toml::from_str::<Config>)
                    .transpose();
                let theme_raw = config
                    .as_ref()
                    .ok()
                    .and_then(|config| config.as_ref())
                    .and_then(|config| config.validate().ok().map(|_| config))
                    .and_then(|config| config.appearance.theme_pack.as_ref())
                    .and_then(|name| {
                        store.themes_dir().ok().and_then(|directory| {
                            fs::read_to_string(directory.join(format!("{name}.toml"))).ok()
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
}
