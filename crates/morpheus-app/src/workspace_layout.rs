//! Workspace pane arrangement: the left-to-right order of the Space rail,
//! room list, conversation and details panel, and which of them are hidden
//! or collapsed, from `config.toml` `[layout]`.
//!
//! The window keeps one element per pane and only places it (see
//! `pane-x` and the `pane-*-w` properties in app-window.slint), so a change
//! never re-creates a timeline, composer, scroll position or focus. The
//! saved-layout geometry is described here by `arrange`, mirrored by the
//! window and pinned by the tests below. Explicit panel navigation temporarily
//! prioritizes its destination in the window; generated-UI tests cover that
//! separate presentation path without changing the stored arrangement:
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

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Weak},
};

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

/// Saved width ranges, as `config.toml` validates them.
pub(crate) const ROOMS_WIDTH: (u16, u16) = (180, 480);
pub(crate) const DETAILS_WIDTH: (u16, u16) = (200, 520);
const CONFLICT: &str =
    "The layout changed elsewhere while you were dragging; your change was not applied.";
const REFUSED: &str = "That change can't be made right now; the layout is unchanged.";

/// One pointer resize or drag arrangement in progress. Its preview lives in
/// the window only; nothing is written until it is committed once, and only
/// if the fields it changes still hold the values it started from.
struct Gesture {
    token: i32,
    account_id: String,
    room_id: String,
    page: i32,
    context_current: Box<dyn Fn() -> bool>,
    /// The arrangement as last applied when the gesture began.
    base: Layout,
}

thread_local! {
    // Slint callbacks and config projections run on the UI thread only.
    static APPLIED: RefCell<Option<Layout>> = const { RefCell::new(None) };
    static GESTURE: RefCell<Option<Gesture>> = const { RefCell::new(None) };
    static NEXT_TOKEN: Cell<i32> = const { Cell::new(0) };
}

/// Why a committed gesture did not change the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// The field changed in the file since the gesture began.
    Conflict,
    /// The value is unchanged, or the pane cannot take it.
    Nothing,
}

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
    order
        .iter()
        .position(|candidate| *candidate == pane)
        .unwrap_or(0)
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
    let set_collapsed = edit(|layout, pane, collapsed| set_collapsed(layout, pane, collapsed != 0));
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

    let weak = ui.as_weak();
    ui.on_begin_pane_gesture(move || {
        weak.upgrade().map_or(0, |ui| {
            // A live gesture requires an exact backend selection, installed
            // by register_live. Presentation strings alone cannot authorize it.
            if ui.get_live_session() {
                return 0;
            }
            begin_gesture(&ui, Box::new(|| true))
        })
    });
    let weak = ui.as_weak();
    ui.on_cancel_pane_gesture(move |token| {
        if let Some(ui) = weak.upgrade() {
            end_gesture(&ui, token);
        }
    });
    let weak = ui.as_weak();
    let width_store = store.clone();
    ui.on_commit_pane_width(move |token, pane, width| {
        if let Some(ui) = weak.upgrade() {
            commit(&ui, &width_store, token, |current, base| {
                merge_width(current, base, pane.as_str(), width)
            });
        }
    });
    let weak = ui.as_weak();
    let order_store = store.clone();
    ui.on_commit_pane_order(move |token, pane, index| {
        if let Some(ui) = weak.upgrade() {
            commit(&ui, &order_store, token, |current, base| {
                merge_order(current, base, pane.as_str(), index)
            });
        }
    });
}

/// Bind production gestures to the exact account selection and navigation
/// epoch. Returning to the same account/room strings never revives a drag.
pub(crate) fn register_live(ui: &AppWindow, store: &ConfigStore, live: &Arc<crate::live::LiveUi>) {
    register(ui, store);
    let weak = ui.as_weak();
    let live = Arc::downgrade(live);
    ui.on_begin_pane_gesture(move || {
        let Some(ui) = weak.upgrade() else {
            return 0;
        };
        let Some(context) = capture_context(&ui, &live) else {
            return 0;
        };
        begin_gesture(&ui, context)
    });
}

fn capture_context(
    ui: &AppWindow,
    weak: &Weak<crate::live::LiveUi>,
) -> Option<Box<dyn Fn() -> bool>> {
    let live = weak.upgrade()?;
    let navigation = live.current_navigation_epoch();
    let was_live = ui.get_live_session();
    let selection = if was_live {
        Some(live.account_selection(ui.get_active_account_local_id().as_str())?)
    } else {
        None
    };
    let weak = weak.clone();
    let current = move || {
        weak.upgrade().is_some_and(|live| {
            live.current_navigation_epoch() == navigation
                && live.is_live.load(std::sync::atomic::Ordering::Acquire) == was_live
                && selection
                    .as_ref()
                    .is_none_or(|selection| selection.is_current())
        })
    };
    if !current() {
        return None;
    }
    Some(Box::new(current))
}

/// A modal, lock, sign-in, recovery or account switch owns the window: no
/// pointer gesture may start or commit, and none may move focus.
fn gesture_blocked(ui: &AppWindow) -> bool {
    (!ui.get_live_session() && !ui.get_preview_session() && ui.get_page() != 1)
        || ui.get_show_sign_in()
        || ui.get_account_switch_pending()
        || ui.get_account_menu_open()
        || ui.get_manage_accounts_open()
        || ui.get_access_open()
        || ui.get_room_actions_open()
        || ui.get_room_admin_open()
        || ui.get_member_confirm_open()
        || ui.get_native_capture_picker_open()
        || ui.get_leave_call_dialog()
        || !ui.get_security_recovery_key().is_empty()
        || ui.get_quick_switcher_open()
        || ui.get_image_preview_open()
        || ui.get_poll_dialog_open()
}

/// Start a gesture: capture the applied arrangement and the account. Returns
/// its token, or 0 when no gesture may start (the window then shows no
/// preview). A new gesture replaces any unfinished one.
fn begin_gesture(ui: &AppWindow, context_current: Box<dyn Fn() -> bool>) -> i32 {
    let Some(base) = APPLIED.with(|applied| applied.borrow().clone()) else {
        return 0;
    };
    if gesture_blocked(ui) || !context_current() {
        return 0;
    }
    let token = NEXT_TOKEN.with(|next| {
        let token = next.get().checked_add(1)?;
        next.set(token);
        Some(token)
    });
    let Some(token) = token else {
        return 0;
    };
    GESTURE.with(|gesture| {
        *gesture.borrow_mut() = Some(Gesture {
            token,
            account_id: ui.get_active_account_local_id().to_string(),
            room_id: ui.get_active_room_id().to_string(),
            page: ui.get_page(),
            context_current,
            base,
        });
    });
    ui.set_layout_notice("".into());
    token
}

/// Forget the gesture `token` (cancelled, interrupted or committed).
fn end_gesture(ui: &AppWindow, token: i32) -> Option<Gesture> {
    let ended = GESTURE.with(|gesture| {
        let mut gesture = gesture.borrow_mut();
        if gesture
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            gesture.take()
        } else {
            None
        }
    });
    if ended.is_some() {
        ui.set_pane_gesture(0);
    }
    ended
}

/// Commit the gesture `token` once: only while its account is still the one
/// shown and nothing modal owns the window, and only the field it changed,
/// merged onto the configuration as re-read by the existing writer. A field
/// that changed since the gesture began is a conflict, never overwritten.
fn commit(
    ui: &AppWindow,
    store: &ConfigStore,
    token: i32,
    merge: impl FnOnce(&mut Layout, &Layout) -> Result<(), Refusal>,
) {
    let Some(gesture) = end_gesture(ui, token) else {
        return;
    };
    if gesture.account_id != ui.get_active_account_local_id().as_str()
        || gesture.room_id != ui.get_active_room_id().as_str()
        || gesture.page != ui.get_page()
        || !(gesture.context_current)()
        || gesture_blocked(ui)
    {
        ui.set_layout_notice(REFUSED.into());
        return;
    }
    let mut refusal = None;
    let saved = crate::save_config_edit(ui, store, |config| {
        match merge(&mut config.layout, &gesture.base) {
            Ok(()) => true,
            Err(why) => {
                refusal = Some(why);
                false
            }
        }
    });
    match (saved, refusal) {
        (Some(_), _) => ui.set_layout_notice("".into()),
        (None, Some(Refusal::Conflict)) => ui.set_layout_notice(CONFLICT.into()),
        (None, Some(Refusal::Nothing)) => {}
        // The writer refused (an invalid file, or a style preview it would
        // not save over); its own error is shown, and nothing changed.
        (None, None) => ui.set_layout_notice(REFUSED.into()),
    }
}

fn width_of(layout: &Layout, pane: PaneId) -> Option<u16> {
    match pane {
        PaneId::Rooms => Some(layout.room_list_width),
        PaneId::Details => Some(layout.details_width),
        PaneId::Spaces | PaneId::Conversation => None,
    }
}

/// Set one pane's saved width (clamped to its range) on `current`, if it
/// still holds the width the gesture started from.
pub(crate) fn merge_width(
    current: &mut Layout,
    base: &Layout,
    pane: &str,
    width: i32,
) -> Result<(), Refusal> {
    let pane = pane_from_id(pane).ok_or(Refusal::Nothing)?;
    let (start, now) = (width_of(base, pane), width_of(current, pane));
    let (Some(start), Some(now)) = (start, now) else {
        return Err(Refusal::Nothing);
    };
    if now != start {
        return Err(Refusal::Conflict);
    }
    let (minimum, maximum) = match pane {
        PaneId::Rooms => ROOMS_WIDTH,
        _ => DETAILS_WIDTH,
    };
    let width = u16::try_from(width.clamp(i32::from(minimum), i32::from(maximum)))
        .map_err(|_| Refusal::Nothing)?;
    if width == now {
        return Err(Refusal::Nothing);
    }
    match pane {
        PaneId::Rooms => current.room_list_width = width,
        _ => current.details_width = width,
    }
    Ok(())
}

/// Move one pane to `index` (0 to 3) of the arrangement on `current`, if its
/// order still is the one the gesture started from. Every pane stays exactly
/// once, and hidden or collapsed panes keep their state and widths.
pub(crate) fn merge_order(
    current: &mut Layout,
    base: &Layout,
    pane: &str,
    index: i32,
) -> Result<(), Refusal> {
    let pane = pane_from_id(pane).ok_or(Refusal::Nothing)?;
    if current.effective_pane_order() != base.effective_pane_order()
        || current.pane_order != base.pane_order
    {
        return Err(Refusal::Conflict);
    }
    let order = reordered(base.effective_pane_order(), pane, index).ok_or(Refusal::Nothing)?;
    if order == current.effective_pane_order() {
        return Err(Refusal::Nothing);
    }
    current.pane_order = order.to_vec();
    Ok(())
}

/// `order` with `pane` moved to `index`, counted in the final order.
pub(crate) fn reordered(order: [PaneId; 4], pane: PaneId, index: i32) -> Option<[PaneId; 4]> {
    let index = usize::try_from(index)
        .ok()
        .filter(|index| *index < order.len())?;
    let mut rest: Vec<PaneId> = order.into_iter().filter(|other| *other != pane).collect();
    rest.insert(index, pane);
    <[PaneId; 4]>::try_from(rest).ok()
}

/// Project an accepted configuration's arrangement into the window.
pub(crate) fn apply_config(ui: &AppWindow, config: &Config) {
    let layout = &config.layout;
    // A new accepted config ends temporary explicit-open presentation. The
    // saved hide/collapse choices remain authoritative after an edit/reload.
    ui.set_details_reveal_open(false);
    // Gestures compare against this; a gesture begun before an external
    // change still holds its own base and is refused as a conflict.
    APPLIED.with(|applied| *applied.borrow_mut() = Some(layout.clone()));
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
        layout
            .collapsed_panes
            .retain(|collapsed| *collapsed != pane);
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
            assert_eq!(
                arrangement.x[at], edge,
                "{pane:?} starts where the last ended"
            );
            edge += arrangement.width[at];
        }
        assert!(edge <= input.window.max(CONVERSATION_MIN) + 0.5);
    }

    #[test]
    fn the_default_arrangement_matches_the_classic_layout() {
        let input = input(STANDARD, 1310.0);
        let shown = arrange(input);
        assert!(
            !shown.account_column,
            "the account control sits in the room list"
        );
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
        assert_eq!(
            (shown.account_x, shown.account_width),
            (0.0, ACCOUNT_COLUMN)
        );
        every_pane_once_without_overlap(&shown, &tight);
    }

    #[test]
    fn the_account_control_stays_bottom_left_in_every_arrangement() {
        let orders = [
            STANDARD,
            [
                PaneId::Conversation,
                PaneId::Details,
                PaneId::Rooms,
                PaneId::Spaces,
            ],
            [
                PaneId::Details,
                PaneId::Conversation,
                PaneId::Spaces,
                PaneId::Rooms,
            ],
            [
                PaneId::Rooms,
                PaneId::Spaces,
                PaneId::Conversation,
                PaneId::Details,
            ],
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
        assert!(
            rows.iter()
                .any(|row| row.id == "conversation" && !row.can_hide)
        );
    }

    #[test]
    fn a_dragged_width_commits_once_clamped_and_never_over_an_external_change() {
        let base = Layout::default();
        let mut current = base.clone();
        // Clamped to the saved range, only the dragged pane's width changes.
        assert_eq!(merge_width(&mut current, &base, "rooms", 9_000), Ok(()));
        assert_eq!(current.room_list_width, ROOMS_WIDTH.1);
        assert_eq!(current.details_width, base.details_width);
        let mut current = base.clone();
        assert_eq!(merge_width(&mut current, &base, "details", 10), Ok(()));
        assert_eq!(current.details_width, DETAILS_WIDTH.0);
        // The file changed during the drag: a conflict, nothing overwritten.
        let mut edited = Layout {
            room_list_width: 333,
            ..base.clone()
        };
        assert_eq!(
            merge_width(&mut edited, &base, "rooms", 250),
            Err(Refusal::Conflict)
        );
        assert_eq!(edited.room_list_width, 333);
        // Unchanged widths and panes without a width change nothing.
        let mut same = base.clone();
        assert_eq!(
            merge_width(&mut same, &base, "rooms", i32::from(base.room_list_width)),
            Err(Refusal::Nothing)
        );
        assert_eq!(
            merge_width(&mut same, &base, "conversation", 300),
            Err(Refusal::Nothing)
        );
        assert_eq!(
            merge_width(&mut same, &base, "members", 300),
            Err(Refusal::Nothing)
        );
        // Another layout field changing meanwhile is kept, not reverted.
        let mut other = Layout {
            sort_unread_first: true,
            ..base.clone()
        };
        assert_eq!(merge_width(&mut other, &base, "rooms", 300), Ok(()));
        assert!(other.sort_unread_first);
    }

    #[test]
    fn a_dragged_pane_lands_exactly_once_at_its_insertion_position() {
        for pane in PaneId::ALL {
            for index in 0..4 {
                let order = reordered(STANDARD, pane, index).unwrap();
                assert_eq!(order[usize::try_from(index).unwrap()], pane);
                for other in PaneId::ALL {
                    assert_eq!(
                        order
                            .iter()
                            .filter(|candidate| **candidate == other)
                            .count(),
                        1
                    );
                }
            }
            assert_eq!(reordered(STANDARD, pane, 4), None);
            assert_eq!(reordered(STANDARD, pane, -1), None);
        }
        // Dragging the conversation to the front keeps the others in order.
        assert_eq!(
            reordered(STANDARD, PaneId::Conversation, 0),
            Some([
                PaneId::Conversation,
                PaneId::Spaces,
                PaneId::Rooms,
                PaneId::Details
            ])
        );
    }

    #[test]
    fn reordering_keeps_hidden_collapsed_and_widths_and_refuses_stale_orders() {
        let base = Layout {
            hidden_panes: vec![PaneId::Spaces],
            collapsed_panes: vec![PaneId::Details],
            room_list_width: 300,
            ..Layout::default()
        };
        let mut current = base.clone();
        // A hidden pane can be moved too; it stays hidden, nothing duplicates.
        assert_eq!(merge_order(&mut current, &base, "spaces", 3), Ok(()));
        assert_eq!(
            current.pane_order,
            vec![
                PaneId::Rooms,
                PaneId::Conversation,
                PaneId::Details,
                PaneId::Spaces
            ]
        );
        assert_eq!(current.hidden_panes, base.hidden_panes);
        assert_eq!(current.collapsed_panes, base.collapsed_panes);
        assert_eq!(current.room_list_width, 300);
        Config {
            layout: current.clone(),
            ..Config::default()
        }
        .validate()
        .unwrap();
        // The order changed elsewhere during the drag (also via the side).
        let mut reordered_elsewhere = Layout {
            pane_order: vec![
                PaneId::Details,
                PaneId::Spaces,
                PaneId::Rooms,
                PaneId::Conversation,
            ],
            ..base.clone()
        };
        assert_eq!(
            merge_order(&mut reordered_elsewhere, &base, "rooms", 0),
            Err(Refusal::Conflict)
        );
        let mut side_changed = Layout {
            details_side: DetailsSide::Left,
            ..base.clone()
        };
        assert_eq!(
            merge_order(&mut side_changed, &base, "rooms", 0),
            Err(Refusal::Conflict)
        );
        // Dropping a pane where it already is saves nothing.
        let mut unchanged = base.clone();
        assert_eq!(
            merge_order(&mut unchanged, &base, "rooms", 1),
            Err(Refusal::Nothing)
        );
        assert!(unchanged.pane_order.is_empty());
    }
}
