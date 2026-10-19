//! Offline `config validate` and `config schema`.
//!
//! `main` dispatches here before the screenshot preview, the updater, the
//! startup session, the window, the account store, the credential vault,
//! sync and any media device start. Nothing here creates, repairs or writes a
//! file: the configuration and its theme are read with the same bounded
//! reader as live reload. Output names fields, codes and positions, never a
//! path, a raw value or file content.
//!
//! On Windows the release app is a GUI-subsystem program without a console;
//! its output is visible only when redirected to a file or pipe, and the exit
//! code is available to the caller either way.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use morpheus_config::diagnostics::{self, Diagnostic, Report, ThemeSource};
use morpheus_config::registry::{self, COLOR_TOKENS, FIELDS, Fallback, FieldKind, TokenSource};
use morpheus_config::{ConfigStore, MAX_CONFIG_BYTES, MAX_THEME_BYTES};
use serde_json::{Value, json};

const USAGE: &str = "Usage:
  morpheus config validate [--json] [--file CONFIG.toml]
  morpheus config schema [--json]

validate  Check config.toml and the theme it selects. Exit status 0 when
          valid, 1 when not, 2 for a usage error. Nothing is written.
--file    Check another configuration file instead of the standard one; its
          themes folder is the one beside it.
schema    Print the field, color token and diagnostic reference.";

/// The longest meaningful argument list: `validate --json --file PATH`.
const MAX_ARGS: usize = 4;

/// Handle `config …` arguments. `None` when the app should start normally;
/// otherwise the process exit status.
pub(crate) fn early_dispatch() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("config")) {
        return None;
    }
    Some(run(&bounded(args)))
}

/// Collect one argument more than can be meaningful, so `run` sees an
/// excess and refuses it without the list growing with the input.
fn bounded(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    args.take(MAX_ARGS + 1).collect()
}

fn run(args: &[OsString]) -> i32 {
    if args.len() > MAX_ARGS {
        return usage();
    }
    let command = args.first().and_then(|arg| arg.to_str());
    let mut json = false;
    let mut file = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].to_str() {
            Some("--json") if !json => json = true,
            Some("--file") if command == Some("validate") && file.is_none() => {
                let Some(path) = args.get(index + 1) else {
                    return usage();
                };
                file = Some(PathBuf::from(path));
                index += 1;
            }
            _ => return usage(),
        }
        index += 1;
    }
    match command {
        Some("validate") => validate(file, json),
        Some("schema") if file.is_none() => {
            print(&if json {
                pretty(&schema())
            } else {
                schema_text()
            });
            0
        }
        Some("help") | Some("--help") if !json && file.is_none() => {
            print(USAGE);
            0
        }
        // No command, a non-UTF-8 one or an unknown one.
        _ => usage(),
    }
}

fn usage() -> i32 {
    print(USAGE);
    2
}

fn print(text: &str) {
    // A missing console is not an error worth failing over.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
}

fn validate(file: Option<PathBuf>, json: bool) -> i32 {
    let store = match file {
        Some(path) => ConfigStore::at(path),
        // Computing the standard path creates no folder or file.
        None => match ConfigStore::standard() {
            Ok(store) => store,
            Err(_) => {
                print("The operating system's configuration folder could not be located.");
                return 1;
            }
        },
    };
    let report = diagnostics::check_store(&store);
    print(&if json {
        pretty(&report_json(&report))
    } else {
        report_text(&report)
    });
    i32::from(!report.is_valid())
}

fn diagnostic_json(diagnostic: &Diagnostic) -> Value {
    json!({
        "file": diagnostic.file.id(),
        "code": diagnostic.code,
        "message": diagnostic.message,
        "field": diagnostic.field,
        "line": diagnostic.line,
        "column": diagnostic.column,
    })
}

fn report_json(report: &Report) -> Value {
    json!({
        "format": "morpheus-config-diagnostics",
        "version": 1,
        "valid": report.is_valid(),
        // A fixed category; the configured theme name is file content.
        "theme": report.theme.id(),
        "diagnostics": report.diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    })
}

fn report_text(report: &Report) -> String {
    if report.is_valid() {
        return match report.theme {
            ThemeSource::None => "Valid: config.toml.",
            ThemeSource::BuiltIn => "Valid: config.toml; it selects a built-in theme.",
            ThemeSource::BuiltInOverride => {
                "Valid: config.toml and the file overriding its built-in theme."
            }
            ThemeSource::Custom => "Valid: config.toml and its custom theme file.",
        }
        .into();
    }
    let mut lines = vec!["Invalid; the app keeps its last valid appearance.".to_owned()];
    for diagnostic in &report.diagnostics {
        let position = match (diagnostic.line, diagnostic.column) {
            (Some(line), Some(column)) => format!(" line {line}, column {column}:"),
            _ => String::new(),
        };
        let field = diagnostic
            .field
            .as_deref()
            .map_or_else(String::new, |field| format!(" {field}"));
        lines.push(format!(
            "{} [{}]{position}{field} {}",
            diagnostic.file.id(),
            diagnostic.code,
            diagnostic.message
        ));
    }
    lines.join("\n")
}

fn kind_json(kind: FieldKind) -> Value {
    match kind {
        FieldKind::Bool => json!({ "type": "boolean" }),
        FieldKind::Integer { minimum, maximum } => {
            json!({ "type": "integer", "minimum": minimum, "maximum": maximum })
        }
        FieldKind::Enum(values) => json!({ "type": "string", "values": values }),
        FieldKind::FontFamily => json!({ "type": "font-family", "max_bytes": 128 }),
        FieldKind::ThemeName => json!({ "type": "theme-name", "max_chars": 64 }),
        FieldKind::Color => json!({ "type": "color" }),
        FieldKind::ColorList => json!({
            "type": "color-list",
            "minimum": 1,
            "maximum": morpheus_config::MAX_SENDER_COLORS,
        }),
        FieldKind::DeviceId => json!({ "type": "device-id", "max_bytes": 4096 }),
        FieldKind::Text => json!({ "type": "string" }),
        FieldKind::TextList => json!({ "type": "string-list" }),
        FieldKind::RoomIdList => json!({ "type": "room-id-list" }),
        FieldKind::Table => json!({ "type": "table" }),
    }
}

fn fallback_json(fallback: Fallback) -> Value {
    match fallback {
        Fallback::Palette => json!({ "kind": "palette" }),
        Fallback::Token(token) => json!({ "kind": "token", "token": token }),
        Fallback::Legacy { example } => json!({ "kind": "built-in", "example": example }),
    }
}

/// The machine-readable reference, generated from the registry.
fn schema() -> Value {
    let base = COLOR_TOKENS
        .iter()
        .filter(|token| token.source == TokenSource::Base)
        .map(|token| token.id)
        .collect::<Vec<_>>();
    let roles = COLOR_TOKENS
        .iter()
        .filter(|token| token.source == TokenSource::Role)
        .map(|token| token.id)
        .collect::<Vec<_>>();
    let states = COLOR_TOKENS
        .iter()
        .filter(|token| token.source == TokenSource::State)
        .map(|token| token.id)
        .collect::<Vec<_>>();
    json!({
        "format": "morpheus-config-schema",
        "schema_version": morpheus_config::SCHEMA_VERSION,
        "hex_color": {
            "forms": ["#RRGGBB", "#RRGGBBAA"],
            "alpha": "last pair; 00 is transparent and FF opaque",
            "case": "either; the editor writes uppercase",
        },
        "limits": { "config_bytes": MAX_CONFIG_BYTES, "theme_bytes": MAX_THEME_BYTES },
        "inheritance": [
            "built-in palette of the effective theme mode",
            "theme pack named by appearance.theme_pack",
            "[colors] overrides in config.toml",
        ],
        "fields": FIELDS.iter().map(|field| {
            let mut value = kind_json(field.kind);
            value["key"] = json!(field.key);
            value["default"] = json!(field.default);
            value["description"] = json!(field.description);
            value["inactive"] = json!(field.inactive);
            if let Some(editor) = &field.editor {
                value["editor"] = json!({
                    "group": editor.group.id(),
                    "label": editor.label,
                    "unit": editor.unit.spoken(),
                    "step": editor.step,
                    "omitted": editor.inherit.map(|inherit| inherit.text),
                    "omitted_start": editor.inherit.map(|inherit| inherit.start),
                    "keywords": editor.keywords,
                });
            }
            value
        }).collect::<Vec<_>>(),
        "color_tokens": COLOR_TOKENS.iter().map(|token| json!({
            "id": token.id,
            "group": token.group.id(),
            "label": token.label,
            "description": token.description,
            "theme_source": match token.source {
                TokenSource::Base => "colors",
                TokenSource::Role => "roles",
                TokenSource::State => "states",
            },
            "fallback": fallback_json(token.fallback),
            "keywords": token.keywords,
        })).collect::<Vec<_>>(),
        "theme_file": {
            "required_colors": base,
            "optional_roles": roles,
            "optional_states": states,
            "sender_colors": { "minimum": 1, "maximum": morpheus_config::MAX_SENDER_COLORS },
        },
        "window_opacity_percent": {
            "minimum": registry::WINDOW_OPACITY_RANGE.start(),
            "maximum": registry::WINDOW_OPACITY_RANGE.end(),
        },
        "shortcut_actions": morpheus_config::ShortcutAction::ALL.iter().map(|action| json!({
            "name": action.name(),
            "label": action.label(),
            "group": action.group().label(),
            "default": action.default_binding(),
        })).collect::<Vec<_>>(),
        // Style profiles: the keys a profile file may contain, as config.toml
        // keys plus `profile_version` and `name`. See docs/STYLE_PROFILES.md.
        "style_profile": json!({
            "profile_version": morpheus_config::style_profiles::PROFILE_VERSION,
            "keys": morpheus_config::style_profiles::schema_paths(),
            "max_bytes": morpheus_config::style_profiles::MAX_PROFILE_BYTES,
            "max_profiles": morpheus_config::style_profiles::MAX_PROFILES,
            "max_name_chars": morpheus_config::style_profiles::MAX_PROFILE_NAME_CHARS,
        }),
        "diagnostic_codes": diagnostics::CODES.iter().map(|(code, meaning)| json!({
            "code": code,
            "meaning": meaning,
        })).collect::<Vec<_>>(),
    })
}

fn schema_text() -> String {
    let mut lines = vec![
        format!(
            "config.toml schema {}; colors are #RRGGBB or #RRGGBBAA, alpha last.",
            morpheus_config::SCHEMA_VERSION
        ),
        String::new(),
        "Fields (key = default: description):".into(),
    ];
    for field in FIELDS {
        let range = match field.kind {
            FieldKind::Integer { minimum, maximum } => format!(" [{minimum}–{maximum}]"),
            FieldKind::Enum(values) => format!(" [{}]", values.join(", ")),
            _ => String::new(),
        };
        let inactive = if field.inactive {
            " (stored, not used yet)"
        } else {
            ""
        };
        lines.push(format!(
            "  {} = {}{range}: {}{inactive}",
            field.key, field.default, field.description
        ));
    }
    lines.push(String::new());
    lines.push("Shortcut actions ([shortcuts] keys = default):".into());
    for action in morpheus_config::ShortcutAction::ALL {
        lines.push(format!(
            "  {} = {}: {}",
            action.name(),
            action.default_binding().unwrap_or("(none)"),
            action.label()
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "Style profile keys (profile_version = {}; omitted keys use the default):",
        morpheus_config::style_profiles::PROFILE_VERSION
    ));
    for key in morpheus_config::style_profiles::schema_paths() {
        lines.push(format!("  {key}"));
    }
    lines.push(String::new());
    lines.push("Color tokens ([colors] keys):".into());
    for token in COLOR_TOKENS {
        lines.push(format!(
            "  {}: {}. {}",
            token.id, token.label, token.description
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn usage_errors_and_help_never_start_validation() {
        assert_eq!(run(&args(&[])), 2);
        assert_eq!(run(&args(&["unknown"])), 2);
        assert_eq!(run(&args(&["validate", "--file"])), 2);
        assert_eq!(run(&args(&["schema", "--file", "x"])), 2);
        assert_eq!(run(&args(&["validate", "--json", "--extra"])), 2);
        assert_eq!(run(&args(&["schema", "--json", "--json"])), 2);
        assert_eq!(run(&args(&["help", "--json"])), 2);
        assert_eq!(run(&args(&["help"])), 0);
    }

    #[test]
    fn excess_arguments_are_refused_rather_than_dropped() {
        // Seven repeated flags and an invalid word after them: the old
        // eight-argument cut dropped the invalid word and succeeded.
        let mut words = vec!["schema"];
        words.extend(["--json"; 7]);
        words.push("--not-an-option");
        let collected = bounded(args(&words).into_iter());
        assert_eq!(collected.len(), MAX_ARGS + 1);
        assert_eq!(run(&collected), 2);
        // Only one more than the limit is ever collected.
        assert_eq!(
            bounded(std::iter::repeat(OsString::from("--json"))).len(),
            MAX_ARGS + 1
        );
        assert_eq!(
            run(&args(&["validate", "--json", "--file", "x", "--json"])),
            2
        );
        let longest = bounded(args(&["validate", "--json", "--file", "x"]).into_iter());
        assert_eq!(longest.len(), MAX_ARGS);
    }

    #[test]
    fn a_valid_custom_theme_name_never_appears_in_output() {
        let secret = "s3cr3t-k9Qv7ZpXw2-token";
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, format!("[appearance]\ntheme_pack = \"{secret}\"\n")).unwrap();
        let store = ConfigStore::at(&path);
        std::fs::create_dir(store.themes_dir().unwrap()).unwrap();
        let theme = include_str!("../../../themes/aurora.toml").replacen(
            "name = \"Aurora\"",
            &format!("name = \"{secret}\""),
            1,
        );
        assert!(theme.contains(secret));
        std::fs::write(store.theme_path(secret).unwrap(), theme).unwrap();
        let report = diagnostics::check_store(&store);
        assert!(report.is_valid());
        assert_eq!(report.theme, ThemeSource::Custom);
        let text = report_text(&report);
        let json = pretty(&report_json(&report));
        assert_eq!(text, "Valid: config.toml and its custom theme file.");
        assert!(json.contains("\"theme\": \"custom\""));
        for output in [&text, &json] {
            assert!(!output.contains(secret) && !output.contains("s3cr3t"));
        }
        // A broken custom theme reports a fixed category and code, not the name.
        std::fs::write(store.theme_path(secret).unwrap(), "schema_version = 1\n").unwrap();
        let broken = diagnostics::check_store(&store);
        assert!(!broken.is_valid());
        for output in [report_text(&broken), pretty(&report_json(&broken))] {
            assert!(!output.contains(secret) && !output.contains("s3cr3t"));
        }
    }

    #[test]
    fn validation_of_a_file_is_read_only_and_reports_fixed_codes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let path_text = path.to_str().unwrap();
        assert_eq!(run(&args(&["validate", "--file", path_text])), 1);
        assert!(!path.exists(), "validation must not create the file");
        std::fs::write(&path, "[geometry]\ncorner_radius = 99\n").unwrap();
        let report = diagnostics::check_store(&ConfigStore::at(&path));
        let value = report_json(&report);
        assert_eq!(value["valid"], false);
        assert_eq!(value["diagnostics"][0]["code"], "out-of-range");
        assert_eq!(value["diagnostics"][0]["field"], "geometry.corner_radius");
        assert_eq!(value["diagnostics"][0]["line"], 2);
        let text = report_text(&report);
        assert!(text.contains("geometry.corner_radius") && !text.contains("99"));
        assert!(!text.contains(path_text) && !pretty(&value).contains(path_text));
        assert_eq!(run(&args(&["validate", "--json", "--file", path_text])), 1);
        std::fs::write(&path, "[geometry]\ncorner_radius = 9\n").unwrap();
        assert_eq!(run(&args(&["validate", "--file", path_text])), 0);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[geometry]\ncorner_radius = 9\n"
        );
        assert!(!directory.path().join("themes").exists());
    }

    #[test]
    fn the_schema_lists_every_field_token_and_code_from_the_registry() {
        let value = schema();
        assert_eq!(value["fields"].as_array().unwrap().len(), FIELDS.len());
        assert_eq!(
            value["color_tokens"].as_array().unwrap().len(),
            COLOR_TOKENS.len()
        );
        assert_eq!(
            value["diagnostic_codes"].as_array().unwrap().len(),
            diagnostics::CODES.len()
        );
        assert_eq!(
            value["theme_file"]["required_colors"]
                .as_array()
                .unwrap()
                .len(),
            11
        );
        let text = schema_text();
        for token in COLOR_TOKENS {
            assert!(text.contains(token.id));
        }
        // Style profiles list their keys, appearance only.
        let profile = &value["style_profile"];
        assert_eq!(profile["profile_version"], 1);
        let keys: Vec<&str> = profile["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key.as_str().unwrap())
            .collect();
        assert!(keys.contains(&"typography.message_size") && keys.contains(&"colors.<token>"));
        for private in [
            "appearance.scale_percent",
            "geometry.focus_width",
            "shortcuts.<action>",
        ] {
            assert!(!keys.contains(&private), "{private}");
        }
        assert!(keys.iter().all(|key| !key.starts_with("notifications")
            && !key.starts_with("media")
            && !key.starts_with("account_overrides")));
        assert!(text.contains("Style profile keys"));
        // Editor metrics carry the same metadata the Settings workspace shows.
        let fields = value["fields"].as_array().unwrap();
        for (field, editor) in registry::metric_fields() {
            let entry = fields
                .iter()
                .find(|entry| entry["key"] == field.key)
                .unwrap();
            assert_eq!(entry["editor"]["label"], editor.label);
            assert_eq!(entry["editor"]["group"], editor.group.id());
            assert!(text.contains(field.key));
        }
        assert!(
            fields
                .iter()
                .filter(|entry| entry.get("editor").is_some())
                .count()
                == registry::metric_fields().count()
        );
        assert_eq!(run(&args(&["schema"])), 0);
        assert_eq!(run(&args(&["schema", "--json"])), 0);
    }
}
