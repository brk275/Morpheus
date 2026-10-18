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

use morpheus_config::diagnostics::{self, Diagnostic, Report};
use morpheus_config::registry::{
    self, COLOR_TOKENS, FIELDS, Fallback, FieldKind, TokenSource,
};
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

/// Handle `config …` arguments. `None` when the app should start normally;
/// otherwise the process exit status.
pub(crate) fn early_dispatch() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("config")) {
        return None;
    }
    // At most a handful of arguments are meaningful; more is a usage error.
    let rest: Vec<OsString> = args.take(8).collect();
    Some(run(&rest))
}

fn run(args: &[OsString]) -> i32 {
    let mut words = args.iter().map(|arg| arg.to_str());
    let command = words.next().flatten();
    let mut json = false;
    let mut file = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].to_str() {
            Some("--json") => json = true,
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
        Some("help") | Some("--help") | None => {
            print(USAGE);
            if command.is_none() { 2 } else { 0 }
        }
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
        // A theme name reaches here only after it validated as a plain name.
        "theme": report.theme,
        "diagnostics": report.diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    })
}

fn report_text(report: &Report) -> String {
    if report.is_valid() {
        return match &report.theme {
            Some(theme) => format!("Valid: config.toml and theme {theme}."),
            None => "Valid: config.toml.".into(),
        };
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
            },
            "fallback": fallback_json(token.fallback),
            "keywords": token.keywords,
        })).collect::<Vec<_>>(),
        "theme_file": {
            "required_colors": base,
            "optional_roles": roles,
            "sender_colors": { "minimum": 1, "maximum": morpheus_config::MAX_SENDER_COLORS },
        },
        "window_opacity_percent": {
            "minimum": registry::WINDOW_OPACITY_RANGE.start(),
            "maximum": registry::WINDOW_OPACITY_RANGE.end(),
        },
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
        let inactive = if field.inactive { " (stored, not used yet)" } else { "" };
        lines.push(format!(
            "  {} = {}{range}: {}{inactive}",
            field.key, field.default, field.description
        ));
    }
    lines.push(String::new());
    lines.push("Color tokens ([colors] keys):".into());
    for token in COLOR_TOKENS {
        lines.push(format!("  {}: {}. {}", token.id, token.label, token.description));
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
        assert_eq!(run(&args(&["help"])), 0);
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
        assert_eq!(value["color_tokens"].as_array().unwrap().len(), COLOR_TOKENS.len());
        assert_eq!(
            value["diagnostic_codes"].as_array().unwrap().len(),
            diagnostics::CODES.len()
        );
        assert_eq!(value["theme_file"]["required_colors"].as_array().unwrap().len(), 11);
        let text = schema_text();
        for token in COLOR_TOKENS {
            assert!(text.contains(token.id));
        }
        assert_eq!(run(&args(&["schema"])), 0);
        assert_eq!(run(&args(&["schema", "--json"])), 0);
    }
}
