//! Offline diagnostics for `config.toml` and the selected theme file.
//!
//! A diagnostic has a stable code, the file it belongs to, a field path and a
//! line and column. Its message is fixed text: it never quotes a value from
//! the file, and a key that is not a plain identifier is shown as `<key>`.
//! Nothing here creates, repairs or writes a file.

use std::path::Path;

use toml::de::{DeTable, DeValue};

use crate::{
    BuiltInStylePreset, Config, ConfigError, ConfigStore, InputError, MAX_CONFIG_BYTES,
    MAX_THEME_BYTES, ThemePack, read_bounded,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: FileKind,
    /// Stable, documented code such as `unknown-field` or `out-of-range`.
    pub code: &'static str,
    /// Fixed explanation; schema bounds at most, never file content.
    pub message: String,
    /// Dotted field path, for example `geometry.corner_radius`.
    pub field: Option<String>,
    /// One-based line and column of the field or error, when known.
    pub line: Option<usize>,
    pub column: Option<usize>,
}

/// The outcome of checking the configuration and its theme without
/// starting anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
    /// The theme name that was checked, when the configuration names one.
    pub theme: Option<String>,
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
                theme: None,
            };
        }
    };
    let config = match check_config_source(&raw) {
        Ok(config) => config,
        Err(diagnostic) => {
            return Report {
                diagnostics: vec![diagnostic],
                theme: None,
            };
        }
    };
    let Some(name) = config.appearance.theme_pack.clone() else {
        return Report {
            diagnostics: Vec::new(),
            theme: None,
        };
    };
    let diagnostics = match store.theme_path(&name) {
        Err(error) => vec![semantic_diagnostic(FileKind::Config, &raw, &error)],
        Ok(path) => check_theme_file(&path, &name).err().into_iter().collect(),
    };
    Report {
        diagnostics,
        theme: Some(name),
    }
}

fn check_theme_file(path: &Path, name: &str) -> Result<(), Diagnostic> {
    match read_bounded(path, MAX_THEME_BYTES) {
        Ok(raw) => check_theme_source(&raw).map(|_| ()),
        Err(InputError::Missing) if BuiltInStylePreset::ALL
            .iter()
            .any(|preset| preset.theme_name() == name) =>
        {
            Ok(())
        }
        Err(error) => Err(input_diagnostic(FileKind::Theme, error)),
    }
}

/// Parse and validate configuration text.
pub fn check_config_source(raw: &str) -> Result<Config, Diagnostic> {
    let config: Config =
        toml::from_str(raw).map_err(|error| decode_diagnostic(FileKind::Config, raw, &error))?;
    config
        .validate()
        .map_err(|error| semantic_diagnostic(FileKind::Config, raw, &error))?;
    Ok(config)
}

/// Parse and validate theme text.
pub fn check_theme_source(raw: &str) -> Result<ThemePack, Diagnostic> {
    let theme: ThemePack =
        toml::from_str(raw).map_err(|error| decode_diagnostic(FileKind::Theme, raw, &error))?;
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

/// A TOML or schema-shape error. The library's own message can quote the
/// value, so it is only classified, never repeated.
fn decode_diagnostic(file: FileKind, raw: &str, error: &toml::de::Error) -> Diagnostic {
    let text = error.message();
    let (code, message) = if text.starts_with("unknown field") {
        ("unknown-field", "this key is not part of the schema")
    } else if text.starts_with("missing field") {
        ("missing-field", "a required key is missing from this table")
    } else if text.starts_with("invalid type") {
        ("wrong-type", "this value has the wrong type")
    } else if text.starts_with("unknown variant") || text.starts_with("invalid value") {
        ("invalid-value", "this value is not one of the accepted options")
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
    let field = span.and_then(|span| field_at(raw, span.start));
    Diagnostic {
        file,
        code,
        message: message.into(),
        field,
        line,
        column,
    }
}

/// A validation error with its field path, located in the source.
fn semantic_diagnostic(file: FileKind, raw: &str, error: &ConfigError) -> Diagnostic {
    let (code, message, field) = match error {
        ConfigError::Field { field, problem } | ConfigError::ThemeField { field, problem } => {
            (problem.code(), problem.to_string(), Some(field.clone()))
        }
        ConfigError::Version(_) => (
            "schema-version",
            "this version supports schema_version = 1".to_owned(),
            Some("schema_version".to_owned()),
        ),
        ConfigError::Input(error) => return input_diagnostic(file, *error),
        _ => ("invalid", "the file is invalid".to_owned(), None),
    };
    let offset = field.as_deref().and_then(|field| locate(raw, field));
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

/// A key segment safe to show: a plain schema-style identifier.
fn shown_key(key: &str) -> String {
    let plain = !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if plain { key.to_owned() } else { "<key>".to_owned() }
}

/// The dotted path of the narrowest key or value that contains `offset`.
/// A `[table]` value spans only its header, so leaves are compared, not
/// table bodies.
fn field_at(raw: &str, offset: usize) -> Option<String> {
    let root = DeTable::parse(raw).ok()?;
    let mut best = None;
    collect_at(root.get_ref(), &mut Vec::new(), offset, &mut best);
    best.map(|(_, path): (usize, Vec<String>)| path.join("."))
}

fn collect_at(
    table: &DeTable<'_>,
    path: &mut Vec<String>,
    offset: usize,
    best: &mut Option<(usize, Vec<String>)>,
) {
    for (key, value) in table.iter() {
        path.push(shown_key(key.get_ref()));
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

/// The byte offset of the key named by a dotted field path such as
/// `colors.link`, `senders.colors[1]` or `account_overrides.local_voice_rooms`
/// (where the account table level is matched by any key).
fn locate(raw: &str, field: &str) -> Option<usize> {
    let root = DeTable::parse(raw).ok()?;
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
    // Account tables are keyed by local account IDs, which a field path
    // never names; the first account holding the field is reported.
    match value.get_ref() {
        DeValue::Table(inner) if name == "account_overrides" => inner
            .iter()
            .filter_map(|(_, account)| match account.get_ref() {
                DeValue::Table(account) => locate_in(account, rest),
                _ => None,
            })
            .next()
            .or(Some(key.span().start)),
        DeValue::Table(inner) => locate_in(inner, rest).or(Some(key.span().start)),
        _ => Some(key.span().start),
    }
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
            Some("account_overrides.local_voice_rooms")
        );
        assert_eq!(room.line, Some(2));
        assert!(!format!("{room:?}").contains("private") && !format!("{room:?}").contains("secret"));
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
        assert!(!store.path().exists(), "validation must not create the file");
        std::fs::write(store.path(), "[appearance]\ntheme_pack = \"aurora\"\n").unwrap();
        let theme = check_store(&store);
        assert_eq!(theme.theme.as_deref(), Some("aurora"));
        assert_eq!(theme.diagnostics[0].file, FileKind::Theme);
        assert_eq!(theme.diagnostics[0].code, "missing-file");
        assert!(!store.themes_dir().unwrap().exists());
        // A built-in name without an override file uses the embedded theme.
        std::fs::write(store.path(), "[appearance]\ntheme_pack = \"paper\"\n").unwrap();
        assert!(check_store(&store).is_valid());
        let oversized = format!("# {}\n", "x".repeat(MAX_CONFIG_BYTES as usize));
        std::fs::write(store.path(), oversized).unwrap();
        assert_eq!(check_store(&store).diagnostics[0].code, "file-too-large");
    }

    #[test]
    fn unusual_keys_are_never_shown() {
        assert_eq!(shown_key("corner_radius"), "corner_radius");
        assert_eq!(shown_key("@someone:example.org"), "<key>");
        assert_eq!(shown_key("Secret Key"), "<key>");
        assert_eq!(shown_key(&"a".repeat(65)), "<key>");
        assert_eq!(position("a\nbé\nc", 5), (Some(2), Some(3)));
    }
}
