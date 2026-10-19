//! Named style profiles: the look of Morpheus saved as a small, versioned,
//! sparse TOML file that people and assistants can read and edit.
//!
//! A profile holds appearance only: the theme mode and theme pack, fonts and
//! type sizes, geometry, message and conversation presentation, pane layout,
//! window opacity, global color tokens and the sender-name palette. It never
//! holds accounts or local IDs, authentication, notifications or highlight
//! words, shortcuts, media devices, drafts, call modes, security or update
//! preferences, per-account overrides, the text scale, Reduce motion or the
//! focus ring width; the last three stay with the person using the app.
//!
//! Every table and key is optional. A key a profile omits is the built-in
//! default when the profile is applied, so a profile always produces the
//! same look. Saving writes only keys that differ from those defaults.
//! Unknown keys, other format versions and invalid values are rejected with
//! fixed messages that never repeat the rejected text.
//!
//! [`ProfileStore`] keeps profiles in one fixed folder beside `config.toml`,
//! opened once per operation as an anchor that every file operation goes
//! through (see [`ProfileStore`] for the trust boundary). It never walks other
//! directories, reads only bounded regular files (never through a link and
//! never blocking on a FIFO or device), names files from a restricted
//! alphabet, and writes through a staging file in the same folder that
//! replaces the destination atomically. Morpheus rewrites a profile file only
//! when it is saved, renamed or imported; comments in it are not kept then.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ChatStyle, Config, ConfigError, ConfigStore, Conversation, Density, DetailsSide, Geometry,
    NavigationMode, SenderColors, ThemeMode, Typography, WindowStyle, diagnostics, parse_toml,
};

/// The profile format this version reads and writes.
pub const PROFILE_VERSION: u32 = 1;
/// Largest accepted profile file.
pub const MAX_PROFILE_BYTES: u64 = 64 * 1024;
/// Most profiles listed from the profiles folder.
pub const MAX_PROFILES: usize = 64;
/// Directory entries examined at most while listing, whatever they are.
const MAX_SCANNED_ENTRIES: usize = 512;
/// Longest profile name, in characters.
pub const MAX_PROFILE_NAME_CHARS: usize = 48;
/// Longest profile file stem; leaves room for a numeric suffix.
const MAX_STEM_CHARS: usize = 40;
// Not `profiles`: on macOS this folder is also the data folder that holds
// the account stores, so the name says what it is.
const PROFILES_DIR: &str = "style-profiles";
const EXTENSION: &str = "toml";
const HEADER: &str = "# Morpheus style profile. Only appearance settings are read from this file;\n\
# omitted keys use the built-in default. See docs/STYLE_PROFILES.md.\n\n";

/// Why a profile operation failed, as fixed text: never a path, file name,
/// profile name or rejected value.
#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("the profile file does not exist")]
    Missing,
    #[error("the profile is not a regular file; links, folders and devices are not opened")]
    NotRegularFile,
    #[error("the profile file is larger than 64 KiB")]
    TooLarge,
    #[error("the profile file could not be read")]
    Unreadable,
    #[error("the profile file is not UTF-8 text")]
    NotUtf8,
    #[error("the file is not a Morpheus style profile: profile_version is missing")]
    NotAProfile,
    /// The rejected number is file content, so it is not repeated.
    #[error("this profile uses another format version; this version reads profile_version = 1")]
    Version,
    #[error("the profile is invalid: {}", describe(.0))]
    Invalid(ConfigError),
    #[error("profile names are 1 to 48 characters, without line breaks or control characters")]
    Name,
    #[error("a profile with this name already exists")]
    NameTaken,
    #[error("there are already 64 profiles; delete one first")]
    TooMany,
    #[error("the profiles folder is not available")]
    Folder,
    #[error("the chosen file is a link, folder or device; choose another file")]
    Target,
    #[error("the profile could not be saved; the previous file was kept")]
    Write,
    #[error("the profile changed on disk; the list was refreshed")]
    Changed,
}

/// A configuration error as profile text: the field and its fixed problem,
/// never the rejected value.
fn describe(error: &ConfigError) -> String {
    match error {
        ConfigError::Field { field, problem } => format!("{field} {problem}"),
        ConfigError::Decode(decode) => decode.to_string(),
        ConfigError::Version => "unsupported schema version".to_owned(),
        _ => "a value is not accepted".to_owned(),
    }
}

impl ProfileError {
    /// A stable code for the error category.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "missing-file",
            Self::NotRegularFile => "not-a-regular-file",
            Self::TooLarge => "file-too-large",
            Self::Unreadable => "unreadable-file",
            Self::NotUtf8 => "not-utf8",
            Self::NotAProfile => "not-a-profile",
            Self::Version => "profile-version",
            Self::Invalid(_) => "invalid-profile",
            Self::Name => "profile-name",
            Self::NameTaken => "name-taken",
            Self::TooMany => "too-many-profiles",
            Self::Folder => "profiles-folder",
            Self::Target => "export-target",
            Self::Write => "write-failed",
            Self::Changed => "changed-on-disk",
        }
    }
}

/// The appearance keys of a profile's `[appearance]` table. The text scale
/// and Reduce motion are not part of a look.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileAppearance {
    pub theme: ThemeMode,
    pub theme_pack: Option<String>,
    pub font_family: String,
    pub density: Density,
    pub hover_duration_ms: u16,
    pub popup_fade_ms: u16,
    pub panel_fade_ms: u16,
}

impl Default for ProfileAppearance {
    fn default() -> Self {
        let appearance = crate::Appearance::default();
        Self {
            theme: appearance.theme,
            theme_pack: appearance.theme_pack,
            font_family: appearance.font_family,
            density: appearance.density,
            hover_duration_ms: appearance.hover_duration_ms,
            popup_fade_ms: appearance.popup_fade_ms,
            panel_fade_ms: appearance.panel_fade_ms,
        }
    }
}

/// Every geometry key except the focus ring width, which stays with the
/// person using the app, as it does for starting points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileGeometry {
    pub corner_radius: u16,
    pub message_gap: u16,
    pub content_padding: u16,
    pub rail_width: u16,
    pub control_radius: Option<u16>,
    pub input_radius: Option<u16>,
    pub composer_radius: Option<u16>,
    pub border_width: u16,
    pub control_padding: u16,
    pub input_padding: u16,
    pub composer_padding: u16,
    pub icon_size: u16,
    pub avatar_size: Option<u16>,
    pub avatar_roundness: Option<u16>,
}

impl Default for ProfileGeometry {
    fn default() -> Self {
        Self::from_geometry(&Geometry::default())
    }
}

impl ProfileGeometry {
    fn from_geometry(geometry: &Geometry) -> Self {
        Self {
            corner_radius: geometry.corner_radius,
            message_gap: geometry.message_gap,
            content_padding: geometry.content_padding,
            rail_width: geometry.rail_width,
            control_radius: geometry.control_radius,
            input_radius: geometry.input_radius,
            composer_radius: geometry.composer_radius,
            border_width: geometry.border_width,
            control_padding: geometry.control_padding,
            input_padding: geometry.input_padding,
            composer_padding: geometry.composer_padding,
            icon_size: geometry.icon_size,
            avatar_size: geometry.avatar_size,
            avatar_roundness: geometry.avatar_roundness,
        }
    }

    fn apply_to(&self, geometry: &mut Geometry) {
        *geometry = Geometry {
            corner_radius: self.corner_radius,
            message_gap: self.message_gap,
            content_padding: self.content_padding,
            rail_width: self.rail_width,
            control_radius: self.control_radius,
            input_radius: self.input_radius,
            composer_radius: self.composer_radius,
            border_width: self.border_width,
            focus_width: geometry.focus_width,
            control_padding: self.control_padding,
            input_padding: self.input_padding,
            composer_padding: self.composer_padding,
            icon_size: self.icon_size,
            avatar_size: self.avatar_size,
            avatar_roundness: self.avatar_roundness,
        };
    }
}

/// The presentation keys of `[layout]`: navigation style, pane sizes, sides,
/// order and density. Grouping and unread sorting organize rooms and stay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileLayout {
    pub navigation: NavigationMode,
    pub room_list_width: u16,
    pub details_width: u16,
    pub show_member_panel: bool,
    pub show_space_labels: bool,
    pub details_side: DetailsSide,
    pub direct_messages_first: bool,
    pub room_density: Option<Density>,
    pub message_density: Option<Density>,
}

impl Default for ProfileLayout {
    fn default() -> Self {
        let layout = crate::Layout::default();
        Self {
            navigation: layout.navigation,
            room_list_width: layout.room_list_width,
            details_width: layout.details_width,
            show_member_panel: layout.show_member_panel,
            show_space_labels: layout.show_space_labels,
            details_side: layout.details_side,
            direct_messages_first: layout.direct_messages_first,
            room_density: layout.room_density,
            message_density: layout.message_density,
        }
    }
}

/// One named look. See the module documentation for what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleProfile {
    pub profile_version: u32,
    pub name: String,
    #[serde(default)]
    pub appearance: ProfileAppearance,
    #[serde(default)]
    pub typography: Typography,
    #[serde(default)]
    pub geometry: ProfileGeometry,
    #[serde(default)]
    pub chat: ChatStyle,
    #[serde(default)]
    pub conversation: Conversation,
    #[serde(default)]
    pub layout: ProfileLayout,
    #[serde(default)]
    pub window: WindowStyle,
    /// Global color tokens; per-account colors are never part of a profile.
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
    #[serde(default)]
    pub senders: SenderColors,
}

impl StyleProfile {
    /// A profile with the given name and the built-in look.
    pub fn named(name: &str) -> Self {
        Self::from_config(name, &Config::default())
    }

    /// The look of `config`, under `name`. Nothing else is read.
    pub fn from_config(name: &str, config: &Config) -> Self {
        let appearance = &config.appearance;
        let layout = &config.layout;
        Self {
            profile_version: PROFILE_VERSION,
            name: name.trim().to_owned(),
            appearance: ProfileAppearance {
                theme: appearance.theme,
                theme_pack: appearance.theme_pack.clone(),
                font_family: appearance.font_family.clone(),
                density: appearance.density,
                hover_duration_ms: appearance.hover_duration_ms,
                popup_fade_ms: appearance.popup_fade_ms,
                panel_fade_ms: appearance.panel_fade_ms,
            },
            typography: config.typography.clone(),
            geometry: ProfileGeometry::from_geometry(&config.geometry),
            chat: config.chat.clone(),
            conversation: config.conversation.clone(),
            layout: ProfileLayout {
                navigation: layout.navigation,
                room_list_width: layout.room_list_width,
                details_width: layout.details_width,
                show_member_panel: layout.show_member_panel,
                show_space_labels: layout.show_space_labels,
                details_side: layout.details_side,
                direct_messages_first: layout.direct_messages_first,
                room_density: layout.room_density,
                message_density: layout.message_density,
            },
            window: config.window.clone(),
            colors: config.colors.clone(),
            senders: config.senders.clone(),
        }
    }

    /// Replace the look of `config` with this profile's, keeping every other
    /// setting, per-account override and the text scale, Reduce motion and
    /// focus ring width.
    pub fn apply_to(&self, config: &mut Config) {
        let appearance = &mut config.appearance;
        appearance.theme = self.appearance.theme;
        appearance.theme_pack = self.appearance.theme_pack.clone();
        appearance.font_family = self.appearance.font_family.clone();
        appearance.density = self.appearance.density;
        appearance.hover_duration_ms = self.appearance.hover_duration_ms;
        appearance.popup_fade_ms = self.appearance.popup_fade_ms;
        appearance.panel_fade_ms = self.appearance.panel_fade_ms;
        config.typography = self.typography.clone();
        self.geometry.apply_to(&mut config.geometry);
        config.chat = self.chat.clone();
        config.conversation = self.conversation.clone();
        let layout = &mut config.layout;
        layout.navigation = self.layout.navigation;
        layout.room_list_width = self.layout.room_list_width;
        layout.details_width = self.layout.details_width;
        layout.show_member_panel = self.layout.show_member_panel;
        layout.show_space_labels = self.layout.show_space_labels;
        layout.details_side = self.layout.details_side;
        layout.direct_messages_first = self.layout.direct_messages_first;
        layout.room_density = self.layout.room_density;
        layout.message_density = self.layout.message_density;
        config.window = self.window.clone();
        config.colors = self.colors.clone();
        config.senders = self.senders.clone();
    }

    /// Whether `config` already shows this look; the name does not matter.
    pub fn matches(&self, config: &Config) -> bool {
        let mut current = Self::from_config("", config);
        current.name.clone_from(&self.name);
        current == *self
    }

    /// Check the version, the name and every value with the configuration's
    /// own validator, so a profile can never apply a look `config.toml`
    /// would reject.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.profile_version != PROFILE_VERSION {
            return Err(ProfileError::Version);
        }
        validate_name(&self.name)?;
        let mut probe = Config::default();
        self.apply_to(&mut probe);
        probe.validate().map_err(ProfileError::Invalid)
    }

    /// Parse and validate a profile file's text.
    pub fn parse(raw: &str) -> Result<Self, ProfileError> {
        // Read the version first, so a newer format reports its version
        // instead of every key it added.
        let table: toml::Table =
            parse_toml(raw, diagnostics::FileKind::Profile).map_err(ProfileError::Invalid)?;
        match table.get("profile_version") {
            None => return Err(ProfileError::NotAProfile),
            Some(toml::Value::Integer(version)) if *version == i64::from(PROFILE_VERSION) => {}
            Some(toml::Value::Integer(_)) => return Err(ProfileError::Version),
            // A wrong type is reported by the typed parse below.
            Some(_) => {}
        }
        let profile: Self =
            parse_toml(raw, diagnostics::FileKind::Profile).map_err(ProfileError::Invalid)?;
        profile.validate()?;
        Ok(profile)
    }

    /// The profile as sparse TOML: only keys that differ from the built-in
    /// look are written, after a short header comment.
    pub fn to_toml(&self) -> Result<String, ProfileError> {
        self.validate()?;
        let encode = |profile: &Self| {
            toml::Value::try_from(profile)
                .map_err(|error| ProfileError::Invalid(ConfigError::Encode(error)))
        };
        let mut value = encode(self)?;
        let defaults = encode(&Self::named(&self.name))?;
        if let (toml::Value::Table(table), toml::Value::Table(defaults)) = (&mut value, &defaults)
        {
            remove_defaults(table, defaults, true);
        }
        let body = toml::to_string_pretty(&value)
            .map_err(|error| ProfileError::Invalid(ConfigError::Encode(error)))?;
        let raw = format!("{HEADER}{body}");
        if raw.len() as u64 > MAX_PROFILE_BYTES {
            return Err(ProfileError::TooLarge);
        }
        Ok(raw)
    }
}

/// Whether two configurations look the same: the same profile look and the
/// same per-account theme and colors. Notifications, shortcuts, call modes
/// and other behavior do not matter here.
pub fn same_look(left: &Config, right: &Config) -> bool {
    let accounts = |config: &Config| {
        config
            .account_overrides
            .iter()
            .filter(|(_, overrides)| overrides.theme.is_some() || !overrides.colors.is_empty())
            .map(|(id, overrides)| (id.clone(), overrides.theme, overrides.colors.clone()))
            .collect::<Vec<_>>()
    };
    StyleProfile::from_config("", left) == StyleProfile::from_config("", right)
        && accounts(left) == accounts(right)
}

/// Remove every key whose value equals the default, and tables left empty.
/// The top level always keeps `profile_version` and `name`.
fn remove_defaults(table: &mut toml::Table, defaults: &toml::Table, top: bool) {
    table.retain(|key, value| {
        if top && (key == "profile_version" || key == "name") {
            return true;
        }
        match (value, defaults.get(key)) {
            (toml::Value::Table(inner), Some(toml::Value::Table(default))) => {
                remove_defaults(inner, default, false);
                !inner.is_empty()
            }
            (value, Some(default)) => &*value != default,
            (_, None) => true,
        }
    });
}

/// The schema paths a profile accepts, as dotted keys; `<token>` stands for
/// any registry color token. Used for sanitized diagnostics and docs.
pub fn schema_paths() -> Vec<&'static str> {
    let mut paths = vec!["profile_version", "name"];
    paths.extend(
        crate::registry::FIELDS
            .iter()
            .map(|field| field.key)
            .filter(|key| profile_key(key)),
    );
    paths
}

/// Whether a `config.toml` key is part of a profile.
fn profile_key(key: &str) -> bool {
    const EXCLUDED: &[&str] = &[
        "appearance.scale_percent",
        "appearance.reduce_motion",
        "geometry.focus_width",
        "layout.group_direct_messages",
        "layout.sort_unread_first",
    ];
    let table = key.split('.').next().unwrap_or_default();
    matches!(
        table,
        "appearance"
            | "typography"
            | "geometry"
            | "chat"
            | "conversation"
            | "layout"
            | "window"
            | "colors"
            | "senders"
    ) && !EXCLUDED.contains(&key)
}

/// Trim and check a profile name.
pub fn validate_name(name: &str) -> Result<String, ProfileError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0
        || length > MAX_PROFILE_NAME_CHARS
        || name.chars().any(|character| {
            // Controls, line separators and bidirectional formatting could
            // disguise the name where it is shown.
            character.is_control()
                || matches!(
                    character,
                    '\u{2028}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
                )
        })
    {
        return Err(ProfileError::Name);
    }
    Ok(name.to_owned())
}

/// A profile's file in the profiles folder, by its validated stem.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileFile(String);

impl ProfileFile {
    /// A stem of ASCII letters, digits, `-` and `_`, at most 64 characters,
    /// not starting with `-` and not a Windows device name: it cannot name
    /// another folder, a hidden file or a device, also when the folder is
    /// shared with Windows.
    pub fn parse(stem: &str) -> Option<Self> {
        let valid = !stem.is_empty()
            && stem.len() <= 64
            && stem
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            && !stem.starts_with('-')
            && !reserved_device_name(stem);
        valid.then(|| Self(stem.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn file_name(&self) -> String {
        format!("{}.{EXTENSION}", self.0)
    }
}

/// The file stem for a new profile name: lowercase ASCII letters and digits,
/// other characters folded into single hyphens.
fn stem_for(name: &str) -> String {
    let mut stem = String::new();
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            stem.push(character.to_ascii_lowercase());
        } else if !stem.is_empty() && !stem.ends_with('-') {
            stem.push('-');
        }
        if stem.len() >= MAX_STEM_CHARS {
            break;
        }
    }
    let stem = stem.trim_end_matches('-');
    if stem.is_empty() {
        "profile".to_owned()
    } else if reserved_device_name(stem) {
        format!("{stem}-profile")
    } else {
        stem.to_owned()
    }
}

/// Windows device names: `CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9` and
/// `LPT0`-`LPT9`, in any case. Windows opens the device for such a name even
/// with an extension, so none of them names a profile file.
fn reserved_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit())
}

/// Whether Windows would open a device for this path's file name: the part
/// before the first dot is a device name. Always false elsewhere.
fn device_path(path: &Path) -> bool {
    cfg!(windows)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split('.').next())
            .is_some_and(reserved_device_name)
}

/// A file name suggested for exporting `name`.
pub fn export_file_name(name: &str) -> String {
    format!("{}.{EXTENSION}", stem_for(name))
}

/// One listed profile file and what reading it gave.
#[derive(Debug)]
pub struct ProfileEntry {
    pub file: ProfileFile,
    pub profile: Result<StyleProfile, ProfileError>,
}

/// The profiles folder as listed.
#[derive(Debug, Default)]
pub struct ProfileListing {
    pub entries: Vec<ProfileEntry>,
    /// More entries existed than were listed or examined.
    pub truncated: bool,
}

/// The fixed profiles folder beside `config.toml`.
///
/// Trust boundary: the folder that holds `config.toml` and its ancestors are
/// trusted, as they are for `config.toml` itself. The `style-profiles` folder
/// is opened once per operation as an anchor and must itself be
/// a directory, not a link or reparse point. Every profile file is then
/// checked, read, written, renamed and removed through that anchor, so a
/// folder replaced by a link or by another directory after it was opened is
/// never read, written or deleted through.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn for_config(store: &ConfigStore) -> Result<Self, ProfileError> {
        let parent = store.path().parent().ok_or(ProfileError::Folder)?;
        Ok(Self::at(parent.join(PROFILES_DIR)))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// List at most [`MAX_PROFILES`] profile files, examining at most a fixed
    /// number of entries. Links, folders, sockets, FIFOs, devices, hidden
    /// files and other names are skipped and never followed. A missing
    /// folder lists nothing.
    pub fn list(&self) -> Result<ProfileListing, ProfileError> {
        let folder = match Folder::open(&self.dir) {
            Err(ProfileError::Missing) => return Ok(ProfileListing::default()),
            Err(error) => return Err(error),
            Ok(folder) => folder,
        };
        // Names come from listing the path, so the path must name the anchored
        // folder before and after; every file is checked and read through it.
        if !folder.is_at(&self.dir) {
            return Err(ProfileError::Folder);
        }
        let mut listing = ProfileListing::default();
        let entries = fs::read_dir(&self.dir).map_err(|_| ProfileError::Folder)?;
        for (examined, entry) in entries.enumerate() {
            if examined >= MAX_SCANNED_ENTRIES {
                listing.truncated = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let Some(file) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".toml"))
                .and_then(ProfileFile::parse)
            else {
                continue;
            };
            if folder.regular(&file.file_name()).is_err() {
                continue;
            }
            if listing.entries.len() >= MAX_PROFILES {
                listing.truncated = true;
                break;
            }
            let profile = folder
                .read(&file.file_name())
                .and_then(|raw| StyleProfile::parse(&raw));
            listing.entries.push(ProfileEntry { file, profile });
        }
        if !folder.is_at(&self.dir) {
            return Err(ProfileError::Folder);
        }
        listing.entries.sort_by(|left, right| left.file.cmp(&right.file));
        Ok(listing)
    }

    /// Read and validate one listed profile through the anchored folder.
    pub fn read(&self, file: &ProfileFile) -> Result<StyleProfile, ProfileError> {
        let folder = Folder::open(&self.dir)?;
        StyleProfile::parse(&folder.read(&file.file_name())?)
    }

    /// Save a new profile under a file name of its own, never replacing an
    /// existing file. Fails when its name is already used.
    pub fn create(&self, profile: &StyleProfile) -> Result<ProfileFile, ProfileError> {
        let raw = profile.to_toml()?;
        let listing = self.list()?;
        if listing.entries.len() >= MAX_PROFILES || listing.truncated {
            return Err(ProfileError::TooMany);
        }
        if name_used(&listing, &profile.name, None) {
            return Err(ProfileError::NameTaken);
        }
        let folder = self.ensure_folder()?;
        let base = stem_for(&profile.name);
        for attempt in 1..=MAX_PROFILES + 1 {
            let stem = if attempt == 1 {
                base.clone()
            } else {
                format!("{base}-{attempt}")
            };
            let Some(file) = ProfileFile::parse(&stem) else {
                return Err(ProfileError::Write);
            };
            match folder.write(&file.file_name(), &raw, false) {
                Ok(()) => return Ok(file),
                Err(WriteError::Exists) => continue,
                Err(WriteError::Failed) => return Err(ProfileError::Write),
            }
        }
        Err(ProfileError::TooMany)
    }

    /// Replace an existing profile file with `profile`, as confirmed by the
    /// person. The file must still be a regular file in the anchored folder.
    pub fn replace(&self, file: &ProfileFile, profile: &StyleProfile) -> Result<(), ProfileError> {
        let raw = profile.to_toml()?;
        let folder = Folder::open(&self.dir)?;
        let listing = self.list()?;
        if name_used(&listing, &profile.name, Some(file)) {
            return Err(ProfileError::NameTaken);
        }
        folder
            .regular(&file.file_name())
            .map_err(|_| ProfileError::Changed)?;
        folder
            .write(&file.file_name(), &raw, true)
            .map_err(|_| ProfileError::Write)
    }

    /// Rename a profile. Its file keeps its name, so the change is one
    /// atomic replacement.
    pub fn rename(&self, file: &ProfileFile, name: &str) -> Result<StyleProfile, ProfileError> {
        let name = validate_name(name)?;
        let mut profile = self.read(file)?;
        profile.name = name;
        self.replace(file, &profile)?;
        Ok(profile)
    }

    /// Save a copy of a profile under a new name.
    pub fn duplicate(&self, file: &ProfileFile, name: &str) -> Result<ProfileFile, ProfileError> {
        let name = validate_name(name)?;
        let mut profile = self.read(file)?;
        profile.name = name;
        self.create(&profile)
    }

    /// Delete one profile file: only a regular file in the anchored folder.
    pub fn delete(&self, file: &ProfileFile) -> Result<(), ProfileError> {
        let folder = Folder::open(&self.dir).map_err(|_| ProfileError::Changed)?;
        folder.remove(&file.file_name())
    }

    /// Create the folder if needed, then anchor it. A link in its place is
    /// refused.
    fn ensure_folder(&self) -> Result<Folder, ProfileError> {
        fs::create_dir_all(&self.dir).map_err(|_| ProfileError::Folder)?;
        Folder::open(&self.dir)
    }
}

fn name_used(listing: &ProfileListing, name: &str, except: Option<&ProfileFile>) -> bool {
    let name = name.trim().to_lowercase();
    listing.entries.iter().any(|entry| {
        Some(&entry.file) != except
            && entry
                .profile
                .as_ref()
                .is_ok_and(|profile| profile.name.trim().to_lowercase() == name)
    })
}

/// Read a profile the person chose to import: a regular file of at most
/// [`MAX_PROFILE_BYTES`], opened without following a link at its final
/// component. On Windows any reparse point is refused, which includes files
/// in a cloud-synced folder that are kept as placeholders or synced links.
pub fn import_profile(path: &Path) -> Result<StyleProfile, ProfileError> {
    if device_path(path) {
        return Err(ProfileError::NotRegularFile);
    }
    StyleProfile::parse(&read_path(path)?)
}

/// Write `profile` to the file the person chose to export to. An existing
/// target must be a regular file (the system dialog confirmed replacing it);
/// a link, folder or device is refused. The write goes to a temporary file
/// in the same folder that then replaces the target's entry, so a link put
/// there meanwhile is replaced, never followed.
pub fn export_profile(profile: &StyleProfile, target: &Path) -> Result<(), ProfileError> {
    let raw = profile.to_toml()?;
    if device_path(target) {
        return Err(ProfileError::Target);
    }
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(ProfileError::Target)?;
    if !fs::metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(ProfileError::Target);
    }
    let replace = match fs::symlink_metadata(target) {
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Ok(metadata) if metadata.is_file() => true,
        _ => return Err(ProfileError::Target),
    };
    match write_atomic(parent, target, &raw, replace) {
        Ok(()) => Ok(()),
        // Created by something else since the check: do not replace it.
        Err(WriteError::Exists) => Err(ProfileError::Target),
        Err(WriteError::Failed) => Err(ProfileError::Write),
    }
}

/// The path is a regular file itself, not a link or other reparse point.
fn regular_file(path: &Path) -> Result<fs::Metadata, ProfileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !is_reparse_point(&metadata) => Ok(metadata),
        Ok(_) => Err(ProfileError::NotRegularFile),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(ProfileError::Missing),
        Err(_) => Err(ProfileError::Unreadable),
    }
}

/// Read a bounded regular file by path: checked first, then opened without
/// following a link and without blocking ([`open_no_follow`]); the opened
/// handle must be a regular file and, on Unix, the very file that was checked.
fn read_path(path: &Path) -> Result<String, ProfileError> {
    let checked = regular_file(path)?;
    if checked.len() > MAX_PROFILE_BYTES {
        return Err(ProfileError::TooLarge);
    }
    let file = open_no_follow(path)?;
    let opened = file.metadata().map_err(|_| ProfileError::Unreadable)?;
    if !same_file(&checked, &opened) {
        return Err(ProfileError::NotRegularFile);
    }
    read_opened(file)
}

/// Read an opened file: it must be a regular file that is not a reparse
/// point, of at most [`MAX_PROFILE_BYTES`], in UTF-8.
fn read_opened(file: fs::File) -> Result<String, ProfileError> {
    let opened = file.metadata().map_err(|_| ProfileError::Unreadable)?;
    if !opened.is_file() || is_reparse_point(&opened) {
        return Err(ProfileError::NotRegularFile);
    }
    if opened.len() > MAX_PROFILE_BYTES {
        return Err(ProfileError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProfileError::Unreadable)?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(ProfileError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| ProfileError::NotUtf8)
}

/// Whether two metadata records describe the same file: device and inode.
#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Windows exposes no stable file identity through std; the handle is opened
/// on a reparse point itself and refused unless it is a plain regular file.
#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    true
}

/// Open for reading without following a link at the final path component and
/// without blocking. Unix adds `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY`: a link
/// fails to open, and a FIFO or device put in place of a checked file opens at
/// once and is then refused instead of blocking the reader. Windows opens a
/// link, junction or other reparse point itself, which is then refused. A
/// failure is classified with fixed text only.
fn open_no_follow(path: &Path) -> Result<fs::File, ProfileError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    options.open(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            return ProfileError::Missing;
        }
        match regular_file(path) {
            Ok(_) => ProfileError::Unreadable,
            Err(ProfileError::Missing) => ProfileError::Missing,
            Err(_) => ProfileError::NotRegularFile,
        }
    })
}

#[cfg(unix)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
}

#[cfg(windows)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(win32::FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn no_follow(_: &mut fs::OpenOptions) {}

/// Win32 values used here; std exposes the calls but not the constants.
#[cfg(windows)]
mod win32 {
    pub const FILE_LIST_DIRECTORY: u32 = 0x0001;
    pub const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    pub const FILE_SHARE_READ: u32 = 0x0001;
    pub const FILE_SHARE_WRITE: u32 = 0x0002;
    pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
}

/// A Windows link, junction or other reparse point; never true elsewhere,
/// where `symlink_metadata` and `O_NOFOLLOW` already distinguish links.
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & win32::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

/// The profiles folder, opened for one operation as an anchor. On Unix it is
/// a directory descriptor opened with `O_DIRECTORY | O_NOFOLLOW`; files are
/// addressed relative to it with `openat`, `fstatat`, `renameat` and
/// `unlinkat`, so a path that comes to name another folder is never used.
#[cfg(unix)]
struct Folder {
    dir: fs::File,
}

#[cfg(unix)]
impl Folder {
    /// Open the folder itself: a link in its place, or anything but a
    /// directory, is refused.
    fn open(path: &Path) -> Result<Self, ProfileError> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| match error.kind() {
                ErrorKind::NotFound => ProfileError::Missing,
                _ => ProfileError::Folder,
            })?;
        if !dir.metadata().is_ok_and(|metadata| metadata.is_dir()) {
            return Err(ProfileError::Folder);
        }
        Ok(Self { dir })
    }

    fn fd(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.dir.as_raw_fd()
    }

    /// Whether `path` still names this folder: a directory, not a link, with
    /// the same device and inode. Only listing names by path relies on it.
    fn is_at(&self, path: &Path) -> bool {
        match (fs::symlink_metadata(path), self.dir.metadata()) {
            (Ok(named), Ok(anchored)) => named.is_dir() && same_file(&named, &anchored),
            _ => false,
        }
    }

    /// The entry is a regular file in this folder; a link is not followed.
    fn regular(&self, name: &str) -> Result<(), ProfileError> {
        let entry = c_name(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `entry` is NUL-terminated, `stat` is writable storage of
        // the type fstatat fills, and `self.dir` is an open directory.
        let result = unsafe {
            libc::fstatat(
                self.fd(),
                entry.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(match std::io::Error::last_os_error().kind() {
                ErrorKind::NotFound => ProfileError::Missing,
                _ => ProfileError::Unreadable,
            });
        }
        // SAFETY: fstatat succeeded, so it filled `stat`.
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
            Ok(())
        } else {
            Err(ProfileError::NotRegularFile)
        }
    }

    /// Read a bounded regular file in this folder without following a link
    /// or blocking on a FIFO or device.
    fn read(&self, name: &str) -> Result<String, ProfileError> {
        use std::os::fd::FromRawFd;
        let entry = c_name(name)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_NOCTTY
            | libc::O_CLOEXEC;
        // SAFETY: as in `regular`; these flags take no mode argument.
        let fd = unsafe { libc::openat(self.fd(), entry.as_ptr(), flags) };
        if fd < 0 {
            if std::io::Error::last_os_error().kind() == ErrorKind::NotFound {
                return Err(ProfileError::Missing);
            }
            return Err(match self.regular(name) {
                Ok(()) => ProfileError::Unreadable,
                Err(ProfileError::Missing) => ProfileError::Missing,
                Err(_) => ProfileError::NotRegularFile,
            });
        }
        // SAFETY: `fd` is a new descriptor owned from here on.
        read_opened(unsafe { fs::File::from_raw_fd(fd) })
    }

    /// Remove a regular file's entry from this folder. `unlinkat` never
    /// follows a link: one put there meanwhile is removed itself.
    fn remove(&self, name: &str) -> Result<(), ProfileError> {
        self.regular(name).map_err(|_| ProfileError::Changed)?;
        let entry = c_name(name)?;
        // SAFETY: as in `regular`.
        if unsafe { libc::unlinkat(self.fd(), entry.as_ptr(), 0) } != 0 {
            return Err(ProfileError::Write);
        }
        Ok(())
    }

    /// Write a new file in this folder, then move it to `name`: replacing the
    /// entry (`replace`) or only when nothing has that name yet.
    fn write(&self, name: &str, raw: &str, replace: bool) -> Result<(), WriteError> {
        use std::os::fd::FromRawFd;
        let target = std::ffi::CString::new(name).map_err(|_| WriteError::Failed)?;
        let staging = std::ffi::CString::new(staging_name()).map_err(|_| WriteError::Failed)?;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: as in `regular`; O_CREAT takes the mode argument.
        let fd = unsafe { libc::openat(self.fd(), staging.as_ptr(), flags, 0o600 as libc::c_uint) };
        if fd < 0 {
            return Err(WriteError::Failed);
        }
        // SAFETY: `fd` is a new descriptor owned from here on.
        let mut file = unsafe { fs::File::from_raw_fd(fd) };
        let written = file
            .write_all(raw.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| WriteError::Failed);
        drop(file);
        let moved = written.and_then(|()| {
            if replace {
                self.rename(&staging, &target)
            } else {
                self.rename_exclusive(&staging, &target)
            }
        });
        // SAFETY: as in `regular`.
        unsafe {
            if moved.is_err() {
                libc::unlinkat(self.fd(), staging.as_ptr(), 0);
            } else {
                // Make the new entry durable; a failure here does not undo it.
                libc::fsync(self.fd());
            }
        }
        moved
    }

    /// `renameat` replaces this folder's entry; a link there is replaced,
    /// never followed.
    fn rename(&self, from: &std::ffi::CStr, to: &std::ffi::CStr) -> Result<(), WriteError> {
        // SAFETY: as in `regular`.
        let result = unsafe { libc::renameat(self.fd(), from.as_ptr(), self.fd(), to.as_ptr()) };
        if result == 0 {
            Ok(())
        } else {
            Err(WriteError::Failed)
        }
    }

    /// Move into place only when nothing has that name: `renameat2` with
    /// `RENAME_NOREPLACE` on Linux, `renameatx_np` with `RENAME_EXCL` on
    /// macOS, else (or on a file system without them) a hard link, which
    /// fails when the name exists, and removal of the staging name.
    fn rename_exclusive(
        &self,
        from: &std::ffi::CStr,
        to: &std::ffi::CStr,
    ) -> Result<(), WriteError> {
        let unsupported = |error: &std::io::Error| {
            matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
            )
        };
        #[cfg(target_os = "linux")]
        {
            // SAFETY: as in `regular`.
            let result = unsafe {
                libc::renameat2(
                    self.fd(),
                    from.as_ptr(),
                    self.fd(),
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::AlreadyExists {
                return Err(WriteError::Exists);
            }
            if !unsupported(&error) {
                return Err(WriteError::Failed);
            }
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: as in `regular`.
            let result = unsafe {
                libc::renameatx_np(
                    self.fd(),
                    from.as_ptr(),
                    self.fd(),
                    to.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::AlreadyExists {
                return Err(WriteError::Exists);
            }
            if !unsupported(&error) {
                return Err(WriteError::Failed);
            }
        }
        // SAFETY: as in `regular`.
        let linked = unsafe { libc::linkat(self.fd(), from.as_ptr(), self.fd(), to.as_ptr(), 0) };
        if linked != 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.kind() == ErrorKind::AlreadyExists {
                WriteError::Exists
            } else {
                WriteError::Failed
            });
        }
        // SAFETY: as in `regular`.
        unsafe { libc::unlinkat(self.fd(), from.as_ptr(), 0) };
        Ok(())
    }
}

/// A name for a staging file: hidden (never listed as a profile), unique to
/// this process and call, and replaced or removed before the operation ends.
#[cfg(unix)]
fn staging_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!(
        ".style-profile-{}-{nanos}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(unix)]
fn c_name(name: &str) -> Result<std::ffi::CString, ProfileError> {
    std::ffi::CString::new(name).map_err(|_| ProfileError::Unreadable)
}

/// The profiles folder, opened for one operation as an anchor. On Windows it
/// is a handle on the folder itself (not a reparse point) opened without
/// delete sharing: while it is open the folder cannot be renamed, deleted or
/// replaced, so paths inside it keep naming this folder. Files inside are
/// then opened without following reparse points, and created, replaced and
/// removed by entry, which never follows a link at the final component.
#[cfg(windows)]
struct Folder {
    _handle: fs::File,
    path: PathBuf,
}

#[cfg(windows)]
impl Folder {
    fn open(path: &Path) -> Result<Self, ProfileError> {
        use std::os::windows::fs::OpenOptionsExt;
        let handle = fs::OpenOptions::new()
            .access_mode(win32::FILE_LIST_DIRECTORY | win32::FILE_READ_ATTRIBUTES)
            .share_mode(win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE)
            .custom_flags(
                win32::FILE_FLAG_BACKUP_SEMANTICS | win32::FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(path)
            .map_err(|error| match error.kind() {
                ErrorKind::NotFound => ProfileError::Missing,
                _ => ProfileError::Folder,
            })?;
        let metadata = handle.metadata().map_err(|_| ProfileError::Folder)?;
        if !metadata.is_dir() || is_reparse_point(&metadata) {
            return Err(ProfileError::Folder);
        }
        Ok(Self {
            _handle: handle,
            path: path.to_owned(),
        })
    }

    /// The handle keeps the folder in place while it is open.
    fn is_at(&self, _: &Path) -> bool {
        true
    }

    fn regular(&self, name: &str) -> Result<(), ProfileError> {
        regular_file(&self.path.join(name)).map(|_| ())
    }

    fn read(&self, name: &str) -> Result<String, ProfileError> {
        read_path(&self.path.join(name))
    }

    fn remove(&self, name: &str) -> Result<(), ProfileError> {
        self.regular(name).map_err(|_| ProfileError::Changed)?;
        fs::remove_file(self.path.join(name)).map_err(|_| ProfileError::Write)
    }

    fn write(&self, name: &str, raw: &str, replace: bool) -> Result<(), WriteError> {
        write_atomic(&self.path, &self.path.join(name), raw, replace)
    }
}

/// Other targets are not shipped: the folder is checked to be a directory
/// that is not a link, and files are addressed by path.
#[cfg(not(any(unix, windows)))]
struct Folder {
    path: PathBuf,
}

#[cfg(not(any(unix, windows)))]
impl Folder {
    fn open(path: &Path) -> Result<Self, ProfileError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(Self {
                path: path.to_owned(),
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(ProfileError::Missing),
            _ => Err(ProfileError::Folder),
        }
    }

    fn is_at(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    fn regular(&self, name: &str) -> Result<(), ProfileError> {
        regular_file(&self.path.join(name)).map(|_| ())
    }

    fn read(&self, name: &str) -> Result<String, ProfileError> {
        read_path(&self.path.join(name))
    }

    fn remove(&self, name: &str) -> Result<(), ProfileError> {
        self.regular(name).map_err(|_| ProfileError::Changed)?;
        fs::remove_file(self.path.join(name)).map_err(|_| ProfileError::Write)
    }

    fn write(&self, name: &str, raw: &str, replace: bool) -> Result<(), WriteError> {
        write_atomic(&self.path, &self.path.join(name), raw, replace)
    }
}

enum WriteError {
    Exists,
    Failed,
}

/// Write through a temporary file in `dir`, then move it into place: the
/// destination is either the previous file or the complete new one. Without
/// `replace`, an existing destination is never overwritten. Used for exports
/// and for profiles where no anchored descriptor is available.
fn write_atomic(dir: &Path, target: &Path, raw: &str, replace: bool) -> Result<(), WriteError> {
    let mut staging = tempfile::NamedTempFile::new_in(dir).map_err(|_| WriteError::Failed)?;
    staging
        .write_all(raw.as_bytes())
        .and_then(|()| staging.as_file().sync_all())
        .map_err(|_| WriteError::Failed)?;
    let result = if replace {
        staging.persist(target).map(|_| ())
    } else {
        staging.persist_noclobber(target).map(|_| ())
    };
    result.map_err(|error| match error.error.kind() {
        ErrorKind::AlreadyExists => WriteError::Exists,
        _ => WriteError::Failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled() -> Config {
        let mut config = Config::default();
        config.appearance.theme = ThemeMode::Light;
        config.appearance.theme_pack = Some("paper".into());
        config.appearance.scale_percent = 150;
        config.appearance.reduce_motion = true;
        config.typography.message_size = 16;
        config.geometry.corner_radius = 4;
        config.geometry.focus_width = 5;
        config.layout.details_side = DetailsSide::Left;
        config.layout.sort_unread_first = true;
        config.colors.insert("link".into(), "#335F86".into());
        config.senders.colors = Some(vec!["#123456".into()]);
        config.notifications.highlight_words = vec!["private".into()];
        config.shortcuts.clear();
        config.media.microphone_device = Some("device".into());
        config
            .account_overrides
            .entry("account".into())
            .or_default()
            .colors
            .insert("canvas".into(), "#101010".into());
        config
    }

    #[test]
    fn a_profile_holds_the_look_only_and_round_trips_sparse_toml() {
        let config = styled();
        let profile = StyleProfile::from_config("  Reading room ", &config);
        assert_eq!(profile.name, "Reading room");
        let raw = profile.to_toml().unwrap();
        assert!(raw.starts_with("# Morpheus style profile."));
        for absent in [
            "scale_percent",
            "reduce_motion",
            "focus_width",
            "sort_unread_first",
            "group_direct_messages",
            "highlight",
            "private",
            "shortcuts",
            "media",
            "device",
            "account",
            "notifications",
            "schema_version",
        ] {
            assert!(!raw.contains(absent), "{absent} leaked into a profile");
        }
        // Only keys that differ from the built-in look are written.
        assert!(raw.contains("message_size = 16"));
        assert!(!raw.contains("message_gap"));
        assert_eq!(StyleProfile::parse(&raw).unwrap(), profile);
        assert!(profile.matches(&config));
        assert!(!profile.matches(&Config::default()));
    }

    #[test]
    fn applying_replaces_the_look_and_keeps_everything_else() {
        let source = styled();
        let profile = StyleProfile::from_config("Look", &source);
        let mut target = Config::default();
        target.appearance.scale_percent = 125;
        target.geometry.focus_width = 3;
        target.layout.group_direct_messages = false;
        target.notifications.sounds = false;
        target.colors.insert("canvas".into(), "#000000".into());
        profile.apply_to(&mut target);
        assert_eq!(target.typography.message_size, 16);
        assert_eq!(target.layout.details_side, DetailsSide::Left);
        // The profile's colors replace the global table entirely.
        assert_eq!(target.colors, source.colors);
        assert_eq!(target.appearance.scale_percent, 125);
        assert!(!target.appearance.reduce_motion);
        assert_eq!(target.geometry.focus_width, 3);
        assert!(!target.layout.group_direct_messages);
        assert!(!target.layout.sort_unread_first);
        assert!(!target.notifications.sounds);
        assert!(target.account_overrides.is_empty());
        target.validate().unwrap();
        // An empty profile is the built-in look.
        let mut reset = source.clone();
        StyleProfile::parse("profile_version = 1\nname = \"Plain\"\n")
            .unwrap()
            .apply_to(&mut reset);
        assert!(StyleProfile::named("Plain").matches(&reset));
    }

    /// The documented schema (the registry keys a profile accepts) is exactly
    /// what the profile type reads and writes, so neither can drift.
    #[test]
    fn the_documented_profile_schema_matches_the_profile_type() {
        fn leaves(value: &toml::Value, path: &str, found: &mut std::collections::BTreeSet<String>) {
            match value {
                toml::Value::Table(table) => {
                    for (key, value) in table {
                        let key = if path == "colors" { "<token>" } else { key.as_str() };
                        let child = if path.is_empty() {
                            key.to_owned()
                        } else {
                            format!("{path}.{key}")
                        };
                        leaves(value, &child, found);
                    }
                }
                _ => {
                    found.insert(path.to_owned());
                }
            }
        }
        let mut config = styled();
        config.typography.message_font_family = Some("serif".into());
        config.typography.sender_weight = Some(600);
        config.typography.sender_size = Some(15);
        config.typography.timestamp_size = Some(11);
        config.typography.composer_size = Some(16);
        config.geometry.control_radius = Some(4);
        config.geometry.input_radius = Some(5);
        config.geometry.composer_radius = Some(6);
        config.geometry.avatar_size = Some(30);
        config.geometry.avatar_roundness = Some(50);
        config.layout.room_density = Some(Density::Compact);
        config.layout.message_density = Some(Density::Spacious);
        let profile = StyleProfile::from_config("Everything", &config);
        let mut found = std::collections::BTreeSet::new();
        leaves(&toml::Value::try_from(&profile).unwrap(), "", &mut found);
        let documented: std::collections::BTreeSet<String> =
            schema_paths().into_iter().map(str::to_owned).collect();
        assert_eq!(found, documented);
    }

    #[test]
    fn unknown_keys_versions_and_values_are_rejected_without_repeating_them() {
        let cases = [
            ("name = \"A\"\n", "not-a-profile"),
            ("profile_version = 2\nname = \"A\"\n", "profile-version"),
            ("profile_version = 1\nname = \"A\"\nschema_version = 1\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[notifications]\nenabled = false\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[shortcuts]\nsearch = \"Mod+F\"\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[appearance]\nscale_percent = 150\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[geometry]\nfocus_width = 3\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[typography]\nmessage_size = 999\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[colors]\nlink = \"#zzz-secret\"\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[colors]\nsecret_token = \"#123456\"\n", "invalid-profile"),
            ("profile_version = 1\nname = \"A\"\n[appearance]\ntheme_pack = \"../secret\"\n", "invalid-profile"),
            ("profile_version = 1\nname = \"\"\n", "profile-name"),
            ("profile_version = 1\nname = \"bad\\nname\"\n", "profile-name"),
            ("profile_version = 1\n[account_overrides.secret]\n", "invalid-profile"),
        ];
        for (raw, code) in cases {
            let error = StyleProfile::parse(raw).expect_err(raw);
            assert_eq!(error.code(), code, "{raw}");
            let text = error.to_string();
            assert!(!text.contains("secret") && !text.contains("999"), "{text}");
        }
    }

    #[test]
    fn looks_compare_appearance_and_account_colors_only() {
        let base = styled();
        let mut behavior = base.clone();
        behavior.notifications.sounds = false;
        behavior.layout.sort_unread_first = !base.layout.sort_unread_first;
        behavior
            .account_overrides
            .entry("account".into())
            .or_default()
            .local_voice_rooms
            .push("!room:example.org".into());
        assert!(same_look(&base, &behavior));
        let mut colors = base.clone();
        colors
            .account_overrides
            .get_mut("account")
            .unwrap()
            .colors
            .insert("link".into(), "#ABCDEF".into());
        assert!(!same_look(&base, &colors));
        let mut size = base.clone();
        size.typography.message_size += 1;
        assert!(!same_look(&base, &size));
    }

    #[test]
    fn names_and_file_stems_are_bounded_and_safe() {
        for disguised in ["a\u{202E}b", "a\u{2066}b", "a\u{200F}b", "a\u{2028}b"] {
            assert!(validate_name(disguised).is_err(), "{disguised:?}");
        }
        assert!(validate_name(&"x".repeat(MAX_PROFILE_NAME_CHARS)).is_ok());
        assert!(validate_name(&"x".repeat(MAX_PROFILE_NAME_CHARS + 1)).is_err());
        assert_eq!(validate_name("  Café ✨ ").unwrap(), "Café ✨");
        assert_eq!(stem_for("Café au lait!"), "caf-au-lait");
        assert_eq!(stem_for("../../etc/passwd"), "etc-passwd");
        assert_eq!(stem_for("✨"), "profile");
        assert!(stem_for(&"a".repeat(200)).len() <= MAX_STEM_CHARS);
        assert_eq!(export_file_name("Reading room"), "reading-room.toml");
        for stem in ["", "..", "a/b", "a\\b", ".hidden", "-x", "a.b", &"a".repeat(65)] {
            assert!(ProfileFile::parse(stem).is_none(), "{stem}");
        }
        assert!(ProfileFile::parse("reading-room_2").is_some());
        // Windows device names never name a profile file, in any case.
        for device in ["con", "CON", "Prn", "aux", "NUL", "com0", "COM9", "lpt1", "Lpt9"] {
            assert!(ProfileFile::parse(device).is_none(), "{device}");
        }
        for allowed in ["com10", "console", "con-2", "nul_profile", "lpt"] {
            assert!(ProfileFile::parse(allowed).is_some(), "{allowed}");
        }
        assert_eq!(stem_for("Con"), "con-profile");
        assert_eq!(export_file_name("AUX"), "aux-profile.toml");
        assert!(ProfileFile::parse(&stem_for("com1")).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn special_files_in_place_of_a_profile_are_refused() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("style-profiles");
        fs::create_dir_all(&dir).unwrap();
        // A short name keeps the socket path within the platform limit.
        let socket = dir.join("s.toml");
        let _listener = UnixListener::bind(&socket).unwrap();
        let store = ProfileStore::at(&dir);
        assert!(store.list().unwrap().entries.is_empty());
        let file = ProfileFile::parse("s").unwrap();
        assert!(matches!(store.read(&file), Err(ProfileError::NotRegularFile)));
        assert!(matches!(store.delete(&file), Err(ProfileError::Changed)));
        assert!(matches!(import_profile(&socket), Err(ProfileError::NotRegularFile)));
        assert!(matches!(
            export_profile(&StyleProfile::named("Export"), &socket),
            Err(ProfileError::Target)
        ));
        assert!(socket.exists());
        // The no-follow open itself refuses a link at the final component.
        let target = dir.join("target.toml");
        fs::write(&target, "profile_version = 1\nname = \"T\"\n").unwrap();
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_no_follow(&link).is_err());
        assert!(open_no_follow(&target).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_profiles_folder_replaced_by_a_link_is_never_followed() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("style-profiles");
        let store = ProfileStore::at(&dir);
        let kept = StyleProfile::named("Kept");
        let file = store.create(&kept).unwrap();
        let elsewhere = directory.path().join("elsewhere");
        fs::rename(&dir, &elsewhere).unwrap();
        symlink(&elsewhere, &dir).unwrap();
        assert!(matches!(store.list(), Err(ProfileError::Folder)));
        assert!(matches!(store.read(&file), Err(ProfileError::Folder)));
        assert!(matches!(
            store.replace(&file, &StyleProfile::named("Other")),
            Err(ProfileError::Folder)
        ));
        assert!(matches!(store.delete(&file), Err(ProfileError::Changed)));
        assert!(matches!(store.create(&StyleProfile::named("New")), Err(ProfileError::Folder)));
        // The real folder's file is untouched and nothing was added there.
        let moved = elsewhere.join("kept.toml");
        assert_eq!(fs::read_to_string(&moved).unwrap(), kept.to_toml().unwrap());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1);
    }

    /// A folder path swapped for a link to another folder after the anchor
    /// was opened: removal and writes still act on the anchored folder and
    /// never reach the other one.
    #[cfg(unix)]
    #[test]
    fn operations_stay_in_the_anchored_folder_when_its_path_is_swapped() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("style-profiles");
        let store = ProfileStore::at(&dir);
        let file = store.create(&StyleProfile::named("Kept")).unwrap();
        let other = directory.path().join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("kept.toml"), "not yours").unwrap();
        let folder = Folder::open(&dir).unwrap();
        let moved = directory.path().join("moved");
        fs::rename(&dir, &moved).unwrap();
        symlink(&other, &dir).unwrap();
        assert!(!folder.is_at(&dir), "the path no longer names the anchor");
        let raw = StyleProfile::named("Written").to_toml().unwrap();
        assert!(folder.write("written.toml", &raw, false).is_ok());
        assert!(folder.read(&file.file_name()).is_ok());
        folder.remove(&file.file_name()).unwrap();
        assert_eq!(fs::read_to_string(other.join("kept.toml")).unwrap(), "not yours");
        assert!(!other.join("written.toml").exists());
        assert!(moved.join("written.toml").exists());
        assert!(!moved.join("kept.toml").exists());
        // A link put in place of a file is refused, never followed.
        symlink(other.join("kept.toml"), moved.join("link.toml")).unwrap();
        assert!(matches!(folder.remove("link.toml"), Err(ProfileError::Changed)));
        assert!(matches!(folder.read("link.toml"), Err(ProfileError::NotRegularFile)));
        assert_eq!(fs::read_to_string(other.join("kept.toml")).unwrap(), "not yours");
        // Exclusive creation never replaces an existing name.
        assert!(matches!(
            folder.write("written.toml", &raw, false),
            Err(WriteError::Exists)
        ));
    }

    #[test]
    fn the_store_creates_lists_renames_duplicates_and_deletes_safely() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProfileStore::at(directory.path().join("profiles"));
        assert!(store.list().unwrap().entries.is_empty());
        let first = store
            .create(&StyleProfile::from_config("Reading room", &styled()))
            .unwrap();
        assert_eq!(first.as_str(), "reading-room");
        // Names are unique ignoring case; files never collide.
        assert!(matches!(
            store.create(&StyleProfile::named("reading ROOM")),
            Err(ProfileError::NameTaken)
        ));
        let copy = store.duplicate(&first, "Reading-room").unwrap();
        assert_eq!(copy.as_str(), "reading-room-2");
        let renamed = store.rename(&copy, "Night").unwrap();
        assert_eq!(renamed.name, "Night");
        assert!(matches!(store.rename(&copy, "reading room"), Err(ProfileError::NameTaken)));
        let listing = store.list().unwrap();
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| entry.profile.as_ref().unwrap().name.clone())
            .collect();
        assert_eq!(names, ["Reading room", "Night"]);
        store.delete(&copy).unwrap();
        assert!(matches!(store.delete(&copy), Err(ProfileError::Changed)));
        assert_eq!(store.list().unwrap().entries.len(), 1);
    }

    #[test]
    fn invalid_files_are_listed_as_invalid_and_other_entries_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("profiles");
        fs::create_dir_all(dir.join("nested.toml")).unwrap();
        fs::write(dir.join("broken.toml"), "profile_version = 1\nname = 3\n").unwrap();
        fs::write(dir.join("notes.txt"), "not a profile").unwrap();
        fs::write(dir.join(".hidden.toml"), "profile_version = 1\nname = \"H\"\n").unwrap();
        fs::write(dir.join("large.toml"), "#".repeat(MAX_PROFILE_BYTES as usize + 1)).unwrap();
        let listing = ProfileStore::at(&dir).list().unwrap();
        let shown: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| (entry.file.as_str(), entry.profile.as_ref().err().map(ProfileError::code)))
            .collect();
        assert_eq!(
            shown,
            [("broken", Some("invalid-profile")), ("large", Some("file-too-large"))]
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed_for_profiles_imports_or_exports() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside.toml");
        fs::write(&outside, "profile_version = 1\nname = \"Outside\"\n").unwrap();
        let dir = directory.path().join("profiles");
        fs::create_dir_all(&dir).unwrap();
        symlink(&outside, dir.join("linked.toml")).unwrap();
        let store = ProfileStore::at(&dir);
        assert!(store.list().unwrap().entries.is_empty());
        let linked = ProfileFile::parse("linked").unwrap();
        assert!(matches!(store.read(&linked), Err(ProfileError::NotRegularFile)));
        assert!(matches!(store.delete(&linked), Err(ProfileError::Changed)));
        assert!(outside.exists());
        assert!(matches!(
            import_profile(&dir.join("linked.toml")),
            Err(ProfileError::NotRegularFile)
        ));
        // Exporting onto a link or a folder is refused; the target is untouched.
        let profile = StyleProfile::named("Export");
        assert!(matches!(
            export_profile(&profile, &dir.join("linked.toml")),
            Err(ProfileError::Target)
        ));
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "profile_version = 1\nname = \"Outside\"\n"
        );
        assert!(matches!(export_profile(&profile, &dir), Err(ProfileError::Target)));
        // A linked profiles folder is not used for writes.
        let linked_dir = directory.path().join("linked-profiles");
        symlink(&dir, &linked_dir).unwrap();
        assert!(matches!(
            ProfileStore::at(&linked_dir).create(&profile),
            Err(ProfileError::Folder)
        ));
    }

    #[test]
    fn export_and_import_round_trip_and_replace_only_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("look.toml");
        let profile = StyleProfile::from_config("Look", &styled());
        export_profile(&profile, &target).unwrap();
        assert_eq!(import_profile(&target).unwrap(), profile);
        // The system dialog confirmed replacing an existing regular file.
        let other = StyleProfile::named("Other");
        export_profile(&other, &target).unwrap();
        assert_eq!(import_profile(&target).unwrap(), other);
        assert!(matches!(
            import_profile(&directory.path().join("absent.toml")),
            Err(ProfileError::Missing)
        ));
    }
}
