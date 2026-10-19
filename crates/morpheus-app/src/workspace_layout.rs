//! Workspace pane arrangement: the left-to-right order of the Space rail,
//! room list, conversation and details panel, and which of them are hidden
//! or collapsed, from `config.toml` `[layout]`.
//!
//! The window keeps one element per pane and only places it (see
//! `pane-x` and the `pane-*-w` properties in app-window.slint), so a change
//! never re-creates a timeline, composer, scroll position or focus. The
//! geometry rules are written once here as `arrange`, which the window
//! mirrors and the tests below pin down:
//!
//! - Every pane appears exactly once; the conversation is never hidden.
//! - A window narrower than the saved panes gives way in a fixed order: the
//!   details panel (to a strip, then away), then the room list, then the
//!   Space rail; the conversation keeps at least `CONVERSATION_MIN`.
//! - The active account's control stays at the bottom left: over the room
//!   list when it leads the arrangement at full width, otherwise in its own
//!   `ACCOUNT_COLUMN` column, which also offers Restore.
//!
//! Edits go through the window's existing validated config writer
//! (`save_config_change`); no other writer exists here. An invalid file never
//! reaches `apply_config`, so the last valid arrangement stays.

use morpheus_config::{Config, ConfigStore, DetailsSide, Layout, PaneId};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{AppWindow, LayoutPaneItem};

/// Width of a collapsed pane's restore strip.
pub(crate) const STRIP: f32 = 28.0;
/// The conversation never gets narrower than this while anything else could
/// give way.
pub(crate) const CONVERSATION_MIN: f32 = 360.0;
/// The account control's own column, when the room list does not lead.
pub(crate) const ACCOUNT_COLUMN: f32 = 64.0;

/// The four built-in presets, in the order Settings shows them.
const PRESET_STANDARD: i32 = 0;
const PRESET_FOCUSED: i32 = 1;
const PRESET_CONVERSATION_FIRST: i32 = 2;
const PRESET_MINIMAL: i32 = 3;

/// What `arrange` lays out: each pane's wanted width (0 for hidden or not
/// shown, `STRIP` for collapsed) and the window width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ArrangeInput {
    pub order: [PaneId; 4],
    pub rail: f32,
    pub rooms: f32,
    /// The room list's full (saved) width, to tell a full room list from a
    /// strip or a hidden one.
    pub rooms_full: f32,
    pub details: f32,
    pub window: f32,
}

/// Where each pane is drawn, and where the account control is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Arrangement {
    /// Indexed like `PaneId::ALL`: spaces, rooms, conversation, details.
    pub x: [f32; 4],
    pub width: [f32; 4],
    /// The account control has its own column at the left edge.
    pub account_column: bool,
    pub account_x: f32,
    pub account_width: f32,
}

fn index(pane: PaneId) -> usize {
    PaneId::ALL
        .iter()
        .position(|candidate| *candidate == pane)
        .unwrap_or(0)
}

fn slot(order: &[PaneId; 4], pane: PaneId) -> usize {
    order.iter().position(|candidate| *candidate == pane).unwrap_or(0)
}

/// The reference geometry; app-window.slint computes the same values.
pub(crate) fn arrange(input: ArrangeInput) -> Arrangement {
    let fits = |space: f32| space >= CONVERSATION_MIN;
    let rooms_slot = slot(&input.order, PaneId::Rooms);
    let rooms_leads = rooms_slot < slot(&input.order, PaneId::Conversation)
        && rooms_slot < slot(&input.order, PaneId::Details);
    let column0 = !(rooms_leads && input.rooms == input.rooms_full);
    let avail0 = input.window - if column0 { ACCOUNT_COLUMN } else { 0.0 };
    let details = if fits(avail0 - input.rail - input.rooms - input.details) {
        input.details
    } else if input.details > 0.0 && fits(avail0 - input.rail - input.rooms - STRIP) {
        STRIP
    } else {
        0.0
    };
    let rooms = if fits(avail0 - input.rail - input.rooms - details) {
        input.rooms
    } else if input.rooms > 0.0 && fits(avail0 - input.rail - STRIP - details) {
        STRIP
    } else {
        0.0
    };
    let account_column = column0 || rooms != input.rooms_full;
    let avail = input.window - if account_column { ACCOUNT_COLUMN } else { 0.0 };
    let rail = if fits(avail - input.rail - rooms - details) {
        input.rail
    } else {
        0.0
    };
    let conversation = (avail - rail - rooms - details).max(0.0);
    let mut width = [0.0; 4];
    width[index(PaneId::Spaces)] = rail;
    width[index(PaneId::Rooms)] = rooms;
    width[index(PaneId::Conversation)] = conversation;
    width[index(PaneId::Details)] = details;
    let mut x = [0.0; 4];
    let mut left = if account_column { ACCOUNT_COLUMN } else { 0.0 };
    for pane in input.order {
        x[index(pane)] = left;
        left += width[index(pane)];
    }
    let (account_x, account_width) = if account_column {
        (0.0, ACCOUNT_COLUMN)
    } else {
        (x[index(PaneId::Rooms)], rooms)
    };
    Arrangement {
        x,
        width,
        account_column,
        account_x,
        account_width,
    }
}

/// Install the arrangement callbacks.
pub(crate) fn register(ui: &AppWindow, store: &ConfigStore) {
    let edit = |change: fn(&mut Layout, &str, i32) -> bool| {
        let weak = ui.as_weak();
        let store = store.clone();
        move |pane: &str, value: i32| {
            if let Some(ui) = weak.upgrade() {
                let _ = crate::save_config_change(&ui, &store, |config| {
                    change(&mut config.layout, pane, value);
                });
            }
        }
    };
    let move_pane = edit(|layout, pane, delta| move_pane(layout, pane, delta));
    ui.on_move_pane(move |pane, delta| move_pane(pane.as_str(), delta));
    let set_hidden = edit(|layout, pane, hidden| set_hidden(layout, pane, hidden != 0));
    ui.on_set_pane_hidden(move |pane, hidden| set_hidden(pane.as_str(), i32::from(hidden)));
    let set_collapsed =
        edit(|layout, pane, collapsed| set_collapsed(layout, pane, collapsed != 0));
    ui.on_set_pane_collapsed(move |pane, collapsed| {
        set_collapsed(pane.as_str(), i32::from(collapsed));
    });
    let preset = edit(|layout, _, preset| apply_preset(layout, preset));
    ui.on_choose_layout_preset(move |index| preset("", index));
    let restore = edit(|layout, _, _| layout.restore_default_panes());
    ui.on_restore_default_layout(move || restore("", 0));
    let reset_order = edit(|layout, _, _| {
        let changed = !layout.pane_order.is_empty();
        layout.pane_order.clear();
        changed
    });
    ui.on_reset_pane_order(move || reset_order("", 0));
    let reset_visibility = edit(|layout, _, _| {
        let changed = !layout.hidden_panes.is_empty() || !layout.collapsed_panes.is_empty();
        layout.hidden_panes.clear();
        layout.collapsed_panes.clear();
        changed
    });
    ui.on_reset_pane_visibility(move || reset_visibility("", 0));
}

/// Project an accepted configuration's arrangement into the window.
pub(crate) fn apply_config(ui: &AppWindow, config: &Config) {
    let layout = &config.layout;
    let order = layout.effective_pane_order();
    let slot_of = |pane| i32::try_from(slot(&order, pane)).unwrap_or(0);
    ui.set_pane_slot_spaces(slot_of(PaneId::Spaces));
    ui.set_pane_slot_rooms(slot_of(PaneId::Rooms));
    ui.set_pane_slot_conversation(slot_of(PaneId::Conversation));
    ui.set_pane_slot_details(slot_of(PaneId::Details));
    ui.set_pane_spaces_hidden(layout.pane_hidden(PaneId::Spaces));
    ui.set_pane_rooms_hidden(layout.pane_hidden(PaneId::Rooms));
    ui.set_pane_rooms_collapsed(layout.pane_collapsed(PaneId::Rooms));
    ui.set_pane_details_hidden(layout.pane_hidden(PaneId::Details));
    ui.set_pane_details_collapsed(layout.pane_collapsed(PaneId::Details));
    ui.set_layout_customized(customized(layout));
    ui.set_layout_preset(matching_preset(layout));
    ui.set_layout_panes(ModelRc::new(VecModel::from(editor_rows(layout))));
}

fn customized(layout: &Layout) -> bool {
    !layout.pane_order.is_empty()
        || !layout.hidden_panes.is_empty()
        || !layout.collapsed_panes.is_empty()
}

fn pane_from_id(id: &str) -> Option<PaneId> {
    PaneId::ALL.into_iter().find(|pane| pane.id() == id)
}

/// Move one pane `delta` places (left negative); writes the full order.
fn move_pane(layout: &mut Layout, id: &str, delta: i32) -> bool {
    let Some(pane) = pane_from_id(id) else {
        return false;
    };
    let mut order = layout.effective_pane_order();
    let from = slot(&order, pane);
    let Some(to) = from
        .checked_add_signed(isize::try_from(delta).unwrap_or(0))
        .filter(|to| *to < order.len())
    else {
        return false;
    };
    if to == from {
        return false;
    }
    order.swap(from, to);
    layout.pane_order = order.to_vec();
    true
}

fn set_hidden(layout: &mut Layout, id: &str, hidden: bool) -> bool {
    let Some(pane) = pane_from_id(id).filter(|pane| pane.can_hide()) else {
        return false;
    };
    let present = layout.hidden_panes.contains(&pane);
    if hidden == present {
        return false;
    }
    if hidden {
        layout.hidden_panes.push(pane);
        layout.collapsed_panes.retain(|collapsed| *collapsed != pane);
    } else {
        layout.hidden_panes.retain(|shown| *shown != pane);
    }
    true
}

fn set_collapsed(layout: &mut Layout, id: &str, collapsed: bool) -> bool {
    let Some(pane) = pane_from_id(id).filter(|pane| pane.can_collapse()) else {
        return false;
    };
    let present = layout.collapsed_panes.contains(&pane);
    if collapsed == present {
        return false;
    }
    if collapsed {
        layout.collapsed_panes.push(pane);
        layout.hidden_panes.retain(|hidden| *hidden != pane);
    } else {
        layout.collapsed_panes.retain(|shown| *shown != pane);
    }
    true
}

fn preset_layout(preset: i32, details_side: DetailsSide) -> Option<Layout> {
    let base = Layout {
        details_side,
        ..Layout::default()
    };
    Some(match preset {
        PRESET_STANDARD => base,
        PRESET_FOCUSED => Layout {
            hidden_panes: vec![PaneId::Spaces],
            collapsed_panes: vec![PaneId::Rooms],
            ..base
        },
        PRESET_CONVERSATION_FIRST => Layout {
            pane_order: vec![
                PaneId::Spaces,
                PaneId::Conversation,
                PaneId::Details,
                PaneId::Rooms,
            ],
            ..base
        },
        PRESET_MINIMAL => Layout {
            hidden_panes: vec![PaneId::Spaces, PaneId::Rooms],
            ..base
        },
        _ => return None,
    })
}

/// A preset changes only the arrangement keys; widths, navigation, side and
/// every other layout setting stay.
fn apply_preset(layout: &mut Layout, preset: i32) -> bool {
    let Some(target) = preset_layout(preset, layout.details_side) else {
        return false;
    };
    let changed = layout.pane_order != target.pane_order
        || layout.hidden_panes != target.hidden_panes
        || layout.collapsed_panes != target.collapsed_panes;
    layout.pane_order = target.pane_order;
    layout.hidden_panes = target.hidden_panes;
    layout.collapsed_panes = target.collapsed_panes;
    changed
}

/// The preset the current arrangement equals, or -1 for a custom one.
fn matching_preset(layout: &Layout) -> i32 {
    [
        PRESET_STANDARD,
        PRESET_FOCUSED,
        PRESET_CONVERSATION_FIRST,
        PRESET_MINIMAL,
    ]
    .into_iter()
    .find(|preset| {
        preset_layout(*preset, layout.details_side).is_some_and(|target| {
            layout.pane_order == target.pane_order
                && layout.hidden_panes == target.hidden_panes
                && layout.collapsed_panes == target.collapsed_panes
        })
    })
    .unwrap_or(-1)
}

fn editor_rows(layout: &Layout) -> Vec<LayoutPaneItem> {
    let order = layout.effective_pane_order();
    order
        .iter()
        .enumerate()
        .map(|(position, pane)| LayoutPaneItem {
            id: pane.id().into(),
            label: pane.label().into(),
            hidden: layout.pane_hidden(*pane),
            collapsed: layout.pane_collapsed(*pane),
            can_hide: pane.can_hide(),
            can_collapse: pane.can_collapse(),
            can_move_up: position > 0,
            can_move_down: position + 1 < order.len(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: [PaneId; 4] = [
        PaneId::Spaces,
        PaneId::Rooms,
        PaneId::Conversation,
        PaneId::Details,
    ];

    fn input(order: [PaneId; 4], window: f32) -> ArrangeInput {
        ArrangeInput {
            order,
            rail: 70.0,
            rooms: 260.0,
            rooms_full: 260.0,
            details: 300.0,
            window,
        }
    }

    fn every_pane_once_without_overlap(arrangement: &Arrangement, input: &ArrangeInput) {
        let mut edge = if arrangement.account_column {
            ACCOUNT_COLUMN
        } else {
            0.0
        };
        for pane in input.order {
            let at = index(pane);
            assert_eq!(arrangement.x[at], edge, "{pane:?} starts where the last ended");
            edge += arrangement.width[at];
        }
        assert!(edge <= input.window.max(CONVERSATION_MIN) + 0.5);
    }

    #[test]
    fn the_default_arrangement_matches_the_classic_layout() {
        let input = input(STANDARD, 1310.0);
        let shown = arrange(input);
        assert!(!shown.account_column, "the account control sits in the room list");
        assert_eq!(shown.width, [70.0, 260.0, 680.0, 300.0]);
        assert_eq!(shown.x, [0.0, 70.0, 330.0, 1010.0]);
        assert_eq!((shown.account_x, shown.account_width), (70.0, 260.0));
        every_pane_once_without_overlap(&shown, &input);
    }

    #[test]
    fn narrow_windows_give_way_in_order_and_keep_the_conversation() {
        // Details shrinks to a strip, then goes; then the room list; then the rail.
        for (window, widths) in [
            (980.0, [70.0, 260.0, 622.0, STRIP]),
            (700.0, [70.0, 260.0, 370.0, 0.0]),
        ] {
            let input = input(STANDARD, window);
            let shown = arrange(input);
            assert_eq!(shown.width, widths, "{window}");
            assert!(!shown.account_column, "{window}: the room list still leads");
            every_pane_once_without_overlap(&shown, &input);
        }
        let tight = input(STANDARD, 640.0);
        let shown = arrange(tight);
        assert_eq!(shown.width[index(PaneId::Details)], 0.0);
        assert!(shown.width[index(PaneId::Rooms)] <= STRIP);
        assert!(shown.width[index(PaneId::Conversation)] >= CONVERSATION_MIN - 0.5);
        // The room list gave way, so the account control has its column.
        assert!(shown.account_column);
        assert_eq!((shown.account_x, shown.account_width), (0.0, ACCOUNT_COLUMN));
        every_pane_once_without_overlap(&shown, &tight);
    }

    #[test]
    fn the_account_control_stays_bottom_left_in_every_arrangement() {
        let orders = [
            STANDARD,
            [PaneId::Conversation, PaneId::Details, PaneId::Rooms, PaneId::Spaces],
            [PaneId::Details, PaneId::Conversation, PaneId::Spaces, PaneId::Rooms],
            [PaneId::Rooms, PaneId::Spaces, PaneId::Conversation, PaneId::Details],
        ];
        for order in orders {
            for rooms in [260.0, STRIP, 0.0] {
                let arranged = ArrangeInput {
                    rooms,
                    ..input(order, 1310.0)
                };
                let shown = arrange(arranged);
                // Either at the window's left edge, or over the room list,
                // which then comes first (only the rail may precede it).
                if shown.account_column {
                    assert_eq!(shown.account_x, 0.0);
                } else {
                    assert_eq!(rooms, 260.0);
                    assert!(shown.account_x <= 70.0, "{order:?}");
                    assert_eq!(shown.account_width, rooms);
                }
                assert!(shown.account_width > 0.0);
                every_pane_once_without_overlap(&shown, &arranged);
            }
        }
    }

    #[test]
    fn hidden_and_collapsed_panes_restore_to_the_default() {
        let mut layout = Layout::default();
        assert!(set_hidden(&mut layout, "rooms", true));
        assert!(set_collapsed(&mut layout, "details", true));
        assert!(move_pane(&mut layout, "conversation", -2));
        // The conversation can never be hidden, the rail never collapsed, and
        // unknown IDs or impossible moves change nothing.
        assert!(!set_hidden(&mut layout, "conversation", true));
        assert!(!set_collapsed(&mut layout, "spaces", true));
        assert!(!set_hidden(&mut layout, "members", true));
        assert!(!move_pane(&mut layout, "spaces", 9));
        assert!(customized(&layout));
        let config = Config {
            layout: layout.clone(),
            ..Config::default()
        };
        config.validate().unwrap();
        // Hiding a collapsed pane replaces its collapse (and the reverse).
        assert!(set_hidden(&mut layout, "details", true));
        assert!(!layout.pane_collapsed(PaneId::Details));
        assert!(layout.restore_default_panes());
        assert!(!customized(&layout));
        assert_eq!(layout.effective_pane_order(), STANDARD);
    }

    #[test]
    fn presets_change_only_the_arrangement_and_are_recognized() {
        let mut layout = Layout {
            room_list_width: 300,
            details_side: DetailsSide::Left,
            ..Layout::default()
        };
        for preset in [
            PRESET_FOCUSED,
            PRESET_CONVERSATION_FIRST,
            PRESET_MINIMAL,
            PRESET_STANDARD,
        ] {
            apply_preset(&mut layout, preset);
            assert_eq!(matching_preset(&layout), preset);
            assert_eq!(layout.room_list_width, 300);
            assert_eq!(layout.details_side, DetailsSide::Left);
            let config = Config {
                layout: layout.clone(),
                ..Config::default()
            };
            config.validate().unwrap();
        }
        assert!(!apply_preset(&mut layout, 42));
        let rows = editor_rows(&layout);
        assert_eq!(rows.len(), 4);
        assert!(!rows[0].can_move_up && !rows[3].can_move_down);
        assert!(rows.iter().any(|row| row.id == "conversation" && !row.can_hide));
    }
}
