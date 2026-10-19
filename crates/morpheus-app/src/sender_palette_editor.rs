//! Sender-name colors: an editable `[senders] colors` palette in
//! `config.toml` that wins over the theme pack's palette.
//!
//! Each sender keeps the stable palette slot derived from their Matrix user
//! ID (`appearance::sender_palette_index`); nothing about a sender is stored.
//! Every change is decided on the re-read file inside the same save as other
//! Settings changes, so an invalid hand edit is never overwritten. A valid
//! `#RRGGBB` or `#RRGGBBAA` value saves at once; partial input only hints.
//! Undo restores the previous palette only while the file still holds the
//! palette this window saved. Low contrast is reported, never corrected.

use std::cell::RefCell;
use std::rc::Rc;

use morpheus_config::registry;
use morpheus_config::{Config, ConfigStore, MAX_SENDER_COLORS, ThemePack};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::{AppWindow, SenderColorRow, color_from_hex, save_config_edit};

/// The identity colors the timeline uses for names when no palette exists;
/// the first edit starts from them so the look does not jump.
const STARTING_COLORS: [&str; 5] = ["#A88CFF", "#78C8B1", "#F39F78", "#64A5E8", "#E78BA4"];
const HEX_HINT: &str =
    "Use #RRGGBB or #RRGGBBAA, with alpha last. The last valid palette stays applied.";
const STALE_UNDO_HINT: &str =
    "The palette was changed outside this window after your change; nothing was undone.";
const LAST_COLOR_HINT: &str =
    "A palette keeps at least one color. Use theme colors to remove your palette.";
const FULL_HINT: &str = "A palette holds at most 16 colors.";
/// WCAG contrast for normal-size text.
const READABLE_CONTRAST: f64 = 4.5;

/// One palette change in this window, as found in and written to the file.
struct UndoStep {
    before: Option<Vec<String>>,
    after: Option<Vec<String>>,
}

#[derive(Default)]
struct Session {
    undo: Option<UndoStep>,
    hint: String,
    source: Option<(Config, Option<ThemePack>)>,
}

thread_local! {
    // Slint callbacks and projections run on the UI thread only.
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
    // Rows update in place while their count is unchanged, so a field being
    // edited keeps its focus when its color saves.
    static ROWS: Rc<VecModel<SenderColorRow>> = Rc::new(VecModel::default());
}

/// Where the shown palette comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Config,
    Theme,
    BuiltIn,
}

/// The palette the timeline uses: `config.toml`, then the theme pack. With
/// neither, names use each sender's identity color; editing starts from those.
fn effective(config: &Config, theme: Option<&ThemePack>) -> (Vec<String>, Source) {
    if let Some(colors) = &config.senders.colors {
        return (colors.clone(), Source::Config);
    }
    if let Some(colors) = theme.and_then(|theme| theme.senders.colors.as_ref()) {
        return (colors.clone(), Source::Theme);
    }
    (
        STARTING_COLORS.iter().map(|color| (*color).to_owned()).collect(),
        Source::BuiltIn,
    )
}

pub(super) fn register(ui: &AppWindow, store: &ConfigStore) {
    let weak = ui.as_weak();
    let edit_store = store.clone();
    ui.on_sender_color_edited(move |index, text| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(rgba) = registry::parse_hex_color(text.trim()) else {
            set_hint(HEX_HINT);
            reproject(&ui);
            return;
        };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        set_hint("");
        let value = registry::format_hex_color(rgba);
        change(&ui, &edit_store, |colors| {
            let slot = colors.get_mut(index)?;
            (*slot != value).then(|| *slot = value)
        });
    });
    let weak = ui.as_weak();
    let move_store = store.clone();
    ui.on_sender_color_move(move |index, direction| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let target = match direction.signum() {
            1 => index.checked_add(1),
            -1 => index.checked_sub(1),
            _ => None,
        };
        let Some(target) = target else { return };
        set_hint("");
        change(&ui, &move_store, |colors| {
            (index < colors.len() && target < colors.len()).then(|| colors.swap(index, target))
        });
    });
    let weak = ui.as_weak();
    let remove_store = store.clone();
    ui.on_sender_color_remove(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        set_hint("");
        let mut last = false;
        change(&ui, &remove_store, |colors| {
            if colors.len() <= 1 {
                last = true;
                return None;
            }
            (index < colors.len()).then(|| {
                colors.remove(index);
            })
        });
        if last {
            set_hint(LAST_COLOR_HINT);
            reproject(&ui);
        }
    });
    let weak = ui.as_weak();
    let add_store = store.clone();
    ui.on_sender_color_add(move || {
        let Some(ui) = weak.upgrade() else { return };
        set_hint("");
        let mut full = false;
        change(&ui, &add_store, |colors| {
            if colors.len() >= MAX_SENDER_COLORS {
                full = true;
                return None;
            }
            // Prefer a starting color the palette does not hold yet.
            let next = STARTING_COLORS
                .iter()
                .find(|candidate| !colors.iter().any(|color| same(color, candidate)))
                .map_or_else(
                    || colors[colors.len() - 1].clone(),
                    |color| (*color).to_owned(),
                );
            colors.push(next);
            Some(())
        });
        if full {
            set_hint(FULL_HINT);
            reproject(&ui);
        }
    });
    let weak = ui.as_weak();
    let reset_store = store.clone();
    ui.on_sender_colors_reset(move || {
        let Some(ui) = weak.upgrade() else { return };
        set_hint("");
        save(&ui, &reset_store, |config| {
            config.senders.colors.take().map(|before| (Some(before), None))
        });
    });
    let weak = ui.as_weak();
    let undo_store = store.clone();
    ui.on_sender_colors_undo(move || {
        let Some(ui) = weak.upgrade() else { return };
        let step = SESSION.with(|session| session.borrow_mut().undo.take());
        let Some(step) = step else { return };
        set_hint("");
        let mut superseded = false;
        let saved = save_config_edit(&ui, &undo_store, |config| {
            // Undo only the change this window made, not a later one.
            if !same_palette(config.senders.colors.as_ref(), step.after.as_ref()) {
                superseded = true;
                return false;
            }
            config.senders.colors = step.before.clone();
            true
        });
        if superseded {
            set_hint(STALE_UNDO_HINT);
        } else if saved.is_none() {
            // Keep the step when the file could not be changed.
            SESSION.with(|session| session.borrow_mut().undo = Some(step));
        }
        reproject(&ui);
    });
}

/// Change the palette as the re-read file holds it, starting from the theme
/// or identity colors when the file has no palette yet. `edit` returns `None`
/// to change nothing.
fn change(
    ui: &AppWindow,
    store: &ConfigStore,
    edit: impl FnOnce(&mut Vec<String>) -> Option<()>,
) {
    let theme = SESSION.with(|session| {
        session
            .borrow()
            .source
            .as_ref()
            .and_then(|(_, theme)| theme.clone())
    });
    save(ui, store, |config| {
        let before = config.senders.colors.clone();
        let (mut colors, _) = effective(config, theme.as_ref());
        edit(&mut colors)?;
        let after = Some(colors);
        if same_palette(before.as_ref(), after.as_ref()) {
            return None;
        }
        config.senders.colors = after.clone();
        Some((before, after))
    });
}

/// Save one palette change decided on the re-read file, keeping one undo
/// step only once the save succeeded.
fn save(
    ui: &AppWindow,
    store: &ConfigStore,
    decide: impl FnOnce(&mut Config) -> Option<(Option<Vec<String>>, Option<Vec<String>>)>,
) {
    let mut step = None;
    let saved = save_config_edit(ui, store, |config| {
        step = decide(config).map(|(before, after)| UndoStep { before, after });
        step.is_some()
    });
    if let (Some(_), Some(step)) = (saved, step) {
        SESSION.with(|session| session.borrow_mut().undo = Some(step));
    }
    reproject(ui);
}

/// Project the editor for an accepted configuration, including reloads of
/// external edits. An edit made elsewhere ends this window's undo step.
pub(super) fn project(ui: &AppWindow, config: &Config, theme: Option<&ThemePack>) {
    SESSION.with(|session| {
        let mut session = session.borrow_mut();
        if session
            .undo
            .as_ref()
            .is_some_and(|step| !same_palette(config.senders.colors.as_ref(), step.after.as_ref()))
        {
            session.undo = None;
        }
        session.source = Some((config.clone(), theme.cloned()));
    });
    reproject(ui);
}

/// Re-project after the active account changed its colors, so the contrast
/// check reads the conversation background now shown.
pub(super) fn refresh(ui: &AppWindow) {
    reproject(ui);
}

fn set_hint(text: &str) {
    SESSION.with(|session| session.borrow_mut().hint = text.into());
}

fn same(left: &str, right: &str) -> bool {
    registry::parse_hex_color(left) == registry::parse_hex_color(right)
}

/// Whether two optional palettes hold the same colors in the same order;
/// spelling and case do not matter.
fn same_palette(left: Option<&Vec<String>>, right: Option<&Vec<String>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| same(left, right))
        }
        _ => false,
    }
}

fn relative_luminance([red, green, blue, _]: [u8; 4]) -> f64 {
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
}

/// The WCAG contrast ratio of two opaque colors.
fn contrast(foreground: [u8; 4], background: [u8; 4]) -> f64 {
    let (first, second) = (relative_luminance(foreground), relative_luminance(background));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn reproject(ui: &AppWindow) {
    SESSION.with(|session| {
        let session = session.borrow();
        let fallback = (Config::default(), None);
        let (config, theme) = session.source.as_ref().unwrap_or(&fallback);
        let (colors, source) = effective(config, theme.as_ref());
        let canvas = ui.invoke_token_color("canvas".into());
        let canvas = [canvas.red(), canvas.green(), canvas.blue(), 255];
        let rows: Vec<SenderColorRow> = colors
            .iter()
            .enumerate()
            .map(|(index, hex)| {
                let rgba = registry::parse_hex_color(hex).unwrap_or([0, 0, 0, 255]);
                SenderColorRow {
                    index: index as i32,
                    hex: hex.as_str().into(),
                    swatch: color_from_hex(hex),
                    readable: contrast(rgba, canvas) >= READABLE_CONTRAST,
                }
            })
            .collect();
        ROWS.with(|model| {
            if model.row_count() == rows.len() {
                for (index, row) in rows.into_iter().enumerate() {
                    if model.row_data(index).as_ref() != Some(&row) {
                        model.set_row_data(index, row);
                    }
                }
            } else {
                model.set_vec(rows);
            }
            ui.set_sender_color_rows(ModelRc::from(model.clone()));
        });
        ui.set_sender_palette_source(
            match source {
                Source::Config => "Your colors, saved in config.toml",
                Source::Theme => "From the selected theme; editing saves your own copy",
                Source::BuiltIn => "Built-in identity colors; editing saves your own palette",
            }
            .into(),
        );
        ui.set_sender_palette_custom(source == Source::Config);
        ui.set_sender_palette_can_undo(session.undo.is_some());
        ui.set_sender_palette_hint(session.hint.as_str().into());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_effective_palette_prefers_config_then_theme_then_identity_colors() {
        let mut config = Config::default();
        let (colors, source) = effective(&config, None);
        assert_eq!(source, Source::BuiltIn);
        assert_eq!(colors.len(), STARTING_COLORS.len());
        let theme = morpheus_config::BuiltInStylePreset::Nocturne.theme().unwrap();
        let (colors, source) = effective(&config, Some(&theme));
        assert_eq!(source, Source::Theme);
        assert_eq!(Some(&colors), theme.senders.colors.as_ref());
        config.senders.colors = Some(vec!["#123456".into()]);
        let (colors, source) = effective(&config, Some(&theme));
        assert_eq!((colors, source), (vec!["#123456".to_owned()], Source::Config));
        for color in STARTING_COLORS {
            assert!(registry::parse_hex_color(color).is_some(), "{color}");
        }
    }

    #[test]
    fn palettes_compare_by_color_and_order_and_contrast_is_symmetric() {
        let upper = vec!["#ABCDEF".to_owned(), "#000000".to_owned()];
        let lower = vec!["#abcdef".to_owned(), "#000000FF".to_owned()];
        assert!(same_palette(Some(&upper), Some(&lower)));
        let reversed: Vec<String> = upper.iter().rev().cloned().collect();
        assert!(!same_palette(Some(&upper), Some(&reversed)));
        assert!(!same_palette(Some(&upper), None));
        assert!(same_palette(None, None));
        let white = [255, 255, 255, 255];
        let black = [0, 0, 0, 255];
        assert!((contrast(white, black) - 21.0).abs() < 0.01);
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 0.001);
    }
}
