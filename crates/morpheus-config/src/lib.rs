//! Versioned, non-secret configuration for the Morpheus desktop client.

use directories::ProjectDirs;
// Use the same opaque room-ID grammar as Ruma/Matrix SDK. Since room v12,
// room IDs need not include a server-name suffix.
use ruma_identifiers_validation::room_id::validate as validate_room_id;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use thiserror::Error;

mod commit;
pub mod diagnostics;
mod input;
pub mod registry;
pub mod style_profiles;
mod watch;

pub use commit::{CommitError, ConfigRevision};
pub use input::{InputError, MAX_CONFIG_BYTES, MAX_THEME_BYTES, read_bounded};
pub use watch::{OwnedWatch, WatchJoinError, WatchStop};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not locate the operating-system configuration directory")]
    MissingConfigDirectory,
    #[error("configuration file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A parse or schema-shape error, reduced to fixed text: the TOML
    /// parser's own message can quote the file, so it is never shown.
    #[error("invalid TOML: {0}")]
    Decode(diagnostics::DecodeError),
    #[error("could not encode TOML configuration: {0}")]
    Encode(#[from] toml::ser::Error),
    /// The rejected number is file content, so the error carries none:
    /// neither its text nor its `Debug` form can repeat it.
    #[error("unsupported configuration schema version; this version reads schema_version = 1")]
    Version,
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("invalid theme pack: {0}")]
    InvalidTheme(String),
    #[error("configuration file: {0}")]
    Input(#[from] InputError),
    /// One field of `config.toml` is invalid. The message never quotes the
    /// rejected value.
    #[error("invalid configuration: {field} {problem}")]
    Field { field: String, problem: Problem },
    /// One field of the selected theme file is invalid.
    #[error("invalid theme pack: {field} {problem}")]
    ThemeField { field: String, problem: Problem },
}

/// What is wrong with one field, as fixed text with a stable code. Bounds
/// come from the schema; the rejected value is never repeated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Range { minimum: u32, maximum: u32 },
    FontFamily,
    ThemeName,
    ThemeDisplayName,
    Color,
    UnknownColorToken,
    PaletteLength { maximum: usize },
    AudioDevice,
    RoomId,
    CallModeConflict,
    UnknownShortcutAction,
    Shortcut(&'static str),
    ShortcutConflict,
    PaneOrder,
    PaneVisibility,
}

impl Problem {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Range { .. } => "out-of-range",
            Self::FontFamily => "invalid-font-family",
            Self::ThemeName => "invalid-theme-name",
            Self::ThemeDisplayName => "invalid-theme-display-name",
            Self::Color => "invalid-color",
            Self::UnknownColorToken => "unknown-color-token",
            Self::PaletteLength { .. } => "invalid-palette-length",
            Self::AudioDevice => "invalid-audio-device",
            Self::RoomId => "invalid-room-id",
            Self::CallModeConflict => "call-mode-conflict",
            Self::UnknownShortcutAction => "unknown-shortcut-action",
            Self::Shortcut(_) => "invalid-shortcut",
            Self::ShortcutConflict => "shortcut-conflict",
            Self::PaneOrder => "invalid-pane-order",
            Self::PaneVisibility => "invalid-pane-visibility",
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Range { minimum, maximum } => {
                write!(formatter, "must be between {minimum} and {maximum}")
            }
            Self::FontFamily => formatter.write_str(
                "must be a nonempty font family of at most 128 bytes without control or direction characters",
            ),
            Self::ThemeName => formatter.write_str("must be a theme name, not a path"),
            Self::ThemeDisplayName => {
                formatter.write_str("must contain between 1 and 80 characters")
            }
            Self::Color => formatter.write_str("must be a #RRGGBB or #RRGGBBAA color"),
            Self::UnknownColorToken => {
                formatter.write_str("names a color token this version does not have")
            }
            Self::PaletteLength { maximum } => write!(
                formatter,
                "must contain between 1 and {maximum} colors; omit it to use the built-in colors"
            ),
            Self::AudioDevice => formatter.write_str(
                "must be a host-qualified device ID of at most 4096 bytes; remove the field to use the system default",
            ),
            Self::RoomId => formatter.write_str("must contain Matrix room IDs"),
            Self::CallModeConflict => {
                formatter.write_str("lists a room that also uses the other Element Call mode")
            }
            Self::UnknownShortcutAction => formatter.write_str("names an unknown shortcut action"),
            Self::Shortcut(reason) => formatter.write_str(reason),
            Self::ShortcutConflict => formatter.write_str("conflicts with another shortcut"),
            Self::PaneOrder => formatter.write_str(
                "must list spaces, rooms, conversation and details once each, or be empty",
            ),
            Self::PaneVisibility => formatter.write_str(
                "must name each pane at most once; the conversation is always shown and only rooms and details can collapse",
            ),
        }
    }
}

/// Parse TOML text, keeping only a sanitized error. Every runtime load, theme
/// load, reload and Settings save, and the offline validator, parse here.
pub(crate) fn parse_toml<T: serde::de::DeserializeOwned>(
    raw: &str,
    file: diagnostics::FileKind,
) -> Result<T, ConfigError> {
    toml::from_str(raw)
        .map_err(|error| ConfigError::Decode(diagnostics::decode_error(file, raw, &error)))
}

pub(crate) fn is_room_id(value: &str) -> bool {
    validate_room_id(value).is_ok()
}

fn field_error(field: impl Into<String>, problem: Problem) -> ConfigError {
    ConfigError::Field {
        field: field.into(),
        problem,
    }
}

/// One account's room appearance table. Neither the account ID nor a room ID
/// is ever part of a field path.
fn validate_room_appearance(rooms: &BTreeMap<String, RoomAppearance>) -> Result<(), ConfigError> {
    const FIELD: &str = "account_overrides.<account>.room_appearance";
    if rooms.len() > MAX_ROOM_APPEARANCES {
        return Err(field_error(
            FIELD,
            Problem::Range {
                minimum: 0,
                maximum: MAX_ROOM_APPEARANCES as u32,
            },
        ));
    }
    for (room_id, room) in rooms {
        if validate_room_id(room_id).is_err() {
            return Err(field_error(FIELD, Problem::RoomId));
        }
        for (id, value) in &room.colors {
            if registry::color_token(id).is_none() {
                return Err(field_error(
                    format!("{FIELD}.<room>.colors"),
                    Problem::UnknownColorToken,
                ));
            }
            if registry::parse_hex_color(value).is_none() {
                return Err(field_error(
                    format!("{FIELD}.<room>.colors.{id}"),
                    Problem::Color,
                ));
            }
        }
    }
    Ok(())
}

fn theme_error(field: impl Into<String>, problem: Problem) -> ConfigError {
    ConfigError::ThemeField {
        field: field.into(),
        problem,
    }
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
    /// What this client tells others about the user's activity. Every switch
    /// is off unless chosen; a missing section keeps them off.
    pub activity: Activity,
    pub shortcuts: BTreeMap<String, String>,
    pub experimental: Experimental,
    pub account_overrides: BTreeMap<String, AccountOverrides>,
    /// Sparse color overrides keyed by `registry::COLOR_TOKENS` IDs. They
    /// apply over the built-in palette and the selected theme pack, in every
    /// theme mode, and survive choosing a starting point.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, String>,
    /// Sender-name colors that replace the theme pack's palette. Each sender
    /// keeps the slot derived from their Matrix user ID; no sender is stored.
    #[serde(skip_serializing_if = "SenderColors::is_empty")]
    pub senders: SenderColors,
    pub window: WindowStyle,
}

/// The native window's own background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowStyle {
    /// Percent of each window surface's own alpha that is painted: 100 is
    /// unchanged and 0 leaves only the desktop behind those surfaces. Text,
    /// icons, controls, cards and dialogs keep their own colors.
    pub opacity_percent: u8,
}

impl Default for WindowStyle {
    fn default() -> Self {
        Self {
            opacity_percent: 100,
        }
    }
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
            activity: Activity::default(),
            shortcuts: default_shortcuts(),
            experimental: Experimental::default(),
            account_overrides: BTreeMap::new(),
            colors: BTreeMap::new(),
            senders: SenderColors::default(),
            window: WindowStyle::default(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::Version);
        }
        for (key, value) in [
            ("appearance.scale_percent", self.appearance.scale_percent),
            (
                "appearance.hover_duration_ms",
                self.appearance.hover_duration_ms,
            ),
            ("appearance.popup_fade_ms", self.appearance.popup_fade_ms),
            ("appearance.panel_fade_ms", self.appearance.panel_fade_ms),
            ("layout.room_list_width", self.layout.room_list_width),
            ("layout.details_width", self.layout.details_width),
        ] {
            validate_field(key, value)?;
        }
        validate_font_family("appearance.font_family", &self.appearance.font_family)?;
        self.typography.validate()?;
        self.geometry.validate()?;
        self.chat.validate()?;
        if let Some(name) = &self.appearance.theme_pack
            && !is_theme_name(name)
        {
            return Err(field_error("appearance.theme_pack", Problem::ThemeName));
        }
        for (id, value) in &self.colors {
            if registry::color_token(id).is_none() {
                return Err(field_error("colors", Problem::UnknownColorToken));
            }
            if registry::parse_hex_color(value).is_none() {
                return Err(field_error(format!("colors.{id}"), Problem::Color));
            }
        }
        self.senders
            .validate(|field, problem| field_error(field, problem))?;
        let (minimum, maximum) = (
            *registry::WINDOW_OPACITY_RANGE.start(),
            *registry::WINDOW_OPACITY_RANGE.end(),
        );
        validate_range(
            "window.opacity_percent",
            self.window.opacity_percent.into(),
            minimum.into(),
            maximum.into(),
        )?;
        validate_shortcuts(&self.shortcuts)?;
        self.media.validate_audio_devices()?;
        self.layout.validate_panes()?;
        for overrides in self.account_overrides.values() {
            for (field, rooms) in [
                ("local_voice_rooms", &overrides.local_voice_rooms),
                (
                    "legacy_element_call_rooms",
                    &overrides.legacy_element_call_rooms,
                ),
                (
                    "element_call_matrix_2_0_rooms",
                    &overrides.element_call_matrix_2_0_rooms,
                ),
            ] {
                if rooms
                    .iter()
                    .any(|room_id| validate_room_id(room_id).is_err())
                {
                    // The account ID is never part of a field path.
                    return Err(field_error(
                        format!("account_overrides.<account>.{field}"),
                        Problem::RoomId,
                    ));
                }
            }
            validate_room_notifications(&overrides.room_notifications)?;
            if overrides
                .element_call_matrix_2_0_rooms
                .iter()
                .any(|room_id| overrides.legacy_element_call_rooms.contains(room_id))
            {
                return Err(field_error(
                    "account_overrides.<account>.element_call_matrix_2_0_rooms",
                    Problem::CallModeConflict,
                ));
            }
            // The account ID is never part of a field path; token IDs are
            // registry names, never file content.
            for (id, value) in &overrides.colors {
                if registry::color_token(id).is_none() {
                    return Err(field_error(
                        "account_overrides.<account>.colors",
                        Problem::UnknownColorToken,
                    ));
                }
                if registry::parse_hex_color(value).is_none() {
                    return Err(field_error(
                        format!("account_overrides.<account>.colors.{id}"),
                        Problem::Color,
                    ));
                }
            }
            validate_room_appearance(&overrides.room_appearance)?;
        }
        Ok(())
    }

    /// The valid override of one color token, if `config.toml` sets one.
    pub fn color_override(&self, id: &str) -> Option<[u8; 4]> {
        registry::parse_hex_color(self.colors.get(id)?)
    }

    /// Materialize a built-in style into the same preferences edited by TOML
    /// and Settings. No hidden preset layer overrides subsequent custom edits.
    /// Account, notification, shortcut and media preferences are untouched,
    /// and so is the focus ring width, an accessibility choice like the text
    /// scale and reduced motion.
    pub fn apply_style_preset(&mut self, preset: BuiltInStylePreset) {
        self.appearance.theme_pack = Some(preset.theme_name().into());
        self.typography = Typography::default();
        self.geometry = Geometry {
            focus_width: self.geometry.focus_width,
            ..Geometry::default()
        };
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

    /// The local notification behavior of one room of an account. A room or
    /// account with nothing of its own removes nothing.
    pub fn room_notification_local(
        &self,
        account_id: &str,
        room_id: &str,
    ) -> RoomNotificationLocal {
        self.account_overrides
            .get(account_id)
            .and_then(|overrides| overrides.room_notifications.get(room_id))
            .copied()
            .unwrap_or_default()
    }

    /// Set one room's local notification behavior. A value that removes
    /// nothing drops the room, and an account left with nothing of its own is
    /// dropped too. Validation, including the room ID and the per-account
    /// bound, happens when the configuration is saved.
    pub fn set_room_notification_local(
        &mut self,
        account_id: &str,
        room_id: &str,
        local: RoomNotificationLocal,
    ) {
        let overrides = self
            .account_overrides
            .entry(account_id.to_owned())
            .or_default();
        if local.is_default() {
            overrides.room_notifications.remove(room_id);
        } else {
            overrides
                .room_notifications
                .insert(room_id.to_owned(), local);
        }
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
    }

    /// Remove one account's per-room notification behavior, and nothing of
    /// any other account; its room IDs do not outlive a logout or removal.
    /// The entry itself goes once nothing else remains in it. Returns whether
    /// anything changed.
    pub fn clear_room_notifications(&mut self, account_id: &str) -> bool {
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return false;
        };
        let changed = !overrides.room_notifications.is_empty();
        overrides.room_notifications.clear();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        changed
    }

    /// The color overrides `account_id` sees: the global `[colors]` table,
    /// then that account's own entries. No account, an empty or unknown ID
    /// and an account without colors all see the global table.
    pub fn colors_for(&self, account_id: Option<&str>) -> BTreeMap<String, String> {
        let mut colors = self.colors.clone();
        if let Some(overrides) = account_id
            .filter(|id| !id.is_empty())
            .and_then(|id| self.account_overrides.get(id))
        {
            colors.extend(
                overrides
                    .colors
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone())),
            );
        }
        colors
    }

    /// One room's own appearance while `account_id` is active and `room_id`
    /// is open; `None` when either is missing or the room sets nothing.
    pub fn room_appearance(
        &self,
        account_id: Option<&str>,
        room_id: Option<&str>,
    ) -> Option<&RoomAppearance> {
        let account = account_id.filter(|id| !id.is_empty())?;
        let room = room_id.filter(|id| !id.is_empty())?;
        self.account_overrides
            .get(account)?
            .room_appearance
            .get(room)
    }

    /// The color overrides in force with global -> account -> room
    /// precedence: the global table, then the active account's colors, then
    /// the open room's. A level that omits a token inherits it.
    pub fn colors_for_room(
        &self,
        account_id: Option<&str>,
        room_id: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut colors = self.colors_for(account_id);
        if let Some(room) = self.room_appearance(account_id, room_id) {
            colors.extend(
                room.colors
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone())),
            );
        }
        colors
    }

    /// The theme mode with global -> account -> room precedence.
    pub fn theme_for_room(&self, account_id: Option<&str>, room_id: Option<&str>) -> ThemeMode {
        self.room_appearance(account_id, room_id)
            .and_then(|room| room.theme)
            .unwrap_or_else(|| self.theme_for(account_id))
    }

    /// Remove one room's own appearance, and nothing else: it then inherits
    /// its account's and the global settings. An account entry left empty
    /// goes too. Returns whether anything changed.
    pub fn clear_room_appearance(&mut self, account_id: &str, room_id: &str) -> bool {
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return false;
        };
        let changed = overrides.room_appearance.remove(room_id).is_some();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        changed
    }

    /// Remove one account's appearance preferences, its theme mode, its
    /// colors and its rooms' appearance, and nothing of any other account.
    /// The entry itself goes once nothing else remains in it. Returns
    /// whether anything changed.
    pub fn clear_account_appearance(&mut self, account_id: &str) -> bool {
        let Some(overrides) = self.account_overrides.get_mut(account_id) else {
            return false;
        };
        let changed = overrides.theme.take().is_some()
            | !overrides.colors.is_empty()
            | !overrides.room_appearance.is_empty();
        overrides.colors.clear();
        overrides.room_appearance.clear();
        if *overrides == AccountOverrides::default() {
            self.account_overrides.remove(account_id);
        }
        changed
    }

    /// The theme mode to show while `account_id` is the active account. An
    /// account without a `theme` override, an unknown ID and no active account
    /// all use the global `appearance.theme`. Theme packs stay global.
    pub fn theme_for(&self, account_id: Option<&str>) -> ThemeMode {
        account_id
            .filter(|id| !id.is_empty())
            .and_then(|id| self.account_overrides.get(id))
            .and_then(|overrides| overrides.theme)
            .unwrap_or(self.appearance.theme)
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
    /// Duration of Morpheus decorative hover fades; reduce_motion overrides it.
    pub hover_duration_ms: u16,
    /// Decorative fade-in of menus, pickers and the quick switcher; 0 is off.
    pub popup_fade_ms: u16,
    /// Decorative fade-in of the details panel; 0 is off.
    pub panel_fade_ms: u16,
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
            hover_duration_ms: 90,
            popup_fade_ms: 0,
            panel_fade_ms: 0,
        }
    }
}

/// Message typography is independent of interface scale and its font family.
/// `None` inherits `appearance.font_family`; fonts are installed family names,
/// never URLs or paths. Missing families use the renderer's normal fallback.
/// Sizes are logical pixels before text scaling; an omitted optional size
/// keeps its surface's built-in size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Typography {
    pub message_font_family: Option<String>,
    /// The message and thread composers' font family; `None` follows the
    /// message font family. An installed family name, never a path.
    pub composer_font_family: Option<String>,
    pub monospace_font_family: String,
    pub message_size: u16,
    /// Omission preserves each sender surface's existing weight.
    pub sender_weight: Option<u16>,
    /// Room-list rows and text-sidebar routes.
    pub sidebar_size: u16,
    /// Timeline sender names; omission follows each layout's message size.
    pub sender_size: Option<u16>,
    /// Timeline and thread-reply clocks; omission keeps 10.
    pub timestamp_size: Option<u16>,
    /// Composer text and placeholder; omission follows `message_size`.
    pub composer_size: Option<u16>,
    /// Line spacing, in percent of each font's natural line height, of the
    /// composer and plain-text previews. Omission keeps natural spacing.
    /// Rich message bodies are drawn as styled text, which has no line
    /// spacing setting in the toolkit, and keep their natural spacing.
    pub line_height_percent: Option<u16>,
    /// Default font weight (300 to 800), inherited by rich message bodies,
    /// composers and text without an explicit weight. Sender, heading and
    /// Markdown span overrides stay explicit. Omission keeps platform defaults.
    pub body_weight: Option<u16>,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            message_font_family: None,
            composer_font_family: None,
            monospace_font_family: "monospace".into(),
            message_size: 14,
            sender_weight: None,
            sidebar_size: 13,
            sender_size: None,
            timestamp_size: None,
            composer_size: None,
            line_height_percent: None,
            body_weight: None,
        }
    }
}

impl Typography {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(family) = &self.message_font_family {
            validate_font_family("typography.message_font_family", family)?;
        }
        if let Some(family) = &self.composer_font_family {
            validate_font_family("typography.composer_font_family", family)?;
        }
        validate_font_family(
            "typography.monospace_font_family",
            &self.monospace_font_family,
        )?;
        validate_field("typography.message_size", self.message_size)?;
        if let Some(weight) = self.sender_weight {
            validate_field("typography.sender_weight", weight)?;
        }
        validate_field("typography.sidebar_size", self.sidebar_size)?;
        for (key, value) in [
            ("typography.sender_size", self.sender_size),
            ("typography.timestamp_size", self.timestamp_size),
            ("typography.composer_size", self.composer_size),
            ("typography.line_height_percent", self.line_height_percent),
            ("typography.body_weight", self.body_weight),
        ] {
            if let Some(value) = value {
                validate_field(key, value)?;
            }
        }
        Ok(())
    }

    pub fn message_family<'a>(&'a self, appearance: &'a Appearance) -> &'a str {
        self.message_font_family
            .as_deref()
            .unwrap_or(&appearance.font_family)
    }

    /// The composers' family: its own, else the message family, else the
    /// interface family.
    pub fn composer_family<'a>(&'a self, appearance: &'a Appearance) -> &'a str {
        self.composer_font_family
            .as_deref()
            .unwrap_or_else(|| self.message_family(appearance))
    }
}

/// Pixel values are logical pixels before desktop DPI scaling. The app's text
/// scale remains independent so users can enlarge text without widening panes.
/// An omitted optional value keeps each surface's built-in proportion of
/// `corner_radius` or its built-in size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Geometry {
    pub corner_radius: u16,
    pub message_gap: u16,
    pub content_padding: u16,
    pub rail_width: u16,
    /// Text and icon buttons.
    pub control_radius: Option<u16>,
    /// Text fields.
    pub input_radius: Option<u16>,
    /// Message and thread composers.
    pub composer_radius: Option<u16>,
    pub border_width: u16,
    /// Keyboard focus rings. Presets keep it, like the text scale.
    pub focus_width: u16,
    pub control_padding: u16,
    pub input_padding: u16,
    pub composer_padding: u16,
    pub icon_size: u16,
    /// Timeline avatars; omission keeps each message layout's size.
    pub avatar_size: Option<u16>,
    /// Percent of each avatar's size; 50 draws a circle.
    pub avatar_roundness: Option<u16>,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            corner_radius: 10,
            message_gap: 12,
            content_padding: 24,
            rail_width: 70,
            control_radius: None,
            input_radius: None,
            composer_radius: None,
            border_width: 1,
            focus_width: 2,
            control_padding: 12,
            input_padding: 10,
            composer_padding: 10,
            icon_size: 18,
            avatar_size: None,
            avatar_roundness: None,
        }
    }
}

impl Geometry {
    fn validate(&self) -> Result<(), ConfigError> {
        for (key, value) in [
            ("geometry.corner_radius", self.corner_radius),
            ("geometry.message_gap", self.message_gap),
            ("geometry.content_padding", self.content_padding),
            ("geometry.rail_width", self.rail_width),
            ("geometry.border_width", self.border_width),
            ("geometry.focus_width", self.focus_width),
            ("geometry.control_padding", self.control_padding),
            ("geometry.input_padding", self.input_padding),
            ("geometry.composer_padding", self.composer_padding),
            ("geometry.icon_size", self.icon_size),
        ] {
            validate_field(key, value)?;
        }
        for (key, value) in [
            ("geometry.control_radius", self.control_radius),
            ("geometry.input_radius", self.input_radius),
            ("geometry.composer_radius", self.composer_radius),
            ("geometry.avatar_size", self.avatar_size),
            ("geometry.avatar_roundness", self.avatar_roundness),
        ] {
            if let Some(value) = value {
                validate_field(key, value)?;
            }
        }
        Ok(())
    }
}

/// The stored value of one editor metric: `Inherited` when an optional
/// metric is omitted and its surfaces keep their built-in value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricValue {
    Set(u16),
    Inherited,
}

impl MetricValue {
    pub fn set(self) -> Option<u16> {
        match self {
            Self::Set(value) => Some(value),
            Self::Inherited => None,
        }
    }
}

enum MetricSlot<'a> {
    Required(&'a mut u16),
    Optional(&'a mut Option<u16>),
}

impl Config {
    /// One editor metric by its registry key; `None` for any other key.
    pub fn metric(&self, key: &str) -> Option<MetricValue> {
        let (typography, geometry) = (&self.typography, &self.geometry);
        let appearance = &self.appearance;
        let required = |value: u16| Some(MetricValue::Set(value));
        let optional =
            |value: Option<u16>| Some(value.map_or(MetricValue::Inherited, MetricValue::Set));
        match key {
            "typography.sidebar_size" => required(typography.sidebar_size),
            "typography.sender_size" => optional(typography.sender_size),
            "typography.timestamp_size" => optional(typography.timestamp_size),
            "typography.composer_size" => optional(typography.composer_size),
            "typography.line_height_percent" => optional(typography.line_height_percent),
            "typography.body_weight" => optional(typography.body_weight),
            "geometry.control_radius" => optional(geometry.control_radius),
            "geometry.input_radius" => optional(geometry.input_radius),
            "geometry.composer_radius" => optional(geometry.composer_radius),
            "geometry.border_width" => required(geometry.border_width),
            "geometry.focus_width" => required(geometry.focus_width),
            "geometry.control_padding" => required(geometry.control_padding),
            "geometry.input_padding" => required(geometry.input_padding),
            "geometry.composer_padding" => required(geometry.composer_padding),
            "geometry.icon_size" => required(geometry.icon_size),
            "geometry.avatar_size" => optional(geometry.avatar_size),
            "geometry.avatar_roundness" => optional(geometry.avatar_roundness),
            "appearance.popup_fade_ms" => required(appearance.popup_fade_ms),
            "appearance.panel_fade_ms" => required(appearance.panel_fade_ms),
            _ => None,
        }
    }

    fn metric_slot(&mut self, key: &str) -> Option<MetricSlot<'_>> {
        let (typography, geometry) = (&mut self.typography, &mut self.geometry);
        let appearance = &mut self.appearance;
        Some(match key {
            "typography.sidebar_size" => MetricSlot::Required(&mut typography.sidebar_size),
            "typography.sender_size" => MetricSlot::Optional(&mut typography.sender_size),
            "typography.timestamp_size" => MetricSlot::Optional(&mut typography.timestamp_size),
            "typography.composer_size" => MetricSlot::Optional(&mut typography.composer_size),
            "typography.line_height_percent" => {
                MetricSlot::Optional(&mut typography.line_height_percent)
            }
            "typography.body_weight" => MetricSlot::Optional(&mut typography.body_weight),
            "geometry.control_radius" => MetricSlot::Optional(&mut geometry.control_radius),
            "geometry.input_radius" => MetricSlot::Optional(&mut geometry.input_radius),
            "geometry.composer_radius" => MetricSlot::Optional(&mut geometry.composer_radius),
            "geometry.border_width" => MetricSlot::Required(&mut geometry.border_width),
            "geometry.focus_width" => MetricSlot::Required(&mut geometry.focus_width),
            "geometry.control_padding" => MetricSlot::Required(&mut geometry.control_padding),
            "geometry.input_padding" => MetricSlot::Required(&mut geometry.input_padding),
            "geometry.composer_padding" => MetricSlot::Required(&mut geometry.composer_padding),
            "geometry.icon_size" => MetricSlot::Required(&mut geometry.icon_size),
            "geometry.avatar_size" => MetricSlot::Optional(&mut geometry.avatar_size),
            "geometry.avatar_roundness" => MetricSlot::Optional(&mut geometry.avatar_roundness),
            "appearance.popup_fade_ms" => MetricSlot::Required(&mut appearance.popup_fade_ms),
            "appearance.panel_fade_ms" => MetricSlot::Required(&mut appearance.panel_fade_ms),
            _ => return None,
        })
    }

    /// Set one editor metric, or return it to its default with `Inherited`:
    /// an optional metric is then omitted and a required one takes the
    /// registry default. Values are validated against the registry range
    /// before anything changes. Returns whether the configuration changed.
    pub fn set_metric(&mut self, key: &str, value: MetricValue) -> Result<bool, ConfigError> {
        let Some((field, _)) = registry::metric_field(key) else {
            return Err(ConfigError::Invalid(
                "this version has no such appearance metric".into(),
            ));
        };
        if let MetricValue::Set(value) = value {
            validate_field(field.key, value)?;
        }
        let default = field
            .integer_default()
            .and_then(|value| u16::try_from(value).ok());
        let Some(slot) = self.metric_slot(key) else {
            return Err(ConfigError::Invalid(
                "this version has no such appearance metric".into(),
            ));
        };
        let changed = match slot {
            MetricSlot::Required(current) => {
                let next = value
                    .set()
                    .or(default)
                    .expect("required metrics have a registry default");
                std::mem::replace(current, next) != next
            }
            MetricSlot::Optional(current) => std::mem::replace(current, value.set()) != value.set(),
        };
        Ok(changed)
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
        validate_field("chat.nickname_width", self.nickname_width)?;
        validate_field("chat.timestamp_width", self.timestamp_width)
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
        return Err(field_error(
            name,
            Problem::Range {
                minimum: minimum.into(),
                maximum: maximum.into(),
            },
        ));
    }
    Ok(())
}

/// Check one integer field against its range in `registry::FIELDS`, the same
/// table the schema output, Settings search and the metric editor read.
fn validate_field(key: &str, value: u16) -> Result<(), ConfigError> {
    let Some((minimum, maximum)) = registry::integer_range(key) else {
        // Every validated key is documented; the registry test checks it.
        return Err(ConfigError::Invalid(
            "this version has no such integer setting".into(),
        ));
    };
    let bound = |limit: i64| u16::try_from(limit).unwrap_or(u16::MAX);
    validate_range(key, value, bound(minimum), bound(maximum))
}

/// A theme file name: ASCII letters, digits, `-` or `_`, never a path.
pub fn is_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn validate_font_family(name: &str, family: &str) -> Result<(), ConfigError> {
    if family.trim().is_empty()
        || family.len() > 128
        || family.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return Err(field_error(name, Problem::FontFamily));
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
        let theme: ThemePack = parse_toml(self.theme_source(), diagnostics::FileKind::Theme)?;
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
    /// Side of the conversation that shows the details panel.
    pub details_side: DetailsSide,
    /// With grouped lists, show direct messages above rooms.
    pub direct_messages_first: bool,
    /// Room-list row density; omission follows `appearance.density`.
    pub room_density: Option<Density>,
    /// Timeline message spacing density; omission follows `appearance.density`.
    pub message_density: Option<Density>,
    /// Left-to-right order of the four workspace panes by stable ID. Empty
    /// follows `details_side` (spaces, rooms, then the conversation and the
    /// details panel on that side). Each pane appears exactly once.
    pub pane_order: Vec<PaneId>,
    /// Panes that are not shown. The conversation is always shown; the
    /// active-account control and a layout restore stay reachable.
    pub hidden_panes: Vec<PaneId>,
    /// Panes shown as a narrow strip with a restore control: rooms, details.
    pub collapsed_panes: Vec<PaneId>,
}

/// Stable IDs of the workspace panes, as written in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneId {
    /// The Space rail (icon rail navigation).
    Spaces,
    /// The room list (and, with sidebar navigation, its Space sections).
    Rooms,
    /// The conversation, call stage, settings and search area.
    Conversation,
    /// The details panel: members, threads and files.
    Details,
}

impl PaneId {
    pub const ALL: [Self; 4] = [Self::Spaces, Self::Rooms, Self::Conversation, Self::Details];

    pub fn id(self) -> &'static str {
        match self {
            Self::Spaces => "spaces",
            Self::Rooms => "rooms",
            Self::Conversation => "conversation",
            Self::Details => "details",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Spaces => "Space rail",
            Self::Rooms => "Room list",
            Self::Conversation => "Conversation",
            Self::Details => "Details panel",
        }
    }

    /// Whether this pane may be hidden (everything but the conversation).
    pub fn can_hide(self) -> bool {
        self != Self::Conversation
    }

    /// Whether this pane may collapse to a narrow strip.
    pub fn can_collapse(self) -> bool {
        matches!(self, Self::Rooms | Self::Details)
    }
}

impl Layout {
    /// The order panes are laid out in, left to right: `pane_order` when it
    /// is set, else the arrangement `details_side` describes.
    pub fn effective_pane_order(&self) -> [PaneId; 4] {
        if let Ok(order) = <[PaneId; 4]>::try_from(self.pane_order.as_slice())
            && valid_pane_order(&order)
        {
            return order;
        }
        match self.details_side {
            DetailsSide::Right => [
                PaneId::Spaces,
                PaneId::Rooms,
                PaneId::Conversation,
                PaneId::Details,
            ],
            DetailsSide::Left => [
                PaneId::Spaces,
                PaneId::Rooms,
                PaneId::Details,
                PaneId::Conversation,
            ],
        }
    }

    pub fn pane_hidden(&self, pane: PaneId) -> bool {
        pane.can_hide() && self.hidden_panes.contains(&pane)
    }

    pub fn pane_collapsed(&self, pane: PaneId) -> bool {
        pane.can_collapse() && !self.pane_hidden(pane) && self.collapsed_panes.contains(&pane)
    }

    /// Back to the default arrangement: no custom order, nothing hidden or
    /// collapsed. Widths, side and navigation are unchanged. Returns whether
    /// anything changed.
    pub fn restore_default_panes(&mut self) -> bool {
        let changed = !self.pane_order.is_empty()
            || !self.hidden_panes.is_empty()
            || !self.collapsed_panes.is_empty();
        self.pane_order.clear();
        self.hidden_panes.clear();
        self.collapsed_panes.clear();
        changed
    }

    fn validate_panes(&self) -> Result<(), ConfigError> {
        if !self.pane_order.is_empty() {
            let valid = <[PaneId; 4]>::try_from(self.pane_order.as_slice())
                .is_ok_and(|order| valid_pane_order(&order));
            if !valid {
                return Err(field_error("layout.pane_order", Problem::PaneOrder));
            }
        }
        let distinct = |panes: &[PaneId]| {
            panes
                .iter()
                .enumerate()
                .all(|(index, pane)| !panes[..index].contains(pane))
        };
        if !distinct(&self.hidden_panes) || self.hidden_panes.iter().any(|pane| !pane.can_hide()) {
            return Err(field_error("layout.hidden_panes", Problem::PaneVisibility));
        }
        if !distinct(&self.collapsed_panes)
            || self.collapsed_panes.iter().any(|pane| !pane.can_collapse())
        {
            return Err(field_error(
                "layout.collapsed_panes",
                Problem::PaneVisibility,
            ));
        }
        Ok(())
    }
}

/// Every pane exactly once.
fn valid_pane_order(order: &[PaneId; 4]) -> bool {
    PaneId::ALL.iter().all(|pane| order.contains(pane))
}

/// Where the details panel sits. Navigation and the account control stay in
/// place either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailsSide {
    #[default]
    Right,
    Left,
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
            details_side: DetailsSide::Right,
            direct_messages_first: false,
            room_density: None,
            message_density: None,
            pane_order: Vec::new(),
            hidden_panes: Vec::new(),
            collapsed_panes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
    pub sounds: bool,
    pub show_message_preview: bool,
    /// Room names and senders in desktop banners. Off, a banner names only
    /// the account and never previews message text.
    pub show_room_and_sender: bool,
    pub highlight_words: Vec<String>,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            sounds: true,
            show_message_preview: false,
            show_room_and_sender: true,
            highlight_words: Vec::new(),
        }
    }
}

/// Typing notices and read receipts this client sends. All off by default:
/// nothing is published until the user turns it on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Activity {
    /// Tell a room while this account is typing (m.typing).
    pub send_typing: bool,
    /// Send public read receipts (m.read), which others can see. Otherwise
    /// receipts are private (m.read.private).
    pub public_read_receipts: bool,
    /// Move this account's read receipt automatically to the newest message
    /// actually shown at the bottom of a live timeline in a focused window.
    pub read_tracking: bool,
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
                return Err(field_error(format!("media.{field}"), Problem::AudioDevice));
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
    /// Sparse color overrides while this account is active, keyed like the
    /// global `[colors]` table and drawn over it. Never resolved from a
    /// display name or Matrix ID: only the local account ID keys them.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, String>,
    /// Local notification behavior for single rooms of this account, keyed
    /// by Matrix room ID. It is separate from the room's level, which is the
    /// account's server push rule, and separate from any room appearance: it
    /// can only make that room's banners quieter or more private than the
    /// global and account settings allow. At most
    /// [`MAX_ROOM_NOTIFICATION_OVERRIDES`] rooms per account.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub room_notifications: BTreeMap<String, RoomNotificationLocal>,
    /// Appearance for single rooms of this account, keyed by Matrix room ID.
    /// While such a room is open its settings draw over the account's, which
    /// draw over the global ones. A room absent here, and a setting a room
    /// omits, inherit. At most [`MAX_ROOM_APPEARANCES`] rooms per account.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub room_appearance: BTreeMap<String, RoomAppearance>,
}

/// Most rooms with their own local notification behavior per account.
pub const MAX_ROOM_NOTIFICATION_OVERRIDES: usize = 256;

/// One room's local notification behavior. Each switch only removes
/// something from that room's banners; none can raise a global ceiling such
/// as notifications, sounds or previews being off. It holds no credentials,
/// paths or message content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomNotificationLocal {
    /// No sound for this room's banners.
    #[serde(skip_serializing_if = "is_false")]
    pub quiet: bool,
    /// Never preview message text from this room, even when previews are on.
    #[serde(skip_serializing_if = "is_false")]
    pub hide_preview: bool,
    /// Local highlight words do not mark this room's banners; Matrix
    /// mentions and keywords still do.
    #[serde(skip_serializing_if = "is_false")]
    pub no_highlights: bool,
}

impl RoomNotificationLocal {
    /// Whether the room removes nothing (and so is not stored).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One account's room notification table. Neither the account ID nor a room
/// ID is ever part of a field path.
fn validate_room_notifications(
    rooms: &BTreeMap<String, RoomNotificationLocal>,
) -> Result<(), ConfigError> {
    const FIELD: &str = "account_overrides.<account>.room_notifications";
    if rooms.len() > MAX_ROOM_NOTIFICATION_OVERRIDES {
        return Err(field_error(
            FIELD,
            Problem::Range {
                minimum: 0,
                maximum: MAX_ROOM_NOTIFICATION_OVERRIDES as u32,
            },
        ));
    }
    if rooms
        .keys()
        .any(|room_id| validate_room_id(room_id).is_err())
    {
        return Err(field_error(FIELD, Problem::RoomId));
    }
    Ok(())
}

/// Most rooms with their own appearance per account.
pub const MAX_ROOM_APPEARANCES: usize = 64;

/// One room's own appearance: only what differs from its account. It holds
/// a theme mode and color overrides, never credentials, paths or
/// expressions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomAppearance {
    /// The theme mode while this room is open; omitted inherits.
    pub theme: Option<ThemeMode>,
    /// Sparse color overrides while this room is open, keyed like the global
    /// `[colors]` table.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, String>,
}

impl RoomAppearance {
    /// Whether the room sets nothing of its own (and so inherits everything).
    pub fn is_empty(&self) -> bool {
        self.theme.is_none() && self.colors.is_empty()
    }
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

/// The default binding of every action, as written to a new `config.toml`.
/// An existing `[shortcuts]` table is used exactly as written: an action it
/// omits stays disabled, so a file never gains a binding silently.
pub fn default_shortcuts() -> BTreeMap<String, String> {
    ShortcutAction::ALL
        .into_iter()
        .filter_map(|action| Some((action.name().into(), action.default_binding()?.into())))
        .collect()
}

/// Where an action appears in the shortcut editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutGroup {
    Navigation,
    Composer,
    Search,
    Panels,
    Account,
}

impl ShortcutGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation",
            Self::Composer => "Composer",
            Self::Search => "Search",
            Self::Panels => "Panels",
            Self::Account => "Account",
        }
    }
}

/// Supported commands, shared by the TOML validator, the in-app editor and
/// the desktop dispatcher. Each one moves focus, opens a view or toggles a
/// panel. None of them sends, removes, logs out, switches accounts or
/// changes a protected setting, so no binding can trigger such an action;
/// those keep their own confirmation flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    QuickSwitcher,
    Search,
    NextUnread,
    PreviousUnread,
    NextRoom,
    PreviousRoom,
    GoHome,
    OpenSettings,
    SearchSettings,
    FocusComposer,
    OpenEmojiPicker,
    ToggleDetailsPanel,
    ShowMembersPanel,
    ShowThreadPanel,
    OpenAccountMenu,
    JumpToFirstUnread,
}

impl ShortcutAction {
    pub const ALL: [Self; 16] = [
        Self::QuickSwitcher,
        Self::Search,
        Self::NextUnread,
        Self::PreviousUnread,
        Self::NextRoom,
        Self::PreviousRoom,
        Self::GoHome,
        Self::OpenSettings,
        Self::SearchSettings,
        Self::FocusComposer,
        Self::OpenEmojiPicker,
        Self::ToggleDetailsPanel,
        Self::ShowMembersPanel,
        Self::ShowThreadPanel,
        Self::OpenAccountMenu,
        Self::JumpToFirstUnread,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::QuickSwitcher => "quick_switcher",
            Self::Search => "search",
            Self::NextUnread => "next_unread",
            Self::PreviousUnread => "previous_unread",
            Self::NextRoom => "next_room",
            Self::PreviousRoom => "previous_room",
            Self::GoHome => "go_home",
            Self::OpenSettings => "open_settings",
            Self::SearchSettings => "search_settings",
            Self::FocusComposer => "focus_composer",
            Self::OpenEmojiPicker => "open_emoji_picker",
            Self::ToggleDetailsPanel => "toggle_details_panel",
            Self::ShowMembersPanel => "show_members_panel",
            Self::ShowThreadPanel => "show_thread_panel",
            Self::OpenAccountMenu => "open_account_menu",
            Self::JumpToFirstUnread => "jump_to_first_unread",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::QuickSwitcher => "Switch room",
            Self::Search => "Search current room",
            Self::NextUnread => "Next unread room",
            Self::PreviousUnread => "Previous unread room",
            Self::NextRoom => "Next room",
            Self::PreviousRoom => "Previous room",
            Self::GoHome => "Go to Home",
            Self::OpenSettings => "Open Settings",
            Self::SearchSettings => "Search Settings",
            Self::FocusComposer => "Focus the message composer",
            Self::OpenEmojiPicker => "Open the emoji picker",
            Self::ToggleDetailsPanel => "Show or hide the details panel",
            Self::ShowMembersPanel => "Show members",
            Self::ShowThreadPanel => "Show the open thread",
            Self::OpenAccountMenu => "Open the account menu",
            Self::JumpToFirstUnread => "Jump to the first unread message",
        }
    }

    pub fn group(self) -> ShortcutGroup {
        match self {
            Self::QuickSwitcher
            | Self::NextUnread
            | Self::PreviousUnread
            | Self::NextRoom
            | Self::PreviousRoom
            | Self::GoHome
            | Self::OpenSettings
            | Self::JumpToFirstUnread => ShortcutGroup::Navigation,
            Self::FocusComposer | Self::OpenEmojiPicker => ShortcutGroup::Composer,
            Self::Search | Self::SearchSettings => ShortcutGroup::Search,
            Self::ToggleDetailsPanel | Self::ShowMembersPanel | Self::ShowThreadPanel => {
                ShortcutGroup::Panels
            }
            Self::OpenAccountMenu => ShortcutGroup::Account,
        }
    }

    /// The binding a new configuration and Restore defaults use. Each one
    /// passes [`Shortcut::parse`] and none conflicts on either platform.
    pub fn default_binding(self) -> Option<&'static str> {
        Some(match self {
            Self::QuickSwitcher => "Mod+K",
            Self::Search => "Mod+F",
            Self::NextUnread => "Alt+Down",
            Self::PreviousUnread => "Alt+Up",
            Self::NextRoom => "Alt+Shift+Down",
            Self::PreviousRoom => "Alt+Shift+Up",
            Self::GoHome => "Mod+1",
            Self::OpenSettings => "Mod+Shift+S",
            Self::SearchSettings => "Mod+Shift+F",
            Self::FocusComposer => "Mod+L",
            Self::OpenEmojiPicker => "Mod+E",
            Self::ToggleDetailsPanel => "Mod+Shift+D",
            Self::ShowMembersPanel => "Mod+Shift+P",
            Self::ShowThreadPanel => "Mod+Shift+T",
            Self::OpenAccountMenu => "Mod+Shift+U",
            Self::JumpToFirstUnread => "Mod+Shift+N",
        })
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
        let invalid = |reason: &'static str| field_error("shortcuts", Problem::Shortcut(reason));
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

    /// Whether both combinations are the same keys on macOS or elsewhere. A
    /// configuration is portable, so either platform's alias conflicts.
    pub fn conflicts_with(&self, other: &Shortcut) -> bool {
        self.key == other.key
            && [false, true]
                .into_iter()
                .any(|macos| self.modifiers(macos) == other.modifiers(macos))
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
            return Err(field_error("shortcuts", Problem::UnknownShortcutAction));
        }
        // The action name is known, so naming it repeats no file content.
        let shortcut = Shortcut::parse(value).map_err(|error| match error {
            ConfigError::Field { problem, .. } => field_error(format!("shortcuts.{name}"), problem),
            other => other,
        })?;
        for (_, other) in &bindings {
            // A config is portable: reject an alias conflict on either platform,
            // even when the file is currently being edited on the other one.
            if other.conflicts_with(&shortcut) {
                return Err(field_error(
                    format!("shortcuts.{name}"),
                    Problem::ShortcutConflict,
                ));
            }
        }
        bindings.push((name, shortcut));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
    // Clones used by this application share terminal write admission. Reads,
    // diagnostics and an independent ConfigStore opened by another process
    // retain their existing behavior.
    write_admission: std::sync::Arc<ConfigWriteAdmission>,
}
#[derive(Debug, Default)]
struct ConfigWriteAdmission {
    closed: std::sync::atomic::AtomicBool,
    active: std::sync::RwLock<()>,
}

impl ConfigStore {
    pub fn standard() -> Result<Self, ConfigError> {
        let dirs = ProjectDirs::from("io.github", "brk275", "Morpheus")
            .ok_or(ConfigError::MissingConfigDirectory)?;
        Ok(Self::at(dirs.config_dir().join("config.toml")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_admission: std::sync::Arc::default(),
        }
    }

    /// Close all cloned application writers without waiting on filesystem I/O
    /// on the UI thread. An already admitted writer keeps its real read guard.
    pub fn close_write_admission(&self) {
        self.write_admission
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    /// A real exclusive barrier, not an active-count snapshot. With terminal
    /// admission closed, it proves every admitted synchronous publish scope
    /// has returned (including its stage/place/flush and original errors).
    pub fn write_shutdown_confirmed(&self) -> Result<bool, CommitError> {
        if !self
            .write_admission
            .closed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(false);
        }
        match self.write_admission.active.try_write() {
            Ok(_exclusive) => Ok(true),
            Err(std::sync::TryLockError::WouldBlock) => Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => Err(CommitError::Config(
                ConfigError::Invalid("Settings writer completion could not be confirmed.".into()),
            )),
        }
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

    /// The theme file a validated `appearance.theme_pack` names. The name was
    /// checked to contain no separator, so it cannot leave the themes folder.
    pub fn theme_path(&self, name: &str) -> Result<PathBuf, ConfigError> {
        if !is_theme_name(name) {
            return Err(field_error("appearance.theme_pack", Problem::ThemeName));
        }
        Ok(self.themes_dir()?.join(format!("{name}.toml")))
    }

    pub fn load_theme(&self, config: &Config) -> Result<Option<ThemePack>, ConfigError> {
        let Some(name) = config.appearance.theme_pack.as_deref() else {
            return Ok(None);
        };
        config.validate()?;
        let raw = read_bounded(&self.theme_path(name)?, MAX_THEME_BYTES);
        decode_theme_sample(name, &raw).map(Some)
    }

    pub fn load_or_create(&self) -> Result<Config, ConfigError> {
        if !self.path.exists() {
            let config = Config::default();
            match self.create(&config) {
                Ok(_) => return Ok(config),
                // Created meanwhile by another Morpheus instance or an
                // editor: that file is loaded below, never replaced.
                Err(CommitError::Changed { .. }) => {}
                Err(error) => return Err(error.into_config_error()),
            }
        }
        self.load()
    }

    pub fn load(&self) -> Result<Config, ConfigError> {
        self.load_revision().map(|(config, _)| config)
    }

    /// Write `config` whatever the file holds now: the last writer wins.
    /// A change decided on a read of the file uses `commit` instead. This
    /// holds the same writer locks, so it never lands between another
    /// Morpheus writer's comparison and rename, and it waits a bounded time
    /// for them (see the `commit` module). It is for background tasks and
    /// startup, not the UI thread.
    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        self.replace(config)
            .map(drop)
            .map_err(CommitError::into_config_error)
    }

    /// Poll the user-editable TOML file. Invalid edits yield an error; consumers
    /// keep the last valid `Config` until a later valid edit arrives.
    pub fn watch(&self, interval: Duration) -> Receiver<Result<Config, ConfigError>> {
        let (sender, receiver) = mpsc::channel();
        let path = self.path.clone();
        thread::spawn(move || {
            let mut last_raw: Option<Result<String, InputError>> = None;
            loop {
                thread::sleep(interval);
                // A missing file is not reported here; any other unreadable,
                // oversized or special file reports once per change.
                let raw = read_bounded(&path, MAX_CONFIG_BYTES);
                if raw == Err(InputError::Missing) || last_raw.as_ref() == Some(&raw) {
                    continue;
                }
                // Publish the same bounded bytes used to detect the change.
                // Reopening here could publish another revision and remember
                // the first one as if it had already been delivered.
                let result = decode_config_sample(&raw);
                last_raw = Some(raw);
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        receiver
    }

    /// Keep change detection and publication bound to the same bounded reads.
    /// A later file revision is observed on the next poll, never substituted
    /// after this sample has been marked as delivered.
    fn sample_with_theme(
        &self,
        config_raw: Result<String, InputError>,
    ) -> (WatchSignature, Result<LoadedConfig, ConfigError>) {
        let mut theme_raw = None;
        let result = decode_config_sample(&config_raw).and_then(|config| {
            let theme = if let Some(name) = config.appearance.theme_pack.as_deref() {
                let raw = read_bounded(&self.theme_path(name)?, MAX_THEME_BYTES);
                let theme = decode_theme_sample(name, &raw);
                theme_raw = Some(raw);
                Some(theme?)
            } else {
                None
            };
            Ok(LoadedConfig { config, theme })
        });
        ((config_raw, theme_raw), result)
    }

    /// Watch the settings file and its selected theme pack as a single unit.
    /// Compatibility receiver API; application-lifetime desktop callers use
    /// `watch_with_theme_owned` to cancel and join unchanged polling.
    /// Callers only apply `Ok` values so a broken external edit leaves the
    /// last valid appearance in place while its error can be shown in the UI.
    pub fn watch_with_theme(
        &self,
        interval: Duration,
    ) -> Receiver<Result<LoadedConfig, ConfigError>> {
        let (sender, receiver) = mpsc::channel();
        let store = self.clone();
        thread::spawn(move || {
            let mut last_signature: Option<WatchSignature> = None;
            loop {
                thread::sleep(interval);
                let (signature, result) =
                    store.sample_with_theme(read_bounded(store.path(), MAX_CONFIG_BYTES));
                if last_signature.as_ref() == Some(&signature) {
                    continue;
                }
                last_signature = Some(signature);
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        receiver
    }

    /// Owned desktop path. The same sample defines both change detection and
    /// publication; callers retain its stop handle and actual join receipt.
    /// Pending notifications coalesce; desktop callers reread current settings
    /// on the UI thread before applying privacy policy.
    pub fn watch_with_theme_owned(
        &self,
        interval: Duration,
    ) -> std::io::Result<OwnedWatch<Result<LoadedConfig, ConfigError>>> {
        let store = self.clone();
        let mut last_signature: Option<WatchSignature> = None;
        watch::spawn(interval, move || {
            let (signature, result) =
                store.sample_with_theme(read_bounded(store.path(), MAX_CONFIG_BYTES));
            if last_signature.as_ref() == Some(&signature) {
                return None;
            }
            last_signature = Some(signature);
            Some(result)
        })
    }
}

type WatchSignature = (
    Result<String, InputError>,
    Option<Result<String, InputError>>,
);

fn decode_config_sample(raw: &Result<String, InputError>) -> Result<Config, ConfigError> {
    match raw {
        Ok(raw) => commit::parse_config(raw),
        Err(error) => Err((*error).into()),
    }
}

fn decode_theme_sample(
    name: &str,
    raw: &Result<String, InputError>,
) -> Result<ThemePack, ConfigError> {
    let raw = match raw {
        Ok(raw) => raw,
        Err(InputError::Missing) => {
            if let Some(preset) = BuiltInStylePreset::from_theme_name(name) {
                return preset.theme();
            }
            return Err(InputError::Missing.into());
        }
        Err(error) => return Err((*error).into()),
    };
    let theme: ThemePack = parse_toml(raw, diagnostics::FileKind::Theme)?;
    theme.validate()?;
    Ok(theme)
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
    /// Optional interaction states; see [`ThemeStates`].
    #[serde(default, skip_serializing_if = "ThemeStates::is_empty")]
    pub states: ThemeStates,
}

impl ThemePack {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::Version);
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 80 {
            return Err(theme_error("name", Problem::ThemeDisplayName));
        }
        for (label, value) in self.colors.entries() {
            validate_theme_color(&format!("colors.{label}"), value)?;
        }
        for (label, value) in self.roles.entries() {
            if let Some(value) = value {
                validate_theme_color(&format!("roles.{label}"), value)?;
            }
        }
        for (label, value) in self.states.entries() {
            if let Some(value) = value {
                validate_theme_color(&format!("states.{label}"), value)?;
            }
        }
        self.senders
            .validate(|field, problem| theme_error(field, problem))
    }
}

impl SenderColors {
    /// 1 to [`MAX_SENDER_COLORS`] colors, each `#RRGGBB` or `#RRGGBBAA`.
    /// `error` builds the theme or `config.toml` error for a field path.
    fn validate(&self, error: fn(String, Problem) -> ConfigError) -> Result<(), ConfigError> {
        let Some(colors) = &self.colors else {
            return Ok(());
        };
        if !(1..=MAX_SENDER_COLORS).contains(&colors.len()) {
            return Err(error(
                "senders.colors".into(),
                Problem::PaletteLength {
                    maximum: MAX_SENDER_COLORS,
                },
            ));
        }
        for (index, value) in colors.iter().enumerate() {
            if registry::parse_hex_color(value).is_none() {
                return Err(error(format!("senders.colors[{index}]"), Problem::Color));
            }
        }
        Ok(())
    }
}

/// Theme colors use the same `#RRGGBB` or `#RRGGBBAA` convention as
/// `config.toml`; see `registry::parse_hex_color`.
fn validate_theme_color(field: &str, value: &str) -> Result<(), ConfigError> {
    if registry::parse_hex_color(value).is_none() {
        return Err(theme_error(field, Problem::Color));
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
    /// Backdrop that dims the window behind dialogs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrim: Option<String>,
    /// Shadow under popups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<String>,
    /// Area around an opened image; never the image pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_backdrop: Option<String>,
    /// Online-member status dot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_online: Option<String>,
    /// Initials drawn on avatar backgrounds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_ink: Option<String>,
}

impl ThemeRoles {
    pub fn entries(&self) -> [(&'static str, Option<&str>); 25] {
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
            ("scrim", self.scrim.as_deref()),
            ("shadow", self.shadow.as_deref()),
            ("media_backdrop", self.media_backdrop.as_deref()),
            ("presence_online", self.presence_online.as_deref()),
            ("avatar_ink", self.avatar_ink.as_deref()),
        ]
    }

    /// The theme's value for one role token, if it sets one.
    pub fn get(&self, id: &str) -> Option<&str> {
        self.entries()
            .into_iter()
            .find(|(name, _)| *name == id)
            .and_then(|(_, value)| value)
    }

    pub fn is_empty(&self) -> bool {
        self.entries().iter().all(|(_, value)| value.is_none())
    }
}

/// Optional interaction states. Each one follows an existing color, or keeps
/// each control's own state, until a theme pack or `config.toml` sets it, so
/// themes without this table and the shipped presets keep their look.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeStates {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_hover: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_hover: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_pressed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_disabled: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composer_surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composer_border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composer_focus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_active: Option<String>,
    /// Navigation rail and sidebar route icons; follow `icon` until set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_navigation: Option<String>,
    /// Conversation header actions; follow `icon` until set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_header: Option<String>,
    /// Composer tools; follow `icon` until set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_composer: Option<String>,
    /// Message quick actions; follow `icon` until set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nav_hover: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nav_selected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nav_selected_ink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention_ink: Option<String>,
}

impl ThemeStates {
    pub fn entries(&self) -> [(&'static str, Option<&str>); 20] {
        [
            ("message_hover", self.message_hover.as_deref()),
            ("control_hover", self.control_hover.as_deref()),
            ("control_pressed", self.control_pressed.as_deref()),
            ("control_disabled", self.control_disabled.as_deref()),
            ("input_surface", self.input_surface.as_deref()),
            ("input_border", self.input_border.as_deref()),
            ("composer_surface", self.composer_surface.as_deref()),
            ("composer_border", self.composer_border.as_deref()),
            ("composer_focus", self.composer_focus.as_deref()),
            ("icon", self.icon.as_deref()),
            ("icon_active", self.icon_active.as_deref()),
            ("icon_navigation", self.icon_navigation.as_deref()),
            ("icon_header", self.icon_header.as_deref()),
            ("icon_composer", self.icon_composer.as_deref()),
            ("icon_message", self.icon_message.as_deref()),
            ("nav_hover", self.nav_hover.as_deref()),
            ("nav_selected", self.nav_selected.as_deref()),
            ("nav_selected_ink", self.nav_selected_ink.as_deref()),
            ("mention", self.mention.as_deref()),
            ("mention_ink", self.mention_ink.as_deref()),
        ]
    }

    /// The theme's value for one state token, if it sets one.
    pub fn get(&self, id: &str) -> Option<&str> {
        self.entries()
            .into_iter()
            .find(|(name, _)| *name == id)
            .and_then(|(_, value)| value)
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
    pub fn entries(&self) -> [(&'static str, &str); 11] {
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
    use std::fs;

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
    fn room_notification_behavior_is_bounded_separate_and_sparse() {
        let mut config = Config::default();
        let quiet = RoomNotificationLocal {
            quiet: true,
            ..Default::default()
        };
        config.set_room_notification_local("work", "!a:example.org", quiet);
        config.validate().unwrap();
        assert_eq!(
            config.room_notification_local("work", "!a:example.org"),
            quiet
        );
        // Other rooms and accounts remove nothing.
        assert!(
            config
                .room_notification_local("work", "!b:example.org")
                .is_default()
        );
        assert!(
            config
                .room_notification_local("home", "!a:example.org")
                .is_default()
        );
        // Only switched-on keys are written, under their own table.
        let written = toml::to_string(&config).unwrap();
        assert!(written.contains("quiet = true"), "{written}");
        assert!(!written.contains("hide_preview") && !written.contains("no_highlights"));
        assert!(!written.contains("room_appearance"));
        let reread: Config = toml::from_str(&written).unwrap();
        assert_eq!(reread, config);
        // A room that removes nothing is dropped, and so is an emptied account.
        config.set_room_notification_local("work", "!a:example.org", Default::default());
        assert!(!config.account_overrides.contains_key("work"));
        // Invalid room IDs, unknown keys and too many rooms are rejected.
        let mut bad = Config::default();
        bad.set_room_notification_local("work", "not a room", quiet);
        assert!(bad.validate().is_err());
        assert!(
            toml::from_str::<Config>(
                "[account_overrides.work.room_notifications.\"!a:example.org\"]\nring = true\n"
            )
            .is_err()
        );
        let mut many = Config::default();
        for index in 0..=MAX_ROOM_NOTIFICATION_OVERRIDES {
            many.set_room_notification_local("work", &format!("!r{index}:example.org"), quiet);
        }
        assert!(many.validate().is_err());
        many.set_room_notification_local("work", "!r0:example.org", Default::default());
        many.validate().unwrap();
    }

    #[test]
    fn a_logged_out_accounts_room_notification_behavior_is_cleared_alone() {
        let quiet = RoomNotificationLocal {
            quiet: true,
            ..Default::default()
        };
        let mut config = Config::default();
        config.set_room_notification_local("work", "!a:example.org", quiet);
        config.set_room_notification_local("home", "!a:example.org", quiet);
        let work = config.account_overrides.get_mut("work").unwrap();
        work.notifications_enabled = Some(false);
        assert!(config.clear_room_notifications("work"));
        assert!(!config.clear_room_notifications("work"));
        // The account keeps its other preferences; other accounts keep theirs.
        assert!(
            config.account_overrides["work"]
                .room_notifications
                .is_empty()
        );
        assert_eq!(
            config.account_overrides["work"].notifications_enabled,
            Some(false)
        );
        assert_eq!(
            config.room_notification_local("home", "!a:example.org"),
            quiet
        );
        // An entry left with nothing is removed.
        assert!(config.clear_room_notifications("home"));
        assert!(!config.account_overrides.contains_key("home"));
        assert!(!config.clear_room_notifications("unknown"));
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
        assert!(matches!(store.load(), Err(ConfigError::Version)));
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
            states: ThemeStates::default(),
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
            Err(ConfigError::ThemeField { .. })
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
            Err(ConfigError::Input(InputError::Missing))
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
    fn account_theme_override_applies_only_to_that_account() {
        let mut config = Config::default();
        assert_eq!(config.theme_for(None), ThemeMode::Dark);
        assert_eq!(config.theme_for(Some("alice")), ThemeMode::Dark);
        config
            .account_overrides
            .entry("alice".into())
            .or_default()
            .theme = Some(ThemeMode::Light);
        assert_eq!(config.theme_for(Some("alice")), ThemeMode::Light);
        assert_eq!(config.theme_for(Some("bob")), ThemeMode::Dark);
        assert_eq!(config.theme_for(Some("")), ThemeMode::Dark);
        assert_eq!(config.theme_for(None), ThemeMode::Dark);
        config.appearance.theme = ThemeMode::HighContrast;
        assert_eq!(config.theme_for(Some("alice")), ThemeMode::Light);
        assert_eq!(config.theme_for(Some("bob")), ThemeMode::HighContrast);
        assert_eq!(config.theme_for(None), ThemeMode::HighContrast);
        // An override entry whose theme is unset is not an override.
        config.account_overrides.get_mut("alice").unwrap().theme = None;
        assert_eq!(config.theme_for(Some("alice")), ThemeMode::HighContrast);
    }

    #[test]
    fn account_theme_override_round_trips_toml_and_rejects_unknown_modes() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        std::fs::write(
            store.path(),
            "[appearance]\ntheme = \"dark\"\n\n[account_overrides.alice]\ntheme = \"light\"\n",
        )
        .unwrap();
        let config = store.load().unwrap();
        assert_eq!(config.theme_for(Some("alice")), ThemeMode::Light);
        assert_eq!(config.theme_for(Some("bob")), ThemeMode::Dark);
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), config);
        std::fs::write(
            store.path(),
            "[account_overrides.alice]\ntheme = \"neon\"\n",
        )
        .unwrap();
        assert!(
            store.load().is_err(),
            "an unknown theme mode is an invalid edit"
        );
        std::fs::write(
            store.path(),
            "[account_overrides.alice]\ntheme_pack = \"terminal\"\n",
        )
        .unwrap();
        assert!(
            store.load().is_err(),
            "theme packs are not an account override"
        );
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
        assert_eq!(message_roles.roles.entries().len(), 25);
        for field in [
            "timestamp",
            "sender",
            "message",
            "own_message",
            "own_surface",
        ] {
            let bad: ThemePack =
                toml::from_str(&format!("{encoded}\n[roles]\n{field} = 'tomato'\n")).unwrap();
            assert!(
                bad.validate().is_err(),
                "{field} must be #RRGGBB or #RRGGBBAA"
            );
        }
    }

    #[test]
    fn optional_theme_roles_reject_malformed_colors_and_unknown_keys() {
        let legacy: ThemePack =
            toml::from_str(include_str!("../../../themes/aurora.toml")).unwrap();
        let encoded = toml::to_string_pretty(&legacy).unwrap();
        for (role, _) in ThemeRoles::default().entries() {
            for bad in [
                "red",
                "#12345",
                "#1234567",
                "#abcdef0",
                "#abcdef000",
                "#gggggg",
                "#éabc",
            ] {
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
    fn account_colors_layer_over_global_colors_for_that_account_only() {
        let mut config = Config::default();
        config.colors.insert("canvas".into(), "#111111".into());
        config.colors.insert("link".into(), "#222222".into());
        config
            .account_overrides
            .entry("first".into())
            .or_default()
            .colors
            .insert("canvas".into(), "#333333".into());
        config.validate().unwrap();
        let first = config.colors_for(Some("first"));
        assert_eq!(first.get("canvas").map(String::as_str), Some("#333333"));
        assert_eq!(first.get("link").map(String::as_str), Some("#222222"));
        for other in [Some("second"), Some(""), None] {
            assert_eq!(config.colors_for(other), config.colors, "{other:?}");
        }
        let encoded = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&encoded).unwrap(), config);
        // Sanitized errors never name the account or repeat the value.
        for raw in [
            "[account_overrides.secretaccount.colors]\nnot_a_token = '#123456'\n",
            "[account_overrides.secretaccount.colors]\ncanvas = 'secret'\n",
        ] {
            let error = toml::from_str::<Config>(raw)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("account_overrides.<account>.colors"),
                "{error}"
            );
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn clearing_one_accounts_appearance_is_narrow() {
        let mut config = Config::default();
        config.colors.insert("canvas".into(), "#111111".into());
        for account in ["first", "second"] {
            let overrides = config.account_overrides.entry(account.into()).or_default();
            overrides.theme = Some(ThemeMode::Light);
            overrides.colors.insert("accent".into(), "#445566".into());
        }
        config
            .account_overrides
            .get_mut("first")
            .unwrap()
            .notifications_enabled = Some(false);
        let second = config.account_overrides["second"].clone();
        assert!(config.clear_account_appearance("first"));
        assert!(!config.clear_account_appearance("first"));
        let first = &config.account_overrides["first"];
        assert_eq!((first.theme, first.colors.is_empty()), (None, true));
        // Its other preferences stay, as do every other account and global.
        assert_eq!(first.notifications_enabled, Some(false));
        assert_eq!(config.account_overrides["second"], second);
        assert_eq!(config.colors.len(), 1);
        // An entry left with nothing is removed.
        assert!(config.clear_account_appearance("second"));
        assert!(!config.account_overrides.contains_key("second"));
        assert!(!config.clear_account_appearance("unknown"));
    }

    #[test]
    fn pane_composition_and_fades_default_off_round_trip_and_survive_presets() {
        let old: Config =
            toml::from_str("schema_version = 1\n[layout]\nroom_list_width = 260\n").unwrap();
        assert_eq!(old.layout.details_side, DetailsSide::Right);
        assert!(!old.layout.direct_messages_first);
        assert_eq!(
            (old.layout.room_density, old.layout.message_density),
            (None, None)
        );
        assert_eq!(
            (old.appearance.popup_fade_ms, old.appearance.panel_fade_ms),
            (0, 0)
        );
        let text = "[layout]\ndetails_side = 'left'\ndirect_messages_first = true\nroom_density = 'spacious'\nmessage_density = 'compact'\n[appearance]\npopup_fade_ms = 160\npanel_fade_ms = 240\nreduce_motion = true\n";
        let mut config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.layout.details_side, DetailsSide::Left);
        assert_eq!(config.layout.room_density, Some(Density::Spacious));
        assert_eq!(config.layout.message_density, Some(Density::Compact));
        assert_eq!(
            toml::from_str::<Config>(&toml::to_string(&config).unwrap()).unwrap(),
            config
        );
        let original = config.clone();
        for preset in BuiltInStylePreset::ALL {
            config.apply_style_preset(preset);
            assert_eq!(config.layout.details_side, DetailsSide::Left);
            assert!(config.layout.direct_messages_first);
            assert_eq!(config.layout.room_density, original.layout.room_density);
            assert_eq!(
                config.layout.message_density,
                original.layout.message_density
            );
            assert_eq!(config.appearance.popup_fade_ms, 160);
            assert_eq!(config.appearance.panel_fade_ms, 240);
            assert!(config.appearance.reduce_motion);
        }
        for bad in [
            "[layout]\ndetails_side = 'top'\n",
            "[layout]\nroom_density = 'tiny'\n",
            "[layout]\nmessage_density = 1\n",
            "[layout]\ndirect_messages_first = 'yes'\n",
        ] {
            assert!(toml::from_str::<Config>(bad).is_err(), "{bad}");
        }
        for bad in [
            "[appearance]\npopup_fade_ms = 1001\n",
            "[appearance]\npanel_fade_ms = 1001\n",
        ] {
            assert!(
                toml::from_str::<Config>(bad).unwrap().validate().is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn interaction_states_are_optional_validated_and_absent_from_shipped_presets() {
        let legacy: ThemePack =
            toml::from_str(include_str!("../../../themes/aurora.toml")).unwrap();
        let encoded = toml::to_string_pretty(&legacy).unwrap();
        assert!(!encoded.contains("[states]"));
        // Presets keep their exact look: every state follows its color.
        for preset in BuiltInStylePreset::ALL {
            assert!(preset.theme().unwrap().states.is_empty(), "{preset:?}");
        }
        // Every state is a registry token of the State source, and back.
        for (name, _) in ThemeStates::default().entries() {
            assert_eq!(
                registry::color_token(name).map(|token| token.source),
                Some(registry::TokenSource::State),
                "{name}"
            );
        }
        assert_eq!(
            registry::COLOR_TOKENS
                .iter()
                .filter(|token| token.source == registry::TokenSource::State)
                .count(),
            ThemeStates::default().entries().len()
        );
        let themed: ThemePack = toml::from_str(&format!(
            "{encoded}\n[states]\nmention = '#AA3344'\ncontrol_pressed = '#00000033'\n"
        ))
        .unwrap();
        themed.validate().unwrap();
        assert_eq!(themed.states.get("mention"), Some("#AA3344"));
        assert_eq!(themed.states.get("nav_hover"), None);
        assert_eq!(
            toml::from_str::<ThemePack>(&toml::to_string_pretty(&themed).unwrap()).unwrap(),
            themed
        );
        for (state, _) in ThemeStates::default().entries() {
            let text = format!("{encoded}\n[states]\n{state} = 'tomato'\n");
            let error = toml::from_str::<ThemePack>(&text)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("states.{state}")), "{error}");
            assert!(!error.contains("tomato"), "{error}");
        }
        let typo = format!("{encoded}\n[states]\nhoverr = '#123456'\n");
        assert!(toml::from_str::<ThemePack>(&typo).is_err());
        // A config.toml override reaches a state token like any color.
        let mut config = Config::default();
        config.colors.insert("mention".into(), "#AA3344".into());
        config.validate().unwrap();
    }

    #[test]
    fn config_sender_palette_is_bounded_round_trips_and_survives_presets() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        assert!(
            !fs::read_to_string(store.path())
                .unwrap()
                .contains("[senders]")
        );
        config.senders.colors = Some(vec!["#C2AEFA".into(), "#9dd8cd80".into()]);
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), config);
        let valid = fs::read(store.path()).unwrap();
        for preset in BuiltInStylePreset::ALL {
            let mut themed = config.clone();
            themed.apply_style_preset(preset);
            assert_eq!(themed.senders, config.senders, "{preset:?}");
        }
        for (colors, field) in [
            (vec![], "senders.colors"),
            (
                vec!["#123456".to_owned(); MAX_SENDER_COLORS + 1],
                "senders.colors",
            ),
            (vec!["#123456".into(), "secret".into()], "senders.colors[1]"),
        ] {
            let mut bad = config.clone();
            bad.senders.colors = Some(colors);
            let error = store.save(&bad).unwrap_err().to_string();
            assert!(
                error.contains(field) && !error.contains("secret"),
                "{error}"
            );
            assert_eq!(fs::read(store.path()).unwrap(), valid);
        }
        assert!(toml::from_str::<Config>("[senders]\ncolours = ['#123456']\n").is_err());
        // Removing the table restores the theme's palette.
        fs::write(store.path(), "schema_version = 1\n").unwrap();
        assert!(store.load().unwrap().senders.is_empty());
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
            Err(ConfigError::ThemeField { .. })
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
            // The online dot is a non-text indicator beside member names.
            for background in surfaces {
                assert_contrast(
                    &theme.name,
                    roles.presence_online.as_deref().unwrap(),
                    background,
                    3.0,
                );
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
    fn watcher_publication_uses_its_bounded_config_and_theme_samples() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = Config::default();
        config.conversation.header = ConversationHeaderMode::Compact;
        config.appearance.theme_pack = Some("custom".into());
        store.save(&config).unwrap();
        fs::create_dir_all(store.themes_dir().unwrap()).unwrap();
        let theme_path = store.theme_path("custom").unwrap();
        let theme = BuiltInStylePreset::Nocturne.theme().unwrap();
        let valid_theme = toml::to_string(&theme).unwrap();
        fs::write(&theme_path, &valid_theme).unwrap();
        let config_sample = read_bounded(store.path(), MAX_CONFIG_BYTES);
        let theme_sample = read_bounded(&theme_path, MAX_THEME_BYTES);

        // Replace the actual config after bounded sampling. The real poll
        // path must publish its captured valid revision, not reopen this edit.
        let invalid_config = "[conversation]\nheader = 'hidden'\n";
        let invalid_theme = "untrusted-theme-value = [\n";
        fs::write(store.path(), invalid_config).unwrap();
        let (signature, loaded) = store.sample_with_theme(config_sample.clone());
        assert_eq!(signature.0, config_sample);
        assert_eq!(signature.1.as_ref(), Some(&theme_sample));
        assert_eq!(
            loaded.unwrap(),
            LoadedConfig {
                config: config.clone(),
                theme: Some(theme.clone()),
            }
        );

        // The same rule applies to the theme decoder's captured bytes.
        fs::write(&theme_path, invalid_theme).unwrap();
        assert_eq!(decode_config_sample(&config_sample).unwrap(), config);
        assert_eq!(decode_theme_sample("custom", &theme_sample).unwrap(), theme);

        // The real poll path keeps the captured config revision and the
        // selected theme's exact error sample together; it never reloads the
        // now-invalid config path or returns the previously valid theme.
        let (signature, loaded) = store.sample_with_theme(config_sample.clone());
        assert_eq!(signature.0, config_sample);
        assert_eq!(signature.1.as_ref().unwrap().as_deref(), Ok(invalid_theme));
        assert!(matches!(loaded, Err(ConfigError::Decode(_))));
        let (signature, invalid) =
            store.sample_with_theme(read_bounded(store.path(), MAX_CONFIG_BYTES));
        assert_eq!(signature.0.as_deref(), Ok(invalid_config));
        assert!(signature.1.is_none());
        assert!(matches!(invalid, Err(ConfigError::Decode(_))));
        assert_eq!(fs::read_to_string(store.path()).unwrap(), invalid_config);

        fs::write(&theme_path, valid_theme).unwrap();
        config.conversation.header = ConversationHeaderMode::Minimal;
        store.save(&config).unwrap();
        let (_, restored) = store.sample_with_theme(read_bounded(store.path(), MAX_CONFIG_BYTES));
        assert_eq!(
            restored.unwrap(),
            LoadedConfig {
                config,
                theme: Some(theme)
            }
        );
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
        // Publish the external edit atomically. Truncate-and-write also
        // exposes an empty (valid-default) file, which is a different edit.
        let invalid = "[conversation]\nheader = 'hidden'\n";
        let editing = directory.path().join("edited.toml");
        fs::write(&editing, invalid).unwrap();
        fs::rename(&editing, store.path()).unwrap();
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ConfigError::Decode(_))
        ));
        assert_eq!(fs::read_to_string(store.path()).unwrap(), invalid);
        assert!(matches!(store.load(), Err(ConfigError::Decode(_))));
        // A later valid commit must still produce the recovery publication.
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

    #[test]
    fn sparse_color_overrides_and_window_opacity_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        // Defaults write no color table, and the window stays opaque.
        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("[colors]"));
        assert_eq!(config.window.opacity_percent, 100);
        config.colors.insert("canvas".into(), "#1E233080".into());
        config.colors.insert("link".into(), "#afcdff".into());
        config.window.opacity_percent = 0;
        store.save(&config).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.color_override("canvas"),
            Some([0x1e, 0x23, 0x30, 0x80])
        );
        assert_eq!(
            loaded.color_override("link"),
            Some([0xaf, 0xcd, 0xff, 0xff])
        );
        assert_eq!(loaded.color_override("panel"), None);
        // Overrides are sparse and survive choosing a starting point.
        let mut preset = loaded.clone();
        preset.apply_style_preset(BuiltInStylePreset::Paper);
        assert_eq!(preset.colors, loaded.colors);
        assert_eq!(preset.window, loaded.window);
        // Every registry token is accepted, in either case and with alpha.
        for token in registry::COLOR_TOKENS {
            let mut each = Config::default();
            each.colors.insert(token.id.into(), "#00000000".into());
            each.validate().unwrap();
        }
    }

    #[test]
    fn invalid_overrides_and_oversized_files_keep_the_last_valid_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut config = store.load_or_create().unwrap();
        config.colors.insert("canvas".into(), "#101010".into());
        store.save(&config).unwrap();
        let saved = fs::read(store.path()).unwrap();
        for (key, value) in [
            ("canvas", "#10101"),
            ("canvas", "red"),
            ("canvas", "#1010100"),
            ("not_a_token", "#101010"),
        ] {
            let mut bad = config.clone();
            bad.colors.insert(key.into(), value.into());
            assert!(matches!(store.save(&bad), Err(ConfigError::Field { .. })));
            assert_eq!(fs::read(store.path()).unwrap(), saved);
        }
        let mut transparent = config.clone();
        transparent.window.opacity_percent = 101;
        assert!(store.save(&transparent).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), saved);

        let updates = store.watch_with_theme(Duration::from_millis(10));
        assert!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_ok()
        );
        let oversized = format!("# {}\n", "x".repeat(MAX_CONFIG_BYTES as usize));
        fs::write(store.path(), oversized).unwrap();
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ConfigError::Input(InputError::TooLarge))
        ));
        assert!(matches!(
            store.load(),
            Err(ConfigError::Input(InputError::TooLarge))
        ));
        fs::write(store.path(), &saved).unwrap();
        assert_eq!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .config,
            config
        );
    }

    #[test]
    fn theme_names_cannot_name_paths_or_escape_the_themes_folder() {
        let store = ConfigStore::at("/profile/config.toml");
        let long = "n".repeat(65);
        for name in [
            "",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "a.toml",
            "C:",
            "~",
            long.as_str(),
        ] {
            assert!(!is_theme_name(name), "{name:?}");
            assert!(store.theme_path(name).is_err());
        }
        assert!(
            store
                .theme_path("my-theme_2")
                .unwrap()
                .ends_with("themes/my-theme_2.toml")
        );
    }

    #[test]
    fn validation_errors_never_repeat_the_rejected_value() {
        let mut config = Config::default();
        config.appearance.font_family = "secret\u{202e}family".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("appearance.font_family") && !error.contains("secret"));
        let mut config = Config::default();
        config
            .shortcuts
            .insert("search".into(), "Mod+SECRET".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("shortcuts.search") && !error.contains("SECRET"));
        let mut config = Config::default();
        config.media.microphone_device = Some("secret device".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("media.microphone_device") && !error.contains("secret"));
    }

    /// Every runtime path that shows an error (load, theme load, reload and
    /// Settings saves, which call `load` first) shows sanitized text only.
    #[test]
    fn runtime_store_errors_never_repeat_file_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        for raw in [
            "[geometry]\ncorner_radius = \"secret-type\"\n",
            "[chat]\nlayout = \"secret-variant\"\n",
            "secret-syntax = [\n",
            "secret_unknown_key = 1\n",
            "[account_overrides.secretaccount]\ntheme = \"secret-mode\"\n",
            "[account_overrides.secretaccount]\nlocal_voice_rooms = [\"secret room\"]\n",
            "[colors]\nlink = \"secret\"\n",
            "[shortcuts]\nsearch = \"Mod+secret\"\n",
            "schema_version = 777\n",
        ] {
            fs::write(store.path(), raw).unwrap();
            let error = store.load().unwrap_err();
            let shown = format!("{error} {error:?}");
            assert!(
                !shown.contains("secret") && !shown.contains("777"),
                "{raw}: {shown}"
            );
        }

        // A theme file's content never reaches its error either.
        let mut config = Config::default();
        config.appearance.theme_pack = Some("custom".into());
        store.save(&config).unwrap();
        fs::create_dir_all(store.themes_dir().unwrap()).unwrap();
        let theme_path = store.theme_path("custom").unwrap();
        for raw in [
            "secret-theme-syntax = [\n",
            "schema_version = 1\nname = \"x\"\nsecret_key = 1\n",
            "schema_version = 1\nname = 7\n",
        ] {
            fs::write(&theme_path, raw).unwrap();
            let error = store.load_theme(&config).unwrap_err();
            let shown = format!("{error} {error:?}");
            assert!(
                !shown.contains("secret") && !shown.contains("7"),
                "{raw}: {shown}"
            );
            assert!(matches!(error, ConfigError::Decode(_)));
        }

        // The reload path reports the same sanitized error.
        fs::write(&theme_path, "secret-theme-syntax = [\n").unwrap();
        let updates = store.watch_with_theme(Duration::from_millis(10));
        let error = updates
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(!format!("{error} {error:?}").contains("secret"));
    }

    #[test]
    fn every_editor_metric_is_a_bounded_field_with_one_accessor() {
        use registry::{MetricGroup, metric_field, metric_fields};
        let defaults = Config::default();
        let mut keys = std::collections::BTreeSet::new();
        for (field, editor) in metric_fields() {
            let key = field.key;
            assert!(keys.insert(key), "duplicate editor metric: {key}");
            let (minimum, maximum) = field.integer_range().expect("metrics are integers");
            assert!(minimum >= 0 && minimum < maximum, "{key}");
            assert!(!editor.label.is_empty() && !field.inactive, "{key}");
            assert!(MetricGroup::ALL.contains(&editor.group), "{key}");
            assert!(editor.step >= 1 && i64::from(editor.step) <= maximum - minimum);
            // Omission is documented exactly when the field is optional, and
            // stepping from it starts inside the range.
            assert_eq!(field.optional(), editor.inherit.is_some(), "{key}");
            if let Some(inherit) = editor.inherit {
                assert!(!inherit.text.is_empty(), "{key}");
                assert!(
                    (minimum..=maximum).contains(&i64::from(inherit.start)),
                    "{key}"
                );
            }
            assert_eq!(metric_field(key).map(|(found, _)| found.key), Some(key));
            match defaults.metric(key).expect(key) {
                MetricValue::Inherited => assert!(field.optional(), "{key}"),
                MetricValue::Set(value) => {
                    assert_eq!(field.integer_default(), Some(i64::from(value)), "{key}")
                }
            }
            let mut config = Config::default();
            for value in [minimum, maximum] {
                let value = u16::try_from(value).unwrap();
                config.set_metric(key, MetricValue::Set(value)).unwrap();
                assert_eq!(config.metric(key), Some(MetricValue::Set(value)), "{key}");
                config.validate().unwrap();
                let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
                assert_eq!(restored.metric(key), Some(MetricValue::Set(value)), "{key}");
            }
            // A rejected value changes nothing.
            let before = config.clone();
            for value in [minimum - 1, maximum + 1] {
                if let Ok(value) = u16::try_from(value) {
                    assert!(config.set_metric(key, MetricValue::Set(value)).is_err());
                    assert_eq!(config, before, "{key} changed on a rejected value");
                }
            }
            // Resetting returns exactly this metric to its default.
            assert!(config.set_metric(key, MetricValue::Inherited).unwrap());
            assert!(!config.set_metric(key, MetricValue::Inherited).unwrap());
            assert_eq!(config, defaults, "{key} reset left another change");
        }
        assert_eq!(
            keys,
            [
                "appearance.panel_fade_ms",
                "appearance.popup_fade_ms",
                "geometry.avatar_roundness",
                "geometry.avatar_size",
                "geometry.border_width",
                "geometry.composer_padding",
                "geometry.composer_radius",
                "geometry.control_padding",
                "geometry.control_radius",
                "geometry.focus_width",
                "geometry.icon_size",
                "geometry.input_padding",
                "geometry.input_radius",
                "typography.body_weight",
                "typography.composer_size",
                "typography.line_height_percent",
                "typography.sender_size",
                "typography.sidebar_size",
                "typography.timestamp_size",
            ]
            .into_iter()
            .collect()
        );
        // Fields without editor metadata are not metrics.
        for key in [
            "typography.message_size",
            "geometry.corner_radius",
            "colors.link",
            "geometry",
            "",
        ] {
            assert_eq!(defaults.metric(key), None, "{key}");
            assert!(
                Config::default()
                    .set_metric(key, MetricValue::Set(1))
                    .is_err()
            );
        }
    }

    #[test]
    fn metric_edits_reject_invalid_files_without_repeating_values_and_recover() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let mut custom = Config::default();
        for (key, value) in [
            ("geometry.focus_width", 4),
            ("typography.sender_size", 17),
            ("geometry.avatar_roundness", 50),
            ("geometry.border_width", 0),
        ] {
            custom.set_metric(key, MetricValue::Set(value)).unwrap();
        }
        store.save(&custom).unwrap();
        let last_valid = fs::read(store.path()).unwrap();
        let updates = store.watch_with_theme(Duration::from_millis(10));
        let next = || updates.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(next().unwrap().config, custom);
        for (raw, rejected) in [
            ("[geometry]\nfocus_width = 0\n", "0"),
            ("[geometry]\nfocus_width = 7\n", "7"),
            ("[geometry]\nborder_width = 5\n", "5"),
            ("[geometry]\nicon_size = 29\n", "29"),
            ("[geometry]\navatar_roundness = 51\n", "51"),
            ("[typography]\nsidebar_size = 9\n", "9"),
            ("[typography]\nsender_size = 33\n", "33"),
        ] {
            let error = toml::from_str::<Config>(raw)
                .unwrap()
                .validate()
                .unwrap_err();
            let shown = error.to_string();
            assert!(matches!(error, ConfigError::Field { .. }), "{raw}");
            assert!(!shown.contains(rejected), "{raw}: {shown}");
            fs::write(store.path(), raw).unwrap();
            assert!(next().is_err(), "{raw}");
            // A rejected external edit stays as written for correction.
            assert_eq!(fs::read_to_string(store.path()).unwrap(), raw);
            assert!(store.save(&custom).is_ok());
            assert_eq!(next().unwrap().config, custom);
        }
        for raw in [
            "[geometry]\nfocus_width = \"wide\"\n",
            "[geometry]\nfocus_widht = 2\n",
            "[typography]\ncomposer_size = -1\n",
            "[geometry]\navatar_size = 1.5\n",
        ] {
            assert!(toml::from_str::<Config>(raw).is_err(), "{raw}");
        }
        fs::write(store.path(), &last_valid).unwrap();
        // Omitting every metric restores the built-in defaults, not the
        // values a previous watcher result carried.
        fs::write(store.path(), "schema_version = 1\n").unwrap();
        let restored = loop {
            let config = next().unwrap().config;
            if config != custom {
                break config;
            }
        };
        assert_eq!(restored.typography, Typography::default());
        assert_eq!(restored.geometry, Geometry::default());
    }

    #[test]
    fn presets_reset_style_metrics_but_keep_the_focus_ring_width() {
        let mut config = Config::default();
        for (key, value) in [
            ("geometry.focus_width", 5),
            ("geometry.icon_size", 24),
            ("geometry.control_radius", 0),
            ("typography.composer_size", 20),
            ("typography.sidebar_size", 16),
        ] {
            config.set_metric(key, MetricValue::Set(value)).unwrap();
        }
        for preset in BuiltInStylePreset::ALL {
            let mut themed = config.clone();
            themed.apply_style_preset(preset);
            themed.validate().unwrap();
            assert_eq!(themed.geometry.focus_width, 5, "{preset:?}");
            assert_eq!(themed.geometry.icon_size, 18, "{preset:?}");
            assert_eq!(themed.geometry.control_radius, None, "{preset:?}");
            assert_eq!(themed.typography.composer_size, None, "{preset:?}");
            assert_eq!(themed.typography.sidebar_size, 13, "{preset:?}");
        }
    }

    #[test]
    fn pane_arrangements_validate_and_keep_older_files_working() {
        // An older file without the new keys keeps today's arrangement.
        let older: Config =
            toml::from_str("schema_version = 1\n[layout]\ndetails_side = 'left'\n").unwrap();
        older.validate().unwrap();
        assert_eq!(
            older.layout.effective_pane_order(),
            [
                PaneId::Spaces,
                PaneId::Rooms,
                PaneId::Details,
                PaneId::Conversation
            ]
        );
        assert_eq!(
            Config::default().layout.effective_pane_order(),
            [
                PaneId::Spaces,
                PaneId::Rooms,
                PaneId::Conversation,
                PaneId::Details
            ]
        );
        let field = |layout: Layout| match (Config {
            layout,
            ..Config::default()
        })
        .validate()
        {
            Err(ConfigError::Field { field, problem }) => Some((field, problem)),
            Ok(()) => None,
            Err(_) => Some(("other".into(), Problem::PaneOrder)),
        };
        let order = |panes: &[PaneId]| Layout {
            pane_order: panes.to_vec(),
            ..Layout::default()
        };
        // Missing, duplicate and partial orders are refused.
        for invalid in [
            &[PaneId::Rooms][..],
            &[
                PaneId::Rooms,
                PaneId::Rooms,
                PaneId::Conversation,
                PaneId::Details,
            ],
            &[PaneId::Spaces, PaneId::Rooms, PaneId::Conversation],
        ] {
            assert_eq!(
                field(order(invalid)),
                Some(("layout.pane_order".into(), Problem::PaneOrder))
            );
        }
        let custom = order(&[
            PaneId::Conversation,
            PaneId::Details,
            PaneId::Rooms,
            PaneId::Spaces,
        ]);
        assert_eq!(field(custom.clone()), None);
        assert_eq!(custom.effective_pane_order()[0], PaneId::Conversation);
        // The conversation can never be hidden; a pane appears once; only the
        // room list and details panel collapse.
        let hidden = |panes: &[PaneId]| Layout {
            hidden_panes: panes.to_vec(),
            ..Layout::default()
        };
        assert!(field(hidden(&[PaneId::Conversation])).is_some());
        assert!(field(hidden(&[PaneId::Rooms, PaneId::Rooms])).is_some());
        assert_eq!(
            field(hidden(&[PaneId::Spaces, PaneId::Rooms, PaneId::Details])),
            None
        );
        let collapsed = Layout {
            collapsed_panes: vec![PaneId::Spaces],
            ..Layout::default()
        };
        assert_eq!(
            field(collapsed),
            Some(("layout.collapsed_panes".into(), Problem::PaneVisibility))
        );
        // A hidden pane is not also collapsed; restoring clears all three.
        let mut both = Layout {
            hidden_panes: vec![PaneId::Rooms],
            collapsed_panes: vec![PaneId::Rooms],
            ..custom
        };
        assert!(both.pane_hidden(PaneId::Rooms) && !both.pane_collapsed(PaneId::Rooms));
        assert!(both.restore_default_panes());
        assert_eq!(
            both.effective_pane_order(),
            Config::default().layout.effective_pane_order()
        );
        assert!(!both.restore_default_panes());
        // Unknown pane names are rejected by the schema itself.
        assert!(
            toml::from_str::<Config>("schema_version = 1\n[layout]\nhidden_panes = ['members']\n")
                .is_err()
        );
    }

    /// `registry::FIELDS` is what the schema output and documentation print,
    /// so it must not drift from the real schema and validator.
    #[test]
    fn the_field_reference_agrees_with_the_real_schema() {
        use registry::{FIELDS, FieldKind};
        use std::collections::BTreeSet;

        fn leaves(value: &toml::Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
            match value {
                toml::Value::Table(table) => {
                    for (key, value) in table {
                        path.push(key.clone());
                        leaves(value, path, out);
                        path.pop();
                    }
                }
                _ => out.push(path.clone()),
            }
        }
        // Dynamic keys are written as the placeholders FIELDS documents.
        fn pattern(path: &[String]) -> String {
            match path.first().map(String::as_str) {
                Some("colors") => "colors.<token>".into(),
                Some("shortcuts") => "shortcuts.<action>".into(),
                Some("account_overrides") if path.get(2).is_some_and(|key| key == "colors") => {
                    "account_overrides.<account>.colors.<token>".into()
                }
                Some("account_overrides")
                    if path.get(2).is_some_and(|key| key == "room_notifications") =>
                {
                    format!(
                        "account_overrides.<account>.room_notifications.<room>.{}",
                        path[4..].join(".")
                    )
                }
                Some("account_overrides")
                    if path.get(2).is_some_and(|key| key == "room_appearance") =>
                {
                    if path.get(4).is_some_and(|key| key == "colors") {
                        "account_overrides.<account>.room_appearance.<room>.colors.<token>".into()
                    } else {
                        format!(
                            "account_overrides.<account>.room_appearance.<room>.{}",
                            path[4..].join(".")
                        )
                    }
                }
                Some("account_overrides") => {
                    format!("account_overrides.<account>.{}", path[2..].join("."))
                }
                _ => path.join("."),
            }
        }

        // Every optional and dynamic field present.
        let mut full = Config::default();
        full.appearance.theme_pack = Some("paper".into());
        full.typography.message_font_family = Some("serif".into());
        full.typography.sender_weight = Some(600);
        full.typography.sender_size = Some(15);
        full.typography.timestamp_size = Some(11);
        full.typography.composer_size = Some(16);
        full.typography.line_height_percent = Some(140);
        full.typography.composer_font_family = Some("monospace".into());
        full.typography.body_weight = Some(500);
        full.geometry.control_radius = Some(4);
        full.geometry.input_radius = Some(5);
        full.geometry.composer_radius = Some(6);
        full.geometry.avatar_size = Some(30);
        full.geometry.avatar_roundness = Some(50);
        full.layout.room_density = Some(Density::Compact);
        full.layout.message_density = Some(Density::Spacious);
        full.media.microphone_device = Some("alsa:microphone".into());
        full.media.speaker_device = Some("alsa:speaker".into());
        full.media.camera_device = Some("camera".into());
        full.colors.insert("link".into(), "#123456".into());
        full.senders.colors = Some(vec!["#123456".into()]);
        full.account_overrides.insert(
            "account".into(),
            AccountOverrides {
                notifications_enabled: Some(true),
                theme: Some(ThemeMode::Light),
                local_voice_rooms: vec!["!a:example.org".into()],
                legacy_element_call_rooms: vec!["!b:example.org".into()],
                element_call_matrix_2_0_rooms: vec!["!c:example.org".into()],
                colors: BTreeMap::from([("link".into(), "#123456".into())]),
                room_notifications: BTreeMap::from([(
                    "!d:example.org".into(),
                    RoomNotificationLocal {
                        quiet: true,
                        hide_preview: true,
                        no_highlights: true,
                    },
                )]),
                room_appearance: BTreeMap::from([(
                    "!d:example.org".into(),
                    RoomAppearance {
                        theme: Some(ThemeMode::Dark),
                        colors: BTreeMap::from([("link".into(), "#654321".into())]),
                    },
                )]),
            },
        );
        full.validate().unwrap();
        let mut found = Vec::new();
        leaves(
            &toml::Value::try_from(&full).unwrap(),
            &mut Vec::new(),
            &mut found,
        );
        let found: BTreeSet<String> = found.iter().map(|path| pattern(path)).collect();
        let documented: BTreeSet<String> =
            FIELDS.iter().map(|field| field.key.to_owned()).collect();
        assert_eq!(found, documented, "FIELDS and the schema differ");

        // Defaults match Config::default(); "(none)" fields are omitted.
        let defaults = toml::Value::try_from(Config::default()).unwrap();
        for field in FIELDS.iter().filter(|field| !field.key.contains('<')) {
            let actual = field
                .key
                .split('.')
                .try_fold(&defaults, |value, key| value.get(key));
            if field.default == "(none)" {
                assert!(actual.is_none(), "{} has a default", field.key);
            } else {
                let expected: toml::Table =
                    toml::from_str(&format!("v = {}", field.default)).unwrap();
                assert_eq!(actual, expected.get("v"), "default of {}", field.key);
            }
        }
        for action in default_shortcuts().keys() {
            assert!(ShortcutAction::parse(action).is_some());
        }

        // Every documented range and option agrees with validation.
        fn document(key: &str, name: &str, value: &str) -> String {
            let key = key
                .replace("<account>", "account")
                .replace("<room>", "\"!room:example.org\"")
                .replace("<token>", name)
                .replace("<action>", name)
                .replace("<room>", "\"!room:example.org\"");
            match key.rsplit_once('.') {
                Some((table, leaf)) => format!("[{table}]\n{leaf} = {value}\n"),
                None => format!("{key} = {value}\n"),
            }
        }
        let accepts = |text: &str| {
            toml::from_str::<Config>(text).is_ok_and(|config| config.validate().is_ok())
        };
        for field in FIELDS {
            let names: Vec<&str> = if field.key.contains("<token>") {
                registry::COLOR_TOKENS
                    .iter()
                    .map(|token| token.id)
                    .collect()
            } else if field.key.contains("<action>") {
                ShortcutAction::ALL
                    .iter()
                    .map(|action| action.name())
                    .collect()
            } else {
                vec![""]
            };
            for name in names {
                let check = |value: &str| accepts(&document(field.key, name, value));
                let (good, bad): (Vec<String>, Vec<String>) = match field.kind {
                    FieldKind::Integer { minimum, maximum } => (
                        vec![minimum.to_string(), maximum.to_string()],
                        vec![(minimum - 1).to_string(), (maximum + 1).to_string()],
                    ),
                    FieldKind::Enum(values) => (
                        values.iter().map(|value| format!("\"{value}\"")).collect(),
                        vec!["\"not_an_option\"".into()],
                    ),
                    FieldKind::Bool => {
                        (vec!["true".into(), "false".into()], vec!["\"yes\"".into()])
                    }
                    FieldKind::FontFamily => (vec!["\"Inter\"".into()], vec!["\"\"".into()]),
                    FieldKind::ThemeName => (vec!["\"paper\"".into()], vec!["\"../paper\"".into()]),
                    FieldKind::Color => (
                        vec!["\"#123456\"".into(), "\"#12345678\"".into()],
                        vec!["\"#12345\"".into()],
                    ),
                    FieldKind::ColorList => (
                        vec![
                            "[\"#123456\"]".into(),
                            "[\"#12345678\", \"#abcdef\"]".into(),
                        ],
                        vec!["[]".into(), "[\"red\"]".into()],
                    ),
                    FieldKind::DeviceId => {
                        (vec!["\"alsa:device\"".into()], vec!["\"no-host\"".into()])
                    }
                    FieldKind::Text => (vec!["\"anything\"".into()], vec!["5".into()]),
                    FieldKind::TextList => (vec!["[\"word\"]".into()], vec!["5".into()]),
                    FieldKind::RoomIdList => (
                        vec!["[\"!a:example.org\"]".into()],
                        vec!["[\"not a room\"]".into()],
                    ),
                    FieldKind::Table => (vec!["\"Mod+J\"".into()], vec!["\"Tab\"".into()]),
                    FieldKind::PaneOrder => (
                        vec![
                            "[]".into(),
                            "[\"details\", \"conversation\", \"rooms\", \"spaces\"]".into(),
                        ],
                        vec![
                            "[\"rooms\"]".into(),
                            "[\"rooms\", \"rooms\", \"conversation\", \"details\"]".into(),
                            "[\"spaces\", \"rooms\", \"conversation\", \"members\"]".into(),
                        ],
                    ),
                    FieldKind::PaneSet(values) => (
                        vec!["[]".into(), format!("[\"{}\"]", values[0])],
                        vec![
                            "[\"conversation\"]".into(),
                            format!("[\"{0}\", \"{0}\"]", values[0]),
                        ],
                    ),
                };
                for value in &good {
                    assert!(check(value), "{} {name} should accept {value}", field.key);
                }
                for value in &bad {
                    assert!(!check(value), "{} {name} should reject {value}", field.key);
                }
            }
        }
    }
}
