//! Settings search over the registry's targets. Results carry only registry
//! IDs; choosing one reveals and focuses its control, or opens Manage
//! accounts on a safe control. Searching never changes a setting, and no
//! result routes to a URL, a path or an action.

use morpheus_config::registry::{self, AccountsFocus, Requirement, Route, SettingTarget};
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
        registry::search_settings(query, |requirement| available(ui, requirement))
            .into_iter()
            .map(result_item)
            .collect();
    ui.set_settings_search_index(if results.is_empty() { -1 } else { 0 });
    ui.set_settings_search_results(ModelRc::new(VecModel::from(results)));
    ui.set_settings_search_dismissed(false);
}

fn result_item(target: &SettingTarget) -> SettingsResult {
    SettingsResult {
        id: target.id.into(),
        label: target.label.into(),
        breadcrumb: target.breadcrumb.into(),
        description: target.description.into(),
    }
}

/// Reveal the control of one registry target. Unknown or unavailable IDs do
/// nothing. Returns whether a destination was requested.
pub(super) fn reveal(ui: &AppWindow, id: &str) -> bool {
    let Some(target) = registry::setting_target(id) else {
        return false;
    };
    if !available(ui, target.requires) {
        return false;
    }
    ui.set_settings_reveal_id(SharedString::from(target.id));
    match target.route {
        Route::Settings => {
            ui.set_page(1);
            let serial = ui.get_settings_reveal_serial();
            ui.set_settings_reveal_serial(serial.checked_add(1).unwrap_or(1).max(1));
        }
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
