//! The typography and geometry workspace: metric groups, one selected
//! metric, stepping or typing its value, and reset to the built-in value.
//!
//! Every metric, range, unit, label and search word comes from
//! `registry::FIELDS`, the table that also validates `config.toml`. A change
//! is decided on the re-read file inside the same save as every other
//! Settings change, so an invalid hand edit is never overwritten. Typing only
//! validates; Enter applies, and the steppers apply at once. Undo is
//! presentation state of this window and applies only while the file still
//! holds the value this window saved, so it never overwrites a later edit
//! made elsewhere.

use std::cell::RefCell;
use std::collections::HashMap;

use morpheus_config::registry::{self, FieldSpec, MetricEditor, MetricGroup};
use morpheus_config::{Config, ConfigStore, MetricValue};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, MetricEditorState, MetricGroupItem, MetricItem, save_config_edit};

const NUMBER_HINT: &str = "Type a whole number in the range shown, then press Enter.";
const STALE_UNDO_HINT: &str =
    "This value was changed outside this window after your change; nothing was undone.";
const MAX_FILTER_CHARS: usize = 64;

#[derive(Default)]
enum ResetAll {
    #[default]
    Idle,
    /// The next activation returns every customized metric to its default.
    Confirm,
    /// Values removed by the last Reset all; the next activation restores
    /// those still at their default.
    Removed(Vec<(&'static str, MetricValue)>),
}

/// One metric's last change in this window, as found in and written to the file.
struct UndoStep {
    before: MetricValue,
    after: MetricValue,
}

#[derive(Default)]
struct Session {
    previous: HashMap<&'static str, UndoStep>,
    hint: String,
    reset_all: ResetAll,
    /// The configuration last applied, for projecting selection and filter
    /// changes without reading the file.
    source: Option<Config>,
}

thread_local! {
    // Slint callbacks and projections run on the UI thread only.
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
}

pub(super) fn register(ui: &AppWindow, store: &ConfigStore) {
    ui.set_metric_groups(ModelRc::new(VecModel::from(group_items())));
    let weak = ui.as_weak();
    ui.on_metric_filter_changed(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let bounded: String = text.chars().take(MAX_FILTER_CHARS).collect();
        if bounded.as_str() != text.as_str() {
            ui.set_metric_filter(bounded.into());
        }
        reproject(&ui, false);
    });
    let weak = ui.as_weak();
    ui.on_metric_group_chosen(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(group) = usize::try_from(index)
            .ok()
            .and_then(|index| MetricGroup::ALL.get(index))
        else {
            return;
        };
        ui.set_metric_group_index(index);
        ui.set_metric_filter(SharedString::new());
        let first = registry::metric_fields().find(|(_, editor)| editor.group == *group);
        if let Some((field, _)) = first {
            ui.set_metric_selected_key(field.key.into());
        }
        set_hint("");
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    ui.on_metric_chosen(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        // A queued click may name a metric outside the current group or
        // filter; honor that exact allowlisted key.
        if let Some((field, editor)) = registry::metric_field(key.as_str()) {
            select(&ui, field, editor);
        }
    });
    let weak = ui.as_weak();
    let step_store = store.clone();
    ui.on_metric_step(move |direction| {
        let Some(ui) = weak.upgrade() else { return };
        let Some((field, editor)) = selected(&ui) else {
            return;
        };
        let delta = match direction.signum() {
            1 => i64::from(editor.step),
            -1 => -i64::from(editor.step),
            _ => return,
        };
        set_hint("");
        // Step from the value in the re-read file, which the applied view
        // can lag by one watcher poll.
        save_metric(&ui, &step_store, field.key, |config| {
            let current = shown_value(config, field, editor)?;
            let (minimum, maximum) = bounds(field);
            let next = u16::try_from((current + delta).clamp(minimum, maximum)).ok()?;
            (i64::from(next) != current).then_some(MetricValue::Set(next))
        });
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    ui.on_metric_value_edited(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let Some((field, _)) = selected(&ui) else {
            return;
        };
        // Typing only checks the value; nothing is saved until Enter.
        let valid = text.trim().is_empty() || parse_value(field, text.as_str()).is_some();
        set_hint(if valid { "" } else { NUMBER_HINT });
        reproject(&ui, false);
    });
    let weak = ui.as_weak();
    let apply_store = store.clone();
    ui.on_metric_value_accepted(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let Some((field, _)) = selected(&ui) else {
            return;
        };
        let Some(value) = parse_value(field, text.as_str()) else {
            set_hint(NUMBER_HINT);
            reproject(&ui, false);
            return;
        };
        set_hint("");
        save_metric(&ui, &apply_store, field.key, |_| {
            Some(MetricValue::Set(value))
        });
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let reset_store = store.clone();
    ui.on_metric_reset(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some((field, _)) = selected(&ui) else {
            return;
        };
        set_hint("");
        save_metric(&ui, &reset_store, field.key, |_| {
            Some(MetricValue::Inherited)
        });
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let undo_store = store.clone();
    ui.on_metric_undo(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some((field, _)) = selected(&ui) else {
            return;
        };
        let step = SESSION.with(|session| session.borrow_mut().previous.remove(field.key));
        let Some(step) = step else { return };
        set_hint("");
        let mut superseded = false;
        let saved = save_config_edit(&ui, &undo_store, |config| {
            // Undo only the change this window made, not a later one.
            if config.metric(field.key) != Some(step.after) {
                superseded = true;
                return false;
            }
            config.set_metric(field.key, step.before).is_ok()
        });
        if superseded {
            set_hint(STALE_UNDO_HINT);
        } else if saved.is_none() {
            // Keep the step when the file could not be changed.
            SESSION.with(|session| session.borrow_mut().previous.insert(field.key, step));
        }
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let all_store = store.clone();
    ui.on_metric_reset_all(move || {
        let Some(ui) = weak.upgrade() else { return };
        let state = SESSION.with(|session| std::mem::take(&mut session.borrow_mut().reset_all));
        let mut written = false;
        let next = match state {
            ResetAll::Idle => ResetAll::Confirm,
            ResetAll::Confirm => {
                // The values the file holds now, not the ones last applied.
                let mut removed = Vec::new();
                let saved = save_config_edit(&ui, &all_store, |config| {
                    let defaults = Config::default();
                    for (field, _) in registry::metric_fields() {
                        let value = config.metric(field.key);
                        if value != defaults.metric(field.key)
                            && let Some(value) = value
                            && config.set_metric(field.key, MetricValue::Inherited).is_ok()
                        {
                            removed.push((field.key, value));
                        }
                    }
                    !removed.is_empty()
                });
                written = saved.is_some();
                if written {
                    ResetAll::Removed(removed)
                } else {
                    ResetAll::Idle
                }
            }
            ResetAll::Removed(removed) => {
                // `None` until the file was read; then whether anything is
                // left to restore. A value changed again since is kept.
                let mut restorable = None;
                let saved = save_config_edit(&ui, &all_store, |config| {
                    let defaults = Config::default();
                    let mut any = false;
                    for (key, value) in &removed {
                        if config.metric(key) == defaults.metric(key)
                            && config.set_metric(key, *value).is_ok()
                        {
                            any = true;
                        }
                    }
                    restorable = Some(any);
                    any
                });
                written = saved.is_some();
                if written || restorable == Some(false) {
                    ResetAll::Idle
                } else {
                    // The file could not be read or written: keep the
                    // snapshot so the restore can be tried again.
                    ResetAll::Removed(removed)
                }
            }
        };
        SESSION.with(|session| {
            let mut session = session.borrow_mut();
            session.reset_all = next;
            if written {
                session.previous.clear();
            }
        });
        reproject(&ui, true);
    });
}

/// Project the workspace for an accepted configuration, including reloads
/// of external edits. An edit made elsewhere ends a pending Reset all
/// confirmation and the undo step of every metric it changed.
pub(super) fn project(ui: &AppWindow, config: &Config) {
    SESSION.with(|session| {
        let mut session = session.borrow_mut();
        if matches!(session.reset_all, ResetAll::Confirm) {
            session.reset_all = ResetAll::Idle;
        }
        session
            .previous
            .retain(|key, step| config.metric(key) == Some(step.after));
        session.source = Some(config.clone());
    });
    reproject(ui, false);
}

/// Select one metric for editing, as a Settings search result does: its
/// group with no filter, and its value in the field. Nothing is saved.
pub(super) fn select_metric(ui: &AppWindow, key: &str) -> bool {
    let Some((field, editor)) = registry::metric_field(key) else {
        return false;
    };
    select(ui, field, editor);
    true
}

fn select(ui: &AppWindow, field: &'static FieldSpec, editor: &'static MetricEditor) {
    if let Some(index) = MetricGroup::ALL
        .iter()
        .position(|group| *group == editor.group)
    {
        ui.set_metric_group_index(index as i32);
    }
    ui.set_metric_filter(SharedString::new());
    ui.set_metric_selected_key(field.key.into());
    set_hint("");
    reproject(ui, true);
}

fn set_hint(text: &str) {
    SESSION.with(|session| session.borrow_mut().hint = text.into());
}

fn bounds(field: &FieldSpec) -> (i64, i64) {
    field.integer_range().unwrap_or((0, 0))
}

/// The number the editor steps from: the stored value, or for an omitted
/// optional metric the value its built-in look is closest to.
fn shown_value(config: &Config, field: &FieldSpec, editor: &MetricEditor) -> Option<i64> {
    Some(match config.metric(field.key)? {
        MetricValue::Set(value) => i64::from(value),
        MetricValue::Inherited => editor.inherit.map_or_else(
            || field.integer_default().unwrap_or(bounds(field).0),
            |inherit| i64::from(inherit.start),
        ),
    })
}

fn selected(ui: &AppWindow) -> Option<(&'static FieldSpec, &'static MetricEditor)> {
    registry::metric_field(ui.get_metric_selected_key().as_str())
}

/// A typed whole number within the field's range; anything else is `None`.
fn parse_value(field: &FieldSpec, text: &str) -> Option<u16> {
    let value: i64 = text.trim().parse().ok()?;
    let (minimum, maximum) = bounds(field);
    (minimum..=maximum)
        .contains(&value)
        .then(|| u16::try_from(value).ok())
        .flatten()
}

/// Save one metric decided on the re-read file, keeping one undo step. The
/// step's prior value is the one in that file, and nothing is written when
/// `decide` declines or the file already holds the requested value.
fn save_metric(
    ui: &AppWindow,
    store: &ConfigStore,
    key: &'static str,
    decide: impl FnOnce(&Config) -> Option<MetricValue>,
) {
    let mut change = None;
    let saved = save_config_edit(ui, store, |config| {
        let Some(before) = config.metric(key) else {
            return false;
        };
        let Some(value) = decide(config) else {
            return false;
        };
        if !config.set_metric(key, value).unwrap_or(false) {
            return false;
        }
        change = config.metric(key).map(|after| UndoStep { before, after });
        true
    });
    if let (Some(_), Some(step)) = (saved, change) {
        SESSION.with(|session| {
            let mut session = session.borrow_mut();
            session.previous.insert(key, step);
            session.reset_all = ResetAll::Idle;
        });
    }
}

fn group_items() -> Vec<MetricGroupItem> {
    MetricGroup::ALL
        .iter()
        .map(|group| MetricGroupItem {
            id: group.id().into(),
            label: group.label().into(),
            count: registry::metric_fields()
                .filter(|(_, editor)| editor.group == *group)
                .count() as i32,
        })
        .collect()
}

/// Metrics matching every word of `filter`, or the metrics of `group`.
fn shown_metrics(group: usize, filter: &str) -> Vec<(&'static FieldSpec, &'static MetricEditor)> {
    let filter = filter.trim().to_lowercase();
    let words: Vec<&str> = filter.split_whitespace().collect();
    if words.is_empty() {
        let Some(group) = MetricGroup::ALL.get(group) else {
            return Vec::new();
        };
        return registry::metric_fields()
            .filter(|(_, editor)| editor.group == *group)
            .collect();
    }
    registry::metric_fields()
        .filter(|(field, editor)| {
            let haystack = format!(
                "{} {} {} {} {}",
                editor.label,
                field.key,
                editor.group.label(),
                editor.keywords.join(" "),
                field.description
            )
            .to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .collect()
}

fn value_text(config: &Config, field: &FieldSpec, editor: &MetricEditor) -> String {
    match config.metric(field.key) {
        Some(MetricValue::Set(value)) => format!("{value}{}", editor.unit.suffix()),
        _ => "Built-in".into(),
    }
}

/// Publish the groups, the shown metrics and the selected metric's editor.
/// `refill` replaces the number field with the shown value; otherwise the
/// field keeps what is being typed.
fn reproject(ui: &AppWindow, refill: bool) {
    SESSION.with(|session| {
        let session = session.borrow();
        let fallback = Config::default();
        let config = session.source.as_ref().unwrap_or(&fallback);
        let defaults = Config::default();
        let group = usize::try_from(ui.get_metric_group_index()).unwrap_or(0);
        let shown = shown_metrics(group, ui.get_metric_filter().as_str());
        let selected_key = ui.get_metric_selected_key();
        let (field, editor) = shown
            .iter()
            .copied()
            .find(|(field, _)| field.key == selected_key.as_str())
            .or_else(|| shown.first().copied())
            .or_else(|| registry::metric_field(selected_key.as_str()))
            .or_else(|| registry::metric_fields().next())
            .expect("the registry has metrics");
        let refill = refill || selected_key.as_str() != field.key;
        ui.set_metric_selected_key(field.key.into());
        let customized = |key: &str| config.metric(key) != defaults.metric(key);
        let items: Vec<MetricItem> = shown
            .iter()
            .map(|(field, editor)| MetricItem {
                key: field.key.into(),
                label: editor.label.into(),
                value_text: value_text(config, field, editor).into(),
                overridden: customized(field.key),
            })
            .collect();
        ui.set_metric_items(ModelRc::new(VecModel::from(items)));
        let override_count = registry::metric_fields()
            .filter(|(field, _)| customized(field.key))
            .count();
        ui.set_metric_override_count(override_count as i32);
        ui.set_metric_reset_all_notice(
            match &session.reset_all {
                ResetAll::Idle => String::new(),
                ResetAll::Confirm => {
                    format!("Activate again to reset all {override_count} customized sizes")
                }
                ResetAll::Removed(removed) => format!("Restore the {} reset sizes", removed.len()),
            }
            .into(),
        );
        let (minimum, maximum) = bounds(field);
        let value = shown_value(config, field, editor).unwrap_or(minimum);
        let inherited = config.metric(field.key) == Some(MetricValue::Inherited);
        let default_text = match field.integer_default() {
            Some(default) => format!("Default {default}{}", editor.unit.suffix()),
            None => editor
                .inherit
                .map_or("Built-in", |inherit| inherit.text)
                .to_owned(),
        };
        ui.set_metric_editor(MetricEditorState {
            key: field.key.into(),
            label: editor.label.into(),
            description: field.description.into(),
            group: editor.group.label().into(),
            value: value as i32,
            minimum: minimum as i32,
            maximum: maximum as i32,
            step: editor.step.into(),
            suffix: editor.unit.suffix().into(),
            unit_spoken: editor.unit.spoken().into(),
            inherited,
            inherit_text: editor.inherit.map_or("", |inherit| inherit.text).into(),
            default_text: default_text.into(),
            overridden: customized(field.key),
            can_undo: session.previous.contains_key(field.key),
            hint: session.hint.as_str().into(),
        });
        if refill {
            ui.set_metric_value_input(if inherited {
                SharedString::new()
            } else {
                value.to_string().into()
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_list_every_metric_once_and_filters_match_every_word() {
        let groups = group_items();
        assert_eq!(groups.len(), MetricGroup::ALL.len());
        let total: i32 = groups.iter().map(|group| group.count).sum();
        assert_eq!(total as usize, registry::metric_fields().count());
        assert!(groups.iter().all(|group| group.count > 0));
        let keys = |filter: &str| {
            shown_metrics(0, filter)
                .iter()
                .map(|(field, _)| field.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys("FOCUS ring"), ["geometry.focus_width"]);
        assert_eq!(keys("composer radius"), ["geometry.composer_radius"]);
        assert!(keys("no such metric").is_empty());
        for (index, group) in MetricGroup::ALL.iter().enumerate() {
            assert!(
                shown_metrics(index, "  ")
                    .iter()
                    .all(|(_, editor)| editor.group == *group)
            );
        }
        assert!(shown_metrics(MetricGroup::ALL.len(), "").is_empty());
    }

    #[test]
    fn typed_values_must_be_whole_numbers_inside_the_registry_range() {
        let (focus, _) = registry::metric_field("geometry.focus_width").unwrap();
        assert_eq!(parse_value(focus, " 4 "), Some(4));
        assert_eq!(parse_value(focus, "1"), Some(1));
        assert_eq!(parse_value(focus, "6"), Some(6));
        for rejected in ["0", "7", "-1", "2.5", "", "two", "4px", "65537", "١"] {
            assert_eq!(parse_value(focus, rejected), None, "{rejected:?}");
        }
        let (roundness, _) = registry::metric_field("geometry.avatar_roundness").unwrap();
        assert_eq!(parse_value(roundness, "0"), Some(0));
        assert_eq!(parse_value(roundness, "50"), Some(50));
        assert_eq!(parse_value(roundness, "51"), None);
        // An omitted optional metric steps from its built-in look.
        let (_, roundness_editor) = registry::metric_field("geometry.avatar_roundness").unwrap();
        let (_, focus_editor) = registry::metric_field("geometry.focus_width").unwrap();
        assert_eq!(
            shown_value(&Config::default(), roundness, roundness_editor),
            Some(35)
        );
        assert_eq!(
            shown_value(&Config::default(), focus, focus_editor),
            Some(2)
        );
    }
}
