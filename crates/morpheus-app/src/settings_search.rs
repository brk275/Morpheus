//! Settings search over the registry's targets and color tokens. Results
//! carry only registry IDs; choosing one reveals and focuses its control,
//! selects one color token and focuses its hex field, or opens Manage
//! accounts on a safe control. Searching never changes a setting, and no
//! result routes to a URL, a path or an action.

use morpheus_config::registry::{self, AccountsFocus, Requirement, Route, SearchHit, SettingTarget};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, SettingsResult};

pub(super) fn register(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_settings_search(move |query| {
        if let Some(ui) = weak.upgrade() {
            show_results(&ui, query.as_str());
        }
    });
    let weak = ui.as_weak();
    ui.on_settings_search_choose(move |id| {
        if let Some(ui) = weak.upgrade() {
            reveal(&ui, id.as_str());
        }
    });
}

fn available(ui: &AppWindow, requirement: Requirement) -> bool {
    match requirement {
        Requirement::Always => true,
        Requirement::LiveSession => ui.get_live_session(),
    }
}

/// The registry query for what was typed. Metric keys such as
/// `typography.sender_size` are indexed as written; a color key typed as
/// shown in results, `colors.<token>`, searches for its token ID.
fn search_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| {
            word.get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("colors."))
                .map_or(word, |_| &word[7..])
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `config.toml` keys a Settings section edits, for sections whose keys
/// are unambiguous. Results show them, and typing one finds its section. A
/// unit test keeps every ID and key in the registry.
const SECTION_KEYS: &[(&str, &[&str])] = &[
    ("appearance.window_opacity", &["window.opacity_percent"]),
    ("appearance.base_palette", &["appearance.theme", "appearance.theme_pack"]),
    ("appearance.reduce_motion", &["appearance.reduce_motion"]),
    ("appearance.hover_duration", &["appearance.hover_duration_ms"]),
    (
        "appearance.fonts",
        &[
            "appearance.font_family",
            "typography.message_font_family",
            "typography.monospace_font_family",
        ],
    ),
    ("appearance.message_size", &["typography.message_size"]),
    ("appearance.sender_weight", &["typography.sender_weight"]),
    ("appearance.room_header", &["conversation.header"]),
    ("appearance.message_layout", &["chat.layout"]),
    (
        "appearance.timestamps",
        &[
            "chat.timestamp_format",
            "chat.timestamp_brackets",
            "chat.timestamp_width",
        ],
    ),
    ("appearance.geometry", &["geometry.corner_radius"]),
    ("appearance.spacing", &["geometry.message_gap", "geometry.content_padding"]),
    ("appearance.rail_width", &["geometry.rail_width"]),
    ("appearance.font_scale", &["appearance.scale_percent"]),
    ("appearance.sender_colors", &["senders.colors"]),
    ("layout.unread_first", &["layout.sort_unread_first"]),
    ("notifications.desktop", &["notifications.enabled"]),
    ("notifications.previews", &["notifications.show_message_preview"]),
    ("notifications.highlights", &["notifications.highlight_words"]),
    ("media.audio_devices", &["media.microphone_device", "media.speaker_device"]),
    ("media.image_previews", &["media.previews_in_encrypted_rooms"]),
];

fn section_keys(id: &str) -> &'static [&'static str] {
    SECTION_KEYS
        .iter()
        .find(|(section, _)| *section == id)
        .map(|&(_, keys)| keys)
        .unwrap_or(&[])
}

/// The section whose `config.toml` key is exactly `query`, if any.
fn section_for_key(query: &str) -> Option<&'static SettingTarget> {
    let query = query.trim();
    SECTION_KEYS
        .iter()
        .find(|(_, keys)| keys.iter().any(|key| key.eq_ignore_ascii_case(query)))
        .and_then(|(id, _)| registry::setting_target(id))
}

fn show_results(ui: &AppWindow, query: &str) {
    let mut hits =
        registry::search(&search_query(query), |requirement| available(ui, requirement));
    // A typed section key leads with its section.
    if let Some(target) = section_for_key(query)
        .filter(|target| available(ui, target.requires))
        .filter(|target| !hits.iter().any(|hit| hit.id() == target.id))
    {
        hits.insert(0, SearchHit::Setting(target));
        hits.truncate(registry::MAX_SEARCH_RESULTS);
    }
    let results: Vec<SettingsResult> = hits.into_iter().map(result_item).collect();
    ui.set_settings_search_index(if results.is_empty() { -1 } else { 0 });
    ui.set_settings_search_results(ModelRc::new(VecModel::from(results)));
    ui.set_settings_search_dismissed(false);
}

fn result_item(hit: SearchHit) -> SettingsResult {
    match hit {
        SearchHit::Setting(target) => SettingsResult {
            id: target.id.into(),
            label: target.label.into(),
            breadcrumb: target.breadcrumb.into(),
            description: target.description.into(),
            // Empty for a section whose keys are not listed.
            key: section_keys(target.id).join(", ").into(),
        },
        SearchHit::Color(token) => SettingsResult {
            id: hit.id().into(),
            label: token.label.into(),
            breadcrumb: format!(
                "Settings › Appearance › Colors and window › {}",
                token.group.label()
            )
            .into(),
            description: token.description.into(),
            key: format!("colors.{}", token.id).into(),
        },
        SearchHit::Metric(field, editor) => SettingsResult {
            id: hit.id().into(),
            label: editor.label.into(),
            breadcrumb: format!(
                "Settings › Appearance › Typography, geometry and motion › {}",
                editor.group.label()
            )
            .into(),
            description: field.description.into(),
            key: field.key.into(),
        },
    }
}

/// Reveal the control of one registry target, the hex field of one color
/// token, or the value field of one typography or geometry metric. Unknown
/// or unavailable IDs do nothing. Returns whether a destination was requested.
pub(super) fn reveal(ui: &AppWindow, id: &str) -> bool {
    if let Some(token) = registry::color_result(id) {
        // Selecting is presentation only; the file is not touched.
        crate::appearance_editor::select_token(ui, token);
        ui.set_settings_reveal_id(SharedString::from(COLOR_HEX_REVEAL));
        request_settings_reveal(ui);
        ui.set_settings_search_dismissed(true);
        return true;
    }
    if let Some((field, _)) = registry::metric_result(id) {
        // Selecting is presentation only; the file is not touched. The value
        // field reports this result's own ID once it has focus.
        crate::metric_editor::select_metric(ui, field.key);
        ui.set_settings_reveal_id(SharedString::from(METRIC_VALUE_REVEAL));
        request_settings_reveal(ui);
        ui.set_settings_search_dismissed(true);
        return true;
    }
    let Some(target) = registry::setting_target(id) else {
        return false;
    };
    if !available(ui, target.requires) {
        return false;
    }
    ui.set_settings_reveal_id(SharedString::from(target.id));
    match target.route {
        Route::Settings => request_settings_reveal(ui),
        Route::ManageAccounts(focus) => {
            ui.set_manage_accounts_focus(
                match focus {
                    AccountsFocus::Close => "close",
                    AccountsFocus::Add => "add",
                    AccountsFocus::Protection => "protection",
                }
                .into(),
            );
            ui.set_account_menu_open(false);
            ui.set_manage_accounts_open(true);
            // The same read-only refresh the account menu's entry runs.
            ui.invoke_manage_accounts();
        }
    }
    ui.set_settings_search_dismissed(true);
    true
}

/// The reveal ID of the selected token's hex field. It is not a search
/// target of its own: color results reach it after selecting their token.
pub(super) const COLOR_HEX_REVEAL: &str = "appearance.color_hex";

/// The reveal ID of the selected metric's value field. Like the hex field it
/// is reached through a metric result, which it reports once focused.
pub(super) const METRIC_VALUE_REVEAL: &str = "appearance.metric_value";

fn request_settings_reveal(ui: &AppWindow) {
    ui.set_page(1);
    let serial = ui.get_settings_reveal_serial();
    ui.set_settings_reveal_serial(serial.checked_add(1).unwrap_or(1).max(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_field_results_name_their_config_key_and_sections_none() {
        let token = registry::color_result("color:link").expect("the link token");
        assert_eq!(result_item(SearchHit::Color(token)).key, "colors.link");

        for (field, editor) in registry::metric_fields() {
            let item = result_item(SearchHit::Metric(field, editor));
            assert_eq!(item.key, field.key);
            // The key is registry text, never a value or a path.
            assert!(!item.key.contains('/') && !item.key.contains('\\'));
        }

        let section = registry::setting_target("appearance.colors").expect("the colors section");
        assert!(result_item(SearchHit::Setting(section)).key.is_empty());
        let fonts = registry::setting_target("appearance.fonts").expect("the fonts section");
        assert_eq!(
            result_item(SearchHit::Setting(fonts)).key,
            "appearance.font_family, typography.message_font_family, typography.monospace_font_family"
        );
    }

    #[test]
    fn listed_section_keys_are_registry_keys_and_find_their_section() {
        for (id, keys) in SECTION_KEYS {
            assert!(registry::setting_target(id).is_some(), "{id} is not a setting target");
            assert!(!keys.is_empty(), "{id} lists no key");
            for key in *keys {
                assert!(
                    registry::FIELDS.iter().any(|field| field.key == *key),
                    "{key} of {id} is not a registry key"
                );
                assert_eq!(section_for_key(key).map(|target| target.id), Some(*id));
            }
        }
        assert_eq!(
            section_for_key(" Typography.Message_Size ").map(|target| target.id),
            Some("appearance.message_size")
        );
        assert!(section_for_key("typography").is_none());
    }

    #[test]
    fn a_typed_config_key_finds_its_field() {
        assert_eq!(search_query("Colors.link"), "link");
        assert_eq!(search_query("typography.sender_size"), "typography.sender_size");
        assert_eq!(search_query("  colors.presence_online  links "), "presence_online links");
        let finds = |query: &str, id: &str| {
            registry::search(&search_query(query), |_| true)
                .iter()
                .any(|hit| hit.id() == id)
        };
        assert!(finds("colors.link", "color:link"));
        assert!(finds("colors.presence_online", "color:presence_online"));
        assert!(finds(
            "typography.sender_size",
            &format!("{}typography.sender_size", registry::METRIC_RESULT_PREFIX)
        ));
    }
}
