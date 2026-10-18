//! Diagnostics for `config.toml` and the selected theme file, shared by the
//! offline validator and every runtime load, reload and Settings save.
//!
//! A diagnostic has a stable code, the file it belongs to, a field path and a
//! line and column. Its message is fixed text and never quotes a value from
//! the file. Field paths are built only from schema names in the registry:
//! a key the schema does not know is shown as `<unknown>`, and every local
//! account ID is shown as `<account>`. Nothing here creates, repairs or
//! writes a file.

use std::collections::HashSet;
use std::path::Path;

use toml::de::{DeTable, DeValue};

use crate::registry::{self, FIELDS, TokenSource};
use crate::{
    BuiltInStylePreset, Config, ConfigError, ConfigStore, InputError, MAX_CONFIG_BYTES,
    MAX_THEME_BYTES, Problem, ShortcutAction, ThemePack, ThemeRoles, read_bounded,
};

/// Every diagnostic code with its meaning, for the schema output and docs.
pub const CODES: &[(&str, &str)] = &[
    ("missing-file", "The file does not exist."),
    (
        "not-a-regular-file",
        "The path is a directory, FIFO, socket or device.",
    ),
    ("file-too-large", "The file exceeds its size limit."),
    ("unreadable-file", "The file could not be read."),
    ("not-utf8", "The file is not UTF-8 text."),
    ("syntax", "The text is not valid TOML at this position."),
    ("unknown-field", "A key that is not part of the schema."),
    ("missing-field", "A required key is missing."),
    ("wrong-type", "A value of the wrong type."),
    (
        "invalid-value",
        "A value that is not one of the accepted options.",
    ),
    ("duplicate-key", "A key defined more than once."),
    ("schema-version", "An unsupported schema_version."),
    ("out-of-range", "A number outside its documented range."),
    (
        "invalid-font-family",
        "An empty, oversized or control-character font family.",
    ),
    (
        "invalid-theme-name",
        "A theme name that is not ASCII letters, digits, - or _.",
    ),
    (
        "invalid-theme-display-name",
        "A theme name field outside 1 to 80 characters.",
    ),
    ("invalid-color", "A color that is not #RRGGBB or #RRGGBBAA."),
    (
        "unknown-color-token",
        "A [colors] key that names no color token.",
    ),
    (
        "invalid-palette-length",
        "A palette with too few or too many colors.",
    ),
    ("invalid-audio-device", "A malformed audio device ID."),
    (
        "invalid-room-id",
        "A list entry that is not a Matrix room ID.",
    ),
    (
        "call-mode-conflict",
        "A room listed for both Element Call modes.",
    ),
    (
        "unknown-shortcut-action",
        "A shortcut for an unknown action.",
    ),
    ("invalid-shortcut", "A key combination that cannot be used."),
    (
        "shortcut-conflict",
        "Two shortcuts with the same key combination.",
    ),
    ("invalid", "Another validation failure."),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Config,
    Theme,
}

impl FileKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Theme => "theme",
        }
    }
}

/// A TOML parse or schema-shape error reduced to fixed text. The parser's
/// own message can quote the file, so it is classified and dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub file: FileKind,
    pub code: &'static str,
    pub message: &'static str,
    /// Registry-allowlisted path; see the module documentation.
    pub field: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} file", self.file.id())?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(formatter, " line {line}, column {column}")?;
        }
        if let Some(field) = &self.field {
            write!(formatter, " ({field})")?;
        }
        write!(formatter, ": {} [{}]", self.message, self.code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: FileKind,
    /// Stable, documented code such as `unknown-field` or `out-of-range`.
    pub code: &'static str,
    /// Fixed explanation; schema bounds at most, never file content.
    pub message: String,
    /// Registry-allowlisted dotted path, for example `geometry.corner_radius`
    /// or `account_overrides.<account>.theme`.
    pub field: Option<String>,
    /// One-based line and column of the field or error, when known.
    pub line: Option<usize>,
    pub column: Option<usize>,
}

/// Where the selected theme comes from, as a fixed category. The configured
/// theme name is file content, so it is never part of a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    /// No theme pack is selected, or the configuration did not validate.
    None,
    /// A built-in theme, without an override file.
    BuiltIn,
    /// A file overriding a built-in theme.
    BuiltInOverride,
    /// A theme file of the user's own.
    Custom,
}

impl ThemeSource {
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BuiltIn => "built-in",
            Self::BuiltInOverride => "built-in-override",
            Self::Custom => "custom",
        }
    }
}

/// The outcome of checking the configuration and its theme without
/// starting anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
    pub theme: ThemeSource,
}

impl Report {
    pub fn is_valid(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Check the configuration file at `store`'s path and the theme file it
/// selects. Read-only: a missing configuration is reported, not created.
pub fn check_store(store: &ConfigStore) -> Report {
    let raw = match read_bounded(store.path(), MAX_CONFIG_BYTES) {
        Ok(raw) => raw,
        Err(error) => {
            return Report {
                diagnostics: vec![input_diagnostic(FileKind::Config, error)],
                theme: ThemeSource::None,
            };
        }
    };
    let config = match check_config_source(&raw) {
        Ok(config) => config,
        Err(diagnostic) => {
            return Report {
                diagnostics: vec![diagnostic],
                theme: ThemeSource::None,
            };
        }
    };
    let Some(name) = config.appearance.theme_pack.clone() else {
        return Report {
            diagnostics: Vec::new(),
            theme: ThemeSource::None,
        };
    };
    let (theme, diagnostic) = match store.theme_path(&name) {
        Err(error) => (
            ThemeSource::None,
            Some(semantic_diagnostic(FileKind::Config, &raw, &error)),
        ),
        Ok(path) => check_theme_file(&path, &name),
    };
    Report {
        diagnostics: diagnostic.into_iter().collect(),
        theme,
    }
}

fn check_theme_file(path: &Path, name: &str) -> (ThemeSource, Option<Diagnostic>) {
    let built_in = BuiltInStylePreset::ALL
        .iter()
        .any(|preset| preset.theme_name() == name);
    let file_source = if built_in {
        ThemeSource::BuiltInOverride
    } else {
        ThemeSource::Custom
    };
    match read_bounded(path, MAX_THEME_BYTES) {
        Ok(raw) => (file_source, check_theme_source(&raw).err()),
        Err(InputError::Missing) if built_in => (ThemeSource::BuiltIn, None),
        Err(error) => (file_source, Some(input_diagnostic(FileKind::Theme, error))),
    }
}

/// Parse and validate configuration text.
pub fn check_config_source(raw: &str) -> Result<Config, Diagnostic> {
    let config: Config = crate::parse_toml(raw, FileKind::Config)
        .map_err(|error| semantic_diagnostic(FileKind::Config, raw, &error))?;
    config
        .validate()
        .map_err(|error| semantic_diagnostic(FileKind::Config, raw, &error))?;
    Ok(config)
}

/// Parse and validate theme text.
pub fn check_theme_source(raw: &str) -> Result<ThemePack, Diagnostic> {
    let theme: ThemePack = crate::parse_toml(raw, FileKind::Theme)
        .map_err(|error| semantic_diagnostic(FileKind::Theme, raw, &error))?;
    theme
        .validate()
        .map_err(|error| semantic_diagnostic(FileKind::Theme, raw, &error))?;
    Ok(theme)
}

pub fn input_diagnostic(file: FileKind, error: InputError) -> Diagnostic {
    Diagnostic {
        file,
        code: error.code(),
        message: error.to_string(),
        field: None,
        line: None,
        column: None,
    }
}

/// Classify a TOML or schema-shape error and locate it.
pub(crate) fn decode_error(file: FileKind, raw: &str, error: &toml::de::Error) -> DecodeError {
    let text = error.message();
    let (code, message) = if text.starts_with("unknown field") {
        ("unknown-field", "this key is not part of the schema")
    } else if text.starts_with("missing field") {
        ("missing-field", "a required key is missing from this table")
    } else if text.starts_with("invalid type") {
        ("wrong-type", "this value has the wrong type")
    } else if text.starts_with("unknown variant") || text.starts_with("invalid value") {
        (
            "invalid-value",
            "this value is not one of the accepted options",
        )
    } else if text.starts_with("duplicate key") {
        ("duplicate-key", "this key is defined more than once")
    } else {
        ("syntax", "the file is not valid TOML here")
    };
    let span = error.span();
    let (line, column) = match &span {
        Some(span) => position(raw, span.start),
        None => (None, None),
    };
    let field = span.and_then(|span| field_at(file, raw, span.start));
    DecodeError {
        file,
        code,
        message,
        field,
        line,
        column,
    }
}

/// Any configuration error as a diagnostic, located in the source.
fn semantic_diagnostic(file: FileKind, raw: &str, error: &ConfigError) -> Diagnostic {
    let (code, message, field) = match error {
        ConfigError::Decode(decode) => {
            return Diagnostic {
                file: decode.file,
                code: decode.code,
                message: decode.message.into(),
                field: decode.field.clone(),
                line: decode.line,
                column: decode.column,
            };
        }
        ConfigError::Field { field, problem } | ConfigError::ThemeField { field, problem } => {
            (problem.code(), problem.to_string(), Some(field.clone()))
        }
        ConfigError::Version => (
            "schema-version",
            "this version supports schema_version = 1".to_owned(),
            Some("schema_version".to_owned()),
        ),
        ConfigError::Input(error) => return input_diagnostic(file, *error),
        _ => ("invalid", "the file is invalid".to_owned(), None),
    };
    let problem = match error {
        ConfigError::Field { problem, .. } => Some(problem),
        _ => None,
    };
    let offset = field
        .as_deref()
        .and_then(|field| locate(raw, field, problem));
    let (line, column) = offset.map_or((None, None), |offset| position(raw, offset));
    Diagnostic {
        file,
        code,
        message,
        field,
        line,
        column,
    }
}

/// One-based line and column (in characters) of a byte offset.
fn position(raw: &str, offset: usize) -> (Option<usize>, Option<usize>) {
    let offset = offset.min(raw.len());
    let Some(before) = raw.get(..offset) else {
        return (None, None);
    };
    let line = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before[start..].chars().count() + 1;
    (Some(line), Some(column))
}

/// The schema paths of one file, as dotted patterns. `<token>`, `<base>`,
/// `<role>` and `<action>` match only registry names; `<account>` matches any
/// key and is always shown as `<account>`.
fn patterns(file: FileKind) -> Vec<Vec<&'static str>> {
    match file {
        FileKind::Config => FIELDS
            .iter()
            .map(|field| field.key.split('.').collect())
            .collect(),
        FileKind::Theme => vec![
            vec!["schema_version"],
            vec!["name"],
            vec!["colors", "<base>"],
            vec!["roles", "<role>"],
            vec!["senders", "colors"],
        ],
    }
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    match pattern {
        "<token>" => registry::color_token(segment).is_some(),
        "<base>" => {
            registry::color_token(segment).is_some_and(|token| token.source == TokenSource::Base)
        }
        "<role>" => ThemeRoles::default()
            .entries()
            .iter()
            .any(|(name, _)| *name == segment),
        "<action>" => ShortcutAction::parse(segment).is_some(),
        "<account>" => true,
        literal => literal == segment,
    }
}

/// A raw key path rewritten with schema names only. The longest matching
/// prefix is kept, `<account>` replaces account IDs, and any remainder is a
/// single `<unknown>`.
fn shown_path(file: FileKind, segments: &[String]) -> String {
    let patterns = patterns(file);
    let mut shown: Vec<&str> = Vec::new();
    for (depth, segment) in segments.iter().enumerate() {
        let matching = patterns.iter().find(|pattern| {
            pattern.len() > depth
                && pattern[..=depth]
                    .iter()
                    .zip(&segments[..=depth])
                    .all(|(pattern, segment)| segment_matches(pattern, segment))
        });
        let Some(pattern) = matching else {
            shown.push("<unknown>");
            break;
        };
        shown.push(if pattern[depth] == "<account>" {
            "<account>"
        } else {
            // Equal to the matched schema name.
            pattern_name(pattern[depth], segment)
        });
    }
    shown.join(".")
}

/// The schema name that a matched segment stands for. Placeholders other
/// than `<account>` only match registry names, which are returned from the
/// registry rather than from the file.
fn pattern_name<'a>(pattern: &'a str, segment: &str) -> &'a str {
    if !pattern.starts_with('<') {
        return pattern;
    }
    registry::COLOR_TOKENS
        .iter()
        .map(|token| token.id)
        .chain(ThemeRoles::default().entries().map(|(name, _)| name))
        .chain(ShortcutAction::ALL.map(ShortcutAction::name))
        .find(|name| *name == segment)
        .unwrap_or("<unknown>")
}

/// The allowlisted path of the narrowest key or value that contains
/// `offset`. A `[table]` value spans only its header, so leaves are compared.
fn field_at(file: FileKind, raw: &str, offset: usize) -> Option<String> {
    let root = DeTable::parse(raw).ok()?;
    let mut best = None;
    collect_at(root.get_ref(), &mut Vec::new(), offset, &mut best);
    best.map(|(_, path): (usize, Vec<String>)| shown_path(file, &path))
}

fn collect_at(
    table: &DeTable<'_>,
    path: &mut Vec<String>,
    offset: usize,
    best: &mut Option<(usize, Vec<String>)>,
) {
    for (key, value) in table.iter() {
        // Raw keys stay in memory only until `shown_path` rewrites them.
        path.push(key.get_ref().to_string());
        for span in [key.span(), value.span()] {
            if span.contains(&offset) || span.start == offset {
                let width = span.len();
                if best.as_ref().is_none_or(|(current, _)| width <= *current) {
                    *best = Some((width, path.clone()));
                }
            }
        }
        if let DeValue::Table(inner) = value.get_ref() {
            collect_at(inner, path, offset, best);
        }
        path.pop();
    }
}

/// The byte offset of the entry a validation error names: a dotted path such
/// as `colors.link`, `senders.colors[1]` or
/// `account_overrides.<account>.local_voice_rooms`. For an account list the
/// failing entry itself is found again, in the validator's account order.
fn locate(raw: &str, field: &str, problem: Option<&Problem>) -> Option<usize> {
    let root = DeTable::parse(raw).ok()?;
    if let Some(list) = field.strip_prefix("account_overrides.<account>.") {
        return locate_account_entry(root.get_ref(), list, problem?);
    }
    let segments: Vec<&str> = field.split('.').collect();
    locate_in(root.get_ref(), &segments)
}

fn locate_in(table: &DeTable<'_>, segments: &[&str]) -> Option<usize> {
    let (first, rest) = segments.split_first()?;
    let (name, index) = match first.split_once('[') {
        Some((name, index)) => (name, index.trim_end_matches(']').parse::<usize>().ok()),
        None => (*first, None),
    };
    let (key, value) = table.iter().find(|(key, _)| key.get_ref() == name)?;
    if let Some(index) = index {
        if let DeValue::Array(items) = value.get_ref() {
            return items.get(index).map(|item| item.span().start);
        }
        return Some(key.span().start);
    }
    if rest.is_empty() {
        return Some(key.span().start);
    }
    match value.get_ref() {
        DeValue::Table(inner) => locate_in(inner, rest).or(Some(key.span().start)),
        _ => Some(key.span().start),
    }
}

/// The first entry of `list` that fails `problem`, scanning accounts in key
/// order as `Config::validate` does. No account ID leaves this function.
fn locate_account_entry(root: &DeTable<'_>, list: &str, problem: &Problem) -> Option<usize> {
    let (_, accounts) = root
        .iter()
        .find(|(key, _)| key.get_ref() == "account_overrides")?;
    let DeValue::Table(accounts) = accounts.get_ref() else {
        return None;
    };
    for (_, account) in accounts.iter() {
        let DeValue::Table(account) = account.get_ref() else {
            continue;
        };
        let entries = |name: &str| -> Vec<(String, usize)> {
            account
                .iter()
                .find(|(key, _)| key.get_ref() == name)
                .and_then(|(_, value)| match value.get_ref() {
                    DeValue::Array(items) => Some(
                        items
                            .iter()
                            .filter_map(|item| match item.get_ref() {
                                DeValue::String(text) => {
                                    Some((text.to_string(), item.span().start))
                                }
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let failing = match problem {
            Problem::RoomId => entries(list)
                .into_iter()
                .find(|(room, _)| !crate::is_room_id(room)),
            Problem::CallModeConflict => {
                let legacy: HashSet<String> = entries("legacy_element_call_rooms")
                    .into_iter()
                    .map(|(room, _)| room)
                    .collect();
                entries(list)
                    .into_iter()
                    .find(|(room, _)| legacy.contains(room))
            }
            _ => None,
        };
        if let Some((_, offset)) = failing {
            return Some(offset);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_error(raw: &str) -> Diagnostic {
        check_config_source(raw).expect_err("the source must be rejected")
    }

    #[test]
    fn decode_errors_are_classified_and_located_without_quoting_values() {
        let unknown = config_error("[geometry]\ncorner_radius = 4\ncorner_radios = 5\n");
        assert_eq!(unknown.code, "unknown-field");
        assert_eq!(unknown.line, Some(3));
        assert_eq!(unknown.field.as_deref(), Some("geometry.<unknown>"));
        let wrong = config_error("[geometry]\ncorner_radius = \"secret-value\"\n");
        assert_eq!(wrong.code, "wrong-type");
        assert_eq!(wrong.field.as_deref(), Some("geometry.corner_radius"));
        assert_eq!(wrong.line, Some(2));
        let variant = config_error("[chat]\nlayout = \"secret-layout\"\n");
        assert_eq!(variant.code, "invalid-value");
        assert_eq!(variant.field.as_deref(), Some("chat.layout"));
        let syntax = config_error("[geometry\ncorner_radius = 4\n");
        assert_eq!(syntax.code, "syntax");
        assert_eq!(syntax.line, Some(1));
        for diagnostic in [unknown, wrong, variant, syntax] {
            assert!(!diagnostic.message.contains("secret"));
            assert!(!format!("{diagnostic:?}").contains("secret"));
        }
    }

    #[test]
    fn validation_errors_name_the_field_and_its_bounds_only() {
        let radius = config_error("[geometry]\n\ncorner_radius = 99\n");
        assert_eq!(radius.code, "out-of-range");
        assert_eq!(radius.field.as_deref(), Some("geometry.corner_radius"));
        assert_eq!((radius.line, radius.column), (Some(3), Some(1)));
        assert_eq!(radius.message, "must be between 0 and 24");
        let color = config_error("[colors]\nlink = \"#12345\"\n");
        assert_eq!(color.code, "invalid-color");
        assert_eq!(color.field.as_deref(), Some("colors.link"));
        assert_eq!(color.line, Some(2));
        let token = config_error("[colors]\nnot_a_token = \"#123456\"\n");
        assert_eq!(token.code, "unknown-color-token");
        assert_eq!(token.field.as_deref(), Some("colors"));
        let opacity = config_error("[window]\nopacity_percent = 101\n");
        assert_eq!(opacity.code, "out-of-range");
        assert_eq!(opacity.field.as_deref(), Some("window.opacity_percent"));
        let shortcut = config_error("[shortcuts]\nsearch = \"Mod+Tab-secret\"\n");
        assert_eq!(shortcut.code, "invalid-shortcut");
        assert_eq!(shortcut.field.as_deref(), Some("shortcuts.search"));
        assert!(!shortcut.message.contains("secret"));
        let room = config_error(
            "[account_overrides.\"@private:example.org\"]\nlocal_voice_rooms = [\"secret room\"]\n",
        );
        assert_eq!(room.code, "invalid-room-id");
        assert_eq!(
            room.field.as_deref(),
            Some("account_overrides.<account>.local_voice_rooms")
        );
        assert_eq!(room.line, Some(2));
        assert!(
            !format!("{room:?}").contains("private") && !format!("{room:?}").contains("secret")
        );
        let version = config_error("schema_version = 7\n");
        assert_eq!(version.code, "schema-version");
        assert_eq!(version.line, Some(1));
    }

    #[test]
    fn theme_errors_locate_array_entries_and_accept_rgba() {
        let base = toml::to_string_pretty(&BuiltInStylePreset::Nocturne.theme().unwrap()).unwrap();
        let mut theme: ThemePack = toml::from_str(&base).unwrap();
        theme.roles.scrim = Some("#00000080".into());
        theme.senders.colors = Some(vec!["#ABCDEF".into(), "#ABCDEF7F".into()]);
        let rgba = toml::to_string_pretty(&theme).unwrap();
        check_theme_source(&rgba).unwrap();
        let broken = rgba.replace("#ABCDEF7F", "#ABCDEF7");
        let diagnostic = check_theme_source(&broken).unwrap_err();
        assert_eq!(diagnostic.file, FileKind::Theme);
        assert_eq!(diagnostic.code, "invalid-color");
        assert_eq!(diagnostic.field.as_deref(), Some("senders.colors[1]"));
        let line = diagnostic.line.unwrap();
        assert!(broken.lines().nth(line - 1).unwrap().contains("#ABCDEF7"));
    }

    #[test]
    fn store_check_reads_without_creating_and_reports_theme_problems() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        let missing = check_store(&store);
        assert_eq!(missing.diagnostics[0].code, "missing-file");
        assert!(
            !store.path().exists(),
            "validation must not create the file"
        );
        std::fs::write(store.path(), "[appearance]\ntheme_pack = \"aurora\"\n").unwrap();
        let theme = check_store(&store);
        assert_eq!(theme.theme, ThemeSource::Custom);
        assert_eq!(theme.diagnostics[0].file, FileKind::Theme);
        assert_eq!(theme.diagnostics[0].code, "missing-file");
        assert!(!store.themes_dir().unwrap().exists());
        // A built-in name without an override file uses the embedded theme.
        std::fs::write(store.path(), "[appearance]\ntheme_pack = \"paper\"\n").unwrap();
        let paper = check_store(&store);
        assert!(paper.is_valid());
        assert_eq!(paper.theme, ThemeSource::BuiltIn);
        let oversized = format!("# {}\n", "x".repeat(MAX_CONFIG_BYTES as usize));
        std::fs::write(store.path(), oversized).unwrap();
        assert_eq!(check_store(&store).diagnostics[0].code, "file-too-large");
    }

    #[test]
    fn every_code_is_documented() {
        let documented = |code: &str| CODES.iter().any(|(known, _)| *known == code);
        for error in [
            InputError::Missing,
            InputError::NotRegularFile,
            InputError::TooLarge,
            InputError::Unreadable,
            InputError::NotUtf8,
        ] {
            assert!(documented(error.code()), "{}", error.code());
        }
        for problem in [
            crate::Problem::Range {
                minimum: 0,
                maximum: 1,
            },
            crate::Problem::FontFamily,
            crate::Problem::ThemeName,
            crate::Problem::ThemeDisplayName,
            crate::Problem::Color,
            crate::Problem::UnknownColorToken,
            crate::Problem::PaletteLength { maximum: 1 },
            crate::Problem::AudioDevice,
            crate::Problem::RoomId,
            crate::Problem::CallModeConflict,
            crate::Problem::UnknownShortcutAction,
            crate::Problem::Shortcut("reason"),
            crate::Problem::ShortcutConflict,
        ] {
            assert!(documented(problem.code()), "{}", problem.code());
        }
        for code in [
            "syntax",
            "unknown-field",
            "missing-field",
            "wrong-type",
            "invalid-value",
            "duplicate-key",
            "schema-version",
            "invalid",
        ] {
            assert!(documented(code), "{code}");
        }
    }

    #[test]
    fn unknown_keys_and_account_ids_are_never_shown() {
        let cases = [
            ("secret_root_key = 1\n", "unknown-field", "<unknown>"),
            (
                "[account_overrides.plainaccount]\ntheme = \"secret-mode\"\n",
                "invalid-value",
                "account_overrides.<account>.theme",
            ),
            (
                "[account_overrides.plainaccount]\nsecret_field = 1\n",
                "unknown-field",
                "account_overrides.<account>.<unknown>",
            ),
            (
                "[colors]\nsecretcolor = 5\n",
                "wrong-type",
                "colors.<unknown>",
            ),
            ("[colors]\nlink = 5\n", "wrong-type", "colors.link"),
            (
                "[shortcuts]\nsecretaction = 5\n",
                "wrong-type",
                "shortcuts.<unknown>",
            ),
            (
                "[shortcuts]\nsearch = 5\n",
                "wrong-type",
                "shortcuts.search",
            ),
        ];
        for (raw, code, field) in cases {
            let diagnostic = config_error(raw);
            assert_eq!(diagnostic.code, code, "{raw}");
            assert_eq!(diagnostic.field.as_deref(), Some(field), "{raw}");
            assert!(
                diagnostic.line.is_some() && diagnostic.column.is_some(),
                "{raw}"
            );
            let shown = format!("{diagnostic:?}");
            for private in ["secret", "plainaccount"] {
                assert!(!shown.contains(private), "{raw}: {shown}");
            }
        }
        let theme = check_theme_source("schema_version = 1\nname = \"x\"\nsecret_table = 1\n");
        let theme = theme.unwrap_err();
        assert_eq!(theme.field.as_deref(), Some("<unknown>"));
        assert_eq!(position("a\nbé\nc", 5), (Some(2), Some(3)));
    }

    #[test]
    fn account_errors_point_at_the_failing_entry_of_the_failing_account() {
        let raw = "[account_overrides.alpha]\nlocal_voice_rooms = [\"!valid:example.org\"]\n\n\
                   [account_overrides.beta]\nlocal_voice_rooms = [\"!fine:example.org\", \"secret room\"]\n";
        let room = config_error(raw);
        assert_eq!(room.code, "invalid-room-id");
        assert_eq!(
            room.field.as_deref(),
            Some("account_overrides.<account>.local_voice_rooms")
        );
        assert_eq!(room.line, Some(5));
        let line = raw.lines().nth(4).unwrap();
        let column = room.column.unwrap();
        assert!(
            line[column - 1..].starts_with("\"secret room\""),
            "{column}"
        );
        let conflict = "[account_overrides.alpha]\nlegacy_element_call_rooms = [\"!a:example.org\"]\n\
                        element_call_matrix_2_0_rooms = [\"!b:example.org\"]\n\n\
                        [account_overrides.beta]\nlegacy_element_call_rooms = [\"!c:example.org\"]\n\
                        element_call_matrix_2_0_rooms = [\"!d:example.org\", \"!c:example.org\"]\n";
        let mode = config_error(conflict);
        assert_eq!(mode.code, "call-mode-conflict");
        assert_eq!(mode.line, Some(7));
        let line = conflict.lines().nth(6).unwrap();
        assert!(line[mode.column.unwrap() - 1..].starts_with("\"!c:example.org\""));
        for diagnostic in [room, mode] {
            let shown = format!("{diagnostic:?}");
            assert!(
                !shown.contains("alpha") && !shown.contains("beta") && !shown.contains("secret")
            );
        }
    }
}
