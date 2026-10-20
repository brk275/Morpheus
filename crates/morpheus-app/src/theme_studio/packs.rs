//! Theme packs in the studio: import a palette file the person chooses,
//! preview it over the current look, then install and apply it or cancel;
//! and export the palette in use. The file format, bounds and every file
//! operation live in `morpheus_config::style_profiles::theme_packs`.
//!
//! A preview saves nothing and shows the pack only until it is applied or
//! cancelled. It ends, restoring the look it replaced, on an account switch,
//! and ends without restoring when any settings change is accepted (the
//! accepted configuration is then shown). It excludes the studio's draft and
//! a style profile preview. Apply installs the pack into the themes folder
//! and then saves `appearance.theme_pack` like any explicit apply.
//!
//! Existing installed packs are never replaced. A preview whose name is
//! already installed remains available to inspect and cancel, and asks for
//! a different name in the imported TOML file. Installation always uses a
//! no-clobber write, so a file that appears after the preview is kept too.
//!
//! Exports never replace a file. Messages name the pack by its own display
//! name, never by a file name or path.
//!
//! Every step after a file dialog is fenced by the [`Scope`] captured
//! before the dialog opened: the appearance epoch, the account and the
//! operation (see `scope`).
//! - An import's answer starts a preview only in that scope. After an
//!   account switch (A -> B -> A included), a detach or a new settings
//!   context, it is ignored and says so.
//! - The preview keeps that scope, and Apply checks it again itself rather
//!   than trusting that every change was notified.
//! - An export's answer is admitted for writing only in that scope and only
//!   while the pack in use is still the one captured. Work not admitted
//!   writes nothing and says so.
//! - An admitted export is one bounded write (at most 64 KiB, no-clobber)
//!   on a worker thread, never on the UI thread. It has explicit state:
//!   - not admitted: nothing written;
//!   - admitted and writing: `State::writing` holds its scope, and the
//!     dialog permit and the busy state stay held;
//!   - done: reported once.
//!   A write once admitted runs to completion even if the account or
//!   settings change meanwhile. Its result is then reported without naming
//!   the pack, and never as "nothing written" after a real write.
//!
//! A native file dialog cannot be cancelled from here; a stale one stays
//! open and its answer is ignored.

use std::cell::RefCell;
use std::path::PathBuf;

use morpheus_config::style_profiles::{
    InstalledPack, PackError, export_theme_pack, import_theme_pack, install_theme_pack,
    installed_theme_pack, pack_name_from, read_theme_pack,
};
use morpheus_config::{Config, ConfigStore, ThemePack};
use slint::ComponentHandle;

use super::scope::{self, Scope};
use crate::AppWindow;
use crate::style_profiles::DialogPermit;

/// Used when a pack's display name has no letter or digit to name its file.
const FALLBACK_NAME: &str = "imported-pack";

struct Preview {
    /// The installed file stem: `themes/<name>.toml`.
    name: String,
    /// The pack's own display name, shown to the person.
    display: String,
    pack: ThemePack,
    /// The look this preview replaced, restored by Cancel.
    base: (Config, Option<ThemePack>),
    /// The scope of the import that started this preview, captured before
    /// its file dialog opened. Apply acts only while it is current.
    scope: Scope,
    /// The installed pack of this name. A present snapshot makes this a
    /// preview-only name collision, never permission to overwrite it.
    replaces: Option<InstalledPack>,
}

#[derive(Default)]
struct State {
    store: Option<ConfigStore>,
    preview: Option<Preview>,
    /// The export admitted in this scope whose write is running on a
    /// worker; reported once when it ends.
    writing: Option<Scope>,
}

thread_local! {
    // Slint callbacks and hooks run on the UI thread only.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(update: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| update(&mut state.borrow_mut()))
}

pub(super) fn register(ui: &AppWindow, store: &ConfigStore) {
    with(|state| state.store = Some(store.clone()));
    let weak = ui.as_weak();
    ui.on_studio_pack_import(move || {
        if let Some(ui) = weak.upgrade() {
            import(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_pack_export(move || {
        if let Some(ui) = weak.upgrade() {
            export(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_pack_apply(move || {
        if let Some(ui) = weak.upgrade() {
            apply(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_pack_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            cancel(
                &ui,
                "The theme pack preview was cancelled; your look is restored.",
            );
        }
    });
    project(ui);
}

pub(super) fn previewing() -> bool {
    with(|state| state.preview.is_some())
}

fn message(ui: &AppWindow, text: impl Into<String>, error: bool) {
    let text = text.into();
    ui.set_studio_pack_message(text.as_str().into());
    ui.set_studio_pack_message_error(error && !text.is_empty());
}

fn project(ui: &AppWindow) {
    let (name, replaces) = with(|state| {
        state
            .preview
            .as_ref()
            .map_or((String::new(), false), |preview| {
                (preview.display.clone(), preview.replaces.is_some())
            })
    });
    ui.set_studio_pack_preview_name(name.into());
    ui.set_studio_pack_replaces(replaces);
}

/// Why a pack cannot be previewed or applied now, or `None`.
fn blocked() -> Option<&'static str> {
    if super::drafting() {
        Some("Apply or discard your unsaved appearance changes first.")
    } else if crate::style_profiles::previewing() {
        Some("Apply or cancel the style profile preview first.")
    } else {
        None
    }
}

fn import(ui: &AppWindow) {
    if with(|state| state.store.is_none()) {
        return;
    }
    if let Some(reason) = blocked() {
        message(ui, reason, true);
        return;
    }
    let Some(permit) = DialogPermit::acquire() else {
        message(ui, "A file dialog is already open.", true);
        return;
    };
    // Captured before the dialog opens: its answer belongs to this account,
    // epoch and operation only.
    let opened = scope::begin_dialog(ui);
    ui.set_studio_pack_busy(true);
    let weak = ui.as_weak();
    let dialog_scope = opened.clone();
    let spawned = std::thread::Builder::new()
        .name("morpheus-theme-pack-import".into())
        .spawn(move || {
            // macOS runs the panel on the main thread for this worker.
            let chosen = rfd::FileDialog::new()
                .set_title("Import a theme pack")
                .add_filter("Morpheus theme pack", &["toml"])
                .pick_file();
            // Reading the chosen file writes nothing; whether the result is
            // used is decided on the UI thread.
            let result =
                chosen.map(|path| import_theme_pack(&path).map_err(|error| error.to_string()));
            drop(permit);
            let _ = weak.upgrade_in_event_loop(move |ui| finish_import(&ui, dialog_scope, result));
        });
    if spawned.is_err() {
        scope::finish_dialog(ui, &opened);
        ui.set_studio_pack_busy(false);
        message(ui, "The file dialog could not open.", true);
    }
}

/// The import dialog opened in `opened` returned. Its answer starts a
/// preview only while that scope is current. A dialog that outlived it
/// never previews in another account or settings context: it is ignored and
/// says so.
fn finish_import(ui: &AppWindow, opened: Scope, result: Option<Result<ThemePack, String>>) {
    ui.set_studio_pack_busy(false);
    if !scope::finish_dialog(ui, &opened) {
        if result.is_some() {
            message(
                ui,
                "The theme pack you chose was not opened because the account or settings changed while the file dialog was open. Nothing was changed.",
                false,
            );
        }
        return;
    }
    match result {
        None => {}
        Some(Err(problem)) => message(
            ui,
            format!("Could not import that file: {problem}. Nothing was changed."),
            true,
        ),
        Some(Ok(pack)) => start_preview(ui, opened, pack),
    }
}

/// The display name shown for a pack: its own name, bounded and without
/// control or bidirectional formatting characters.
fn display_name(pack: &ThemePack) -> String {
    let shown: String = pack
        .name
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(*character,
                    '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        .take(48)
        .collect();
    let shown = shown.trim();
    if shown.is_empty() {
        "Imported pack".to_owned()
    } else {
        shown.to_owned()
    }
}

fn start_preview(ui: &AppWindow, opened: Scope, pack: ThemePack) {
    // Checked again: the dialog may have stayed open while something began.
    if let Some(reason) = blocked() {
        message(ui, reason, true);
        return;
    }
    if !scope::current(ui, &opened) {
        return;
    }
    let Some(store) = with(|state| state.store.clone()) else {
        return;
    };
    let Some(base) = crate::appearance::remembered_theme_source() else {
        message(ui, "There is no accepted look to preview over yet.", true);
        return;
    };
    let name = pack_name_from(&pack.name).unwrap_or_else(|| FALLBACK_NAME.to_owned());
    let replaces = match installed_theme_pack(&store, &name) {
        Ok(installed) => installed,
        Err(error) => {
            message(
                ui,
                format!("Could not preview the theme pack: {error}."),
                true,
            );
            return;
        }
    };
    let mut candidate = base.0.clone();
    candidate.appearance.theme_pack = Some(name.clone());
    if let Err(error) = candidate.validate() {
        message(
            ui,
            format!("Could not preview the theme pack: {error}."),
            true,
        );
        return;
    }
    let display = display_name(&pack);
    let shown = pack.clone();
    let previous = with(|state| state.preview.take());
    // A second import replaces the first preview and keeps the original
    // look, when that preview belongs to the same scope.
    let base = match previous {
        Some(previous) if scope::current(ui, &previous.scope) => previous.base,
        _ => base,
    };
    with(|state| {
        state.preview = Some(Preview {
            name,
            display: display.clone(),
            pack,
            base,
            scope: opened,
            replaces,
        });
    });
    crate::apply_appearance(ui, &candidate, Some(&shown), false);
    message(
        ui,
        format!("Previewing the {display} theme pack. Nothing is saved until you apply it."),
        false,
    );
    project(ui);
}

/// End the preview and show the look it replaced. `note` replaces the
/// message when not empty.
fn cancel(ui: &AppWindow, note: &str) {
    let Some(preview) = with(|state| state.preview.take()) else {
        return;
    };
    let (config, theme) = &preview.base;
    crate::apply_appearance(ui, config, theme.as_ref(), true);
    if !note.is_empty() {
        message(ui, note, false);
    }
    project(ui);
}

fn apply(ui: &AppWindow) {
    let Some(store) = with(|state| state.store.clone()) else {
        return;
    };
    let Some((name, display, pack, replaces, opened)) = with(|state| {
        state.preview.as_ref().map(|preview| {
            (
                preview.name.clone(),
                preview.display.clone(),
                preview.pack.clone(),
                preview.replaces.clone(),
                preview.scope.clone(),
            )
        })
    }) else {
        return;
    };
    // The exact scope the import started in, checked here rather than
    // trusting that every account or settings change was notified. Nothing
    // has been written yet.
    if !scope::current(ui, &opened) {
        cancel(
            ui,
            "The theme pack preview ended because the account or settings changed. Nothing was installed or applied.",
        );
        return;
    }
    // Admitted: from here this step installs and saves, synchronously on the
    // UI thread, and its messages say what was actually written.
    if let Err(error) = install_theme_pack(&store, &name, &pack, replaces.as_ref()) {
        match error {
            // A name collision is never overwrite permission. Keep the
            // preview, say how to import under a new name, and write nothing.
            PackError::ReplacementUnavailable | PackError::NameTaken => rebind(ui, &store, &name),
            error => {
                cancel(ui, "");
                message(
                    ui,
                    format!("Could not install the theme pack: {error}. Nothing was changed."),
                    true,
                );
            }
        }
        return;
    }
    // The save below shows the new look; the preview ends without restoring.
    let preview = with(|state| state.preview.take());
    let saved = crate::write_config_edit(ui, &store, |config| {
        config.appearance.theme_pack = Some(name.clone());
        // A missing pack can already have been selected in config.toml.
        // Its first installation must still reload and apply that file.
        true
    });
    match saved {
        Some(_) => message(
            ui,
            format!("Installed and applied the {display} theme pack."),
            false,
        ),
        None => {
            if let Some(preview) = preview {
                let (config, theme) = &preview.base;
                crate::apply_appearance(ui, config, theme.as_ref(), true);
            }
            message(
                ui,
                format!(
                    "The {display} theme pack was installed, but the settings file has a problem (shown above), so it is not applied."
                ),
                true,
            );
        }
    }
    project(ui);
}

/// Apply found an installed name collision and wrote nothing. Refresh the
/// collision state so the preview explains how to preserve both packs.
fn rebind(ui: &AppWindow, store: &ConfigStore, name: &str) {
    let (text, keep) = match installed_theme_pack(store, name) {
        Ok(Some(installed)) => {
            with(|state| {
                if let Some(preview) = state.preview.as_mut() {
                    preview.replaces = Some(installed);
                }
            });
            (
                "An installed theme pack already uses this name, and it was kept. Change the name field in the imported TOML file, then import it again to keep both packs.",
                true,
            )
        }
        Ok(None) => {
            with(|state| {
                if let Some(preview) = state.preview.as_mut() {
                    preview.replaces = None;
                }
            });
            (
                "The installed pack of this name was removed. Nothing was written; Apply now installs this preview as a new pack without replacing anything.",
                true,
            )
        }
        Err(_) => (
            "Nothing was replaced: the installed theme pack of the same name changed and cannot be read now. Nothing was changed.",
            false,
        ),
    };
    if !keep {
        cancel(ui, "");
    }
    message(ui, text, true);
    project(ui);
}

/// Called before every accepted configuration is shown: a settings change
/// ends a preview, and the accepted look is shown instead of restoring.
pub(super) fn config_accepted(ui: &AppWindow) {
    if with(|state| state.preview.take()).is_some() {
        message(
            ui,
            "The theme pack preview ended because the settings changed.",
            false,
        );
        project(ui);
    }
}

/// Called whenever the active account is published.
///
/// Also the scope hook: every account publication and detach is observed
/// here, so A -> B -> A ends every scope captured before it.
pub(super) fn check_account(ui: &AppWindow) {
    scope::observe(ui);
    let stale = with(|state| state.preview.as_ref().map(|preview| preview.scope.clone()))
        .is_some_and(|opened| !scope::current(ui, &opened));
    if stale {
        cancel(ui, "Switching accounts cancelled the theme pack preview.");
    }
}

fn export(ui: &AppWindow) {
    let Some(store) = with(|state| state.store.clone()) else {
        return;
    };
    // The previewed pack is not installed yet; export the look in use.
    if previewing() {
        message(ui, "Apply or cancel the theme pack preview first.", true);
        return;
    }
    let Some(name) = crate::appearance::remembered_theme_source()
        .and_then(|(config, _)| config.appearance.theme_pack)
    else {
        message(
            ui,
            "The built-in colors are not a theme pack file. Choose a starting point or import a pack first.",
            true,
        );
        return;
    };
    let pack = match read_theme_pack(&store, &name) {
        Ok(pack) => pack,
        Err(error) => {
            message(
                ui,
                format!("Could not export the theme pack: {error}."),
                true,
            );
            return;
        }
    };
    let Some(permit) = DialogPermit::acquire() else {
        message(ui, "A file dialog is already open.", true);
        return;
    };
    // Captured before the dialog opens; the write is admitted only in this
    // scope (see finish_export).
    let opened = scope::begin_dialog(ui);
    ui.set_studio_pack_busy(true);
    let suggested = format!(
        "{}.toml",
        pack_name_from(&pack.name).unwrap_or_else(|| FALLBACK_NAME.to_owned())
    );
    let documents = directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(std::path::Path::to_path_buf));
    let weak = ui.as_weak();
    let dialog_scope = opened.clone();
    let spawned = std::thread::Builder::new()
        .name("morpheus-theme-pack-export".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Export theme pack")
                .add_filter("Morpheus theme pack", &["toml"])
                .set_file_name(suggested);
            if let Some(documents) = documents {
                dialog = dialog.set_directory(documents);
            }
            // Only the chosen path leaves this thread. Nothing is written
            // here; the write is admitted on the UI thread. The permit goes
            // with it, so one dialog and its file work run at a time.
            let chosen = dialog.save_file();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                finish_export(&ui, dialog_scope, name, pack, chosen, permit);
            });
        });
    if spawned.is_err() {
        scope::finish_dialog(ui, &opened);
        ui.set_studio_pack_busy(false);
        message(ui, "The file dialog could not open.", true);
    }
}

/// The export dialog opened in `opened` returned with `chosen`. This is the
/// admission boundary.
///
/// The write is admitted only while that scope is current and the pack in
/// use is still `name`, holding exactly `pack`. Otherwise nothing is
/// written, and the message says so. An admitted write runs on a worker
/// with `permit` held (see the module documentation) and is reported by
/// [`finish_write`]. Whatever the system dialog asked about an existing
/// file, the export never replaces one: it asks for a new name instead.
fn finish_export(
    ui: &AppWindow,
    opened: Scope,
    name: String,
    pack: ThemePack,
    chosen: Option<PathBuf>,
    permit: DialogPermit,
) {
    let admitted = scope::finish_dialog(ui, &opened);
    let Some(path) = chosen else {
        ui.set_studio_pack_busy(false);
        return;
    };
    if !admitted {
        ui.set_studio_pack_busy(false);
        message(
            ui,
            "The theme pack was not exported because the account or settings changed while the file dialog was open. Nothing was written.",
            false,
        );
        return;
    }
    let still_in_use = with(|state| state.store.clone()).is_some_and(|store| {
        crate::appearance::remembered_theme_source()
            .and_then(|(config, _)| config.appearance.theme_pack)
            .is_some_and(|in_use| in_use == name)
            && read_theme_pack(&store, &name).is_ok_and(|current| current == pack)
    });
    if previewing() || !still_in_use {
        ui.set_studio_pack_busy(false);
        message(
            ui,
            "The theme pack in use changed while the file dialog was open. Nothing was written; export again.",
            true,
        );
        return;
    }
    // Admitted. From here the write is under way and will be reported.
    with(|state| state.writing = Some(opened.clone()));
    let display = display_name(&pack);
    let weak = ui.as_weak();
    let spawned = std::thread::Builder::new()
        .name("morpheus-theme-pack-write".into())
        .spawn(move || {
            let result = export_theme_pack(&pack, &path).map_err(|error| error.to_string());
            drop(permit);
            let _ =
                weak.upgrade_in_event_loop(move |ui| finish_write(&ui, opened, display, result));
        });
    if spawned.is_err() {
        with(|state| state.writing = None);
        ui.set_studio_pack_busy(false);
        message(
            ui,
            "The theme pack export could not start. Nothing was written.",
            true,
        );
    }
}

/// The admitted export of `admitted` ended. It is reported once, in place
/// while its scope is current. Otherwise it is reported without naming the
/// pack, stating only what happened to the file.
fn finish_write(ui: &AppWindow, admitted: Scope, display: String, result: Result<(), String>) {
    let ours = with(|state| {
        state
            .writing
            .take_if(|writing| *writing == admitted)
            .is_some()
    });
    if !ours {
        return;
    }
    ui.set_studio_pack_busy(false);
    let failed = result.is_err();
    let text = match (result, scope::current(ui, &admitted)) {
        (Ok(()), true) => format!("Exported the {display} theme pack."),
        // Every export error is a refusal or a failed no-clobber write that
        // removed its staged file: nothing is at the chosen path.
        (Err(problem), true) => {
            format!("Could not export the theme pack: {problem}. Nothing was written.")
        }
        (Ok(()), false) => "A theme pack export started before the account or settings changed has finished; the file was written.".to_owned(),
        (Err(_), false) => "A theme pack export started before the account or settings changed could not be written. Nothing was written.".to_owned(),
    };
    message(ui, text, failed);
}

/// Preview `pack` as if the person had imported it from a file.
#[cfg(test)]
pub(super) fn preview_for_test(ui: &AppWindow, pack: ThemePack) {
    let opened = scope::capture(ui);
    start_preview(ui, opened, pack);
}

/// Deliver an import dialog's answer for `opened`, as the dialog thread does.
#[cfg(test)]
pub(super) fn finish_import_for_test(ui: &AppWindow, opened: Scope, pack: ThemePack) {
    finish_import(ui, opened, Some(Ok(pack)));
}

/// Deliver an export dialog's answer for `opened`, as the dialog thread
/// does, holding the dialog permit and the busy state as export() does.
#[cfg(test)]
pub(super) fn finish_export_for_test(
    ui: &AppWindow,
    opened: Scope,
    name: String,
    pack: ThemePack,
    chosen: PathBuf,
) {
    let permit = DialogPermit::acquire().expect("no other file dialog in the test");
    ui.set_studio_pack_busy(true);
    finish_export(ui, opened, name, pack, Some(chosen), permit);
}

#[cfg(test)]
pub(super) fn reset_for_test() {
    with(|state| *state = State::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_bounded_and_plain() {
        let mut pack = morpheus_config::BuiltInStylePreset::Paper.theme().unwrap();
        pack.name = "Ocean\u{202e} Dusk\n".into();
        assert_eq!(display_name(&pack), "Ocean Dusk");
        pack.name = "\u{200b}".into();
        assert_eq!(display_name(&pack), "Imported pack");
        pack.name = "x".repeat(80);
        assert_eq!(display_name(&pack).chars().count(), 48);
    }
}
