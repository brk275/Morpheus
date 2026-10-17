//! Versioned, non-secret configuration for the Morpheus desktop client.

use directories::ProjectDirs;
// Use the same opaque room-ID grammar as Ruma/Matrix SDK. Since room v12,
// room IDs need not include a server-name suffix.
use ruma_identifiers_validation::room_id::validate as validate_room_id;
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
    pub typography: Typography,
    pub geometry: Geometry,
    pub chat: ChatStyle,
    pub conversation: Conversation,
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
            typography: Typography::default(),
            geometry: Geometry::default(),
            chat: ChatStyle::default(),
            conversation: Conversation::default(),
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
        validate_font_family("appearance.font_family", &self.appearance.font_family)?;
        self.typography.validate()?;
        self.geometry.validate()?;
        self.chat.validate()?;
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
        self.media.validate_audio_devices()?;
        for overrides in self.account_overrides.values() {
            for room_id in &overrides.local_voice_rooms {
                if validate_room_id(room_id).is_err() {
                    return Err(ConfigError::Invalid(
                        "account_overrides.local_voice_rooms must contain Matrix room IDs".into(),
                    ));
                }
            }
            for room_id in &overrides.legacy_element_call_rooms {
                if validate_room_id(room_id).is_err() {
                    return Err(ConfigError::Invalid(
                        "account_overrides.legacy_element_call_rooms must contain Matrix room IDs"
                            .into(),
                    ));
                }
            }
            for room_id in &overrides.element_call_matrix_2_0_rooms {
                if validate_room_id(room_id).is_err() {
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

    /// Materialize a built-in style into the same preferences edited by TOML
    /// and Settings. No hidden preset layer overrides subsequent custom edits.
    /// Account, notification, shortcut and media preferences are untouched.
    pub fn apply_style_preset(&mut self, preset: BuiltInStylePreset) {
        self.appearance.theme_pack = Some(preset.theme_name().into());
        self.typography = Typography::default();
        self.geometry = Geometry::default();
        self.chat = ChatStyle::default();
        match preset {
            BuiltInStylePreset::Nocturne => {
                self.appearance.theme = ThemeMode::Dark;
                self.appearance.font_family = "Inter".into();
                self.appearance.density = Density::Comfortable;
                self.geometry.corner_radius = 8;
                self.geometry.message_gap = 10;
                self.layout.room_list_width = 260;
                self.layout.details_width = 280;
                self.layout.show_member_panel = true;
            }
            BuiltInStylePreset::Terminal => {
                self.appearance.theme = ThemeMode::Dark;
                self.appearance.font_family = "monospace".into();
                self.appearance.density = Density::Compact;
                self.typography.message_font_family = Some("monospace".into());
                self.typography.message_size = 13;
                self.geometry.corner_radius = 0;
                self.geometry.message_gap = 2;
                self.geometry.content_padding = 10;
                self.geometry.rail_width = 52;
                self.chat.layout = MessageLayout::Columns;
                self.chat.show_avatars = false;
                self.chat.color_senders = true;
                self.chat.nickname_width = 112;
                self.chat.nickname_brackets = true;
                self.layout.room_list_width = 220;
                self.layout.details_width = 240;
                self.layout.show_member_panel = false;
            }
            BuiltInStylePreset::Paper => {
                self.appearance.theme = ThemeMode::Light;
                self.appearance.font_family = "sans-serif".into();
                self.appearance.density = Density::Comfortable;
                self.typography.message_font_family = Some("serif".into());
                self.typography.message_size = 16;
                self.geometry.corner_radius = 3;
                self.geometry.message_gap = 14;
                self.geometry.content_padding = 32;
                self.geometry.rail_width = 60;
                self.chat.layout = MessageLayout::Compact;
                self.chat.show_avatars = false;
                self.layout.room_list_width = 240;
                self.layout.details_width = 280;
                self.layout.show_member_panel = false;
            }
        }
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
        if account_id.is_empty() || validate_room_id(room_id).is_err() {
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
        if account_id.is_empty() || validate_room_id(room_id).is_err() {
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

/// Message typography is independent of interface scale and its font family.
/// `None` inherits `appearance.font_family`; fonts are installed family names,
/// never URLs or paths. Missing families use the renderer's normal fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Typography {
    pub message_font_family: Option<String>,
    pub monospace_font_family: String,
    pub message_size: u16,
    /// Omission preserves each sender surface's existing weight.
    pub sender_weight: Option<u16>,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            message_font_family: None,
            monospace_font_family: "monospace".into(),
            message_size: 14,
            sender_weight: None,
        }
    }
}

impl Typography {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(family) = &self.message_font_family {
            validate_font_family("typography.message_font_family", family)?;
        }
        validate_font_family(
            "typography.monospace_font_family",
            &self.monospace_font_family,
        )?;
        validate_range("typography.message_size", self.message_size, 10, 32)?;
        if let Some(weight) = self.sender_weight {
            validate_range("typography.sender_weight", weight, 100, 900)?;
        }
        Ok(())
    }

    pub fn message_family<'a>(&'a self, appearance: &'a Appearance) -> &'a str {
        self.message_font_family
            .as_deref()
            .unwrap_or(&appearance.font_family)
    }
}

/// Pixel values are logical pixels before desktop DPI scaling. The app's text
/// scale remains independent so users can enlarge text without widening panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Geometry {
    pub corner_radius: u16,
    pub message_gap: u16,
    pub content_padding: u16,
    pub rail_width: u16,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            corner_radius: 10,
            message_gap: 12,
            content_padding: 24,
            rail_width: 70,
        }
    }
}

impl Geometry {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_range("geometry.corner_radius", self.corner_radius, 0, 24)?;
        validate_range("geometry.message_gap", self.message_gap, 0, 32)?;
        validate_range("geometry.content_padding", self.content_padding, 0, 48)?;
        validate_range("geometry.rail_width", self.rail_width, 44, 100)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageLayout {
    #[default]
    Stacked,
    Compact,
    Columns,
}

/// Fixed local-clock formats, independent of locale and arbitrary format strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampFormat {
    #[default]
    TwelveHour,
    TwentyFourHour,
    TwentyFourHourSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatStyle {
    pub layout: MessageLayout,
    pub show_avatars: bool,
    pub show_timestamps: bool,
    pub timestamp_format: TimestampFormat,
    pub timestamp_brackets: bool,
    pub timestamp_width: u16,
    pub color_senders: bool,
    pub nickname_width: u16,
    /// Wrap the Columns nickname as `<name>`; accessible names stay plain.
    pub nickname_brackets: bool,
    /// Text alignment inside the Columns nickname column.
    pub nickname_alignment: NicknameAlignment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NicknameAlignment {
    Left,
    #[default]
    Right,
}

impl Default for ChatStyle {
    fn default() -> Self {
        Self {
            layout: MessageLayout::Stacked,
            show_avatars: true,
            show_timestamps: true,
            timestamp_format: TimestampFormat::TwelveHour,
            timestamp_brackets: false,
            timestamp_width: 72,
            color_senders: false,
            nickname_width: 128,
            nickname_brackets: false,
            nickname_alignment: NicknameAlignment::Right,
        }
    }
}

impl ChatStyle {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_range("chat.nickname_width", self.nickname_width, 64, 240)?;
        validate_range("chat.timestamp_width", self.timestamp_width, 48, 160)
    }
}

/// Room-header presentation; status, calls and room actions remain visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationHeaderMode {
    /// Preserve the existing room header and introductory welcome/topic block.
    #[default]
    Full,
    /// Keep the room header and its topic, without the introductory block.
    Compact,
    /// Keep room identity and actions; omit topic and introductory decoration.
    Minimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Conversation {
    pub header: ConversationHeaderMode,
}

fn validate_range(name: &str, value: u16, minimum: u16, maximum: u16) -> Result<(), ConfigError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(ConfigError::Invalid(format!(
            "{name} must be between {minimum} and {maximum}"
        )));
    }
    Ok(())
}

fn validate_font_family(name: &str, family: &str) -> Result<(), ConfigError> {
    if family.trim().is_empty()
        || family.len() > 128
        || family.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return Err(ConfigError::Invalid(format!(
            "{name} must be a nonempty font family of at most 128 bytes without control or direction characters"
        )));
    }
    Ok(())
}

/// Original starting points shipped with Morpheus. Users can override any
/// built-in palette with a TOML file of the same name in their themes folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInStylePreset {
    Nocturne,
    Terminal,
    Paper,
}

impl BuiltInStylePreset {
    pub const ALL: [Self; 3] = [Self::Nocturne, Self::Terminal, Self::Paper];

    pub fn theme_name(self) -> &'static str {
        match self {
            Self::Nocturne => "nocturne",
            Self::Terminal => "terminal",
            Self::Paper => "paper",
        }
    }

    fn theme_source(self) -> &'static str {
        match self {
            Self::Nocturne => include_str!("../../../themes/nocturne.toml"),
            Self::Terminal => include_str!("../../../themes/terminal.toml"),
            Self::Paper => include_str!("../../../themes/paper.toml"),
        }
    }

    pub fn theme(self) -> Result<ThemePack, ConfigError> {
        let theme: ThemePack = toml::from_str(self.theme_source())?;
        theme.validate()?;
        Ok(theme)
    }

    fn from_theme_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.theme_name() == name)
    }
}

/// Location and presentation of the same Home, Space and application routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationMode {
    #[default]
    Rail,
    Sidebar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub navigation: NavigationMode,
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
            navigation: NavigationMode::Rail,
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
    /// CPAL host-qualified device ID; None follows the system default.
    pub microphone_device: Option<String>,
    /// CPAL host-qualified device ID; None follows the system default.
    pub speaker_device: Option<String>,
    pub camera_device: Option<String>,
    pub noise_suppression: bool,
    pub autoplay_animated_media: bool,
    pub previews_in_encrypted_rooms: bool,
}

impl Media {
    fn validate_audio_devices(&self) -> Result<(), ConfigError> {
        for (field, id) in [
            ("microphone_device", &self.microphone_device),
            ("speaker_device", &self.speaker_device),
        ] {
            let Some(id) = id else { continue };
            // The opaque suffix may contain colons and Unicode. Accept other
            // platforms' host names so moving a config never silently chooses
            // a different device; availability is checked at call startup.
            let valid = id.len() <= 4096
                && !id.chars().any(char::is_control)
                && id.split_once(':').is_some_and(|(host, device)| {
                    !host.is_empty()
                        && host
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                        && !device.trim().is_empty()
                });
            if !valid {
                return Err(ConfigError::Invalid(format!(
                    "media.{field} must be a host-qualified device ID of at most 4096 bytes; remove the field to use the system default"
                )));
            }
        }
        Ok(())
    }
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
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(preset) = BuiltInStylePreset::from_theme_name(name) {
                    return preset.theme().map(Some);
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
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
    /// Optional semantic roles. Missing values preserve each control's legacy
    /// palette choice; they are not synthesized from a different theme.
    #[serde(default, skip_serializing_if = "ThemeRoles::is_empty")]
    pub roles: ThemeRoles,
    /// Sender-name text colors. Avatar backgrounds keep their own palette.
    #[serde(default, skip_serializing_if = "SenderColors::is_empty")]
    pub senders: SenderColors,
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
            validate_theme_color(&format!("colors.{label}"), value)?;
        }
        for (label, value) in self.roles.entries() {
            if let Some(value) = value {
                validate_theme_color(&format!("roles.{label}"), value)?;
            }
        }
        if let Some(colors) = &self.senders.colors {
            if !(1..=MAX_SENDER_COLORS).contains(&colors.len()) {
                return Err(ConfigError::InvalidTheme(format!(
                    "senders.colors must contain between 1 and {MAX_SENDER_COLORS} colors; omit it to use the legacy sender colors"
                )));
            }
            for (index, value) in colors.iter().enumerate() {
                validate_theme_color(&format!("senders.colors[{index}]"), value)?;
            }
        }
        Ok(())
    }
}

fn validate_theme_color(field: &str, value: &str) -> Result<(), ConfigError> {
    if value.len() != 7
        || !value.starts_with('#')
        || !value[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(ConfigError::InvalidTheme(format!(
            "{field} must be a #RRGGBB color"
        )));
    }
    Ok(())
}

/// Semantic colors are optional so schema-1 packs containing only `colors`
/// keep their previous behavior. User colors are syntax-checked; contrast is
/// a theme author's choice. Built-in themes have separate contrast tests.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeRoles {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_ink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_ink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_ink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_ink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_ink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Timeline and thread clock text; omitted keeps the muted color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Sender names when the sender palette is off; omitted keeps ink.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Message body text; omitted keeps ink.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Name and body text of the account's own messages; omitted falls back
    /// to `sender` and `message`, then ink. The sender palette still wins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_message: Option<String>,
    /// Row background of the account's own messages when neither selected
    /// nor hovered; omitted keeps the previous row background.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_surface: Option<String>,
}

impl ThemeRoles {
    pub fn entries(&self) -> [(&'static str, Option<&str>); 20] {
        [
            ("error", self.error.as_deref()),
            ("error_surface", self.error_surface.as_deref()),
            ("error_ink", self.error_ink.as_deref()),
            ("success", self.success.as_deref()),
            ("success_surface", self.success_surface.as_deref()),
            ("success_ink", self.success_ink.as_deref()),
            ("selection", self.selection.as_deref()),
            ("selection_ink", self.selection_ink.as_deref()),
            ("focus", self.focus.as_deref()),
            ("link", self.link.as_deref()),
            ("unread", self.unread.as_deref()),
            ("unread_ink", self.unread_ink.as_deref()),
            ("destructive", self.destructive.as_deref()),
            ("destructive_ink", self.destructive_ink.as_deref()),
            ("voice", self.voice.as_deref()),
            ("timestamp", self.timestamp.as_deref()),
            ("sender", self.sender.as_deref()),
            ("message", self.message.as_deref()),
            ("own_message", self.own_message.as_deref()),
            ("own_surface", self.own_surface.as_deref()),
        ]
    }

    pub fn is_empty(&self) -> bool {
        self.entries().iter().all(|(_, value)| value.is_none())
    }
}

pub const MAX_SENDER_COLORS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SenderColors {
    /// None preserves legacy sender text colors. Some must contain 1..=16
    /// colors. This palette never changes avatar or account identity colors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colors: Option<Vec<String>>,
}

impl SenderColors {
    pub fn is_empty(&self) -> bool {
        self.colors.is_none()
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
    fn audio_ids_roundtrip_without_device_names_or_schema_changes() {
        let mut config: Config = toml::from_str("schema_version = 1").unwrap();
        assert_eq!(config.media.microphone_device, None);
        assert_eq!(config.media.speaker_device, None);
        config.media.microphone_device = Some("coreaudio:USB:Microphone-東京".into());
        config.media.speaker_device = Some("wasapi:{0.0.0.00000000}.{example}".into());
        config.validate().unwrap(); // Other operating systems' IDs are valid config.
        let encoded = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&encoded).unwrap(), config);
    }

    #[test]
    fn malformed_audio_ids_never_replace_last_valid_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        config.media.microphone_device = Some("alsa:hw:CARD=USB,DEV=0".into());
        store.save(&config).unwrap();
        let saved = fs::read(store.path()).unwrap();
        for bad in [
            "".into(),
            "USB Microphone".into(),
            ":device".into(),
            "alsa:".into(),
            "alsa:  ".into(),
            "alsa:device\n".into(),
            "alsa:\0".into(),
            format!("alsa:{}", "x".repeat(4096)),
        ] {
            for microphone in [true, false] {
                let mut invalid = config.clone();
                if microphone {
                    invalid.media.microphone_device = Some(bad.clone());
                } else {
                    invalid.media.speaker_device = Some(bad.clone());
                }
                assert!(store.save(&invalid).is_err());
                assert_eq!(fs::read(store.path()).unwrap(), saved);
                assert_eq!(store.load().unwrap().media, config.media);
            }
        }
        fs::write(store.path(), "[media]\nmicrophone_device = 'not an ID'\n").unwrap();
        assert!(store.load().is_err());
        assert_eq!(
            config.media.microphone_device.as_deref(),
            Some("alsa:hw:CARD=USB,DEV=0")
        );
    }

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
    fn navigation_defaults_round_trip_and_reject_unknown_modes() {
        let old: Config =
            toml::from_str("schema_version = 1\n[layout]\nroom_list_width = 260\n").unwrap();
        assert_eq!(old.layout.navigation, NavigationMode::Rail);
        assert!(!old.layout.show_space_labels);
        assert!(old.layout.group_direct_messages);
        let mut config: Config = toml::from_str("schema_version = 1\n[layout]\nnavigation = \"sidebar\"\nshow_space_labels = true\ngroup_direct_messages = false\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.layout.navigation, NavigationMode::Sidebar);
        assert_eq!(
            toml::from_str::<Config>(&toml::to_string(&config).unwrap()).unwrap(),
            config
        );
        for preset in BuiltInStylePreset::ALL {
            config.apply_style_preset(preset);
            assert_eq!(config.layout.navigation, NavigationMode::Sidebar);
            assert!(config.layout.show_space_labels);
            assert!(!config.layout.group_direct_messages);
        }
        for bad in ["\"floating\"", "true", "1"] {
            assert!(toml::from_str::<Config>(&format!("[layout]\nnavigation = {bad}\n")).is_err());
        }
    }

    #[test]
    fn invalid_navigation_file_is_rejected_and_omission_restores_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut valid = Config::default();
        valid.layout.navigation = NavigationMode::Sidebar;
        valid.layout.group_direct_messages = false;
        store.save(&valid).unwrap();
        let updates = store.watch_with_theme(Duration::from_millis(10));
        assert_eq!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .config
                .layout
                .navigation,
            NavigationMode::Sidebar
        );
        fs::write(store.path(), "[layout]\nnavigation = \"unknown\"\n").unwrap();
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ConfigError::Decode(_))
        ));
        assert!(store.load().is_err());
        // A failed reload leaves the user's invalid edit intact for correction.
        assert_eq!(
            fs::read_to_string(store.path()).unwrap(),
            "[layout]\nnavigation = \"unknown\"\n"
        );
        // Removing the table restores all three defaults.
        fs::write(store.path(), "schema_version = 1\n").unwrap();
        let restored = updates
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .config;
        assert_eq!(restored.layout, Layout::default());
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
            roles: ThemeRoles::default(),
            senders: SenderColors::default(),
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
    #[test]
    fn legacy_and_serverless_room_preferences_roundtrip_without_cross_account_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        for room in [
            "!fixture:example.org",
            "!Nhcu5BS-UMnFX7hBVfVSoXiD7OgH6iRT-xyIuqDnpYQ",
        ] {
            assert!(config.set_local_voice_room("primary", room, true).unwrap());
            for mode in [
                RoomCallMode::ElementCallMatrix2,
                RoomCallMode::ElementCallLegacy,
                RoomCallMode::Standard,
            ] {
                config.set_room_call_mode("primary", room, mode).unwrap();
                store.save(&config).unwrap();
                config = store.load().unwrap();
                assert!(config.is_local_voice_room("primary", room));
                assert!(!config.is_local_voice_room("other", room));
                assert_eq!(config.room_call_mode("primary", room), mode);
                assert_eq!(config.room_call_mode("other", room), RoomCallMode::Standard);
                let overrides = &config.account_overrides["primary"];
                assert_eq!(
                    overrides
                        .legacy_element_call_rooms
                        .iter()
                        .any(|id| id == room),
                    mode == RoomCallMode::ElementCallLegacy
                );
                assert_eq!(
                    overrides
                        .element_call_matrix_2_0_rooms
                        .iter()
                        .any(|id| id == room),
                    mode == RoomCallMode::ElementCallMatrix2
                );
            }
            assert!(config.set_local_voice_room("primary", room, false).unwrap());
            store.save(&config).unwrap();
        }
        assert!(store.load().unwrap().account_overrides.is_empty());
    }

    #[test]
    fn invalid_room_ids_cannot_mutate_preferences_or_replace_valid_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let valid = store.load_or_create().unwrap();
        let original = fs::read(store.path()).unwrap();
        for room in [
            "",
            "room:example.org",
            "#alias:example.org",
            "@user:example.org",
            "!bad\0:example.org",
        ] {
            let mut config = valid.clone();
            assert!(config.set_local_voice_room("primary", room, true).is_err());
            for mode in [
                RoomCallMode::Standard,
                RoomCallMode::ElementCallLegacy,
                RoomCallMode::ElementCallMatrix2,
            ] {
                assert!(config.set_room_call_mode("primary", room, mode).is_err());
                assert_eq!(config, valid);
            }
            // Also cover hand-edited TOML: each stored room list is validated,
            // independently of the setter and before atomic replacement.
            for list in 0..3 {
                let mut invalid = valid.clone();
                let overrides = invalid
                    .account_overrides
                    .entry("primary".into())
                    .or_default();
                match list {
                    0 => &mut overrides.local_voice_rooms,
                    1 => &mut overrides.legacy_element_call_rooms,
                    _ => &mut overrides.element_call_matrix_2_0_rooms,
                }
                .push(room.into());
                assert!(invalid.validate().is_err());
                assert!(store.save(&invalid).is_err());
                assert_eq!(fs::read(store.path()).unwrap(), original);
                assert_eq!(store.load().unwrap(), valid);
            }
        }
    }

    #[test]
    fn old_schema_one_configs_get_complete_style_defaults() {
        let config: Config = toml::from_str(
            "schema_version = 1\n[appearance]\ndensity = 'compact'\nfont_family = 'Custom Font'\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.typography, Typography::default());
        assert_eq!(config.geometry, Geometry::default());
        assert_eq!(config.chat, ChatStyle::default());
        assert_eq!(config.appearance.density, Density::Compact);
        assert_eq!(
            config.typography.message_family(&config.appearance),
            "Custom Font"
        );
    }

    #[test]
    fn style_values_reject_out_of_bounds_and_unknown_options() {
        for raw in [
            "[typography]\nmessage_size=9",
            "[typography]\nmessage_size=33",
            "[geometry]\ncorner_radius=25",
            "[geometry]\nmessage_gap=33",
            "[geometry]\ncontent_padding=49",
            "[geometry]\nrail_width=43",
            "[geometry]\nrail_width=101",
            "[chat]\nnickname_width=63",
            "[chat]\nnickname_width=241",
        ] {
            let config: Config = toml::from_str(raw).unwrap();
            assert!(config.validate().is_err(), "accepted {raw}");
        }
        for raw in [
            "[chat]\nlayout='unknown'",
            "[geometry]\ncorner_raduis=2",
            "[typography]\nmessage_size=-1",
        ] {
            assert!(toml::from_str::<Config>(raw).is_err(), "accepted {raw}");
        }
        let bounds: Config = toml::from_str("[typography]\nmessage_size=32\n[geometry]\ncorner_radius=0\nmessage_gap=0\ncontent_padding=0\nrail_width=44\n[chat]\nnickname_width=240").unwrap();
        bounds.validate().unwrap();
    }

    #[test]
    fn font_names_reject_hidden_controls_and_excessive_size() {
        for family in [
            "".to_owned(),
            "   ".into(),
            "A\nB".into(),
            "A\u{202e}B".into(),
            "A\u{2067}B".into(),
            "a".repeat(129),
        ] {
            let mut config = Config::default();
            config.appearance.font_family = family.clone();
            assert!(config.validate().is_err());
            config.appearance.font_family = "Inter".into();
            config.typography.message_font_family = Some(family.clone());
            assert!(config.validate().is_err());
            config.typography.message_font_family = None;
            config.typography.monospace_font_family = family;
            assert!(config.validate().is_err());
        }
        let mut config = Config::default();
        config.typography.message_font_family = Some("Noto Sans 日本語".into());
        config.validate().unwrap();
        assert_eq!(
            config.typography.message_family(&config.appearance),
            "Noto Sans 日本語"
        );
    }

    #[test]
    fn presets_materialize_distinct_styles_and_preserve_other_preferences() {
        let mut config = Config::default();
        config.notifications.enabled = false;
        config.notifications.sounds = false;
        config.shortcuts.clear();
        config.appearance.scale_percent = 150;
        config.appearance.reduce_motion = true;
        config.layout.sort_unread_first = true;
        let original = config.clone();
        let mut styles = Vec::new();
        for preset in BuiltInStylePreset::ALL {
            config.apply_style_preset(preset);
            config.validate().unwrap();
            preset.theme().unwrap();
            assert_eq!(config.notifications, original.notifications);
            assert_eq!(config.media, original.media);
            assert_eq!(config.shortcuts, original.shortcuts);
            assert_eq!(config.account_overrides, original.account_overrides);
            assert_eq!(config.appearance.scale_percent, 150);
            assert!(config.appearance.reduce_motion);
            assert!(config.layout.sort_unread_first);
            assert_eq!(
                config.appearance.theme_pack.as_deref(),
                Some(preset.theme_name())
            );
            assert_eq!(
                config.chat.nickname_brackets,
                preset == BuiltInStylePreset::Terminal,
                "only the Terminal preset brackets nicknames"
            );
            assert_eq!(config.chat.nickname_alignment, NicknameAlignment::Right);
            styles.push((
                config.chat.layout,
                config.geometry.corner_radius,
                config.typography.message_size,
            ));
        }
        assert_ne!(styles[0], styles[1]);
        assert_ne!(styles[1], styles[2]);
        assert_ne!(styles[0], styles[2]);
    }

    #[test]
    fn builtin_theme_override_errors_never_fall_back_silently() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        config.apply_style_preset(BuiltInStylePreset::Terminal);
        let embedded = BuiltInStylePreset::Terminal.theme().unwrap();
        assert_eq!(store.load_theme(&config).unwrap(), Some(embedded.clone()));
        fs::create_dir(store.themes_dir().unwrap()).unwrap();
        let path = store.themes_dir().unwrap().join("terminal.toml");
        let mut custom = embedded.clone();
        custom.colors.accent = "#abcdef".into();
        fs::write(&path, toml::to_string_pretty(&custom).unwrap()).unwrap();
        assert_eq!(store.load_theme(&config).unwrap(), Some(custom));
        fs::write(&path, "broken = [").unwrap();
        assert!(store.load_theme(&config).is_err());
        fs::remove_file(&path).unwrap();
        assert_eq!(store.load_theme(&config).unwrap(), Some(embedded));
        config.appearance.theme_pack = Some("unknown".into());
        assert!(store.load_theme(&config).is_err());
    }

    #[test]
    fn custom_style_round_trips_without_a_hidden_preset_layer() {
        let mut config = Config::default();
        config.apply_style_preset(BuiltInStylePreset::Terminal);
        config.geometry.corner_radius = 19;
        config.chat.show_timestamps = false;
        config.typography.message_font_family = Some("User Font".into());
        let serialized = toml::to_string_pretty(&config).unwrap();
        let restored: Config = toml::from_str(&serialized).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored, config);
    }

    #[test]
    fn legacy_theme_has_no_implicit_roles_or_sender_palette() {
        let theme: ThemePack = toml::from_str(include_str!("../../../themes/aurora.toml")).unwrap();
        theme.validate().unwrap();
        assert!(theme.roles.is_empty());
        assert!(theme.senders.is_empty());
        let encoded = toml::to_string_pretty(&theme).unwrap();
        assert!(!encoded.contains("[roles]"));
        assert!(!encoded.contains("[senders]"));
        assert_eq!(toml::from_str::<ThemePack>(&encoded).unwrap(), theme);

        let partial = format!("{encoded}\n[roles]\nlink = '#123456'\n");
        let partial: ThemePack = toml::from_str(&partial).unwrap();
        partial.validate().unwrap();
        assert_eq!(partial.roles.link.as_deref(), Some("#123456"));
        assert_eq!(partial.roles.selection, None);
        assert_eq!(partial.roles.error, None);
        assert_eq!(partial.roles.timestamp, None);
        assert_eq!(partial.roles.own_surface, None);
        assert_eq!(partial.senders.colors, None);
        let message_roles = format!(
            "{encoded}\n[roles]\ntimestamp = '#A5AFBE'\nsender = '#E8EDF2'\nmessage = '#E8EDF2'\nown_message = '#F3F6FA'\nown_surface = '#20283A'\n"
        );
        let message_roles: ThemePack = toml::from_str(&message_roles).unwrap();
        message_roles.validate().unwrap();
        assert_eq!(message_roles.roles.own_surface.as_deref(), Some("#20283A"));
        assert_eq!(message_roles.roles.entries().len(), 20);
        for field in [
            "timestamp",
            "sender",
            "message",
            "own_message",
            "own_surface",
        ] {
            let bad: ThemePack =
                toml::from_str(&format!("{encoded}\n[roles]\n{field} = 'tomato'\n")).unwrap();
            assert!(bad.validate().is_err(), "{field} must be #RRGGBB");
        }
    }

    #[test]
    fn optional_theme_roles_reject_malformed_colors_and_unknown_keys() {
        let legacy: ThemePack =
            toml::from_str(include_str!("../../../themes/aurora.toml")).unwrap();
        let encoded = toml::to_string_pretty(&legacy).unwrap();
        for (role, _) in ThemeRoles::default().entries() {
            for bad in ["red", "#12345", "#1234567", "#abcdef00", "#gggggg", "#éabc"] {
                let text = format!("{encoded}\n[roles]\n{role} = '{bad}'\n");
                let theme: ThemePack = toml::from_str(&text).unwrap();
                let error = theme.validate().unwrap_err().to_string();
                assert!(error.contains(&format!("roles.{role}")), "{error}");
            }
        }
        for extension in [
            "[roles]\nunknwon = '#123456'\n",
            "[senders]\ncolours = ['#123456']\n",
        ] {
            assert!(toml::from_str::<ThemePack>(&format!("{encoded}\n{extension}")).is_err());
        }
    }

    #[test]
    fn sender_text_palette_is_bounded_and_does_not_rewrite_theme_colors() {
        let mut theme = BuiltInStylePreset::Nocturne.theme().unwrap();
        let original_colors = theme.colors.clone();
        for count in [1, MAX_SENDER_COLORS] {
            theme.senders.colors = Some(vec!["#aBcDeF".into(); count]);
            theme.validate().unwrap();
            let encoded = toml::to_string_pretty(&theme).unwrap();
            assert_eq!(toml::from_str::<ThemePack>(&encoded).unwrap(), theme);
            assert_eq!(theme.colors, original_colors);
        }
        for count in [0, MAX_SENDER_COLORS + 1] {
            theme.senders.colors = Some(vec!["#abcdef".into(); count]);
            assert!(
                theme
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("senders.colors")
            );
        }
        theme.senders.colors = Some(vec!["#abcdef".into(), "secret-color".into()]);
        assert!(
            theme
                .validate()
                .unwrap_err()
                .to_string()
                .contains("senders.colors[1]")
        );
        theme.senders.colors = None;
        theme.validate().unwrap();
    }

    #[test]
    fn custom_theme_contrast_is_an_authors_choice() {
        let mut theme = BuiltInStylePreset::Paper.theme().unwrap();
        // Validation checks the declarative format, not user aesthetic choices.
        theme.roles.link = Some(theme.colors.canvas.clone());
        theme.senders.colors = Some(vec![theme.colors.canvas.clone()]);
        theme.validate().unwrap();
    }

    #[test]
    fn malformed_semantic_override_does_not_load_embedded_theme_instead() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        config.apply_style_preset(BuiltInStylePreset::Terminal);
        fs::create_dir(store.themes_dir().unwrap()).unwrap();
        let path = store.themes_dir().unwrap().join("terminal.toml");
        let mut override_theme = BuiltInStylePreset::Terminal.theme().unwrap();
        override_theme.roles.error = Some("invalid".into());
        let override_text = toml::to_string_pretty(&override_theme).unwrap();
        fs::write(&path, &override_text).unwrap();
        assert!(matches!(
            store.load_theme(&config),
            Err(ConfigError::InvalidTheme(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), override_text);
    }

    #[test]
    fn shipped_semantic_roles_and_sender_text_have_readable_contrast() {
        fn luminance(hex: &str) -> f64 {
            let mut linear = [0.0; 3];
            for (index, value) in linear.iter_mut().enumerate() {
                let offset = 1 + 2 * index;
                let channel =
                    u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap() as f64 / 255.0;
                *value = if channel <= 0.04045 {
                    channel / 12.92
                } else {
                    ((channel + 0.055) / 1.055).powf(2.4)
                };
            }
            0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
        }
        fn assert_contrast(name: &str, foreground: &str, background: &str, minimum: f64) {
            let foreground_luminance = luminance(foreground);
            let background_luminance = luminance(background);
            let ratio = (foreground_luminance.max(background_luminance) + 0.05)
                / (foreground_luminance.min(background_luminance) + 0.05);
            assert!(
                ratio >= minimum,
                "{name}: {foreground} on {background} is {ratio:.3}, below {minimum}"
            );
        }
        for preset in BuiltInStylePreset::ALL {
            let theme = preset.theme().unwrap();
            let roles = &theme.roles;
            assert!(roles.entries().iter().all(|(_, value)| value.is_some()));
            let surfaces = [
                &theme.colors.rail,
                &theme.colors.sidebar,
                &theme.colors.canvas,
                &theme.colors.panel,
                &theme.colors.elevated,
                &theme.colors.hover,
            ];
            for foreground in [&roles.error, &roles.success, &roles.link, &roles.voice] {
                for background in surfaces {
                    assert_contrast(&theme.name, foreground.as_deref().unwrap(), background, 4.5);
                }
            }
            for (foreground, background) in [
                (&roles.error_ink, &roles.error_surface),
                (&roles.success_ink, &roles.success_surface),
                (&roles.selection_ink, &roles.selection),
                (&roles.unread_ink, &roles.unread),
                (&roles.destructive_ink, &roles.destructive),
            ] {
                assert_contrast(
                    &theme.name,
                    foreground.as_deref().unwrap(),
                    background.as_deref().unwrap(),
                    4.5,
                );
            }
            for background in surfaces.into_iter().map(String::as_str).chain([
                roles.selection.as_deref().unwrap(),
                roles.own_surface.as_deref().unwrap(),
            ]) {
                assert_contrast(
                    &theme.name,
                    roles.focus.as_deref().unwrap(),
                    background,
                    3.0,
                );
            }
            // Message text roles draw on the row surfaces and on an own-message
            // surface; selected rows switch every text to selection_ink instead.
            for foreground in [
                &roles.timestamp,
                &roles.sender,
                &roles.message,
                &roles.own_message,
            ] {
                for background in surfaces
                    .into_iter()
                    .map(String::as_str)
                    .chain([roles.own_surface.as_deref().unwrap()])
                {
                    assert_contrast(&theme.name, foreground.as_deref().unwrap(), background, 4.5);
                }
            }
            assert_ne!(roles.own_surface, Some(theme.colors.canvas.clone()));
            assert_contrast(
                &theme.name,
                roles.voice.as_deref().unwrap(),
                roles.selection.as_deref().unwrap(),
                4.5,
            );
            for foreground in theme.senders.colors.as_ref().unwrap() {
                for background in [
                    theme.colors.canvas.as_str(),
                    theme.colors.panel.as_str(),
                    theme.colors.elevated.as_str(),
                    theme.colors.hover.as_str(),
                    roles.selection.as_deref().unwrap(),
                    roles.own_surface.as_deref().unwrap(),
                ] {
                    assert_contrast(&theme.name, foreground, background, 4.5);
                }
            }
        }
    }

    #[test]
    fn room_header_defaults_and_modes_are_schema_one_compatible() {
        for source in ["schema_version = 1", "[conversation]"] {
            let config: Config = toml::from_str(source).unwrap();
            assert_eq!(config.conversation.header, ConversationHeaderMode::Full);
        }
        for (value, expected) in [
            ("full", ConversationHeaderMode::Full),
            ("compact", ConversationHeaderMode::Compact),
            ("minimal", ConversationHeaderMode::Minimal),
        ] {
            let source = format!("[conversation]\nheader = '{value}'\n");
            let config: Config = toml::from_str(&source).unwrap();
            config.validate().unwrap();
            assert_eq!(config.conversation.header, expected);
            let encoded = toml::to_string_pretty(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&encoded).unwrap(), config);
        }
        for source in [
            "[conversation]\nheader = 'hidden'\n",
            "[conversation]\nheader = 'Compact'\n",
            "[conversation]\nheader = 1\n",
            "[conversation]\nheadre = 'compact'\n",
        ] {
            assert!(
                toml::from_str::<Config>(source).is_err(),
                "accepted {source}"
            );
        }
    }

    #[test]
    fn appearance_presets_preserve_independent_room_header_choice() {
        let mut config = Config::default();
        config.conversation.header = ConversationHeaderMode::Minimal;
        for preset in BuiltInStylePreset::ALL {
            config.apply_style_preset(preset);
            assert_eq!(config.conversation.header, ConversationHeaderMode::Minimal);
        }
    }

    #[test]
    fn room_header_watcher_reports_invalid_edit_and_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        config.conversation.header = ConversationHeaderMode::Compact;
        store.save(&config).unwrap();
        let updates = store.watch_with_theme(Duration::from_millis(10));
        assert_eq!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .config
                .conversation
                .header,
            ConversationHeaderMode::Compact
        );
        fs::write(store.path(), "[conversation]\nheader = 'hidden'\n").unwrap();
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ConfigError::Decode(_))
        ));
        config.conversation.header = ConversationHeaderMode::Minimal;
        store.save(&config).unwrap();
        assert_eq!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .config
                .conversation
                .header,
            ConversationHeaderMode::Minimal
        );
    }

    #[test]
    fn conversation_metadata_defaults_formats_and_bounds_round_trip() {
        let original: Config = toml::from_str("schema_version=1").unwrap();
        assert_eq!(original.typography.sender_weight, None);
        assert_eq!(original.chat.timestamp_format, TimestampFormat::TwelveHour);
        assert!(!original.chat.timestamp_brackets);
        assert_eq!(original.chat.timestamp_width, 72);
        for (format, expected) in [
            ("twelve_hour", TimestampFormat::TwelveHour),
            ("twenty_four_hour", TimestampFormat::TwentyFourHour),
            (
                "twenty_four_hour_seconds",
                TimestampFormat::TwentyFourHourSeconds,
            ),
        ] {
            for (weight, width) in [(100, 48), (650, 72), (900, 160)] {
                let text = format!(
                    "[typography]\nsender_weight={weight}\n[chat]\ntimestamp_format='{format}'\ntimestamp_brackets=true\ntimestamp_width={width}"
                );
                let config: Config = toml::from_str(&text).unwrap();
                config.validate().unwrap();
                assert_eq!(config.typography.sender_weight, Some(weight));
                assert_eq!(config.chat.timestamp_format, expected);
                assert!(config.chat.timestamp_brackets);
                assert_eq!(config.chat.timestamp_width, width);
                let encoded = toml::to_string_pretty(&config).unwrap();
                assert_eq!(toml::from_str::<Config>(&encoded).unwrap(), config);
            }
        }
        for (text, brackets, alignment) in [
            ("schema_version=1", false, NicknameAlignment::Right),
            (
                "[chat]\nnickname_brackets=true",
                true,
                NicknameAlignment::Right,
            ),
            (
                "[chat]\nnickname_brackets=true\nnickname_alignment='left'",
                true,
                NicknameAlignment::Left,
            ),
            (
                "[chat]\nnickname_alignment='right'",
                false,
                NicknameAlignment::Right,
            ),
        ] {
            let config: Config = toml::from_str(text).unwrap();
            config.validate().unwrap();
            assert_eq!(config.chat.nickname_brackets, brackets);
            assert_eq!(config.chat.nickname_alignment, alignment);
            let encoded = toml::to_string_pretty(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&encoded).unwrap(), config);
        }
        for text in [
            "[chat]\nnickname_alignment='center'",
            "[chat]\nnickname_alignment=1",
            "[chat]\nnickname_brackets='yes'",
        ] {
            assert!(toml::from_str::<Config>(text).is_err(), "{text}");
        }
        for text in [
            "[typography]\nsender_weight=0",
            "[typography]\nsender_weight=99",
            "[typography]\nsender_weight=901",
            "[chat]\ntimestamp_width=47",
            "[chat]\ntimestamp_width=161",
        ] {
            assert!(toml::from_str::<Config>(text).unwrap().validate().is_err());
        }
        for text in [
            "[chat]\ntimestamp_format='%H:%M'",
            "[chat]\ntimestamp_format='TwentyFourHour'",
            "[chat]\ntimestamp_brackets='yes'",
            "[typography]\nsender_weight=-1",
        ] {
            assert!(toml::from_str::<Config>(text).is_err());
        }
    }

    #[test]
    fn metadata_reload_preserves_valid_file_then_removes_every_override() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut custom = Config::default();
        custom.typography.sender_weight = Some(500);
        custom.chat.timestamp_format = TimestampFormat::TwentyFourHourSeconds;
        custom.chat.timestamp_brackets = true;
        custom.chat.timestamp_width = 112;
        store.save(&custom).unwrap();
        let last_valid = fs::read(store.path()).unwrap();
        let updates = store.watch_with_theme(Duration::from_millis(10));
        assert_eq!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .config,
            custom
        );
        let mut invalid = custom.clone();
        invalid.typography.sender_weight = Some(901);
        assert!(store.save(&invalid).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), last_valid);
        fs::write(store.path(), "[chat]\ntimestamp_format='unsupported'\n").unwrap();
        assert!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        // Omitting all new fields restores their defaults, not the old custom
        // values retained in a previous watcher result.
        fs::write(store.path(), "schema_version=1\n").unwrap();
        let restored = updates
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .config;
        assert_eq!(restored.typography.sender_weight, None);
        assert_eq!(restored.chat, ChatStyle::default());
        for preset in BuiltInStylePreset::ALL {
            let mut reset = custom.clone();
            reset.apply_style_preset(preset);
            assert_eq!(reset.typography.sender_weight, None);
            assert_eq!(reset.chat.timestamp_format, TimestampFormat::TwelveHour);
            assert!(!reset.chat.timestamp_brackets);
            assert_eq!(reset.chat.timestamp_width, 72);
        }
    }
}
