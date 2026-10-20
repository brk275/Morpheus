//! The theme studio: look changes made in Settings are a draft. They show at
//! once everywhere in the window and are saved only by Apply. A bar on every
//! page counts them and offers Undo, Redo, Discard and Apply.
//!
//! Every Settings save is decided on the settings file as re-read, as
//! before. Behavior settings (notifications, shortcuts, devices, updates)
//! and the accessibility choices kept outside the draft (text size, Reduce
//! motion, the focus ring) still save at once. Only look settings join the
//! draft; while it holds any, look changes made on other pages (a pane
//! resized, the details panel toggled) join it too, so nothing the bar
//! counts is half saved.
//!
//! Apply re-reads the file. A drafted setting that the file meanwhile
//! changed to a third value is a conflict: Apply then asks whether to keep
//! this window's values or the file's instead of overwriting one silently.
//! Settings changed only in the file are kept either way. A reload of the
//! file while drafting shows the draft over the new file.
//!
//! Every studio write, Apply and routed behavior saves alike, is committed
//! over the exact revision it was decided on (`ConfigStore::commit`). When
//! the file changed after that read, nothing is written. The file keeps
//! the other version, the draft stays as it was, and Apply offers the
//! choice for any drafted setting the other version changed. Another
//! program that does not take Morpheus's writer lock can still lose a write
//! that lands in the instant between the commit's comparison and its
//! rename; morpheus-config's `commit` module states the exact guarantee.
//!
//! Account colors in the draft belong to the account that was active when
//! they were drafted. When another account becomes active they are
//! discarded (with a notice), never written to another account. A style
//! profile preview and the draft exclude each other.

mod fonts;
mod packs;
mod presets;
pub(crate) mod scope;
mod session;

use std::cell::RefCell;
use std::time::Duration;

use morpheus_config::{CommitError, Config, ConfigRevision, ConfigStore, ThemePack};
use slint::{ModelRc, VecModel};

use crate::{AppWindow, StudioVariation};
use session::{Draft, Edits, Group, LookKey, Resolution, look, overlay};

/// Conflicting settings named in the bar; the rest are counted.
const NAMED_CONFLICTS: usize = 3;

/// Apply never waits for another Morpheus writer on the UI thread. When the
/// commit finds one mid-save (`CommitError::Busy`), Apply is tried again on
/// a timer, at most this many times, this far apart.
const APPLY_RETRIES: u8 = 8;
const APPLY_RETRY_DELAY: Duration = Duration::from_millis(150);

/// An Apply waiting to be tried again after `Busy`, and what it was for. It
/// is tried again only while the draft is unchanged and its scope (the
/// appearance epoch and account, see `scope`) is still current, so an
/// account switch, A -> B -> A included, drops it. The choice made for
/// conflicts is repeated; a newer Apply replaces it.
struct PendingApply {
    attempt: u8,
    edits: Edits,
    scope: scope::Scope,
    resolution: Option<Resolution>,
}

#[derive(Default)]
struct Studio {
    store: Option<ConfigStore>,
    draft: Draft,
    /// The local account the draft's account colors belong to.
    account: String,
    /// The last accepted configuration and its theme: what the draft is shown
    /// over, and what Discard shows.
    accepted: Option<(Config, Option<ThemePack>)>,
    /// The theme pack the drafted look names, when an edit chose one.
    draft_theme: Option<Option<ThemePack>>,
    /// Settings Apply found changed in the file too, while it waits for a
    /// choice.
    conflicts: Vec<LookKey>,
    /// An Apply waiting for another writer to finish.
    retry: Option<PendingApply>,
}

thread_local! {
    // Slint callbacks, hooks and projections run on the UI thread only.
    static STUDIO: RefCell<Studio> = RefCell::new(Studio::default());
    static APPLY_RETRY: slint::Timer = slint::Timer::default();
}

fn with<R>(update: impl FnOnce(&mut Studio) -> R) -> R {
    STUDIO.with(|studio| update(&mut studio.borrow_mut()))
}

pub(crate) fn register(ui: &AppWindow, store: &ConfigStore) {
    // A new settings context: work captured for an earlier one is stale.
    scope::invalidate();
    with(|studio| studio.store = Some(store.clone()));
    let weak = ui.as_weak();
    ui.on_studio_apply(move || {
        if let Some(ui) = weak.upgrade() {
            apply(&ui, None);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_resolve(move |choice| {
        if let Some(ui) = weak.upgrade() {
            match choice {
                1 => apply(&ui, Some(Resolution::KeepMine)),
                2 => apply(&ui, Some(Resolution::KeepFile)),
                _ => {
                    with(|studio| studio.conflicts.clear());
                    project(&ui);
                }
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_discard(move || {
        if let Some(ui) = weak.upgrade() {
            discard(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_undo(move || {
        if let Some(ui) = weak.upgrade() {
            step(&ui, true);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_redo(move || {
        if let Some(ui) = weak.upgrade() {
            step(&ui, false);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_reset_group(move |index| {
        if let Some(ui) = weak.upgrade() {
            reset_group(&ui, index);
        }
    });
    let weak = ui.as_weak();
    ui.on_studio_choose_variation(move |index| {
        if let Some(ui) = weak.upgrade() {
            choose_variation(&ui, index);
        }
    });
    ui.set_studio_variations(ModelRc::new(VecModel::from(
        presets::VARIATIONS
            .iter()
            .map(|variation| StudioVariation {
                name: variation.name.into(),
                description: variation.description.into(),
            })
            .collect::<Vec<_>>(),
    )));
    packs::register(ui, store);
    fonts::register(ui, store);
    let weak = ui.as_weak();
    ui.on_studio_notice_dismiss(move || {
        if let Some(ui) = weak.upgrade() {
            notify(&ui, "", false);
        }
    });
    project(ui);
}

/// Whether the draft holds look changes.
pub(crate) fn drafting() -> bool {
    with(|studio| !studio.draft.is_empty())
}

/// Whether a theme pack preview owns the shown look.
/// After the shown look or the active account changed: refresh the font
/// browser, or close it when its scope ended.
pub(crate) fn font_browser_refresh(ui: &AppWindow) {
    fonts::refresh(ui);
}

/// Load a font catalog built by a test into the browser, as a finished read.
#[cfg(test)]
pub(crate) fn font_catalog_for_test(families: Vec<(&str, bool, Vec<u16>)>) {
    fonts::set_catalog_for_test(fonts::Catalog::from_families(
        families
            .into_iter()
            .map(|(name, monospace, weights)| fonts::Family {
                name: name.to_owned(),
                monospace,
                weights,
            }),
        Vec::new(),
    ));
}

pub(crate) fn pack_previewing() -> bool {
    packs::previewing()
}

/// Whether a Settings save goes through the draft: once the studio is
/// registered (the window does at startup), every save made on the Settings
/// page, where the studio's controls are, and every save while the draft
/// holds changes. A style profile preview keeps its own rules.
pub(crate) fn routes(ui: &AppWindow) -> bool {
    with(|studio| studio.store.is_some())
        && !crate::style_profiles::previewing()
        && !packs::previewing()
        && (ui.get_page() == 1 || drafting())
}

/// A routed Settings save. `change` is decided on the file as re-read with
/// the draft shown over it, so editors see the values they show. Behavior
/// changes are written at once; look changes join the draft. Returns the
/// configuration now shown, as a direct save would.
pub(crate) fn save(
    ui: &AppWindow,
    store: &ConfigStore,
    change: impl FnOnce(&mut Config) -> bool,
) -> Option<Config> {
    let (file, revision) = match store.load_revision() {
        Ok(loaded) => loaded,
        Err(error) => {
            ui.set_config_error(error.to_string().into());
            return None;
        }
    };
    let edits = with(|studio| studio.draft.edits().clone());
    let Ok(shown) = overlay(&file, &edits) else {
        discard_unusable(ui);
        return None;
    };
    let mut candidate = shown.clone();
    if !change(&mut candidate) {
        return None;
    }
    if let Err(error) = candidate.validate() {
        ui.set_config_error(error.to_string().into());
        return None;
    }
    let Ok(written) = session::with_look_of(&candidate, &file) else {
        return None;
    };
    // A new look must load before it is shown or drafted. An unrelated
    // broken theme must not prevent an explicit privacy or device change.
    let theme = match store.load_theme(&candidate) {
        Ok(theme) => theme,
        Err(error) => {
            if behavior_change_only(&shown, &candidate) && written != file {
                if let Err(error) = commit(store, &written, &revision) {
                    ui.set_config_error(error.to_string().into());
                    return None;
                }
                // Keep the actual accepted look and the draft exactly as
                // they are. In particular, never accept the unreadable
                // theme or publish any pending appearance setting here.
                with(|studio| {
                    if let Some((accepted, _)) = studio.accepted.as_mut() {
                        copy_behavior(accepted, &written);
                    }
                });
                crate::apply_behavior(ui, &written);
                ui.set_config_error(error.to_string().into());
                notify(
                    ui,
                    "Saved your settings. The theme needs attention; your last valid look and unsaved appearance changes are kept.",
                    false,
                );
                project(ui);
                return Some(candidate);
            }
            ui.set_config_error(error.to_string().into());
            return None;
        }
    };
    // A behavior change is written only over the revision it was decided
    // on. If the file changed since, nothing is written or drafted; the
    // draft is left as it was and the reload shows the other version.
    if written != file
        && let Err(error) = commit(store, &written, &revision)
    {
        ui.set_config_error(error.to_string().into());
        return None;
    }
    let accepted_theme = if look(&written) == look(&candidate) {
        theme.clone()
    } else {
        store.load_theme(&written).ok().flatten()
    };
    let active = ui.get_active_account_local_id().to_string();
    with(|studio| {
        if studio.draft.is_empty() {
            studio.account = active;
        }
        studio
            .draft
            .record(&look(&written), &look(&shown), &look(&candidate));
        studio.accepted = Some((written.clone(), accepted_theme));
        studio.draft_theme = Some(theme.clone());
        studio.conflicts.clear();
    });
    crate::apply_behavior(ui, &written);
    crate::apply_appearance(ui, &candidate, theme.as_ref(), true);
    crate::style_profiles::config_applied(ui);
    ui.set_config_error("".into());
    project(ui);
    Some(candidate)
}

/// The fallback is intentionally limited to settings applied by the
/// behavior path. Accessibility, geometry, account overrides and every
/// other appearance field still require a valid look. Comparing complete
/// configurations after removing these four deltas also rejects a callback
/// that changes behavior and appearance together.
fn behavior_change_only(before: &Config, after: &Config) -> bool {
    let mut without_behavior = after.clone();
    copy_behavior(&mut without_behavior, before);
    without_behavior == *before
}

fn copy_behavior(target: &mut Config, source: &Config) {
    target.notifications = source.notifications.clone();
    target.media = source.media.clone();
    target.shortcuts = source.shortcuts.clone();
    target.experimental = source.experimental.clone();
}

/// Record an accepted configuration (startup, a reload or a save). While
/// the draft holds changes, show them over it and return true; the caller
/// then applies only its behavior.
pub(crate) fn show_over_accepted(
    ui: &AppWindow,
    config: &Config,
    theme: Option<&ThemePack>,
) -> bool {
    // An accepted settings change ends a theme pack preview; the accepted
    // look is shown instead.
    packs::config_accepted(ui);
    let shown = with(|studio| {
        studio.accepted = Some((config.clone(), theme.cloned()));
        if studio.draft.is_empty() {
            return None;
        }
        let view = overlay(config, studio.draft.edits()).ok();
        let view_theme = if studio.draft.chooses_theme() {
            studio.draft_theme.clone().unwrap_or_else(|| theme.cloned())
        } else {
            theme.cloned()
        };
        let conflicts = studio.draft.conflicts(&look(config)).len();
        Some((view, view_theme, conflicts))
    });
    let Some((view, view_theme, conflicts)) = shown else {
        return false;
    };
    let Some(view) = view else {
        discard_unusable(ui);
        return false;
    };
    crate::apply_appearance(ui, &view, view_theme.as_ref(), true);
    if conflicts > 0 {
        notify(
            ui,
            format!(
                "The settings file changed {} of your unsaved appearance settings. Apply asks which values to keep.",
                conflicts
            ),
            false,
        );
    }
    project(ui);
    true
}

/// Called whenever the active account is published. Account colors drafted
/// for another account are discarded, never moved to this one.
pub(crate) fn check_account(ui: &AppWindow) {
    packs::check_account(ui);
    let active = ui.get_active_account_local_id().to_string();
    let dropped = with(|studio| {
        if studio.account == active {
            return 0;
        }
        let dropped = studio.draft.retain_account(&active);
        studio.account = active;
        studio.conflicts.clear();
        dropped
    });
    if dropped > 0 {
        notify(
            ui,
            "Unsaved colors for the previous account were discarded. Your other unsaved appearance changes are kept.",
            false,
        );
        // The remembered appearance source still contains the discarded
        // account edits. Replace it now, before apply_theme_mode reads it
        // for the new account; otherwise A -> B -> A can display A's old
        // draft even though neither Apply nor Undo can see it anymore.
        // The account marker above was updated before redisplay, so its
        // nested appearance projection cannot discard/re-enter again.
        redisplay(ui);
    }
}

/// Show the draft over the accepted configuration again, after it changed
/// without a save (undo, redo, discard).
fn redisplay(ui: &AppWindow) {
    let shown = with(|studio| {
        let (config, theme) = studio.accepted.clone()?;
        let view = overlay(&config, studio.draft.edits()).ok()?;
        let view_theme = if studio.draft.chooses_theme() {
            match studio.store.as_ref().map(|store| store.load_theme(&view)) {
                Some(Ok(loaded)) => loaded,
                _ => theme,
            }
        } else {
            theme
        };
        studio.draft_theme = Some(view_theme.clone());
        Some((view, view_theme))
    });
    if let Some((view, theme)) = shown {
        crate::apply_appearance(ui, &view, theme.as_ref(), true);
        crate::style_profiles::config_applied(ui);
    }
    project(ui);
}

fn step(ui: &AppWindow, back: bool) {
    let changed = with(|studio| {
        studio.conflicts.clear();
        if back {
            studio.draft.undo()
        } else {
            studio.draft.redo()
        }
    });
    if changed {
        redisplay(ui);
    }
}

fn discard(ui: &AppWindow) {
    let had = with(|studio| {
        let had = !studio.draft.is_empty();
        studio.draft.discard();
        studio.conflicts.clear();
        studio.draft_theme = None;
        had
    });
    if had {
        notify(ui, "Your unsaved appearance changes were discarded.", false);
    }
    redisplay(ui);
}

/// The draft could not be laid over the settings file (its structure
/// changed under it). Nothing of it can be shown or saved safely.
fn discard_unusable(ui: &AppWindow) {
    with(|studio| {
        studio.draft.discard();
        studio.conflicts.clear();
        studio.draft_theme = None;
    });
    notify(
        ui,
        "Your unsaved appearance changes no longer fit the settings file and were discarded.",
        true,
    );
    redisplay(ui);
}

fn reset_group(ui: &AppWindow, index: i32) {
    let Some(group) = usize::try_from(index)
        .ok()
        .and_then(|index| Group::ALL.get(index).copied())
    else {
        return;
    };
    let Some(store) = with(|studio| studio.store.clone()) else {
        return;
    };
    // An ordinary Settings edit, so it is drafted and can be undone.
    let reset = crate::save_config_edit(ui, &store, |config| {
        match session::reset_group(config, group) {
            Ok(reset) if reset != *config => {
                *config = reset;
                true
            }
            _ => false,
        }
    });
    if reset.is_some() {
        notify(
            ui,
            format!("{} reset in the preview; Apply saves it.", group.label()),
            false,
        );
    }
}

/// Fill in a variation of a built-in starting point. In Settings it is
/// drafted, so it shows at once and Apply saves it.
fn choose_variation(ui: &AppWindow, index: i32) {
    let Some(variation) = usize::try_from(index)
        .ok()
        .and_then(|index| presets::VARIATIONS.get(index))
    else {
        return;
    };
    let Some(store) = with(|studio| studio.store.clone()) else {
        return;
    };
    let shown = crate::save_config_edit(ui, &store, |config| {
        variation.apply(config);
        true
    });
    if shown.is_some() && drafting() {
        notify(
            ui,
            format!(
                "Showing {}. Apply saves it; Discard restores your look.",
                variation.name
            ),
            false,
        );
    }
}

/// Apply chosen by the person: it replaces any Apply still waiting to be
/// tried again.
fn apply(ui: &AppWindow, resolution: Option<Resolution>) {
    APPLY_RETRY.with(slint::Timer::stop);
    with(|studio| studio.retry = None);
    apply_attempt(ui, resolution, 0);
}

fn apply_attempt(ui: &AppWindow, resolution: Option<Resolution>, attempt: u8) {
    // A retry repeats what the person asked for, not what this attempt
    // resolved it to: conflicts are decided again on the file as it is then.
    let requested = resolution;
    let Some(store) = with(|studio| studio.store.clone()) else {
        return;
    };
    // Only the active account's drafted colors may be written.
    check_account(ui);
    if !drafting() {
        project(ui);
        return;
    }
    let (file, revision) = match store.load_revision() {
        Ok(loaded) => loaded,
        Err(error) => {
            ui.set_config_error(error.to_string().into());
            notify(
                ui,
                "The settings file has a problem (shown above); nothing was saved. Your changes are still shown.",
                true,
            );
            return;
        }
    };
    let file_look = look(&file);
    let conflicts = with(|studio| studio.draft.conflicts(&file_look));
    let resolution = match resolution {
        Some(resolution) => resolution,
        None if conflicts.is_empty() => Resolution::KeepMine,
        None => {
            with(|studio| studio.conflicts = conflicts);
            project(ui);
            return;
        }
    };
    let edits = with(|studio| studio.draft.resolved(&file_look, resolution));
    let Ok(merged) = overlay(&file, &edits) else {
        discard_unusable(ui);
        return;
    };
    if let Err(error) = merged.validate() {
        notify(
            ui,
            format!("Nothing was saved: {error}. Your changes are still shown."),
            true,
        );
        return;
    }
    let theme = match store.load_theme(&merged) {
        Ok(theme) => theme,
        Err(error) => {
            notify(
                ui,
                format!("Nothing was saved: {error}. Your changes are still shown."),
                true,
            );
            return;
        }
    };
    match commit(&store, &merged, &revision) {
        Ok(_) => {}
        Err(CommitError::Changed { current }) => {
            changed_while_applying(ui, &current);
            return;
        }
        // Another Morpheus writer is mid-save: nothing was written. Try
        // again shortly without blocking the window.
        Err(CommitError::Busy) if attempt < APPLY_RETRIES => {
            retry_later(ui, requested, attempt);
            return;
        }
        Err(CommitError::Busy) => {
            notify(
                ui,
                "Nothing was saved: another Morpheus window or task kept saving settings. Your changes are still shown; Apply again.",
                true,
            );
            return;
        }
        Err(error) => {
            ui.set_config_error(error.to_string().into());
            notify(
                ui,
                "Nothing was saved; the settings file could not be written. Your changes are still shown.",
                true,
            );
            return;
        }
    }
    let kept_from_file = match resolution {
        Resolution::KeepFile => conflicts.len(),
        Resolution::KeepMine => 0,
    };
    with(|studio| {
        studio.draft.discard();
        studio.conflicts.clear();
        studio.draft_theme = None;
    });
    crate::apply_config(ui, &merged, theme.as_ref());
    ui.set_config_error("".into());
    if kept_from_file > 0 {
        notify(
            ui,
            format!(
                "Saved your appearance changes, keeping the settings file's value for {kept_from_file} of them."
            ),
            false,
        );
    } else {
        notify(ui, "Saved your appearance changes.", false);
    }
    project(ui);
}

/// Schedule another Apply attempt, bound to the draft, account and
/// conflict choice of this one.
fn retry_later(ui: &AppWindow, resolution: Option<Resolution>, attempt: u8) {
    let captured = scope::capture(ui);
    with(|studio| {
        studio.retry = Some(PendingApply {
            attempt,
            edits: studio.draft.edits().clone(),
            scope: captured,
            resolution,
        });
    });
    if attempt == 0 {
        notify(
            ui,
            "Another Morpheus window or task is saving settings; Apply will try again in a moment. Nothing was written yet.",
            false,
        );
    }
    let weak = ui.as_weak();
    APPLY_RETRY.with(|timer| {
        timer.start(slint::TimerMode::SingleShot, APPLY_RETRY_DELAY, move || {
            if let Some(ui) = weak.upgrade() {
                retry_apply(&ui);
            }
        });
    });
}

/// Try a waiting Apply again, only for the same draft, account and choice.
/// Anything the person did meanwhile (an edit, Undo, Discard, an account
/// switch) owns the draft now, so the old Apply is dropped.
fn retry_apply(ui: &AppWindow) {
    let Some(pending) = with(|studio| studio.retry.take()) else {
        return;
    };
    let same =
        with(|studio| *studio.draft.edits() == pending.edits) && scope::current(ui, &pending.scope);
    if !same {
        if drafting() {
            notify(
                ui,
                "Apply did not try again because your unsaved changes or the account changed. Apply again to save them.",
                false,
            );
        }
        return;
    }
    apply_attempt(ui, pending.resolution, pending.attempt + 1);
}

/// Apply found the file changed after it read it (another program or
/// another Morpheus window). Nothing was written, so the file keeps that
/// version and the draft is unchanged. When that version is valid and
/// changed drafted settings too, the choice between them is offered now.
fn changed_while_applying(ui: &AppWindow, current: &ConfigRevision) {
    let conflicts = match current.config() {
        Ok(file) => with(|studio| studio.draft.conflicts(&look(&file))),
        Err(_) => Vec::new(),
    };
    let count = conflicts.len();
    with(|studio| studio.conflicts = conflicts);
    project(ui);
    if count > 0 {
        notify(
            ui,
            format!(
                "The settings file changed while saving, including {count} of your unsaved appearance settings. Nothing was written; choose which values to keep."
            ),
            true,
        );
    } else {
        notify(
            ui,
            "The settings file changed while saving. Nothing was written: the file's version is kept and your changes are still shown. Apply again to save them over it.",
            true,
        );
    }
}

/// Every studio write: committed only over the revision it was decided on.
fn commit(
    store: &ConfigStore,
    config: &Config,
    revision: &ConfigRevision,
) -> Result<ConfigRevision, CommitError> {
    store.commit_with(config, revision, before_publish)
}

#[cfg(not(test))]
fn before_publish(_: &std::path::Path) {}

/// Runs an edit queued by a test at the real pre-publish boundary of the
/// next studio commit: after staging and locking, before the comparison.
#[cfg(test)]
fn before_publish(path: &std::path::Path) {
    if let Some(edit) = BEFORE_PUBLISH.with(|hook| hook.borrow_mut().take()) {
        edit(path);
    }
}

#[cfg(test)]
type PublishEdit = Box<dyn FnOnce(&std::path::Path)>;

#[cfg(test)]
thread_local! {
    static BEFORE_PUBLISH: RefCell<Option<PublishEdit>> = const { RefCell::new(None) };
}

/// Queue `edit` to change the settings file at the pre-publish boundary of
/// the next studio commit, as another program could. It must write with
/// `std::fs`, never through a `ConfigStore` (the writer lock is held).
#[cfg(test)]
pub(crate) fn before_next_publish_for_test(edit: impl FnOnce(&std::path::Path) + 'static) {
    BEFORE_PUBLISH.with(|hook| *hook.borrow_mut() = Some(Box::new(edit)));
}

fn notify(ui: &AppWindow, text: impl Into<String>, error: bool) {
    let text = text.into();
    ui.set_studio_notice(text.as_str().into());
    ui.set_studio_notice_error(error && !text.is_empty());
}

/// A drafted setting as a person reads it: its `config.toml` key. Account
/// colors never show the account's ID.
fn setting_name(key: &LookKey) -> String {
    match key {
        LookKey::Field(section, field) => format!("{section}.{field}"),
        LookKey::Color(token) => format!("colors.{token}"),
        LookKey::Senders => "senders.colors".to_owned(),
        LookKey::AccountTheme(_) => "this account's theme".to_owned(),
        LookKey::AccountColor(_, token) => format!("this account's colors.{token}"),
        // Neither the account nor the room ID is shown.
        LookKey::RoomTheme(..) => "a room's theme".to_owned(),
        LookKey::RoomColor(_, _, token) => format!("a room's colors.{token}"),
    }
}

fn project(ui: &AppWindow) {
    let (pending, undo, redo, conflicts) = with(|studio| {
        (
            studio.draft.len(),
            studio.draft.can_undo(),
            studio.draft.can_redo(),
            studio.conflicts.clone(),
        )
    });
    ui.set_studio_pending(i32::try_from(pending).unwrap_or(i32::MAX));
    ui.set_studio_can_undo(undo);
    ui.set_studio_can_redo(redo);
    ui.set_studio_conflicts(i32::try_from(conflicts.len()).unwrap_or(i32::MAX));
    let mut summary: Vec<String> = conflicts
        .iter()
        .take(NAMED_CONFLICTS)
        .map(setting_name)
        .collect();
    if conflicts.len() > NAMED_CONFLICTS {
        summary.push(format!("{} more", conflicts.len() - NAMED_CONFLICTS));
    }
    ui.set_studio_conflict_summary(summary.join(", ").into());
}

/// Preview `pack` as if the person had imported it from a file.
#[cfg(test)]
pub(crate) fn preview_pack_for_test(ui: &AppWindow, pack: ThemePack) {
    packs::preview_for_test(ui, pack);
}

/// Deliver a theme pack import dialog's answer for `opened` through the
/// production controller, as the dialog thread does.
#[cfg(test)]
pub(crate) fn finish_pack_import_for_test(ui: &AppWindow, opened: scope::Scope, pack: ThemePack) {
    packs::finish_import_for_test(ui, opened, pack);
}

/// Deliver a theme pack export dialog's answer for `opened` through the
/// production controller, as the dialog thread does.
#[cfg(test)]
pub(crate) fn finish_pack_export_for_test(
    ui: &AppWindow,
    opened: scope::Scope,
    name: String,
    pack: ThemePack,
    chosen: std::path::PathBuf,
) {
    packs::finish_export_for_test(ui, opened, name, pack, chosen);
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    APPLY_RETRY.with(slint::Timer::stop);
    with(|studio| *studio = Studio::default());
    BEFORE_PUBLISH.with(|hook| hook.borrow_mut().take());
    packs::reset_for_test();
    fonts::reset_for_test();
    scope::reset_for_test();
}
