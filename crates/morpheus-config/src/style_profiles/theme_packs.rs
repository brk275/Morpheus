//! Theme packs: the `themes/<name>.toml` palettes beside `config.toml`,
//! imported from and exported to files the person chooses, and installed
//! into the themes folder.
//!
//! A pack is the declarative [`ThemePack`]: base colors, semantic roles,
//! interaction states and sender colors. It holds no accounts, credentials,
//! paths, notification history, audio preferences or other settings, and
//! nothing in it is executed or fetched. Every pack is parsed with unknown
//! keys refused and validated before it is shown or written.
//!
//! Files are read, written and listed with the same primitives as style
//! profiles:
//! - A chosen file is a bounded regular file opened without following a
//!   link.
//! - An export creates a new file through a temporary file and a no-clobber
//!   rename. It never replaces anything at the chosen path, whatever the
//!   system dialog asked.
//! - The themes folder is opened once as an anchor that refuses a link in
//!   its place, and every installed file is written through it.
//! - At most [`MAX_THEME_PACKS`] packs are installed.
//! - Installing never replaces an existing pack. A conflicting imported
//!   pack needs a different name (see [`install_theme_pack`]).
//!
//! Messages are fixed text: never a path, file name or rejected value.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use thiserror::Error;

use super::{
    Folder, MAX_SCANNED_ENTRIES, ProfileError, WriteError, describe, device_path, read_path,
    reserved_device_name, write_atomic,
};
use crate::{
    BuiltInStylePreset, Config, ConfigError, ConfigStore, SenderColors, ThemePack, diagnostics,
    is_theme_name, parse_toml, registry,
};

/// Most theme packs installed in the themes folder.
pub const MAX_THEME_PACKS: usize = 64;
/// Longest installed pack name; [`is_theme_name`] allows up to 64.
const MAX_PACK_NAME_CHARS: usize = 40;
const PACK_HEADER: &str = "# Morpheus theme pack: colors only, nothing here is executed.\n\
# See docs/THEME_STUDIO.md and docs/CUSTOMIZATION.md.\n\n";

/// Why a theme pack operation failed, as fixed text.
#[derive(Debug, Error)]
pub enum PackError {
    #[error("the theme pack file does not exist")]
    Missing,
    #[error("the theme pack is not a regular file; links, folders and devices are not opened")]
    NotRegularFile,
    #[error("the theme pack file is larger than 64 KiB")]
    TooLarge,
    #[error("the theme pack file could not be read")]
    Unreadable,
    #[error("the theme pack file is not UTF-8 text")]
    NotUtf8,
    #[error("the theme pack is invalid: {}", describe(.0))]
    Invalid(ConfigError),
    #[error("theme pack names are 1 to 40 lowercase letters, digits, - or _")]
    Name,
    #[error("a theme pack with this name is already installed")]
    NameTaken,
    #[error(
        "existing theme packs are kept; give the imported pack a different name and import it again"
    )]
    ReplacementUnavailable,
    #[error("there are already 64 theme packs; remove one from the themes folder first")]
    TooMany,
    #[error("the themes folder is not available")]
    Folder,
    #[error("the chosen file is a link, folder or device; choose another file")]
    Target,
    /// Exports never replace a file, whatever the system dialog asked.
    #[error(
        "a file with that name already exists; exports never replace a file, so choose a new name"
    )]
    TargetExists,
    /// Retained for callers of the earlier replacement API. New requests
    /// are refused with `ReplacementUnavailable` before a replacement check.
    #[error("the installed theme pack changed after you chose to replace it; nothing was replaced")]
    InstalledChanged,
    #[error("the theme pack could not be saved; the previous file was kept")]
    Write,
}

impl PackError {
    /// A stable code for the error category.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "missing-file",
            Self::NotRegularFile => "not-a-regular-file",
            Self::TooLarge => "file-too-large",
            Self::Unreadable => "unreadable-file",
            Self::NotUtf8 => "not-utf8",
            Self::Invalid(_) => "invalid-theme-pack",
            Self::Name => "theme-pack-name",
            Self::NameTaken => "name-taken",
            Self::ReplacementUnavailable => "replacement-unavailable",
            Self::TooMany => "too-many-theme-packs",
            Self::Folder => "themes-folder",
            Self::Target => "export-target",
            Self::TargetExists => "export-target-exists",
            Self::InstalledChanged => "installed-pack-changed",
            Self::Write => "write-failed",
        }
    }
}

impl From<ProfileError> for PackError {
    fn from(error: ProfileError) -> Self {
        match error {
            ProfileError::Missing => Self::Missing,
            ProfileError::NotRegularFile => Self::NotRegularFile,
            ProfileError::TooLarge => Self::TooLarge,
            ProfileError::NotUtf8 => Self::NotUtf8,
            ProfileError::Folder => Self::Folder,
            ProfileError::Target => Self::Target,
            ProfileError::TargetExists => Self::TargetExists,
            ProfileError::Write => Self::Write,
            _ => Self::Unreadable,
        }
    }
}

/// Parse and validate a theme pack's text.
pub fn parse_theme_pack(raw: &str) -> Result<ThemePack, PackError> {
    let pack: ThemePack =
        parse_toml(raw, diagnostics::FileKind::Theme).map_err(PackError::Invalid)?;
    pack.validate().map_err(PackError::Invalid)?;
    Ok(pack)
}

/// A validated pack as the text Morpheus writes.
pub fn theme_pack_toml(pack: &ThemePack) -> Result<String, PackError> {
    pack.validate().map_err(PackError::Invalid)?;
    let body = toml::to_string_pretty(pack).map_err(|_| PackError::Write)?;
    Ok(format!("{PACK_HEADER}{body}"))
}

/// The accepted global palette, without account/room overrides or any other
/// configuration fields. This changes only an export copy of the pack.
pub fn theme_pack_with_overrides(
    pack: &ThemePack,
    colors: &std::collections::BTreeMap<String, String>,
    senders: &SenderColors,
) -> Result<ThemePack, PackError> {
    pack.validate().map_err(PackError::Invalid)?;
    let mut checked = Config::default();
    checked.colors = colors.clone();
    checked.senders = senders.clone();
    checked.validate().map_err(PackError::Invalid)?;
    let mut value = toml::Value::try_from(pack).map_err(|_| PackError::Write)?;
    let table = value.as_table_mut().ok_or(PackError::Write)?;
    for (id, color) in colors {
        let token = registry::color_token(id).ok_or(PackError::Write)?;
        let group = match token.source {
            registry::TokenSource::Base => "colors",
            registry::TokenSource::Role => "roles",
            registry::TokenSource::State => "states",
        };
        table
            .entry(group.to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or(PackError::Write)?
            .insert(id.clone(), toml::Value::String(color.clone()));
    }
    let mut exported: ThemePack = value.try_into().map_err(|_| PackError::Write)?;
    if senders.colors.is_some() {
        exported.senders = senders.clone();
    }
    exported.validate().map_err(PackError::Invalid)?;
    Ok(exported)
}

/// Read a theme pack the person chose to import: a regular file of at most
/// 64 KiB, opened without following a link at its final component.
pub fn import_theme_pack(path: &Path) -> Result<ThemePack, PackError> {
    if device_path(path) {
        return Err(PackError::NotRegularFile);
    }
    parse_theme_pack(&read_path(path)?)
}

/// Write `pack` to a new file at the path the person chose to export to.
///
/// An export never replaces anything. Neither the system dialog's answer
/// nor a check made here is consent bound to what is at that path when the
/// file is written. So:
/// - an existing regular file is refused with [`PackError::TargetExists`],
///   and the person chooses a new name;
/// - a link, folder or device is refused with [`PackError::Target`];
/// - the write is a temporary file moved into place with a no-clobber
///   rename, so anything that appears at the path meanwhile is kept, never
///   replaced or followed.
///
/// On a filesystem with neither a no-clobber rename nor hard links, the
/// export fails rather than risk replacing a file.
pub fn export_theme_pack(pack: &ThemePack, target: &Path) -> Result<(), PackError> {
    let raw = theme_pack_toml(pack)?;
    if device_path(target) {
        return Err(PackError::Target);
    }
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(PackError::Target)?;
    if !fs::metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(PackError::Target);
    }
    // Only for a precise refusal; the write below never replaces either way.
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Ok(metadata) if metadata.is_file() => return Err(PackError::TargetExists),
        _ => return Err(PackError::Target),
    }
    match write_atomic(parent, target, &raw, false) {
        Ok(()) => Ok(()),
        // Created by something else since the check: kept, not replaced.
        Err(WriteError::Exists) => Err(PackError::TargetExists),
        Err(WriteError::Failed) => Err(PackError::Write),
    }
}

/// The name a pack chosen from a file is installed under: its file name
/// without the extension, lowercased, with every other character than a
/// letter, digit, `-` or `_` dropped, at most 40 characters, never a Windows
/// device name. `None` when nothing usable remains.
pub fn pack_name_from(file_stem: &str) -> Option<String> {
    let name: String = file_stem
        .chars()
        .filter_map(|character| {
            let character = character.to_ascii_lowercase();
            (character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_'))
            .then_some(character)
        })
        .take(MAX_PACK_NAME_CHARS)
        .collect();
    (is_theme_name(&name) && !reserved_device_name(&name)).then_some(name)
}

fn valid_install_name(name: &str) -> bool {
    is_theme_name(name)
        && name.chars().count() <= MAX_PACK_NAME_CHARS
        && name == name.to_ascii_lowercase()
        && !reserved_device_name(name)
}

/// The anchored themes folder of `store`, created when missing. A link or
/// anything but a folder in its place is refused.
fn themes_folder(store: &ConfigStore) -> Result<(Folder, std::path::PathBuf), PackError> {
    let dir = store.themes_dir().map_err(|_| PackError::Folder)?;
    fs::create_dir_all(&dir).map_err(|_| PackError::Folder)?;
    let folder = Folder::open(&dir).map_err(|_| PackError::Folder)?;
    Ok((folder, dir))
}

/// How many packs are installed, counting only regular `<name>.toml` files
/// and examining at most a fixed number of entries; a folder with more
/// entries counts as full.
fn installed_count(folder: &Folder, dir: &Path) -> Result<usize, PackError> {
    if !folder.is_at(dir) {
        return Err(PackError::Folder);
    }
    let entries = fs::read_dir(dir).map_err(|_| PackError::Folder)?;
    let mut count = 0;
    for (examined, entry) in entries.enumerate() {
        if examined >= MAX_SCANNED_ENTRIES {
            return Ok(MAX_THEME_PACKS);
        }
        let Ok(entry) = entry else { continue };
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".toml"))
            .filter(|name| is_theme_name(name))
            .map(str::to_owned)
        else {
            continue;
        };
        if folder.regular(&format!("{name}.toml")).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

/// Whether a pack named `name` is installed as a regular file.
pub fn theme_pack_installed(store: &ConfigStore, name: &str) -> Result<bool, PackError> {
    if !valid_install_name(name) {
        return Err(PackError::Name);
    }
    let dir = store.themes_dir().map_err(|_| PackError::Folder)?;
    let folder = match Folder::open(&dir) {
        Ok(folder) => folder,
        Err(ProfileError::Missing) => return Ok(false),
        Err(_) => return Err(PackError::Folder),
    };
    match folder.regular(&format!("{name}.toml")) {
        Ok(()) => Ok(true),
        Err(ProfileError::Missing) => Ok(false),
        Err(_) => Err(PackError::NotRegularFile),
    }
}

/// The exact text of an installed pack, read for a preview. Currently
/// [`install_theme_pack`] refuses every replacement;
/// keeping this snapshot lets the controller identify a name collision
/// without making its contents public.
#[derive(Clone, PartialEq, Eq)]
pub struct InstalledPack(String);

/// Never shows the text.
impl fmt::Debug for InstalledPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstalledPack")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// The installed `themes/<name>.toml`, read through the anchored folder
/// without following a link, or `None` when no pack has that name. A file
/// there that is not a bounded regular UTF-8 file is an error, so it can
/// never be offered for replacement.
pub fn installed_theme_pack(
    store: &ConfigStore,
    name: &str,
) -> Result<Option<InstalledPack>, PackError> {
    if !valid_install_name(name) {
        return Err(PackError::Name);
    }
    let dir = store.themes_dir().map_err(|_| PackError::Folder)?;
    let folder = match Folder::open(&dir) {
        Ok(folder) => folder,
        Err(ProfileError::Missing) => return Ok(None),
        Err(_) => return Err(PackError::Folder),
    };
    match folder.read(&format!("{name}.toml")) {
        Ok(raw) => Ok(Some(InstalledPack(raw))),
        Err(ProfileError::Missing) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Install `pack` as `themes/<name>.toml`.
///
/// - Without `replacing`, nothing is ever replaced. A pack of that name,
///   even one installed meanwhile, is refused with
///   [`PackError::NameTaken`], and a full folder with
///   [`PackError::TooMany`].
/// - With `replacing`, the request is refused with
///   [`PackError::ReplacementUnavailable`] before any filesystem write.
///   Comparing text before an unconditional rename cannot preserve an
///   editor's intervening save or later writes through an open handle.
///   Replacement stays unavailable until that preservation is implemented;
///   giving the incoming pack a new name keeps both versions.
pub fn install_theme_pack(
    store: &ConfigStore,
    name: &str,
    pack: &ThemePack,
    replacing: Option<&InstalledPack>,
) -> Result<(), PackError> {
    if !valid_install_name(name) {
        return Err(PackError::Name);
    }
    let raw = theme_pack_toml(pack)?;
    if replacing.is_some() {
        return Err(PackError::ReplacementUnavailable);
    }
    let (folder, dir) = themes_folder(store)?;
    let file = format!("{name}.toml");
    if installed_count(&folder, &dir)? >= MAX_THEME_PACKS {
        return Err(PackError::TooMany);
    }
    folder
        .write(&file, &raw, false)
        .map_err(|error| match error {
            WriteError::Exists => PackError::NameTaken,
            WriteError::Failed => PackError::Write,
        })
}

/// The pack a theme name selects, for export: the installed file, else the
/// built-in pack of that name.
pub fn read_theme_pack(store: &ConfigStore, name: &str) -> Result<ThemePack, PackError> {
    if !is_theme_name(name) {
        return Err(PackError::Name);
    }
    let dir = store.themes_dir().map_err(|_| PackError::Folder)?;
    match Folder::open(&dir) {
        Ok(folder) => match folder.read(&format!("{name}.toml")) {
            Ok(raw) => return parse_theme_pack(&raw),
            Err(ProfileError::Missing) => {}
            Err(error) => return Err(error.into()),
        },
        Err(ProfileError::Missing) => {}
        Err(_) => return Err(PackError::Folder),
    }
    match BuiltInStylePreset::from_theme_name(name) {
        Some(preset) => preset.theme().map_err(PackError::Invalid),
        None => Err(PackError::Missing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ConfigStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        (directory, store)
    }

    fn paper() -> ThemePack {
        BuiltInStylePreset::Paper.theme().unwrap()
    }

    #[test]
    fn names_from_files_are_bounded_and_safe() {
        assert_eq!(
            pack_name_from("Ocean Breeze").as_deref(),
            Some("oceanbreeze")
        );
        assert_eq!(pack_name_from("../../etc").as_deref(), Some("etc"));
        assert_eq!(pack_name_from("dusk-2").as_deref(), Some("dusk-2"));
        assert_eq!(pack_name_from("...").as_deref(), None);
        assert_eq!(pack_name_from("").as_deref(), None);
        assert!(pack_name_from(&"a".repeat(500)).unwrap().len() <= MAX_PACK_NAME_CHARS);
        if cfg!(windows) {
            assert_eq!(pack_name_from("com1"), None);
        }
        assert!(!valid_install_name("Upper"));
        assert!(!valid_install_name("a/b"));
        assert!(!valid_install_name(&"a".repeat(41)));
    }

    #[test]
    fn packs_carry_only_palette_data() {
        let encoded = toml::to_string_pretty(&paper()).unwrap();
        for extra in [
            "[media]\nmicrophone_device = 'secret'\n",
            "[notifications]\nenabled = false\n",
            "[account_overrides.secret]\ntheme = 'dark'\n",
            "secret_path = '/home/secret'\n",
            "[colors]\nscript = 'run.sh'\n",
        ] {
            let error = parse_theme_pack(&format!("{encoded}\n{extra}")).expect_err(extra);
            assert_eq!(error.code(), "invalid-theme-pack", "{extra}");
            assert!(!error.to_string().contains("secret"), "{extra}");
        }
        let error = parse_theme_pack(&encoded.replace("#", "#zz")).expect_err("bad color");
        assert_eq!(error.code(), "invalid-theme-pack");
        assert!(parse_theme_pack(&encoded).is_ok());
    }

    #[test]
    fn import_export_and_install_round_trip_within_bounds() {
        let (directory, store) = store();
        let exported = directory.path().join("paper copy.toml");
        export_theme_pack(&paper(), &exported).unwrap();
        let imported = import_theme_pack(&exported).unwrap();
        assert_eq!(imported, paper());

        assert!(!theme_pack_installed(&store, "paper-copy").unwrap());
        assert_eq!(installed_theme_pack(&store, "paper-copy").unwrap(), None);
        install_theme_pack(&store, "paper-copy", &imported, None).unwrap();
        assert!(theme_pack_installed(&store, "paper-copy").unwrap());
        // Never replaced, including when a caller supplies the exact
        // installed snapshot it previously reviewed.
        assert_eq!(
            install_theme_pack(&store, "paper-copy", &imported, None)
                .unwrap_err()
                .code(),
            "name-taken"
        );
        let shown = installed_theme_pack(&store, "paper-copy").unwrap().unwrap();
        assert_eq!(
            install_theme_pack(&store, "paper-copy", &imported, Some(&shown))
                .unwrap_err()
                .code(),
            "replacement-unavailable"
        );
        assert_eq!(read_theme_pack(&store, "paper-copy").unwrap(), paper());
        // Built-in names read their embedded pack until one is installed.
        let terminal = BuiltInStylePreset::Terminal.theme().unwrap();
        assert_eq!(read_theme_pack(&store, "terminal").unwrap(), terminal);
        let missing = read_theme_pack(&store, "missing").unwrap_err();
        assert_eq!(missing.code(), "missing-file");
        let traversal = install_theme_pack(&store, "../x", &imported, None).unwrap_err();
        assert_eq!(traversal.code(), "theme-pack-name");

        let large = directory.path().join("large.toml");
        let mut raw = toml::to_string_pretty(&paper()).unwrap();
        while raw.len() <= 64 * 1024 {
            raw.push_str("# padding padding padding padding padding padding padding\n");
        }
        fs::write(&large, raw).unwrap();
        assert_eq!(
            import_theme_pack(&large).unwrap_err().code(),
            "file-too-large"
        );
    }

    #[test]
    fn a_full_themes_folder_refuses_another_pack() {
        let (_directory, store) = store();
        for index in 0..MAX_THEME_PACKS {
            install_theme_pack(&store, &format!("pack-{index}"), &paper(), None).unwrap();
        }
        assert_eq!(
            install_theme_pack(&store, "one-more", &paper(), None)
                .unwrap_err()
                .code(),
            "too-many-theme-packs"
        );
        // A full folder does not authorize replacing any installed pack.
        let shown = installed_theme_pack(&store, "pack-0").unwrap().unwrap();
        assert_eq!(
            install_theme_pack(&store, "pack-0", &paper(), Some(&shown))
                .unwrap_err()
                .code(),
            "replacement-unavailable"
        );
    }

    #[test]
    fn exports_never_replace_a_file() {
        let (directory, _store) = store();
        let target = directory.path().join("chosen.toml");
        // What another program left there, or what the system dialog asked
        // about replacing: either way it is kept.
        fs::write(&target, "kept").unwrap();
        let error = export_theme_pack(&paper(), &target).unwrap_err();
        assert_eq!(error.code(), "export-target-exists");
        assert!(error.to_string().contains("choose a new name"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
        // A new name is written.
        let fresh = directory.path().join("fresh.toml");
        export_theme_pack(&paper(), &fresh).unwrap();
        assert_eq!(import_theme_pack(&fresh).unwrap(), paper());
        // Exporting again to the name just written is refused too.
        assert_eq!(
            export_theme_pack(&paper(), &fresh).unwrap_err().code(),
            "export-target-exists"
        );
    }

    #[test]
    fn an_installed_pack_is_never_replaced_even_with_a_reviewed_snapshot() {
        let (directory, store) = store();
        install_theme_pack(&store, "dusk", &paper(), None).unwrap();
        let shown = installed_theme_pack(&store, "dusk").unwrap().unwrap();
        let installed = directory.path().join("themes").join("dusk.toml");
        // A deterministic edit after the choice was offered, before the
        // replacement: the edited file is kept and nothing is replaced.
        let edited = format!(
            "{}\n# edited elsewhere\n",
            fs::read_to_string(&installed).unwrap()
        );
        fs::write(&installed, &edited).unwrap();
        let terminal = BuiltInStylePreset::Terminal.theme().unwrap();
        let error = install_theme_pack(&store, "dusk", &terminal, Some(&shown)).unwrap_err();
        assert_eq!(error.code(), "replacement-unavailable");
        assert_eq!(fs::read_to_string(&installed).unwrap(), edited);
        // Re-reading the current file still cannot authorize a replacement.
        let shown = installed_theme_pack(&store, "dusk").unwrap().unwrap();
        // The bound text never prints.
        assert!(!format!("{shown:?}").contains("edited elsewhere"));
        assert_eq!(
            install_theme_pack(&store, "dusk", &terminal, Some(&shown))
                .unwrap_err()
                .code(),
            "replacement-unavailable"
        );
        assert_eq!(fs::read_to_string(&installed).unwrap(), edited);
        // A file removed after the choice is not written again.
        let shown = installed_theme_pack(&store, "dusk").unwrap().unwrap();
        fs::remove_file(&installed).unwrap();
        let error = install_theme_pack(&store, "dusk", &paper(), Some(&shown)).unwrap_err();
        assert_eq!(error.code(), "replacement-unavailable");
        assert!(!installed.exists());
        // Initial installation under a new name remains available and does
        // not mutate the original. This is how both versions are kept.
        install_theme_pack(&store, "dusk-copy", &terminal, None).unwrap();
        assert_eq!(read_theme_pack(&store, "dusk-copy").unwrap(), terminal);
        assert!(!installed.exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        let (directory, store) = store();
        let real = directory.path().join("real.toml");
        export_theme_pack(&paper(), &real).unwrap();
        let link = directory.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            import_theme_pack(&link).unwrap_err().code(),
            "not-a-regular-file"
        );
        // A link in place of the themes folder is refused.
        let elsewhere = directory.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, directory.path().join("themes")).unwrap();
        assert_eq!(
            install_theme_pack(&store, "linked", &paper(), None)
                .unwrap_err()
                .code(),
            "themes-folder"
        );
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn exported_palette_keeps_global_overrides_and_original_fallthrough() {
        let mut original = paper();
        assert_eq!(original.roles.shadow.as_deref(), Some("#25342F33"));
        original.roles.link = Some("#112233".into());
        original.states.control_hover = Some("#223344".into());
        original.senders.colors = Some(vec!["#123456".into(), "#654321".into()]);
        let saved = original.clone();
        let mut config = Config::default();
        config.colors.insert("accent".into(), "#445566".into());
        config.colors.insert("link".into(), "#55667788".into());
        config
            .colors
            .insert("control_hover".into(), "#667788".into());
        config.senders.colors = Some(vec!["#112233".into(), "#778899".into()]);
        config
            .account_overrides
            .entry("private-account".into())
            .or_default()
            .colors
            .insert("canvas".into(), "#010101".into());
        let exported =
            theme_pack_with_overrides(&original, &config.colors, &config.senders).unwrap();
        let raw = theme_pack_toml(&exported).unwrap();
        assert_eq!(parse_theme_pack(&raw).unwrap(), exported);
        assert_eq!(original, saved);
        assert_eq!(exported.schema_version, crate::SCHEMA_VERSION);
        assert_eq!(exported.colors.accent, "#445566");
        assert_eq!(exported.colors.canvas, original.colors.canvas);
        assert_eq!(exported.roles.link.as_deref(), Some("#55667788"));
        assert_eq!(exported.roles.shadow.as_deref(), Some("#25342F33"));
        assert_eq!(exported.states.control_hover.as_deref(), Some("#667788"));
        assert_eq!(exported.senders, config.senders);
        assert!(!raw.contains("private-account"));
        assert!(!raw.contains("account_overrides"));
        assert_eq!(
            theme_pack_with_overrides(&original, &Default::default(), &Default::default()).unwrap(),
            original
        );
        // Absence must still fall through, while a global RGBA override must
        // survive export/import without changing the original sparse pack.
        for (pack_shadow, shadow) in [(None, None), (Some("#25342F33"), Some("#0A1B2C44"))] {
            let mut source = original.clone();
            source.roles.shadow = pack_shadow.map(str::to_owned);
            let mut colors = config.colors.clone();
            if let Some(shadow) = shadow {
                colors.insert("shadow".into(), shadow.into());
            }
            let exported = theme_pack_with_overrides(&source, &colors, &config.senders).unwrap();
            let imported = parse_theme_pack(&theme_pack_toml(&exported).unwrap()).unwrap();
            assert_eq!(imported, exported);
            assert_eq!(imported.roles.shadow.as_deref(), shadow);
            assert_eq!(source.roles.shadow.as_deref(), pack_shadow);
        }
        assert_eq!(original, saved);
    }
}
