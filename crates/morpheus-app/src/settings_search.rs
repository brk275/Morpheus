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
    let results: Vec<SettingsResult> = registry::search(query, |requirement| {
        available(ui, requirement)
    })
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
        },
    }
}

/// Reveal the control of one registry target, or the hex field of one color
/// token. Unknown or unavailable IDs do nothing. Returns whether a
/// destination was requested.
pub(super) fn reveal(ui: &AppWindow, id: &str) -> bool {
    if let Some(token) = registry::color_result(id) {
        // Selecting is presentation only; the file is not touched.
        crate::appearance_editor::select_token(ui, token);
        ui.set_settings_reveal_id(SharedString::from(COLOR_HEX_REVEAL));
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

fn request_settings_reveal(ui: &AppWindow) {
    ui.set_page(1);
    let serial = ui.get_settings_reveal_serial();
    ui.set_settings_reveal_serial(serial.checked_add(1).unwrap_or(1).max(1));
}
