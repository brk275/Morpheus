//! Settings search over the registry's targets and color tokens. Results
//! carry only registry IDs; choosing one reveals and focuses its control,
//! selects one color token and focuses its hex field, or opens Manage
//! accounts on a safe control. Searching never changes a setting, and no
//! result routes to a URL, a path or an action.

use morpheus_config::registry::{self, AccountsFocus, Requirement, Route, SearchHit};
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

fn show_results(ui: &AppWindow, query: &str) {
    let results: Vec<SettingsResult> =
        registry::search(query, |requirement| available(ui, requirement))
            .into_iter()
            .map(result_item)
            .collect();
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
            // A section can hold several keys; its controls name them.
            key: SharedString::new(),
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
    }
}
