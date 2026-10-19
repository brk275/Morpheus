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
//! [`ProfileStore`] keeps profiles in one fixed folder beside `config.toml`.
//! It never walks other directories, reads only bounded regular files (never
//! through a symbolic link), names files from a restricted alphabet, and
//! writes through a temporary file in the same folder that replaces the
//! destination atomically. Morpheus rewrites a profile file only when it is
//! saved, renamed or imported; comments in it are not kept then.

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
    /// number of entries. Links, folders, hidden files and other names are
    /// skipped and never followed. A missing folder lists nothing.
    pub fn list(&self) -> Result<ProfileListing, ProfileError> {
        let folder = match self.folder() {
            Err(ProfileError::Missing) => return Ok(ProfileListing::default()),
            Err(error) => return Err(error),
            Ok(folder) => folder,
        };
        let mut listing = ProfileListing::default();
        let entries = fs::read_dir(&self.dir).map_err(|_| ProfileError::Folder)?;
        for (examined, entry) in entries.enumerate() {
            if examined >= MAX_SCANNED_ENTRIES {
                listing.truncated = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            // `DirEntry::file_type` does not follow symbolic links.
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(file) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".toml"))
                .and_then(ProfileFile::parse)
            else {
                continue;
            };
            if listing.entries.len() >= MAX_PROFILES {
                listing.truncated = true;
                break;
            }
            let profile = self.read(&file);
            listing.entries.push(ProfileEntry { file, profile });
        }
        // The entries must have come from the folder that was checked.
        self.same_folder(&folder).map_err(|_| ProfileError::Folder)?;
        listing.entries.sort_by(|left, right| left.file.cmp(&right.file));
        Ok(listing)
    }

    /// Read and validate one listed profile. The folder must be the checked
    /// one before and after the file is opened.
    pub fn read(&self, file: &ProfileFile) -> Result<StyleProfile, ProfileError> {
        let folder = self.folder()?;
        let raw = read_regular(&self.dir.join(file.file_name()), Place::Folder);
        self.same_folder(&folder)?;
        StyleProfile::parse(&raw?)
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
            self.same_folder(&folder)?;
            match write_atomic(&self.dir, &self.dir.join(file.file_name()), &raw, false) {
                Ok(()) => return self.same_folder(&folder).map(|()| file),
                Err(WriteError::Exists) => continue,
                Err(WriteError::Failed) => return Err(ProfileError::Write),
            }
        }
        Err(ProfileError::TooMany)
    }

    /// Replace an existing profile file with `profile`, as confirmed by the
    /// person. The folder must be the checked one, and the file still a
    /// regular file in it.
    pub fn replace(&self, file: &ProfileFile, profile: &StyleProfile) -> Result<(), ProfileError> {
        let raw = profile.to_toml()?;
        let folder = self.folder()?;
        let listing = self.list()?;
        if name_used(&listing, &profile.name, Some(file)) {
            return Err(ProfileError::NameTaken);
        }
        let path = self.dir.join(file.file_name());
        regular_file(&path, Place::Folder).map_err(|_| ProfileError::Changed)?;
        self.same_folder(&folder)?;
        write_atomic(&self.dir, &path, &raw, true).map_err(|_| ProfileError::Write)?;
        // Report a folder replaced during the write instead of claiming it.
        self.same_folder(&folder)
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

    /// Delete one profile file. Only a regular file in the checked folder is
    /// removed; a folder replaced by a link is never followed.
    pub fn delete(&self, file: &ProfileFile) -> Result<(), ProfileError> {
        let folder = self.folder().map_err(|_| ProfileError::Changed)?;
        let path = self.dir.join(file.file_name());
        regular_file(&path, Place::Folder).map_err(|_| ProfileError::Changed)?;
        self.same_folder(&folder)?;
        fs::remove_file(&path).map_err(|_| ProfileError::Write)
    }

    /// Create the folder if needed. A link in its place is refused.
    fn ensure_folder(&self) -> Result<FolderId, ProfileError> {
        fs::create_dir_all(&self.dir).map_err(|_| ProfileError::Folder)?;
        self.folder()
    }

    /// The profiles folder itself, which must be a directory and not a link,
    /// junction or other reparse point. `Missing` when it does not exist.
    fn folder(&self) -> Result<FolderId, ProfileError> {
        match fs::symlink_metadata(&self.dir) {
            Ok(metadata) if metadata.is_dir() && !is_reparse_point(&metadata) => {
                Ok(FolderId::of(&metadata))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Err(ProfileError::Missing),
            _ => Err(ProfileError::Folder),
        }
    }

    /// The folder is still the checked one: not replaced by a link or by
    /// another directory (compared by device and inode on Unix).
    fn same_folder(&self, before: &FolderId) -> Result<(), ProfileError> {
        match self.folder() {
            Ok(now) if now == *before => Ok(()),
            _ => Err(ProfileError::Changed),
        }
    }
}

/// The identity of the profiles folder as checked.
#[derive(Debug, PartialEq, Eq)]
struct FolderId {
    #[cfg(unix)]
    id: (u64, u64),
}

impl FolderId {
    #[cfg(unix)]
    fn of(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            id: (metadata.dev(), metadata.ino()),
        }
    }

    /// Without a stable identity in std, the folder is still checked to be a
    /// directory that is not a reparse point each time.
    #[cfg(not(unix))]
    fn of(_: &fs::Metadata) -> Self {
        Self {}
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

/// Read a profile the person chose to import. Only a regular file, not a
/// link, of at most [`MAX_PROFILE_BYTES`] is read.
pub fn import_profile(path: &Path) -> Result<StyleProfile, ProfileError> {
    if device_path(path) {
        return Err(ProfileError::NotRegularFile);
    }
    StyleProfile::parse(&read_regular(path, Place::Chosen)?)
}

/// Write `profile` to the file the person chose to export to. An existing
/// target must be a regular file (the system dialog confirmed replacing it);
/// a link, folder or device is refused. The write is atomic.
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
fn regular_file(path: &Path, place: Place) -> Result<fs::Metadata, ProfileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !place.refuses(&metadata) => Ok(metadata),
        Ok(_) => Err(ProfileError::NotRegularFile),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(ProfileError::Missing),
        Err(_) => Err(ProfileError::Unreadable),
    }
}

/// Read a bounded regular file. The path is checked first; the open then
/// follows no link and never blocks (see [`open_no_follow`]), and the opened
/// handle must be a regular file and, on Unix, the very file that was checked.
fn read_regular(path: &Path, place: Place) -> Result<String, ProfileError> {
    let checked = regular_file(path, place)?;
    if checked.len() > MAX_PROFILE_BYTES {
        return Err(ProfileError::TooLarge);
    }
    let file = open_no_follow(path, place)?;
    let opened = file.metadata().map_err(|_| ProfileError::Unreadable)?;
    if !opened.is_file() || place.refuses(&opened) || !same_file(&checked, &opened) {
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

/// Whether two metadata records describe the same file. Unix compares the
/// device and inode, so a path swapped for a link after the check is refused.
#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Windows exposes no stable file identity through std. The handle is opened
/// on a reparse point itself rather than its target, then refused unless it
/// is a regular file that is not a reparse point, and the read is bounded.
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
fn open_no_follow(path: &Path, place: Place) -> Result<fs::File, ProfileError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options, place);
    options.open(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            return ProfileError::Missing;
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !place.refuses(&metadata) => {
                ProfileError::Unreadable
            }
            Ok(_) => ProfileError::NotRegularFile,
            Err(_) => ProfileError::Unreadable,
        }
    })
}

/// `open(2)` flags `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY` for each shipped Unix
/// target, as the libc 0.2.190 sources this workspace locks define them
/// (`unix/bsd/mod.rs` and `unix/bsd/apple/mod.rs` for macOS on both
/// architectures; `linux/{gnu,musl}/b64/{x86_64,aarch64}/mod.rs` for Linux).
/// This crate does not depend on libc; see the handoff for that alternative.
#[cfg(target_os = "macos")]
const NO_FOLLOW_FLAGS: Option<i32> = Some(0x0100 | 0x0004 | 0x0002_0000);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const NO_FOLLOW_FLAGS: Option<i32> = Some(0x0002_0000 | 0x0800 | 0x0100);
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const NO_FOLLOW_FLAGS: Option<i32> = Some(0x8000 | 0x0800 | 0x0100);
/// Unix targets that are not shipped keep only the checks around a plain
/// open; there a FIFO put in place between the check and the open can block.
#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))
    ))
))]
const NO_FOLLOW_FLAGS: Option<i32> = None;

#[cfg(unix)]
fn no_follow(options: &mut fs::OpenOptions, _: Place) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(flags) = NO_FOLLOW_FLAGS {
        options.custom_flags(flags);
    }
}

#[cfg(windows)]
fn no_follow(options: &mut fs::OpenOptions, place: Place) {
    use std::os::windows::fs::OpenOptionsExt;
    // Open a reparse point itself instead of what it points to.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    if place == Place::Folder {
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(not(any(unix, windows)))]
fn no_follow(_: &mut fs::OpenOptions, _: Place) {}

/// Where a file being read lives, which decides how strict the checks are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// The app's own profiles folder: any Windows reparse point is refused,
    /// and the open never follows one.
    Folder,
    /// A file the person chose in the system dialog. Links and junctions are
    /// refused, but other Windows reparse points are read, because files in a
    /// cloud-synced Documents folder (OneDrive placeholders) are reparse
    /// points too.
    Chosen,
}

impl Place {
    /// Whether this place refuses a file whose type is otherwise a regular
    /// file. Links and junctions are never regular files in std's view.
    fn refuses(self, metadata: &fs::Metadata) -> bool {
        self == Self::Folder && is_reparse_point(metadata)
    }
}

/// A Windows link, junction or other reparse point; never true elsewhere,
/// where `symlink_metadata` and `O_NOFOLLOW` already distinguish links.
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

enum WriteError {
    Exists,
    Failed,
}

/// Write through a temporary file in `dir`, then move it into place: the
/// destination is either the previous file or the complete new one. Without
/// `replace`, an existing destination is never overwritten.
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
        assert!(open_no_follow(&link, Place::Folder).is_err());
        assert!(open_no_follow(&target, Place::Folder).is_ok());
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
