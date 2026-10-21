//! Real generated Slint properties, keyboard routing, and account presentation.
//! The headless backend has no native display or screen reader. These tests do
//! not authenticate or remove a core account, and cannot access a credential vault.
//! The public ElementHandle accessibility-action query requires compile-time
//! SLINT_EMIT_DEBUG_INFO=1; the app-test CI step enables that metadata.

use super::*;
use crate::{
    AccountItem, CallParticipantItem, CallTileItem, MemberItem, MessageItem, MessageSearchItem,
    RoomItem, apply_config, color_from_hex, voice_spaces::VoiceSpaces,
};
use morpheus_config::{BuiltInStylePreset, Config, NavigationMode};
use slint::fontique_011::fontique;
use slint::platform::{Key, WindowEvent};
use slint::{Color, StyledText};
use std::time::Duration;

fn key(ui: &impl slint::ComponentHandle, text: impl Into<slint::SharedString>) {
    let text = text.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

fn type_text(ui: &AppWindow, text: &str) {
    for character in text.chars() {
        key(ui, character);
    }
}

async fn settle(ui: &AppWindow) {
    // AccountAccessUi and LiveUi post changes to Slint from worker tasks. A
    // FIFO event-loop barrier makes assertions observe the posted UI update.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    ui.as_weak()
        .upgrade_in_event_loop(move |_| {
            let _ = sender.send(());
        })
        .unwrap();
    receiver.await.unwrap();
    // Instantiate conditional dialogs and execute their actual init/focus and
    // change handlers; this does not render pixels or require elapsed wall time.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

/// Check the actual property against an independently queried installed face.
/// The testing backend keeps the real system-font collection; no rendering or
/// fixed-width glyph-metric assertion is implied by matching a family identity.
fn assert_installed_generic_family(actual: &str, generic: fontique::GenericFamily) {
    assert!(
        !["monospace", "serif", "sans-serif"]
            .iter()
            .any(|word| actual.eq_ignore_ascii_case(word)),
        "the generated UI property must contain a concrete family, got {actual:?}"
    );
    let mut collection = slint::fontique_011::shared_collection();
    let mut source_cache = fontique::SourceCache::default();
    let expected = {
        let mut query = collection.query(&mut source_cache);
        query.set_families([fontique::QueryFamily::Generic(generic)]);
        let mut identity = None;
        query.matches_with(|font| {
            identity = Some(font.family.0);
            fontique::QueryStatus::Stop
        });
        identity.expect("the supported desktop test host needs a usable generic font")
    };
    assert_eq!(
        collection.family_id(actual),
        Some(expected),
        "the actual UI font must be the host's usable {generic:?} family"
    );
}

fn assert_live_font_projections(ui: &AppWindow) {
    use fontique::GenericFamily::{Monospace, SansSerif, Serif};

    let mut config = Config::default();
    for (preset, interface, messages, metadata) in [
        (
            BuiltInStylePreset::Terminal,
            Monospace,
            Monospace,
            Monospace,
        ),
        (BuiltInStylePreset::Paper, SansSerif, Serif, Monospace),
    ] {
        config.apply_style_preset(preset);
        let theme = preset.theme().unwrap();
        // Exercise the complete production projection, including main's later
        // settings, so a raw UI-family overwrite cannot evade this regression.
        apply_config(ui, &config, Some(&theme));
        assert_installed_generic_family(ui.get_ui_font_family().as_str(), interface);
        assert_installed_generic_family(ui.get_message_font_family().as_str(), messages);
        assert_installed_generic_family(ui.get_monospace_font_family().as_str(), metadata);
    }

    // A named interface family and omitted message override must clear the
    // Paper/Terminal choices on the same generated window. Reset the rest of
    // the configuration before continuing the existing account-focus scenario.
    let restored = Config::default();
    apply_config(ui, &restored, None);
    assert_eq!(
        ui.get_ui_font_family().as_str(),
        restored.appearance.font_family
    );
    assert_eq!(
        ui.get_message_font_family().as_str(),
        restored.typography.message_family(&restored.appearance)
    );
    assert_installed_generic_family(ui.get_monospace_font_family().as_str(), Monospace);
}

async fn assert_navigation_projection_and_focus(ui: &AppWindow) {
    let default = Config::default();
    let mut sidebar = default.clone();
    sidebar.layout.navigation = NavigationMode::Sidebar;
    sidebar.layout.show_space_labels = true;
    sidebar.layout.group_direct_messages = false;
    let room = |id: &str, name: &str, unread| RoomItem {
        id: id.into(),
        name: name.into(),
        unread,
        ..Default::default()
    };
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(vec![room("!z", "Zebra", 0)])));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(vec![room("!a", "Aster", 1)])));
    ui.set_spaces(ModelRc::new(VecModel::from(vec![crate::SpaceItem {
        id: "!workshop:example.org".into(),
        name: "Workshop".into(),
        initials: "W".into(),
        ..Default::default()
    }])));
    apply_config(ui, &default, None);
    // Use the existing production focus return, rather than a test-only focus API.
    ui.set_show_sign_in(true);
    settle(ui).await;
    ui.set_show_sign_in(false);
    settle(ui).await;
    assert_eq!(
        crate::navigation::visible_rooms(ui)
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["!z", "!a"]
    );

    // From the initial root focus, Tab reaches the existing rail Home action.
    // A live mode switch must move that disappearing focus to the account
    // trigger; Return then opens the real account menu.
    key(ui, Key::Tab);
    settle(ui).await;
    apply_config(ui, &sidebar, None);
    settle(ui).await;
    assert_eq!(ui.get_navigation_mode(), 1);
    assert!(ui.get_show_space_labels());
    assert!(!ui.get_group_direct_messages());
    assert_eq!(
        crate::navigation::visible_rooms(ui)
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["!a", "!z"]
    );
    assert!(ui.get_combined_rooms().row_data(0).unwrap().direct);
    key(ui, Key::Return);
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);

    // The existing sign-in transition returns focus to shortcut-scope. The
    // sidebar begins with Create/join, quick switcher, four app routes, then
    // the first Space. This checks a dynamically repeated focus target too.
    ui.set_show_sign_in(true);
    settle(ui).await;
    ui.set_show_sign_in(false);
    settle(ui).await;
    for _ in 0..7 {
        key(ui, Key::Tab);
    }
    settle(ui).await;
    apply_config(ui, &default, None);
    settle(ui).await;
    key(ui, Key::Return);
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);
    assert_eq!(ui.get_navigation_mode(), 0);
    assert!(!ui.get_show_space_labels());
    assert!(ui.get_group_direct_messages());

    // An external config change must not move focus out of an active composer.
    ui.set_composer_text("".into());
    ui.invoke_focus_composer(false);
    settle(ui).await;
    type_text(ui, "first");
    apply_config(ui, &sidebar, None);
    settle(ui).await;
    type_text(ui, "second");
    assert_eq!(ui.get_composer_text(), "firstsecond");
    apply_config(ui, &default, None);
    settle(ui).await;
    type_text(ui, "third");
    assert_eq!(ui.get_composer_text(), "firstsecondthird");

    ui.set_composer_text("".into());
    ui.set_spaces(ModelRc::new(VecModel::<crate::SpaceItem>::default()));
    ui.set_unsorted_channels(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::<RoomItem>::default()));
    crate::apply_room_sort(ui);
}

async fn assert_header_menu_capture_and_detach(ui: &AppWindow) {
    use std::{cell::Cell, rc::Rc};

    let previous = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_active_account_local_id(),
        ui.get_active_room_id(),
        ui.get_active_room_name(),
    );
    let captures = Rc::new(Cell::new(0usize));
    let count = captures.clone();
    let global = ui.global::<crate::RoomOrganizationState>();
    global.on_capture_header_menu(move |room| {
        assert_eq!(room.as_str(), "!menu-fixture:example.org");
        count.set(count.get() + 1);
        count.get().to_string().into()
    });
    let actions = Rc::new(Cell::new(0usize));
    let count = actions.clone();
    global.on_organize_header(move |_, _, _| count.set(count.get() + 1));
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("menu-fixture-a".into());
    ui.set_active_room_id("!menu-fixture:example.org".into());
    ui.set_active_room_name("Menu fixture".into());
    settle(ui).await;
    let label = "Organize Menu fixture";
    click_control_once(ui, label);
    settle(ui).await;
    assert_eq!(
        captures.get(),
        1,
        "the physical click captures once before showing the menu"
    );
    assert_eq!(actions.get(), 0, "opening a menu performs no tag operation");

    // A header survives account changes, even for a room both accounts joined.
    // The old popup closes; keyboard activation now captures a fresh menu.
    ui.set_active_account_local_id("menu-fixture-b".into());
    settle(ui).await;
    request_bridge_focus(ui, label);
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(captures.get(), 2);
    assert_eq!(
        actions.get(),
        0,
        "account changes must not activate an old menu choice"
    );
    ui.set_live_session(false);
    settle(ui).await;
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(
        captures.get(),
        2,
        "a detached header cannot open another menu"
    );
    assert_eq!(actions.get(), 0);

    global.on_capture_header_menu(|_| slint::SharedString::default());
    global.on_organize_header(|_, _, _| {});
    ui.set_active_account_local_id(previous.2);
    ui.set_active_room_id(previous.3);
    ui.set_active_room_name(previous.4);
    ui.set_live_session(previous.1);
    ui.set_page(previous.0);
    ui.invoke_focus_composer(false);
    settle(ui).await;
}

fn assert_passwords_empty(ui: &AppWindow) {
    assert!(ui.get_access_current_password().is_empty());
    assert!(ui.get_access_new_password().is_empty());
    assert!(ui.get_access_confirm_password().is_empty());
}

async fn assert_sign_in_keyboard(ui: &AppWindow) {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    // Observe the real UI callbacks without installing any authentication,
    // network, preview-controller or credential-vault behavior.
    let passwords = Rc::new(RefCell::new(Vec::<String>::new()));
    let browser = Rc::new(Cell::new(0));
    let previews = Rc::new(Cell::new(0));
    let calls = passwords.clone();
    ui.on_submit_login(move |server, user, secret| {
        assert_eq!(server.as_str(), "https://keyboard.example.org");
        assert_eq!(user.as_str(), "fixture");
        calls.borrow_mut().push(secret.to_string());
    });
    let calls = browser.clone();
    ui.on_submit_sso(move |server| {
        assert_eq!(server.as_str(), "https://keyboard.example.org");
        calls.set(calls.get() + 1);
    });
    let calls = previews.clone();
    ui.on_preview_demo(move || calls.set(calls.get() + 1));

    for scale in [100, 200] {
        ui.set_show_sign_in(false);
        settle(ui).await;
        ui.set_saved_accounts(ModelRc::new(VecModel::<SavedAccountItem>::default()));
        ui.set_font_scale_percent(scale);
        ui.set_login_loading(false);
        ui.set_login_homeserver("".into());
        ui.set_show_sign_in(true);
        settle(ui).await;
        type_text(ui, "https://keyboard.example.org");
        key(ui, Key::Tab);
        if scale == 100 {
            type_text(ui, "fixture");
        }
        key(ui, Key::Tab);
        type_text(ui, "input fixture");
        key(ui, Key::Tab); // password -> Sign in button
        let before = passwords.borrow().len();
        key(ui, Key::Return);
        assert_eq!(passwords.borrow().len(), before + 1);
        assert_eq!(passwords.borrow().last().unwrap(), "input fixture");
        assert!(ui.get_login_loading());
        key(ui, Key::Return); // a disabled action cannot submit twice
        assert_eq!(passwords.borrow().len(), before + 1);

        ui.set_login_loading(false);
        ui.set_show_sign_in(false);
        settle(ui).await;
        ui.set_show_sign_in(true);
        settle(ui).await;
        key(ui, Key::Tab);
        key(ui, Key::Tab); // return to password without typing again
        key(ui, Key::Return);
        assert_eq!(passwords.borrow().len(), before + 2);
        assert!(passwords.borrow().last().unwrap().is_empty());
        // The second callback proves the private password field was cleared;
        // no production password getter is exposed for testing.

        ui.set_login_loading(false);
        ui.set_show_sign_in(false);
        settle(ui).await;
        ui.set_show_sign_in(true);
        settle(ui).await;
        for _ in 0..4 {
            key(ui, Key::Tab); // homeserver -> username -> password -> submit -> browser
        }
        let before = browser.get();
        key(ui, Key::Space);
        assert_eq!(browser.get(), before + 1);
        assert!(ui.get_login_loading());

        ui.set_login_loading(false);
        ui.set_show_sign_in(false);
        settle(ui).await;
        ui.set_show_sign_in(true);
        settle(ui).await;
        key(ui, Key::Backtab); // first field wraps to the preview action
        let before = previews.get();
        key(ui, Key::Return);
        assert_eq!(previews.get(), before + 1);
        key(ui, Key::Tab); // preview wraps to the visible first field
        ui.set_login_homeserver("".into());
        type_text(ui, "wrap");
        assert_eq!(ui.get_login_homeserver().as_str(), "wrap");
        assert!(!ui.get_account_menu_open());
    }

    // When saved accounts are present the first visible control changes. The
    // wrap must reach that control, not the covered sidebar account trigger.
    ui.set_saved_accounts(ModelRc::new(VecModel::from(vec![SavedAccountItem {
        id: "sign-in-presentation".into(),
        handle: "@river:example.net".into(),
        ..Default::default()
    }])));
    settle(ui).await;
    key(ui, Key::Tab); // saved-account shortcut -> first sign-in field
    ui.set_login_homeserver("".into());
    type_text(ui, "saved-focus");
    assert_eq!(ui.get_login_homeserver().as_str(), "saved-focus");
    key(ui, Key::Backtab); // first field -> saved-account shortcut
    key(ui, Key::Backtab);
    let before = previews.get();
    key(ui, Key::Return);
    assert_eq!(previews.get(), before + 1);
    key(ui, Key::Tab);
    ui.set_login_loading(true);
    key(ui, Key::Backtab); // saved shortcut -> password while actions are disabled
    type_text(ui, "loading focus fixture");
    ui.set_login_homeserver("https://keyboard.example.org".into());
    ui.set_login_loading(false);
    let before = passwords.borrow().len();
    key(ui, Key::Return); // the normal field callback proves the focus target
    assert_eq!(passwords.borrow().len(), before + 1);
    assert_eq!(passwords.borrow().last().unwrap(), "loading focus fixture");
    assert!(ui.get_login_loading());
    key(ui, Key::Tab); // password wraps to the saved shortcut during login
    key(ui, Key::Return);
    assert!(ui.get_manage_accounts_open());
    assert!(!ui.get_account_menu_open());

    ui.set_manage_accounts_open(false);
    ui.set_saved_accounts(ModelRc::new(VecModel::<SavedAccountItem>::default()));
    ui.set_show_sign_in(false);
    ui.set_font_scale_percent(100);
    ui.set_login_loading(false);
    ui.set_login_homeserver("https://matrix.org".into());
    settle(ui).await;
}

// This observer uses only the already pinned Slint generated-code API. It
// neither forces focus nor activates an accessible action. The public testing
// element search needs debug-info code generation; this direct item-tree read
// keeps the normal build and production properties unchanged.
fn focused_control_has_label(ui: &impl slint::ComponentHandle, expected: &str) -> bool {
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let Some(focused) = window.focus_item.borrow().upgrade() else {
        return false;
    };
    let root = ItemRc::new_root(window.component());
    root.visit_descendants(|item| {
        if item
            .accessible_string_property(AccessibleStringProperty::Label)
            .as_deref()
            != Some(expected)
        {
            return ControlFlow::Continue(());
        }
        let contains_focus = item == &focused
            || item
                .visit_descendants(|child| {
                    if child == &focused {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .is_some();
        if contains_focus {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_some()
}

fn control_with_label(
    ui: &AppWindow,
    label: &str,
) -> slint::private_unstable_api::re_exports::ItemRc {
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut matches = Vec::new();
    root.visit_descendants(|item| {
        if item
            .accessible_string_property(AccessibleStringProperty::Label)
            .as_deref()
            == Some(label)
        {
            matches.push(item.clone());
        }
        ControlFlow::<()>::Continue(())
    });
    assert_eq!(matches.len(), 1, "the requested control must be unique");
    matches.pop().unwrap()
}

fn request_bridge_focus(ui: &AppWindow, label: &str) {
    use slint::private_unstable_api::re_exports::{FocusReason, FocusScope, WindowInner};

    // This is the exact pinned AccessKit SetFocus path, including its reason.
    // Calling the generated component's .focus() would use Programmatic and
    // miss the installed AT-SPI regression.
    let target = control_with_label(ui, label);
    let window = WindowInner::from_pub(ui.window());
    window.set_focus_item(&target, true, FocusReason::PointerClick);
    assert!(
        focused_control_has_label(ui, label),
        "accessible focus did not reach the requested control: {label}"
    );
    let focused = window.focus_item.borrow().upgrade().unwrap();
    let scope = focused
        .downcast::<FocusScope>()
        .expect("a live action scope");
    assert!(scope.as_pin_ref().has_focus());
    assert!(focused.is_visible());
    assert!(focused.geometry().size.width > 0.0 && focused.geometry().size.height > 0.0);
    assert_eq!(focused.geometry().size, target.geometry().size);
}

#[track_caller]
fn click_control_once(ui: &AppWindow, label: &str) {
    let target = control_with_label(ui, label);
    // map_to_window takes a point in the item's parent coordinates; it does
    // not add the item's own x/y. Pass the full geometry's center so an action
    // positioned at the bottom of a layout is actually the one we click.
    let center = target.map_to_window(target.geometry().center());
    let position = slint::LogicalPosition::new(center.x, center.y);
    let window_size = ui.window().size().to_logical(ui.window().scale_factor());
    assert!(position.x > 0.0 && position.x < window_size.width);
    assert!(position.y > 0.0 && position.y < window_size.height);
    assert!(
        !focused_control_has_label(ui, label),
        "click an unfocused action"
    );
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
}

async fn assert_accessible_action_focus_and_first_click(ui: &AppWindow) {
    use std::{cell::Cell, rc::Rc};

    let settings = Rc::new(Cell::new(0));
    let observed = settings.clone();
    ui.on_open_security(move || observed.set(observed.get() + 1));
    let profile = format!(
        "Account menu for {}, {}",
        ui.get_active_account_name(),
        ui.get_active_account_handle()
    );
    for scale in [100, 200] {
        ui.set_font_scale_percent(scale);
        ui.set_page(0);
        settle(ui).await;
        ui.invoke_focus_composer(false);
        settle(ui).await;
        assert!(!focused_control_has_label(ui, &profile));
        request_bridge_focus(ui, &profile);
        key(ui, Key::Return);
        assert!(
            ui.get_account_menu_open(),
            "accessible focus must support Return"
        );
        ui.set_account_menu_open(false);
        settle(ui).await;
        ui.invoke_focus_composer(false);
        settle(ui).await;
        click_control_once(ui, &profile);
        assert!(
            ui.get_account_menu_open(),
            "one click must open the account menu"
        );
        ui.set_account_menu_open(false);
        settle(ui).await;

        // IconAction uses the same focus reason and hit-testing contract.
        ui.invoke_focus_composer(false);
        settle(ui).await;
        request_bridge_focus(ui, "Open settings");
        let before = settings.get();
        key(ui, Key::Space);
        assert_eq!(settings.get(), before + 1);
        assert_eq!(ui.get_page(), 1);
        ui.set_page(0);
        settle(ui).await;
        ui.invoke_focus_composer(false);
        settle(ui).await;
        let before = settings.get();
        click_control_once(ui, "Open settings");
        assert_eq!(
            settings.get(),
            before + 1,
            "one click must activate exactly once"
        );
        assert_eq!(ui.get_page(), 1);
    }
    ui.on_open_security(|| {});
    ui.set_font_scale_percent(100);
    ui.set_page(0);
    settle(ui).await;
}

#[track_caller]
fn tab_to_control(ui: &AppWindow, label: &str) {
    use slint::private_unstable_api::re_exports::{FocusScope, TextInput, WindowInner};

    // Walk the real current tab order without activating intervening controls.
    // A missing FocusScope must fail instead of silently invoking the callback.
    let window = WindowInner::from_pub(ui.window());
    // One full cycle of the Settings page (about 180 stops with the metric,
    // sender color and shortcut editors) plus margin; every step is checked.
    for _ in 0..400 {
        if focused_control_has_label(ui, label) {
            return;
        }
        key(ui, Key::Tab);
        // A later Tab must not hide a focus loss by restarting at the root.
        // End this scope before another key can dispatch or rebuild the tree.
        {
            let focused = window.focus_item.borrow().upgrade().unwrap_or_else(|| {
                panic!("Tab navigation lost its focus target while finding {label}")
            });
            if let Some(scope) = focused.downcast::<FocusScope>() {
                assert!(
                    scope.as_pin_ref().has_focus(),
                    "Tab did not focus a live control"
                );
            }
            if let Some(input) = focused.downcast::<TextInput>() {
                assert!(
                    input.as_pin_ref().has_focus(),
                    "Tab did not focus a live input"
                );
            }
        }
    }
    panic!("Appearance control is not keyboard reachable: {label}");
}

fn settings_hover_opacity(ui: &AppWindow) -> f32 {
    use slint::private_unstable_api::re_exports::Opacity;
    use std::ops::ControlFlow;

    let control = control_with_label(ui, "Open settings");
    let mut levels = Vec::new();
    control.visit_descendants(|item| {
        if let Some(opacity) = item.downcast::<Opacity>() {
            levels.push(opacity.as_pin_ref().opacity());
        }
        ControlFlow::<()>::Continue(())
    });
    // The decorative layer is the only changing opacity in this non-quiet
    // action. Read real generated items, not an assumed animation progress.
    assert!(!levels.is_empty(), "the hover surface must exist");
    assert!(levels.iter().all(|level| (0.0..=1.0).contains(level)));
    assert!(levels.iter().filter(|level| **level != 1.0).count() <= 1);
    levels.into_iter().product()
}

async fn assert_native_motion_and_preference(ui: &AppWindow, config_path: &std::path::Path) {
    use morpheus_config::ConfigStore;

    let defaults = Config::default();
    apply_config(ui, &defaults, None);
    ui.set_page(0);
    settle(ui).await;
    request_bridge_focus(ui, "Open settings");
    ui.window().dispatch_event(WindowEvent::PointerExited);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(settings_hover_opacity(ui), 0.0);

    let control = control_with_label(ui, "Open settings");
    let bounds = control.geometry();
    let center = control.map_to_window(bounds.center());
    let position = slint::LogicalPosition::new(center.x, center.y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    assert_eq!(settings_hover_opacity(ui), 0.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
    let during = settings_hover_opacity(ui);
    assert!(
        during > 0.0 && during < 1.0,
        "native hover must interpolate"
    );
    assert!(focused_control_has_label(ui, "Open settings"));
    assert_eq!(control_with_label(ui, "Open settings").geometry(), bounds);

    let mut reduced = defaults.clone();
    reduced.appearance.reduce_motion = true;
    apply_config(ui, &reduced, None);
    assert_eq!(
        ui.global::<crate::StyleTokens>().get_interaction_duration(),
        0
    );
    assert_eq!(
        settings_hover_opacity(ui),
        1.0,
        "live preference interrupts visible motion"
    );
    assert_eq!(control_with_label(ui, "Open settings").geometry(), bounds);
    assert!(focused_control_has_label(ui, "Open settings"));
    ui.window().dispatch_event(WindowEvent::PointerExited);
    assert_eq!(
        settings_hover_opacity(ui),
        0.0,
        "reduced feedback has no delay"
    );

    apply_config(ui, &defaults, None);
    assert_eq!(
        ui.global::<crate::StyleTokens>().get_interaction_duration(),
        90
    );
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(settings_hover_opacity(ui), 0.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    assert_eq!(settings_hover_opacity(ui), 0.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(settings_hover_opacity(ui), 1.0);
    assert!(
        !ui.window().has_active_animations(),
        "settled feedback must become idle"
    );
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
    assert_eq!(settings_hover_opacity(ui), 1.0);
    assert!(!ui.window().has_active_animations());
    ui.window().dispatch_event(WindowEvent::PointerExited);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(settings_hover_opacity(ui), 0.0);

    let store = ConfigStore::at(config_path);
    crate::appearance::install_callbacks(ui, &store);
    for scale in [100, 200] {
        let mut config = defaults.clone();
        config.appearance.scale_percent = scale;
        store.save(&config).unwrap();
        apply_config(ui, &config, None);
        ui.set_page(1);
        settle(ui).await;
        tab_to_control(ui, "Reduce motion, off");
        let _ = crate::appearance::take_motion_probe();
        let focus_before = focused_control_has_label(ui, "Reduce motion, off");
        let bounds_before = focused_control_bounds(ui);
        key(ui, Key::Space);
        let persisted = store.load().unwrap().appearance.reduce_motion;
        if !persisted || bounds_before.is_none_or(|bounds| !inside_window(ui, bounds)) {
            let (calls, requested, saved) = crate::appearance::take_motion_probe();
            let numeric = |bounds: Option<Bounds>| {
                bounds.map(|bounds| [bounds.left, bounds.top, bounds.right, bounds.bottom])
            };
            eprintln!(
                "\nMORPHEUS_MOTION_FAILURE {}",
                serde_json::json!({
                    "scale": scale, "calls": calls, "requested": requested, "saved": saved,
                    "focus_before": focus_before,
                    "focus_after": focused_control_has_label(ui, "Reduce motion, off"),
                    "bounds_before": numeric(bounds_before),
                    "bounds_after": numeric(focused_control_bounds(ui)),
                    "window": window_size(ui), "page": ui.get_page(),
                    "layout": motion_layout_geometry(ui),
                    "config_error_present": !ui.get_config_error().is_empty(),
                    "presentation": ui.global::<crate::StyleTokens>().get_reduced_motion(),
                })
            );
        }
        assert!(persisted);
        assert!(
            inside_window(
                ui,
                bounds_before.expect("focused motion control has bounds")
            ),
            "keyboard focus must reveal the whole motion control at {scale}%"
        );
        assert!(ui.global::<crate::StyleTokens>().get_reduced_motion());
        assert_eq!(
            ui.global::<crate::StyleTokens>().get_interaction_duration(),
            0
        );
        assert!(focused_control_has_label(ui, "Reduce motion, on"));
        key(ui, Key::Return);
        assert!(!store.load().unwrap().appearance.reduce_motion);
        assert!(!ui.global::<crate::StyleTokens>().get_reduced_motion());
        assert!(focused_control_has_label(ui, "Reduce motion, off"));
    }
    // Use default fonts returns the three families only; other preferences,
    // such as the message size, are kept.
    let mut fonts = defaults.clone();
    fonts.appearance.font_family = "serif".into();
    fonts.typography.message_font_family = Some("monospace".into());
    fonts.typography.monospace_font_family = "serif".into();
    fonts.typography.message_size = 17;
    store.save(&fonts).unwrap();
    apply_config(ui, &fonts, None);
    ui.invoke_reset_style_fonts();
    let reset = store.load().unwrap();
    assert_eq!(
        reset.appearance.font_family,
        defaults.appearance.font_family
    );
    assert_eq!(
        reset.typography.message_font_family,
        defaults.typography.message_font_family
    );
    assert_eq!(
        reset.typography.monospace_font_family,
        defaults.typography.monospace_font_family
    );
    assert_eq!(
        reset.typography.message_size, 17,
        "the reset kept other preferences"
    );
    assert_eq!(
        ui.get_appearance_ui_font_input().as_str(),
        defaults.appearance.font_family
    );
    store.save(&defaults).unwrap();
    apply_config(ui, &defaults, None);
    let invalid = "[appearance\nreduce_motion = true\n";
    std::fs::write(config_path, invalid).unwrap();
    key(ui, Key::Space);
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), invalid);
    assert!(!ui.global::<crate::StyleTokens>().get_reduced_motion());
    assert!(!ui.get_config_error().is_empty());
    store.save(&defaults).unwrap();
    apply_config(ui, &store.load().unwrap(), None);
    ui.set_config_error("".into());
    // Edit the same persisted preference through a real generated button,
    // then verify reduced motion takes priority without discarding it.
    tab_to_control(ui, "Increase Hover fade duration");
    key(ui, Key::Space);
    assert_eq!(store.load().unwrap().appearance.hover_duration_ms, 100);
    assert_eq!(
        ui.global::<crate::StyleTokens>().get_interaction_duration(),
        100
    );
    tab_to_control(ui, "Reduce motion, off");
    key(ui, Key::Space);
    assert_eq!(store.load().unwrap().appearance.hover_duration_ms, 100);
    assert_eq!(
        ui.global::<crate::StyleTokens>().get_interaction_duration(),
        0
    );
    key(ui, Key::Return);
    assert_eq!(
        ui.global::<crate::StyleTokens>().get_interaction_duration(),
        100
    );
    store.save(&defaults).unwrap();
    apply_config(ui, &defaults, None);
    ui.on_set_reduce_motion(|_| {});
    ui.set_page(0);
    settle(ui).await;
}

async fn assert_lazy_timeline_view(ui: &AppWindow) {
    use crate::timeline_model::{projection_count, tests::rich_fixture, update_shared};
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::{ops::ControlFlow, sync::Arc};

    let has_label = |label: &str| {
        let window = WindowInner::from_pub(ui.window());
        ItemRc::new_root(window.component())
            .visit_descendants(|item| {
                if item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .as_deref()
                    == Some(label)
                {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .is_some()
    };
    ui.set_page(0);
    ui.set_font_scale_percent(100);
    let mut rows: Vec<_> = (0..10_000).map(|i| Arc::new(rich_fixture(i))).collect();
    let original = rows[0].body.clone();
    let model = update_shared(ModelRc::default(), rows.clone());
    assert_eq!(projection_count(&model), 0);
    ui.set_messages(model.clone());
    settle(ui).await;
    let initial = projection_count(&model);
    assert!(
        initial > 0 && initial < 1_000,
        "the real ListView must project a bounded visible subset: {initial}"
    );
    assert!(has_label(&original));
    // Read actual laid-out text, not the fixture's string lengths: the first
    // two delegates must have different native heights at the same width.
    let short = control_bounds(ui, &original);
    let multiline = control_bounds(ui, &rows[1].body);
    assert_eq!(short.len(), 1, "one short body is materialized");
    assert_eq!(multiline.len(), 1, "one multiline body is materialized");
    assert!(inside_window(ui, short[0]));
    assert!(
        multiline[0].bottom - multiline[0].top > short[0].bottom - short[0].top + 0.5,
        "the rich timeline workload must exercise different native body heights: short={:?}, multiline={:?}",
        short[0],
        multiline[0]
    );

    let edited = Arc::make_mut(&mut rows[0]);
    edited.body = "Updated visible timeline fixture".into();
    edited.rich_markdown = Some(edited.body.clone());
    edited.rich_accessible = Some(edited.body.clone());
    let changed = update_shared(ui.get_messages(), rows.clone());
    assert_eq!(changed, model, "an edit must retain the live model");
    ui.set_messages(changed);
    settle(ui).await;
    assert!(has_label(&rows[0].body));
    assert!(!has_label(&original));
    let after_edit = projection_count(&model);
    assert!(
        after_edit - initial < 1_000,
        "editing one row must not format the entire history"
    );

    rows.push(Arc::new(rich_fixture(10_000)));
    let appended = update_shared(ui.get_messages(), rows);
    assert_eq!(appended, model);
    ui.set_messages(appended);
    settle(ui).await;
    assert_eq!(ui.get_messages().row_count(), 10_001);
    assert!(
        projection_count(&model) - after_edit < 1_000,
        "appending must not format the entire history"
    );
    // Exercise the real ListView's wheel path through mixed-height rows.
    // This is a bounded virtualization regression, not a frame-time benchmark.
    let before_scroll = projection_count(&model);
    let position = slint::LogicalPosition::new(
        (short[0].left + short[0].right) / 2.0,
        (short[0].top + short[0].bottom) / 2.0,
    );
    for _ in 0..10 {
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position,
            delta_x: 0.0,
            delta_y: -600.0,
        });
        settle(ui).await;
    }
    assert!(
        !has_label("Updated visible timeline fixture"),
        "wheel scrolling must move past the first delegate"
    );
    assert!(
        projection_count(&model) - before_scroll < 1_000,
        "a bounded wheel burst must not format all 10,000 messages"
    );
    assert_eq!(ui.get_messages(), model, "scrolling keeps the shared model");
    ui.set_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    settle(ui).await;
    assert_eq!(ui.get_messages().row_count(), 0);
    assert!(!has_label("Updated visible timeline fixture"));

    // Same length and newest row after coalescing: redacting B and decrypting
    // an earlier X must still request placement against the final model. Use
    // the actual live-publication helper and generated unread-scope callback;
    // this fixture does not fabricate an SDK marker or a placement result.
    let observed = std::rc::Rc::new(std::cell::RefCell::new(None));
    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let (last, count, weak) = (observed.clone(), calls.clone(), ui.as_weak());
    ui.global::<crate::UnreadNavigation>()
        .on_context_changed(move || {
            let ui = weak.upgrade().unwrap();
            let rows = ui.get_messages();
            *last.borrow_mut() = Some((
                rows.row_count(),
                rows.row_data(1).map(|row| row.event_id),
                rows.row_data(rows.row_count().saturating_sub(1))
                    .map(|row| row.event_id),
                ui.get_conversation_focused(),
            ));
            count.set(count.get() + 1);
        });
    let before: Vec<_> = [1, 2, 3, 4]
        .into_iter()
        .map(|i| Arc::new(rich_fixture(i)))
        .collect();
    let after: Vec<_> = [1, 5, 3, 4]
        .into_iter()
        .map(|i| Arc::new(rich_fixture(i)))
        .collect();
    ui.set_conversation_focused(true);
    crate::live::publish_live_messages(ui, before.clone());
    settle(ui).await;
    let initial_refreshes = calls.get();
    assert!(initial_refreshes > 0);
    assert_eq!(
        observed
            .borrow()
            .as_ref()
            .unwrap()
            .1
            .as_ref()
            .unwrap()
            .as_str(),
        "$fixture-2:example.org"
    );
    let delivery = crate::timeline_delivery::Latest::new();
    let mut without_b = before.clone();
    without_b.remove(1);
    assert!(delivery.submit(without_b));
    assert!(
        !delivery.submit(after),
        "only the final snapshot is delivered"
    );
    crate::live::publish_live_messages(ui, delivery.take().unwrap());
    settle(ui).await;
    assert!(
        calls.get() > initial_refreshes,
        "same count/newest must refresh placement"
    );
    assert_eq!(ui.get_messages().row_count(), before.len());
    {
        let final_rows = observed.borrow();
        let final_rows = final_rows.as_ref().unwrap();
        assert_eq!(final_rows.0, before.len());
        assert_eq!(
            final_rows.1.as_ref().unwrap().as_str(),
            "$fixture-5:example.org"
        );
        assert_eq!(
            final_rows.2.as_ref().unwrap().as_str(),
            "$fixture-4:example.org"
        );
        assert!(!final_rows.3, "refresh observes the final live-view state");
    }
    let final_refreshes = calls.get();
    ui.global::<crate::UnreadNavigation>()
        .set_first_unread_event_id("$fixture-5:example.org".into());
    settle(ui).await;
    assert_eq!(
        calls.get(),
        final_refreshes,
        "divider publication cannot refresh itself"
    );
    ui.global::<crate::UnreadNavigation>()
        .on_context_changed(|| {});
    ui.global::<crate::UnreadNavigation>()
        .set_first_unread_event_id("".into());
    ui.set_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    settle(ui).await;
}

async fn assert_palette_and_density_keyboard(ui: &AppWindow) {
    use std::{cell::RefCell, rc::Rc};

    let themes = Rc::new(RefCell::new(Vec::new()));
    let observed = themes.clone();
    let weak = ui.as_weak();
    ui.on_choose_theme(move |index| {
        observed.borrow_mut().push(index);
        weak.upgrade().unwrap().set_theme_mode(index);
    });
    let densities = Rc::new(RefCell::new(Vec::new()));
    let observed = densities.clone();
    let weak = ui.as_weak();
    ui.on_choose_density(move |index| {
        observed.borrow_mut().push(index);
        weak.upgrade().unwrap().set_density_mode(index);
    });

    for scale in [100, 200] {
        ui.set_page(1);
        ui.set_font_scale_percent(scale);
        // Existing production focus return resets the route to shortcut-scope.
        ui.set_show_sign_in(true);
        settle(ui).await;
        ui.set_show_sign_in(false);
        settle(ui).await;
        for (index, label, activation) in [
            (0, "Use Dark theme", Key::Return),
            (1, "Use Light theme", Key::Space),
            (2, "Use High contrast theme", Key::Return),
            (3, "Use System theme", Key::Space),
        ] {
            tab_to_control(ui, label);
            let previous = themes.borrow().len();
            key(ui, activation);
            assert_eq!(themes.borrow().len(), previous + 1);
            assert_eq!(themes.borrow().last(), Some(&index));
            assert_eq!(ui.get_theme_mode(), index);
            assert!(focused_control_has_label(ui, label));
        }
        for (index, label, activation) in [
            (0, "Use Compact room and message density", Key::Space),
            (1, "Use Comfortable room and message density", Key::Return),
            (2, "Use Spacious room and message density", Key::Space),
        ] {
            tab_to_control(ui, label);
            let previous = densities.borrow().len();
            key(ui, activation);
            assert_eq!(densities.borrow().len(), previous + 1);
            assert_eq!(densities.borrow().last(), Some(&index));
            assert_eq!(ui.get_density_mode(), index);
            assert!(focused_control_has_label(ui, label));
        }
    }

    assert_nickname_controls_keyboard(ui).await;

    // These choices are always enabled in the product. Existing sign-in tests
    // cover the shared ActionRect's disabled-retained-focus activation guard.
    // Keep later account scenarios on the same window with their prior state.
    ui.on_choose_theme(|_| {});
    ui.on_choose_density(|_| {});
    apply_config(ui, &Config::default(), None);
    ui.set_page(0);
    settle(ui).await;
}

/// Nickname brackets and alignment reach the generated window from the
/// configuration and stay keyboard-editable through the real controls; the
/// accessible names of the controls carry their state, never a message.
async fn assert_nickname_controls_keyboard(ui: &AppWindow) {
    use morpheus_config::NicknameAlignment;
    use std::{cell::RefCell, rc::Rc};

    let flags = Rc::new(RefCell::new(Vec::new()));
    let observed = flags.clone();
    let weak = ui.as_weak();
    ui.on_set_style_flag(move |field, value| {
        observed.borrow_mut().push((field, value));
        if field == 4 {
            weak.upgrade().unwrap().set_nickname_brackets(value);
        }
    });
    let alignments = Rc::new(RefCell::new(Vec::new()));
    let observed = alignments.clone();
    let weak = ui.as_weak();
    ui.on_choose_nickname_alignment(move |index| {
        observed.borrow_mut().push(index);
        weak.upgrade().unwrap().set_nickname_alignment(index);
    });

    // Defaults keep the previous presentation: plain names, right-aligned.
    apply_config(ui, &Config::default(), None);
    assert!(!ui.get_nickname_brackets());
    assert_eq!(ui.get_nickname_alignment(), 1);
    let mut config = Config::default();
    config.chat.nickname_brackets = true;
    config.chat.nickname_alignment = NicknameAlignment::Left;
    apply_config(ui, &config, None);
    assert!(ui.get_nickname_brackets());
    assert_eq!(ui.get_nickname_alignment(), 0);
    config.apply_style_preset(BuiltInStylePreset::Terminal);
    apply_config(ui, &config, None);
    assert!(
        ui.get_nickname_brackets(),
        "the Terminal preset brackets nicknames"
    );
    assert_eq!(ui.get_nickname_alignment(), 1);
    apply_config(ui, &Config::default(), None);

    ui.set_page(1);
    settle(ui).await;
    tab_to_control(ui, "Nickname angle brackets, off");
    key(ui, Key::Space);
    assert_eq!(flags.borrow().as_slice(), &[(4, true)]);
    assert!(ui.get_nickname_brackets());
    assert!(focused_control_has_label(ui, "Nickname angle brackets, on"));
    key(ui, Key::Return);
    assert_eq!(flags.borrow().as_slice(), &[(4, true), (4, false)]);
    assert!(focused_control_has_label(
        ui,
        "Nickname angle brackets, off"
    ));

    tab_to_control(ui, "Nickname alignment: Left");
    key(ui, Key::Return);
    assert_eq!(alignments.borrow().as_slice(), &[0]);
    assert_eq!(ui.get_nickname_alignment(), 0);
    assert!(focused_control_has_label(
        ui,
        "Nickname alignment: Left, selected"
    ));
    tab_to_control(ui, "Nickname alignment: Right");
    key(ui, Key::Space);
    assert_eq!(alignments.borrow().as_slice(), &[0, 1]);
    assert_eq!(ui.get_nickname_alignment(), 1);
    assert!(focused_control_has_label(
        ui,
        "Nickname alignment: Right, selected"
    ));

    ui.on_set_style_flag(|_, _| {});
    ui.on_choose_nickname_alignment(|_| {});
    ui.set_page(0);
    settle(ui).await;
}

/// A scrolled Settings action needs a nonzero focus rectangle matching the
/// complete value row, so Slint reveals the setting as well as the button.
/// This checks generated geometry; installed accessibility remains a separate
/// check against the actual packaged application.
async fn assert_scale_keyboard_and_focus_bounds(ui: &AppWindow) {
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::{cell::RefCell, ops::ControlFlow, rc::Rc};

    let requests = Rc::new(RefCell::new(Vec::new()));
    let observed = requests.clone();
    let weak = ui.as_weak();
    ui.on_change_font_scale(move |value| {
        observed.borrow_mut().push(value);
        weak.upgrade()
            .unwrap()
            .set_font_scale_percent(value.clamp(75, 200));
    });
    for scale in [100, 200] {
        apply_config(ui, &Config::default(), None);
        ui.set_page(1);
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        for (label, activation, delta) in [
            ("Increase font scale", Key::Space, 10),
            ("Decrease font scale", Key::Return, -10),
        ] {
            tab_to_control(ui, label);
            settle(ui).await;
            let window = WindowInner::from_pub(ui.window());
            let focused = window.focus_item.borrow().upgrade().unwrap();
            let root = ItemRc::new_root(window.component());
            let mut controls = Vec::new();
            root.visit_descendants(|item| {
                if item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .as_deref()
                    == Some(label)
                {
                    controls.push(item.clone());
                }
                ControlFlow::<()>::Continue(())
            });
            assert_eq!(controls.len(), 1, "unique scale action");
            assert!(focused_control_has_label(ui, label));
            assert!(focused.geometry().size.width > 0.0 && focused.geometry().size.height > 0.0);
            assert_eq!(
                focused.geometry().size.width,
                controls[0].geometry().size.width
            );
            assert!(focused.geometry().size.height > controls[0].geometry().size.height);
            assert!(focused.is_visible() && controls[0].is_visible());
            // The installed failure occurred after the button was reached:
            // its smaller focus band left this sibling value below the clip.
            let value_label = format!("{}% · text throughout the app", ui.get_font_scale_percent());
            let values = control_bounds(ui, &value_label);
            assert_eq!(values.len(), 1, "unique scale value");
            let reveal = item_bounds(&focused);
            assert!(
                inside_window(ui, reveal),
                "scale row must be revealed: {reveal:?}"
            );
            assert!(
                values[0].top >= reveal.top - 0.5 && values[0].bottom <= reveal.bottom + 0.5,
                "scale value must fit inside the revealed row (initial scale {scale}, {label}): {:?} vs {reveal:?}",
                values[0]
            );
            let expected = ui.get_font_scale_percent() + delta;
            let before = requests.borrow().len();
            key(ui, activation);
            assert_eq!(requests.borrow().len(), before + 1);
            assert_eq!(requests.borrow().last(), Some(&expected));
            assert_eq!(ui.get_font_scale_percent(), expected.clamp(75, 200));
        }
    }
    ui.on_change_font_scale(|_| {});
    apply_config(ui, &Config::default(), None);
    ui.set_page(0);
    settle(ui).await;
}

// Colors of every native text item currently showing `text`, read from the
// generated item tree so the assertions trace the real color bindings rather
// than a helper called with assumed inputs. Items inside `for` and `if` are
// instantiated by settle(); a missing text yields an empty list.
fn text_colors_of(ui: &AppWindow, text: &str) -> Vec<Color> {
    use slint::private_unstable_api::re_exports::{
        ComplexText, ItemRc, SimpleText, StyledTextItem, WindowInner,
    };
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut colors = Vec::new();
    root.visit_descendants(|item| {
        if let Some(simple) = item.downcast::<SimpleText>() {
            let simple = simple.as_pin_ref();
            if simple.text().as_str() == text {
                colors.push(simple.color().color());
            }
        } else if let Some(complex) = item.downcast::<ComplexText>() {
            let complex = complex.as_pin_ref();
            if complex.text().as_str() == text {
                colors.push(complex.color().color());
            }
        } else if let Some(styled) = item.downcast::<StyledTextItem>() {
            let styled = styled.as_pin_ref();
            if styled.text() == StyledText::from_plain_text(text) {
                colors.push(styled.default_color().color());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    colors
}

fn assert_text_color(ui: &AppWindow, text: &str, expected: Color, minimum_items: usize) {
    let colors = text_colors_of(ui, text);
    assert!(
        colors.len() >= minimum_items,
        "expected at least {minimum_items} text item(s) showing {text:?}, found {}",
        colors.len()
    );
    for color in colors {
        assert_eq!(color, expected, "color of {text:?}");
    }
}

fn role_message(
    event_id: &str,
    own: bool,
    sender: &str,
    palette: i32,
    time: &str,
    accent: Color,
) -> MessageItem {
    MessageItem {
        event_id: event_id.into(),
        is_own: own,
        sender: sender.into(),
        sender_palette_index: palette,
        time: time.into(),
        body: format!("{sender} body").into(),
        rich_body: StyledText::from_plain_text(&format!("{sender} body")),
        rich_accessible_body: format!("{sender} body").into(),
        accent,
        ..Default::default()
    }
}

/// The message roles reach the real timeline, compact header, thread root and
/// thread reply bindings with the documented precedence, omitted roles fall
/// back to the legacy colors, and the opened thread root carries the actual
/// message ownership, palette index and accent until the thread is closed.
async fn assert_message_role_precedence_and_thread_metadata(ui: &AppWindow) {
    use morpheus_config::ThemeRoles;

    let own_accent = Color::from_rgb_u8(0x11, 0x22, 0x33);
    let other_accent = Color::from_rgb_u8(0x44, 0x55, 0x66);
    let mut full = BuiltInStylePreset::Nocturne.theme().unwrap();
    // Use distinct synthetic colors: shipped timestamps can equal muted and
    // own-message ink can equal selection ink, masking a broken precedence
    // or omission binding. Shipped contrast is tested separately.
    full.roles.timestamp = Some("#123456".into());
    full.roles.sender = Some("#234567".into());
    full.roles.message = Some("#345678".into());
    full.roles.own_message = Some("#456789".into());
    full.roles.own_surface = Some("#56789A".into());
    full.roles.selection = Some("#6789AB".into());
    full.roles.selection_ink = Some("#789ABC".into());
    let role = |value: &Option<String>| color_from_hex(value.as_deref().unwrap());
    let timestamp = role(&full.roles.timestamp);
    let sender = role(&full.roles.sender);
    let own_message = role(&full.roles.own_message);
    let own_surface = role(&full.roles.own_surface);
    let selection = role(&full.roles.selection);
    let selection_ink = role(&full.roles.selection_ink);
    let ink = color_from_hex(&full.colors.ink);
    let muted = color_from_hex(&full.colors.muted);
    let elevated = color_from_hex(&full.colors.elevated);
    let hover = color_from_hex(&full.colors.hover);
    let transparent = Color::default();

    let mut config = Config::default();
    config.apply_style_preset(BuiltInStylePreset::Nocturne);
    assert!(!config.chat.color_senders);
    ui.set_page(0);
    ui.set_room_search_selected_event_id("$selected".into());
    ui.set_messages(ModelRc::new(VecModel::from(vec![
        role_message("$own", true, "Own Author", 1, "10:15 AM", own_accent),
        role_message("$other", false, "Other Author", 2, "Sending…", other_accent),
        role_message(
            "$selected",
            false,
            "Selected Author",
            3,
            "10:17 AM",
            other_accent,
        ),
    ])));
    apply_config(ui, &config, Some(&full));
    settle(ui).await;

    // Plain roles: own messages use own_message, others the sender role, the
    // selected row its selection ink, and clocks (including delivery status)
    // the timestamp role.
    assert_text_color(ui, "Own Author", own_message, 1);
    assert_text_color(ui, "Other Author", sender, 1);
    assert_text_color(ui, "Selected Author", selection_ink, 1);
    assert_text_color(ui, "10:15 AM", timestamp, 1);
    assert_text_color(ui, "Sending…", timestamp, 1);
    assert_text_color(ui, "Own Author body", own_message, 1);
    assert_text_color(ui, "Other Author body", role(&full.roles.message), 1);
    assert_text_color(ui, "Selected Author body", selection_ink, 1);
    assert_eq!(ui.invoke_body_ink(false, true), own_message);
    assert_eq!(ui.invoke_body_ink(false, false), role(&full.roles.message));
    assert_eq!(ui.invoke_body_ink(true, true), selection_ink);
    assert_eq!(
        ui.invoke_row_surface(false, false, true, transparent),
        own_surface
    );
    assert_eq!(
        ui.invoke_row_surface(false, true, true, transparent),
        elevated
    );
    assert_eq!(
        ui.invoke_row_surface(true, false, true, transparent),
        selection
    );
    assert_eq!(
        ui.invoke_row_surface(false, false, false, transparent),
        transparent
    );
    assert_eq!(
        ui.invoke_row_surface(false, false, false, elevated),
        elevated
    );

    // The colored sender palette wins over own_message and sender, but never
    // over the selection ink; clocks are unaffected.
    config.chat.color_senders = true;
    apply_config(ui, &config, Some(&full));
    settle(ui).await;
    let palette = ui.get_sender_palette();
    let palette_len = palette.row_count();
    assert!(palette_len > 0);
    assert_text_color(
        ui,
        "Own Author",
        palette.row_data(1 % palette_len).unwrap(),
        1,
    );
    assert_text_color(
        ui,
        "Other Author",
        palette.row_data(2 % palette_len).unwrap(),
        1,
    );
    assert_text_color(ui, "Selected Author", selection_ink, 1);
    assert_text_color(ui, "10:15 AM", timestamp, 1);
    assert_eq!(ui.invoke_body_ink(false, true), own_message);
    assert_text_color(ui, "Own Author body", own_message, 1);

    // Without a palette the colored-name setting falls back to the message
    // accent, still ahead of own_message and sender.
    let mut without_palette = full.clone();
    without_palette.senders.colors = None;
    apply_config(ui, &config, Some(&without_palette));
    settle(ui).await;
    assert_eq!(ui.get_sender_palette().row_count(), 0);
    assert_text_color(ui, "Own Author", own_accent, 1);
    assert_text_color(ui, "Other Author", other_accent, 1);
    assert_text_color(ui, "Selected Author", selection_ink, 1);

    // Replacing the theme with one that omits every role clears them: text
    // returns to ink and muted, own rows to transparent, selection to hover.
    let mut bare = full.clone();
    bare.roles = ThemeRoles::default();
    config.chat.color_senders = false;
    apply_config(ui, &config, Some(&bare));
    settle(ui).await;
    assert_text_color(ui, "Own Author", ink, 1);
    assert_text_color(ui, "Other Author", ink, 1);
    assert_text_color(ui, "Selected Author", ink, 1);
    assert_text_color(ui, "10:15 AM", muted, 1);
    assert_text_color(ui, "Sending…", muted, 1);
    assert_text_color(ui, "Own Author body", ink, 1);
    assert_text_color(ui, "Other Author body", ink, 1);
    assert_text_color(ui, "Selected Author body", ink, 1);
    assert_eq!(ui.invoke_body_ink(false, true), ink);
    assert_eq!(ui.invoke_body_ink(true, false), ink);
    assert_eq!(
        ui.invoke_row_surface(false, false, true, transparent),
        transparent
    );
    assert_eq!(ui.invoke_row_surface(true, false, true, transparent), hover);

    // Opening a thread from the real timeline action forwards the root
    // message's ownership, palette index and accent; the thread root heading
    // and replies then follow the same precedence as the timeline.
    apply_config(ui, &config, Some(&full));
    let replies = vec![
        role_message(
            "$reply-other",
            false,
            "Reply Author",
            2,
            "Failed to send",
            other_accent,
        ),
        role_message(
            "$reply-own",
            true,
            "Own Reply Author",
            1,
            "9:00 AM",
            own_accent,
        ),
    ];
    settle(ui).await;
    assert!(!ui.get_thread_root_is_own());
    tab_to_control(ui, "Open thread for Own Author");
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(ui.get_thread_root_id().as_str(), "$own");
    assert_eq!(ui.get_thread_root_sender().as_str(), "Own Author");
    assert!(ui.get_thread_root_is_own());
    assert_eq!(ui.get_thread_root_palette_index(), 1);
    assert_eq!(ui.get_thread_root_accent(), own_accent);
    assert_eq!(ui.get_side_panel_tab(), 1);
    assert!(ui.get_show_side_panel());
    // The real open action clears the old reply model before the asynchronous
    // response arrives. Populate this synthetic response only after that
    // boundary, rather than asserting against replies the action removed.
    assert_eq!(ui.get_thread_messages().row_count(), 0);
    ui.set_thread_messages(ModelRc::new(VecModel::from(replies)));
    settle(ui).await;
    // Timeline row and thread-root heading both show the own author's name.
    assert_text_color(ui, "Own Author", own_message, 2);
    assert_text_color(ui, "Own Author body", own_message, 2);
    assert_text_color(ui, "Reply Author", sender, 1);
    assert_text_color(ui, "Own Reply Author", own_message, 1);
    assert_text_color(ui, "Reply Author body", role(&full.roles.message), 1);
    assert_text_color(ui, "Own Reply Author body", own_message, 1);
    assert_text_color(ui, "· Failed to send", timestamp, 1);
    assert_text_color(ui, "· 9:00 AM", timestamp, 1);
    assert_eq!(
        ui.invoke_row_surface(false, false, ui.get_thread_root_is_own(), transparent),
        own_surface
    );
    assert_eq!(
        ui.invoke_body_ink(false, ui.get_thread_root_is_own()),
        own_message
    );
    assert_eq!(
        ui.invoke_row_surface(false, false, true, elevated),
        own_surface
    );
    assert_eq!(
        ui.invoke_row_surface(false, false, false, elevated),
        elevated
    );

    // Hidden optional timestamps keep the failed-delivery clock in replies.
    config.chat.show_timestamps = false;
    apply_config(ui, &config, Some(&full));
    settle(ui).await;
    assert!(text_colors_of(ui, "· 9:00 AM").is_empty());
    assert_text_color(ui, "· Failed to send", timestamp, 1);
    config.chat.show_timestamps = true;

    // The palette applies to the forwarded thread root as well.
    config.chat.color_senders = true;
    apply_config(ui, &config, Some(&full));
    settle(ui).await;
    assert_text_color(
        ui,
        "Own Author",
        palette.row_data(1 % palette_len).unwrap(),
        2,
    );
    assert_text_color(
        ui,
        "Own Reply Author",
        palette.row_data(1 % palette_len).unwrap(),
        1,
    );
    config.chat.color_senders = false;

    // Opening another author's thread replaces the metadata with that message.
    apply_config(ui, &config, Some(&full));
    settle(ui).await;
    tab_to_control(ui, "Open thread for Other Author");
    key(ui, Key::Space);
    settle(ui).await;
    assert_eq!(ui.get_thread_root_id().as_str(), "$other");
    assert!(!ui.get_thread_root_is_own());
    assert_eq!(ui.get_thread_root_palette_index(), 2);
    assert_eq!(ui.get_thread_root_accent(), other_accent);
    assert_text_color(ui, "Other Author", sender, 2);
    assert_text_color(ui, "Other Author body", role(&full.roles.message), 2);
    assert_eq!(
        ui.invoke_body_ink(false, ui.get_thread_root_is_own()),
        role(&full.roles.message)
    );
    assert_eq!(
        ui.invoke_row_surface(false, false, ui.get_thread_root_is_own(), transparent),
        transparent
    );

    // Closing the thread resets the forwarded metadata.
    ui.set_thread_root_is_own(true);
    tab_to_control(ui, "Close thread");
    key(ui, Key::Return);
    settle(ui).await;
    assert!(ui.get_thread_root_id().is_empty());
    assert!(!ui.get_thread_root_is_own());
    assert_eq!(ui.get_thread_root_palette_index(), 0);
    assert_eq!(ui.get_thread_root_accent(), transparent);

    // Narrow rows at 200% use the compact header: the name keeps its role and
    // the clock, including delivery status, is a separate timestamp-role text.
    // Use the actual supported minimum, including saved pane sizes. A
    // 400-pixel window would be below the production minimum and would not
    // establish behavior available to a desktop user.
    let rail = if ui.get_navigation_mode() == 0 {
        ui.get_rail_width_px()
    } else {
        0
    };
    let details = if ui.get_show_side_panel() {
        ui.get_details_width()
    } else {
        0
    };
    let narrow_width = (rail + ui.get_room_list_width() + details + 360).max(940);
    ui.window()
        .set_size(slint::PhysicalSize::new(narrow_width as u32, 760));
    apply_config(ui, &config, Some(&full));
    ui.set_font_scale_percent(200);
    settle(ui).await;
    assert_text_color(ui, "Own Author", own_message, 1);
    assert_text_color(ui, "· 10:15 AM", timestamp, 1);
    assert_text_color(ui, "· Sending…", timestamp, 1);
    assert_text_color(ui, "Selected Author", selection_ink, 1);
    assert_text_color(ui, "· 10:17 AM", selection_ink, 1);
    assert!(text_colors_of(ui, "10:15 AM").is_empty());
    apply_config(ui, &config, Some(&bare));
    settle(ui).await;
    assert_text_color(ui, "· 10:15 AM", muted, 1);
    assert_text_color(ui, "Own Author", ink, 1);

    // Restore the window for the remaining account scenarios.
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    ui.set_thread_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    ui.set_room_search_selected_event_id("".into());
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

/// The window palette follows the account that the accepted account-menu
/// snapshot publishes; the Settings picker shows the global choice with a
/// notice while that account overrides it. No activation is simulated: a
/// wrong password, cancel or stale switch never reaches the snapshot adapter.
fn assert_account_theme_follows_accepted_snapshot(ui: &AppWindow) {
    use morpheus_config::ThemeMode;
    let account = |id: &str, name: &str| morpheus_matrix::AccountInfo {
        id: id.into(),
        user_id: format!("@{name}:example.org"),
        device_id: "DEVICE".into(),
        homeserver: "https://example.org".into(),
    };
    let plain = account("plain-account", "plain");
    let themed = account("old-account", "casey");
    let mut config = Config::default();
    config.appearance.theme = ThemeMode::System;
    config
        .account_overrides
        .entry("old-account".into())
        .or_default()
        .theme = Some(ThemeMode::Light);
    // Sign-in screen, no active account: the global mode everywhere.
    ui.set_active_account_local_id("".into());
    apply_config(ui, &config, None);
    assert_eq!(ui.get_theme_mode(), 3);
    assert_eq!(ui.get_global_theme_mode(), 3);
    assert!(ui.get_account_theme_notice().is_empty());
    // An activation hook that runs before the accepted snapshot sees the
    // still-visible account and changes nothing.
    crate::appearance::apply_theme_mode(ui);
    assert_eq!(ui.get_theme_mode(), 3);
    // The winning snapshot publishes the themed account: its override wins,
    // while the picker keeps showing the global choice plus a notice.
    crate::live::apply_account_menu(ui, &[plain.clone(), themed.clone()], &themed);
    assert_eq!(ui.get_active_account_local_id().as_str(), "old-account");
    assert_eq!(ui.get_theme_mode(), 1);
    assert_eq!(ui.get_global_theme_mode(), 3);
    assert_eq!(
        ui.get_account_theme_notice().as_str(),
        "This account overrides it with Light in config.toml."
    );
    // A later snapshot for an account without an override returns to the
    // global palette, even though the earlier hook ran before it.
    crate::live::apply_account_menu(ui, &[plain.clone(), themed.clone()], &plain);
    assert_eq!(ui.get_active_account_local_id().as_str(), "plain-account");
    assert_eq!(ui.get_theme_mode(), 3);
    assert!(ui.get_account_theme_notice().is_empty());
    // Editing the global theme through Settings changes the picker and the
    // palette of accounts without an override; the override stays untouched.
    config.appearance.theme = ThemeMode::HighContrast;
    apply_config(ui, &config, None);
    assert_eq!(ui.get_global_theme_mode(), 2);
    assert_eq!(ui.get_theme_mode(), 2);
    crate::live::apply_account_menu(ui, &[plain, themed.clone()], &themed);
    assert_eq!(ui.get_theme_mode(), 1);
    assert_eq!(ui.get_global_theme_mode(), 2);
    assert_eq!(
        ui.get_account_theme_notice().as_str(),
        "This account overrides it with Light in config.toml."
    );
    // A snapshot that is not accepted (unknown active account) publishes
    // nothing, so the current palette stays.
    crate::live::apply_account_menu(ui, &[themed], &account("stale-account", "stale"));
    assert_eq!(ui.get_active_account_local_id().as_str(), "old-account");
    assert_eq!(ui.get_theme_mode(), 1);
}

fn assert_removed_account_menu_projection(ui: &AppWindow) {
    let account = |id: &str, name: &str| morpheus_matrix::AccountInfo {
        id: id.into(),
        user_id: format!("@{name}:example.org"),
        device_id: "PRESENTATION".into(),
        homeserver: "https://example.org".into(),
    };
    let removed = account("removed", "jules");
    let active = account("active", "casey");
    let remaining = account("remaining", "sam");
    crate::live::apply_account_menu(ui, &[removed, active.clone(), remaining.clone()], &active);
    assert_eq!(ui.get_active_account_index(), 1);
    assert_eq!(
        ui.get_account_options().row_data(0).unwrap().local_id,
        "active"
    );

    // Removing a row before the active one changes numeric indices. Preserve
    // each remaining option's identity and the selected alias, without a room
    // snapshot or a new sign-in being needed to refresh the menu.
    crate::live::apply_account_menu(ui, &[active.clone(), remaining], &active);
    assert_eq!(ui.get_active_account_index(), 0);
    assert_eq!(ui.get_active_account_local_id(), "active");
    assert_eq!(ui.get_active_account_handle(), "@casey:example.org");
    assert_eq!(ui.get_accounts().row_count(), 2);
    let options = ui.get_account_options().iter().collect::<Vec<_>>();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0].local_id, "active");
    assert_eq!(options[0].index, 0);
    assert_eq!(options[1].local_id, "remaining");
    assert_eq!(options[1].index, 1);
    assert!(
        ui.get_accounts()
            .iter()
            .all(|item| item.handle != "@jules:example.org")
    );
}

const ACCOUNT_UI_TOTAL_BUDGET: Duration = Duration::from_secs(180);
const ACCOUNT_UI_PHASE_BUDGET: Duration = Duration::from_secs(30);

// Bookkeeping for the existing single-window test. Use real monotonic time:
// Slint's mock clock only runs the UI handlers and must not drive deadlines.
struct AccountUiProgress {
    phase: std::cell::Cell<u8>,
    started: std::time::Instant,
    phase_started: std::cell::Cell<std::time::Instant>,
    observations: std::cell::Cell<u8>,
}

impl AccountUiProgress {
    fn new(started: std::time::Instant) -> Self {
        Self {
            phase: std::cell::Cell::new(0),
            started,
            phase_started: std::cell::Cell::new(started),
            observations: std::cell::Cell::new(0),
        }
    }

    fn get(&self) -> u8 {
        self.phase.get()
    }

    fn phase_expired_at(&self, now: std::time::Instant) -> bool {
        now.duration_since(self.phase_started.get()) >= ACCOUNT_UI_PHASE_BUDGET
    }

    fn expired_at(&self, now: std::time::Instant) -> bool {
        self.phase_expired_at(now) || now.duration_since(self.started) >= ACCOUNT_UI_TOTAL_BUDGET
    }

    fn observe(&self, now: std::time::Instant) {
        let observations = self.observations.get();
        if observations < 64 {
            // Closed numeric fields only, bounded in both count and value.
            // Deadline checks use uncapped durations, independently of logs.
            let cap = ACCOUNT_UI_TOTAL_BUDGET.as_millis();
            eprintln!(
                "MORPHEUS_UI_PHASE phase={} elapsed_ms={} phase_ms={}",
                self.get(),
                now.duration_since(self.started).as_millis().min(cap),
                now.duration_since(self.phase_started.get())
                    .as_millis()
                    .min(cap),
            );
            self.observations.set(observations + 1);
        }
    }

    fn set(&self, next: u8) {
        let now = std::time::Instant::now();
        self.observe(now);
        // A ready UI continuation may beat a timer poll. Check the completed
        // phase before replacing its clock, so progress cannot hide a stall.
        if self.expired_at(now) {
            eprintln!("morpheus-ui-timeout-phase={}", self.get());
            panic!("account UI scenario exceeded a finite phase or whole-suite deadline");
        }
        self.phase.set(next);
        self.phase_started.set(now);
    }

    async fn wait_for_stall(&self) {
        loop {
            let deadline = self.phase_started.get() + ACCOUNT_UI_PHASE_BUDGET;
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            // Progress only moves the deadline forward. An old timer waking
            // after progress must re-read the current phase before failing it.
            if self.phase_expired_at(std::time::Instant::now()) {
                return;
            }
        }
    }
}

#[test]
fn real_slint_account_dialogs_and_detach_clear_private_presentation() {
    // Slint's event-loop proxy is process-global. Keep all real-window account
    // scenarios in this single test; the remaining app tests do not create UI.
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let runtime_handle = runtime.handle().clone();
    slint::spawn_local(async move {
        let started = std::time::Instant::now();
        let phase = std::rc::Rc::new(AccountUiProgress::new(started));
        // The original fourteen-scenario budget now covers 47 transitions,
        // including the thread browser and room leave. Keep a finite three-minute
        // whole budget and a separate phase cap that catches one stall sooner.
        let outcome = tokio::select! {
            result = tokio::time::timeout(
                ACCOUNT_UI_TOTAL_BUDGET,
                exercise_account_ui(runtime_handle.clone(), phase.clone()),
            ) => result.map_err(|_| ()),
            () = phase.wait_for_stall() => Err(()),
        };
        let finished = std::time::Instant::now();
        phase.observe(finished);
        // Also check ready completion: cooperative timers cannot interrupt
        // synchronous work, or take precedence over every ready continuation.
        let outcome = if phase.expired_at(finished) {
            Err(())
        } else {
            outcome
        };
        eprintln!("morpheus-ui-scenario-ms={}", started.elapsed().as_millis());
        if outcome.is_err() {
            // A closed numeric stage only: never log UI text, files or accounts.
            eprintln!("morpheus-ui-timeout-phase={}", phase.get());
        }
        outcome.expect("account UI scenario must finish without network or vault access");
        // Opt-in live room/thread controller coverage shares this one event
        // loop; it returns at once unless a disposable local fixture is set.
        crate::live::timeline_controller_tests::run_if_configured(runtime_handle.clone()).await;
        // Opt-in too: the People busy path on the real window and fixture.
        crate::people_directory::live_tests::run_if_configured(runtime_handle).await;
        slint::quit_event_loop().unwrap();
    })
    .unwrap();
    slint::run_event_loop_until_quit().unwrap();
}

#[cfg(not(morpheus_timeline_acceptance))]
fn exercise_startup_recovery() {
    use morpheus_matrix::MatrixCoreError;
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::{cell::Cell, fs, ops::ControlFlow, rc::Rc};

    // This runs inside the existing, single Slint test event loop. It never
    // signs in, opens a credential vault or creates a second backend/proxy.
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("profile");
    let ids = [
        "10000000-0000-4000-8000-000000000001",
        "10000000-0000-4000-8000-000000000002",
    ];
    let aliases = ["@morgan:matrix-a.test", "@reese:matrix-b.test"];
    let servers = ["https://matrix-a.test", "https://matrix-b.test"];
    let accounts = ids
        .iter()
        .zip(aliases)
        .zip(servers)
        .map(|((id, user), server)| {
            serde_json::json!({
                "id": id, "user_id": user, "device_id": "SYNTHETIC",
                "homeserver": server
            })
        })
        .collect::<Vec<_>>();
    let registry = serde_json::to_vec(&serde_json::json!({
        "version": 1, "active_account_id": ids[0], "accounts": accounts
    }))
    .unwrap();
    for id in ids {
        let store = data.join("accounts").join(id);
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("preserved"), b"synthetic stored bytes").unwrap();
    }
    fs::write(data.join("accounts.json"), &registry).unwrap();
    for malformed in [false, true] {
        if malformed {
            fs::write(data.join("account-access.json"), b"{}").unwrap();
        }
        let error = match morpheus_matrix::AccountManager::open(&data) {
            Ok(_) => panic!("an existing profile without a valid policy must stay closed"),
            Err(error) => error,
        };
        assert!(matches!(error, MatrixCoreError::AppPasswordUnavailable));
        let ui = crate::StartupErrorWindow::new().unwrap();
        let detail = crate::startup_error::message(&error);
        ui.set_detail(detail.into());
        let closes = Rc::new(Cell::new(0));
        let callback = closes.clone();
        ui.on_exit_requested(move || callback.set(callback.get() + 1));
        ui.show().unwrap();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        let window = WindowInner::from_pub(ui.window());
        let root = ItemRc::new_root(window.component());
        let mut labels = Vec::new();
        let _ = root.visit_descendants(|item| {
            if let Some(label) = item.accessible_string_property(AccessibleStringProperty::Label) {
                labels.push(label.to_string());
            }
            ControlFlow::<()>::Continue(())
        });
        assert!(labels.iter().any(|label| label == detail));
        assert!(labels.iter().any(|label| label == "Close Morpheus"));
        for alias in aliases {
            assert!(labels.iter().all(|label| !label.contains(alias)));
        }
        for forbidden in [
            "Sign in to Matrix",
            "Add account",
            "Manage accounts",
            "Explore the interactive preview",
        ] {
            assert!(labels.iter().all(|label| label != forbidden));
        }
        for _ in 0..8 {
            if focused_control_has_label(&ui, "Close Morpheus") {
                break;
            }
            key(&ui, Key::Tab);
        }
        assert!(focused_control_has_label(&ui, "Close Morpheus"));
        key(&ui, Key::Return);
        assert_eq!(closes.get(), 1);
        ui.hide().unwrap();
        assert_eq!(fs::read(data.join("accounts.json")).unwrap(), registry);
        for id in ids {
            assert_eq!(
                fs::read(data.join("accounts").join(id).join("preserved")).unwrap(),
                b"synthetic stored bytes"
            );
        }
        if malformed {
            assert_eq!(fs::read(data.join("account-access.json")).unwrap(), b"{}");
        } else {
            assert!(!data.join("account-access.json").exists());
        }
    }
}

/// The Appearance workspace and Settings search on the generated window: hex
/// apply, partial-input hints, invalid-file retention, Reset and Undo,
/// window opacity rollback and persistence, single-paint translucency, every
/// search target reaching a real control, and bounded results at the smallest
/// window with 200% text.
async fn assert_appearance_workspace_and_search(ui: &AppWindow, directory: &std::path::Path) {
    use i_slint_backend_testing::ElementHandle;
    use morpheus_config::{ConfigStore, registry};

    let store = ConfigStore::at(directory.join("workspace.toml"));
    let config = store.load_or_create().unwrap();
    crate::appearance_editor::register(ui, &store);
    crate::settings_search::register(ui);
    apply_config(ui, &config, None);
    ui.set_page(1);
    settle(ui).await;
    for token in registry::COLOR_TOKENS {
        assert!(
            ui.invoke_token_known(token.id.into()),
            "{} has no color",
            token.id
        );
    }

    // A partial value only hints; a complete one applies and saves at once.
    ui.invoke_color_token_chosen("canvas".into());
    settle(ui).await;
    assert_eq!(ui.get_color_selected_id(), "canvas");
    ui.invoke_color_hex_edited("#10203".into());
    settle(ui).await;
    assert!(!ui.get_color_editor().hint.is_empty());
    assert!(
        store.load().unwrap().colors.is_empty(),
        "a partial value was saved"
    );
    ui.invoke_color_hex_edited("#10203040".into());
    settle(ui).await;
    let canvas = || store.load().unwrap().colors.get("canvas").cloned();
    assert_eq!(canvas().as_deref(), Some("#10203040"));
    let applied = Color::from_argb_u8(0x40, 0x10, 0x20, 0x30);
    assert_eq!(ui.invoke_token_color("canvas".into()), applied);
    assert!(ui.get_color_editor().overridden && ui.get_color_editor().can_undo);
    // A partial-alpha pane is painted once at 100%: the window clears.
    assert!(ui.get_window_background_cleared());

    // An invalid hand edit is never overwritten and the last valid color stays.
    let saved = std::fs::read_to_string(store.path()).unwrap();
    let broken = "[colors]\ncanvas = \"broken\"\n";
    std::fs::write(store.path(), broken).unwrap();
    ui.invoke_color_hex_edited("#AABBCC".into());
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), broken);
    assert_eq!(ui.invoke_token_color("canvas".into()), applied);
    assert!(!ui.get_config_error().is_empty());
    assert!(!ui.get_config_error().contains("broken"));
    std::fs::write(store.path(), &saved).unwrap();

    // Reset removes one override; Undo restores it.
    ui.invoke_color_reset();
    settle(ui).await;
    assert_eq!(canvas(), None);
    assert!(
        !ui.get_window_background_cleared(),
        "opaque panes keep the window background"
    );
    ui.invoke_color_undo();
    settle(ui).await;
    assert_eq!(canvas().as_deref(), Some("#10203040"));
    ui.invoke_color_reset();
    settle(ui).await;

    // This test runs no watcher, so a valid edit written straight to the file
    // stays undelivered until the editor's own next save. The undo step takes
    // its prior value from the re-read file, never from the applied view, and
    // the save keeps the other edit.
    let colors = || store.load().unwrap().colors;
    let color = |id: &str| colors().get(id).cloned();
    ui.invoke_color_hex_edited("#112233".into());
    settle(ui).await;
    assert_eq!(canvas().as_deref(), Some("#112233"));
    let mut external = store.load().unwrap();
    external.colors.insert("canvas".into(), "#445566".into());
    external.colors.insert("link".into(), "#778899".into());
    store.save(&external).unwrap();
    ui.invoke_color_hex_edited("#ABCDEF".into());
    settle(ui).await;
    assert_eq!(canvas().as_deref(), Some("#ABCDEF"));
    assert_eq!(
        color("link").as_deref(),
        Some("#778899"),
        "an undelivered edit was overwritten"
    );
    ui.invoke_color_undo();
    settle(ui).await;
    assert_eq!(
        canvas().as_deref(),
        Some("#445566"),
        "undo restored the stale applied value"
    );
    assert_eq!(color("link").as_deref(), Some("#778899"));
    // Undo never overwrites a later edit made elsewhere: the step is dropped
    // with a hint and the file is left as it is.
    ui.invoke_color_hex_edited("#123456".into());
    settle(ui).await;
    assert!(ui.get_color_editor().can_undo);
    let mut external = store.load().unwrap();
    external.colors.insert("canvas".into(), "#654321".into());
    store.save(&external).unwrap();
    let superseded = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_color_undo();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), superseded);
    assert!(!ui.get_color_editor().can_undo);
    assert!(!ui.get_color_editor().hint.is_empty());

    // A queued selection outside the current group/filter must reveal that
    // exact color, not silently edit the first color in the old group.
    ui.invoke_color_filter_changed("canvas".into());
    settle(ui).await;
    let before_selection = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_color_token_chosen("link".into());
    settle(ui).await;
    assert_eq!(ui.get_color_selected_id(), "link");
    assert!(ui.get_color_filter().is_empty());
    assert_eq!(
        std::fs::read_to_string(store.path()).unwrap(),
        before_selection
    );
    // Arming Reset all keeps single-color undo; the removal takes the
    // overrides the file holds then, including an undelivered one.
    ui.invoke_color_hex_edited("#010203".into());
    settle(ui).await;
    ui.invoke_color_reset_all();
    settle(ui).await;
    assert!(!ui.get_color_reset_all_notice().is_empty());
    assert!(
        ui.get_color_editor().can_undo,
        "arming Reset all dropped undo"
    );
    let mut external = store.load().unwrap();
    external.colors.insert("accent".into(), "#0A0B0C".into());
    store.save(&external).unwrap();
    let expected = store.load().unwrap().colors;
    assert_eq!(expected.len(), 3);
    ui.invoke_color_reset_all();
    settle(ui).await;
    assert!(colors().is_empty());
    assert_eq!(
        ui.get_color_reset_all_notice(),
        "Restore the 3 removed colors"
    );
    assert!(!ui.get_color_editor().can_undo);
    // A failed restore leaves the file alone and keeps the snapshot, so the
    // restore can be tried again once the file is valid.
    let cleared = std::fs::read_to_string(store.path()).unwrap();
    std::fs::write(store.path(), "not = [valid\n").unwrap();
    ui.invoke_color_reset_all();
    settle(ui).await;
    assert_eq!(
        std::fs::read_to_string(store.path()).unwrap(),
        "not = [valid\n"
    );
    assert!(!ui.get_config_error().is_empty());
    assert_eq!(
        ui.get_color_reset_all_notice(),
        "Restore the 3 removed colors"
    );
    std::fs::write(store.path(), &cleared).unwrap();
    ui.invoke_color_reset_all();
    settle(ui).await;
    assert_eq!(colors(), expected);
    assert!(ui.get_color_reset_all_notice().is_empty());
    // Back to no overrides, as applied, for the checks below.
    let mut plain = store.load().unwrap();
    plain.colors.clear();
    store.save(&plain).unwrap();
    apply_config(ui, &plain, None);
    ui.invoke_color_token_chosen("canvas".into());
    settle(ui).await;

    // A rejected opacity save rolls the preview back to the last valid value.
    let tokens = ui.global::<crate::StyleTokens>();
    ui.invoke_preview_window_opacity(30);
    settle(ui).await;
    assert!((tokens.get_window_opacity() - 0.3).abs() < 0.001);
    assert!(ui.get_window_background_cleared());
    let valid = std::fs::read_to_string(store.path()).unwrap();
    std::fs::write(store.path(), "not = [valid\n").unwrap();
    ui.invoke_set_window_opacity(30);
    settle(ui).await;
    assert_eq!(tokens.get_window_opacity(), 1.0);
    assert_eq!(ui.get_window_opacity_percent(), 100);
    assert!(!ui.get_window_background_cleared());
    std::fs::write(store.path(), &valid).unwrap();
    ui.invoke_set_window_opacity(99);
    settle(ui).await;
    assert_eq!(store.load().unwrap().window.opacity_percent, 99);
    assert!(ui.get_window_background_cleared());
    ui.invoke_set_window_opacity(100);
    settle(ui).await;
    assert!(!ui.get_window_background_cleared());

    // Keyboard changes persist on key release; accessibility changes after
    // the value settles, with no pointer release.
    assert!(crate::settings_search::reveal(
        ui,
        "appearance.window_opacity"
    ));
    settle(ui).await;
    assert_eq!(ui.get_settings_revealed_id(), "appearance.window_opacity");
    assert!(focused_control_has_label(
        ui,
        "Window background opacity, percent"
    ));
    key(ui, Key::LeftArrow);
    settle(ui).await;
    assert_eq!(store.load().unwrap().window.opacity_percent, 99);
    let mut sliders =
        ElementHandle::find_by_accessible_label(ui, "Window background opacity, percent").filter(
            |element| {
                element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Slider)
            },
        );
    let slider = sliders
        .next()
        .expect("the opacity slider is exposed to assistive technology (build tests with SLINT_EMIT_DEBUG_INFO=1)");
    assert!(
        sliders.next().is_none(),
        "the opacity slider must be unique"
    );
    slider.set_accessible_value("50");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
    settle(ui).await;
    assert_eq!(store.load().unwrap().window.opacity_percent, 50);

    // Home and End commit on key release, as the arrows do.
    assert!(crate::settings_search::reveal(
        ui,
        "appearance.window_opacity"
    ));
    settle(ui).await;
    key(ui, Key::Home);
    settle(ui).await;
    assert_eq!(store.load().unwrap().window.opacity_percent, 0);
    key(ui, Key::End);
    settle(ui).await;
    assert_eq!(store.load().unwrap().window.opacity_percent, 100);
    // A press away from the thumb moves it there and only previews; the
    // pointer release commits.
    let track = control_with_label(ui, "Window background opacity, percent");
    let center = track.map_to_window(track.geometry().center());
    let near_start =
        slint::LogicalPosition::new(center.x - track.geometry().size.width * 0.45, center.y);
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: near_start,
    });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: near_start,
        button: slint::platform::PointerEventButton::Left,
    });
    settle(ui).await;
    assert_eq!(
        store.load().unwrap().window.opacity_percent,
        100,
        "a press only previews"
    );
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: near_start,
        button: slint::platform::PointerEventButton::Left,
    });
    settle(ui).await;
    assert!(store.load().unwrap().window.opacity_percent < 50);
    ui.invoke_set_window_opacity(100);
    settle(ui).await;

    // Every search target reaches a real, enabled control; none changes a
    // setting or runs an action.
    use std::{cell::Cell, rc::Rc};
    let actions = Rc::new(Cell::new(0));
    let counted = actions.clone();
    ui.on_check_for_updates(move || counted.set(counted.get() + 1));
    let counted = actions.clone();
    ui.on_refresh_security(move || counted.set(counted.get() + 1));
    let counted = actions.clone();
    ui.on_select_audio_microphone(move |_| counted.set(counted.get() + 1));
    let counted = actions.clone();
    ui.on_add_account(move || counted.set(counted.get() + 1));
    let counted = actions.clone();
    ui.on_open_app_password(move |_| counted.set(counted.get() + 1));
    let counted = actions.clone();
    ui.on_activate_saved_account(move |_| counted.set(counted.get() + 1));
    let counted = actions.clone();
    ui.on_remove_saved_account(move |_| counted.set(counted.get() + 1));
    let before = std::fs::read_to_string(store.path()).unwrap();
    let live = ui.get_live_session();
    ui.set_live_session(true);
    ui.set_update_can_check(true);
    ui.set_audio_devices_enabled(true);
    ui.set_audio_devices_busy(false);
    ui.set_security_loading(false);
    ui.set_security_busy(false);
    settle(ui).await;
    for (index, target) in registry::SETTING_TARGETS.iter().enumerate() {
        ui.set_manage_accounts_open(false);
        ui.set_settings_revealed_id("".into());
        settle(ui).await;
        let requested = crate::settings_search::reveal(ui, target.id);
        if !requested {
            // Fixed registry position and availability only; never emit account
            // identifiers, search text, config contents or arbitrary errors.
            eprintln!(
                "MORPHEUS_SETTINGS_TARGET_FAILURE stage=0 index={index} live={}",
                ui.get_live_session()
            );
        }
        assert!(requested, "{}", target.id);
        settle(ui).await;
        if target.id == "files.transfers" {
            // Exercise the real deferred focus timer after the conditional
            // panel is instantiated; keep the real-focus assertion below.
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            settle(ui).await;
        }
        if ui.get_settings_revealed_id() != target.id {
            eprintln!(
                "MORPHEUS_SETTINGS_TARGET_FAILURE stage=1 index={index} live={}",
                ui.get_live_session()
            );
            if target.id == "files.transfers" {
                let heading = format!("Transfers: {}", ui.get_transfer_summary());
                // Pinned Slint's descendant query already excludes invisible
                // items; ElementHandle itself has no is_visible() method.
                let visible = ElementHandle::find_by_accessible_label(ui, &heading).count();
                // Numeric/boolean UI state only; never print the summary,
                // profile, transfer labels or arbitrary assertion text.
                eprintln!(
                    "MORPHEUS_TRANSFER_FOCUS_STATE page={} panel={} tab={} pending={} headings={visible}",
                    ui.get_page(),
                    ui.get_show_side_panel(),
                    ui.get_side_panel_tab(),
                    ui.get_transfer_focus_pending()
                );
            }
        }
        assert_eq!(
            ui.get_settings_revealed_id(),
            target.id,
            "{} has no destination",
            target.id
        );
        let edit_profile_label =
            format!("Edit public profile of {}", ui.get_active_account_handle());
        let account_label = match target.id {
            "accounts.manage" => Some("Add account"),
            "accounts.remove" => Some("Done"),
            "accounts.switch_password" => Some(if ui.get_app_password_enabled() {
                "Change password"
            } else {
                "Set app password"
            }),
            // Focus only: the editor opens on its own activation.
            "accounts.public_profile" => Some(edit_profile_label.as_str()),
            _ => None,
        };
        if let Some(label) = account_label {
            assert!(focused_control_has_label(ui, label), "{} focus", target.id);
        }
        if target.id == "files.transfers" {
            assert_eq!(ui.get_page(), 0);
            assert!(ui.get_show_side_panel());
            assert_eq!(ui.get_side_panel_tab(), 4);
            assert!(
                focused_control_has_label(ui, &format!("Transfers: {}", ui.get_transfer_summary())),
                "transfer destination must have real keyboard focus"
            );
            assert!(!ui.get_transfer_focus_pending());
            ui.set_settings_revealed_id("".into());
            assert!(crate::settings_search::reveal(ui, "files.transfers"));
            settle(ui).await;
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            settle(ui).await;
            assert_eq!(ui.get_settings_revealed_id(), "files.transfers");
            assert!(focused_control_has_label(
                ui,
                &format!("Transfers: {}", ui.get_transfer_summary())
            ));
        }
    }
    for (target, label) in [
        ("appearance.reduce_motion", "Reduce motion, off"),
        ("notifications.desktop", "Desktop notifications"),
        ("updates", "Check for updates"),
    ] {
        assert!(crate::settings_search::reveal(ui, target));
        settle(ui).await;
        assert!(focused_control_has_label(ui, label), "{target} focus");
    }
    // Unavailable controls are never claimed: their status line is revealed
    // and reported as such, and nothing is invoked.
    ui.set_update_can_check(false);
    ui.set_audio_devices_enabled(false);
    ui.set_security_busy(true);
    settle(ui).await;
    let update_status = format!(
        "Application update status: {}",
        if ui.get_update_status().is_empty() {
            "No update status yet.".into()
        } else {
            ui.get_update_status()
        }
    );
    let security_status = format!(
        "Device security status: {}",
        ui.get_security_device_status()
    );
    for (target, label) in [
        ("updates", update_status.as_str()),
        ("security.recovery", security_status.as_str()),
    ] {
        assert!(crate::settings_search::reveal(ui, target));
        settle(ui).await;
        assert_eq!(ui.get_settings_revealed_id(), format!("{target}:status"));
        assert!(
            focused_control_has_label(ui, label),
            "{target} status focus"
        );
    }
    for (enabled, busy) in [(false, false), (true, true)] {
        ui.set_audio_devices_enabled(enabled);
        ui.set_audio_devices_busy(busy);
        ui.set_settings_revealed_id("".into());
        settle(ui).await;
        assert!(crate::settings_search::reveal(ui, "media.audio_devices"));
        settle(ui).await;
        assert_eq!(ui.get_settings_revealed_id(), "media.audio_devices:status");
    }
    assert_eq!(actions.get(), 0, "search ran an action");
    ui.on_check_for_updates(|| {});
    ui.on_refresh_security(|| {});
    ui.on_select_audio_microphone(|_| {});
    ui.set_security_busy(false);
    ui.set_audio_devices_busy(false);
    ui.set_manage_accounts_open(false);
    ui.set_live_session(live);
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);

    // One color is a direct result: typing its ID or a word of its name, then
    // Down and Enter, selects exactly that token and focuses its hex field.
    // Choosing it saves nothing.
    for (attempt, (query, token)) in [
        ("presence_online", "presence_online"),
        ("online", "presence_online"),
        ("links", "link"),
    ]
    .into_iter()
    .enumerate()
    {
        ui.set_settings_search_query("".into());
        ui.invoke_settings_search("".into());
        settle(ui).await;
        // Returning to results intentionally focuses this field. A later
        // iteration must move away before exercising pointer focus again.
        if focused_control_has_label(ui, "Search settings") {
            key(ui, Key::Tab);
            settle(ui).await;
        }
        click_control_once(ui, "Search settings");
        type_text(ui, query);
        settle(ui).await;
        let results = ui.get_settings_search_results();
        assert!(results.row_count() <= registry::MAX_SEARCH_RESULTS);
        let id = format!("{}{token}", registry::COLOR_RESULT_PREFIX);
        let row = (0..results.row_count())
            .find(|row| results.row_data(*row).unwrap().id == id.as_str())
            .unwrap_or_else(|| panic!("{query} does not offer {token}"));
        // Exact fields name their config.toml key.
        assert_eq!(
            results.row_data(row).unwrap().key,
            format!("colors.{token}").as_str(),
            "{query}"
        );
        for _ in 0..row {
            key(ui, Key::DownArrow);
        }
        key(ui, Key::Return);
        settle(ui).await;
        assert_eq!(ui.get_color_selected_id(), token, "{query}");
        assert_eq!(
            ui.get_settings_revealed_id(),
            crate::settings_search::COLOR_HEX_REVEAL,
            "{query}"
        );
        let label = registry::color_token(token).unwrap().label;
        assert!(
            focused_control_has_label(ui, &format!("Hex color for {label}, #RRGGBB or #RRGGBBAA")),
            "{query}: the hex field of {token} is not focused"
        );
        // The chosen result's list comes back without retyping, with the
        // same highlight: by its action, by Down in the field, or by the
        // Search Settings shortcut.
        assert!(ui.get_settings_search_dismissed(), "{query}");
        match attempt {
            0 => click_control_once(ui, &format!("Back to the results for {query}")),
            1 => {
                click_control_once(ui, "Search settings");
                key(ui, Key::DownArrow);
            }
            _ => ui.invoke_focus_settings_search(),
        }
        settle(ui).await;
        assert!(
            !ui.get_settings_search_dismissed(),
            "{query}: results stay hidden"
        );
        assert_eq!(ui.get_settings_search_index(), row as i32, "{query}");
        assert!(
            focused_control_has_label(ui, "Search settings"),
            "{query}: the search field is not focused"
        );
        assert_eq!(
            ui.get_color_selected_id(),
            token,
            "{query}: returning changed the token"
        );
    }
    assert_eq!(
        std::fs::read_to_string(store.path()).unwrap(),
        before,
        "choosing a color result saved"
    );
    ui.set_settings_search_query("".into());
    ui.invoke_settings_search("".into());
    settle(ui).await;

    // Lengths keep their px unit; the unitless sender weight shows none.
    let weight = match ui.get_sender_font_weight() {
        0 => 650,
        weight => weight,
    };
    assert!(!control_bounds(ui, &weight.to_string()).is_empty());
    assert!(
        control_bounds(ui, &format!("{weight} px")).is_empty(),
        "the sender weight is shown as a length"
    );
    assert!(!control_bounds(ui, &format!("{} px", ui.get_message_font_size())).is_empty());

    // At the smallest window, at 100% and 200% text, the token list is a
    // bounded region and choosing a token by keyboard brings its editor into
    // view with the hex field focused.
    for scale in [100, 200] {
        ui.window().set_size(slint::PhysicalSize::new(940, 620));
        ui.set_font_scale_percent(scale);
        ui.set_page(1);
        settle(ui).await;
        assert!(crate::settings_search::reveal(ui, "appearance.colors"));
        settle(ui).await;
        assert_eq!(ui.get_settings_revealed_id(), "appearance.colors");
        let window_height = ui
            .window()
            .size()
            .to_logical(ui.window().scale_factor())
            .height;
        let list = control_with_label(ui, "Colors to edit");
        assert!(
            list.geometry().size.height <= (140.0f32).max(window_height * 0.24) + 1.0,
            "{scale}%: the token list is not bounded"
        );
        type_text(ui, "link");
        settle(ui).await;
        tab_to_control(ui, "Links, selected");
        key(ui, Key::Return);
        settle(ui).await;
        let hex_label = "Hex color for Links, #RRGGBB or #RRGGBBAA";
        assert!(
            focused_control_has_label(ui, hex_label),
            "{scale}%: the editor field has focus"
        );
        let hex = control_with_label(ui, hex_label);
        let top = hex.map_to_window(hex.geometry().origin).y;
        assert!(
            top >= 0.0 && top + hex.geometry().size.height <= window_height + 0.5,
            "{scale}%: the hex field is outside the window"
        );
        ui.set_color_filter("".into());
        ui.invoke_color_filter_changed("".into());
        settle(ui).await;
    }

    // Eight long results at the smallest window and 200% text stay inside the
    // window; Down keeps the highlighted row visible and Enter reveals it.
    // Whatever the eighth result is, its control is available here.
    ui.set_update_can_check(true);
    ui.set_audio_devices_enabled(true);
    ui.window().set_size(slint::PhysicalSize::new(940, 620));
    ui.set_font_scale_percent(200);
    ui.set_settings_search_query("".into());
    ui.invoke_settings_search("".into());
    settle(ui).await;
    click_control_once(ui, "Search settings");
    type_text(ui, "e");
    settle(ui).await;
    let results = ui.get_settings_search_results();
    assert_eq!(results.row_count(), registry::MAX_SEARCH_RESULTS);
    let window_height = ui
        .window()
        .size()
        .to_logical(ui.window().scale_factor())
        .height;
    let field = control_with_label(ui, "Search settings");
    let field_bottom =
        field.map_to_window(field.geometry().origin).y + field.geometry().size.height;
    assert!(
        field_bottom < window_height / 2.0,
        "the search field stays near the top"
    );
    for _ in 1..registry::MAX_SEARCH_RESULTS {
        key(ui, Key::DownArrow);
    }
    settle(ui).await;
    assert_eq!(
        ui.get_settings_search_index(),
        registry::MAX_SEARCH_RESULTS as i32 - 1
    );
    assert!(
        focused_control_has_label(ui, "Search settings"),
        "the field keeps focus"
    );
    let last = results.row_data(registry::MAX_SEARCH_RESULTS - 1).unwrap();
    let key_note = if last.key.is_empty() {
        String::new()
    } else {
        format!(" In config.toml: {}.", last.key)
    };
    let row = control_with_label(
        ui,
        &format!(
            "{}, {}. {}{key_note}",
            last.label, last.breadcrumb, last.description
        ),
    );
    let top = row.map_to_window(row.geometry().origin).y;
    let bottom = top + row.geometry().size.height;
    assert!(
        top >= field_bottom - 0.5 && bottom <= window_height + 0.5,
        "{top}..{bottom}"
    );
    key(ui, Key::Return);
    settle(ui).await;
    // A color result reveals its token's hex field.
    let expected = if registry::color_result(last.id.as_str()).is_some() {
        crate::settings_search::COLOR_HEX_REVEAL.to_owned()
    } else {
        last.id.to_string()
    };
    assert_eq!(ui.get_settings_revealed_id(), expected.as_str());

    ui.set_manage_accounts_open(false);
    ui.set_update_can_check(false);
    ui.set_audio_devices_enabled(false);
    ui.set_settings_search_query("".into());
    ui.invoke_settings_search("".into());
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_page(0);
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

// A cached profile alone must not make a second switch actionable while the
// first request is still waiting to own the production transition lock.
async fn assert_switch_readiness(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    access: &Arc<AccountAccessUi>,
) {
    use slint::private_unstable_api::re_exports::{AccessibilityAction, AccessibleStringProperty};
    use std::{cell::Cell, rc::Rc};

    let options = ui.get_account_options();
    ui.set_account_options(ModelRc::new(VecModel::from(vec![crate::AccountOption {
        index: 0,
        local_id: "presentation-only".into(),
        name: "Rowan".into(),
        handle: "@rowan:example.org".into(),
        ..Default::default()
    }])));
    let activations = Rc::new(Cell::new(0));
    let recorded = activations.clone();
    ui.on_switch_account(move |_| recorded.set(recorded.get() + 1));
    let transition = live.account_transition.lock().await;
    access.request("presentation-only".into());
    assert!(ui.get_account_switch_pending());
    assert!(access.busy.load(Ordering::Acquire));
    ui.set_account_menu_open(true);
    settle(ui).await;
    let label = "Switch to Rowan, @rowan:example.org";
    let choice = control_with_label(ui, label);
    assert_eq!(
        choice
            .accessible_string_property(AccessibleStringProperty::Enabled)
            .as_deref(),
        Some("false")
    );
    choice.accessible_action(&AccessibilityAction::Default);
    assert_eq!(activations.get(), 0);
    assert!(ui.get_account_menu_open());
    drop(transition);
    tokio::time::timeout(Duration::from_secs(2), async {
        while ui.get_account_switch_pending() {
            settle(ui).await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a rejected switch releases presentation readiness");
    assert!(!access.busy.load(Ordering::Acquire));
    assert!(!live.security_gate.blocked());
    let choice = control_with_label(ui, label);
    assert_eq!(
        choice
            .accessible_string_property(AccessibleStringProperty::Enabled)
            .as_deref(),
        Some("true")
    );
    choice.accessible_action(&AccessibilityAction::Default);
    assert_eq!(activations.get(), 1);
    ui.set_account_menu_open(false);
    ui.set_account_options(options);
    ui.on_switch_account(|_| {});
    ui.set_account_access_notice("".into());
    settle(ui).await;
}

/// Font sizes and window-relative tops of the real Text items showing `text`.
fn text_sizes_and_tops(ui: &AppWindow, text: &str) -> Vec<(f32, f32)> {
    use slint::private_unstable_api::re_exports::{ComplexText, ItemRc, SimpleText, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut found = Vec::new();
    root.visit_descendants(|item| {
        let size = if let Some(simple) = item.downcast::<SimpleText>() {
            let simple = simple.as_pin_ref();
            (simple.text().as_str() == text).then(|| simple.font_size().get())
        } else if let Some(complex) = item.downcast::<ComplexText>() {
            let complex = complex.as_pin_ref();
            (complex.text().as_str() == text).then(|| complex.font_size().get())
        } else {
            None
        };
        if let Some(size) = size {
            found.push((size, item_bounds(item).top));
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// Typography and geometry metrics on the real generated window: a metric
/// result reveals its value field and saves nothing; the steppers and an
/// entered value save through the re-read file while typing alone does not;
/// Undo never overwrites a later edit; an invalid hand edit is never replaced
/// and the last valid tokens stay; an omitted metric steps from its built-in
/// look and Use built-in omits it again; Reset all restores. At the largest
/// icon and clock sizes with 200% text, default-size icon buttons fit inside
/// both composers, and a narrow row keeps its actions below the clock.
async fn assert_metric_workspace_and_extremes(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::{ConfigStore, MetricValue};

    let store = ConfigStore::at(directory.join("metrics.toml"));
    let config = store.load_or_create().unwrap();
    crate::metric_editor::register(ui, &store);
    crate::settings_search::register(ui);
    apply_config(ui, &config, None);
    ui.set_page(1);
    settle(ui).await;
    let tokens = ui.global::<crate::StyleTokens>();
    assert_eq!(tokens.get_focus_width(), 2.0);
    assert_eq!(tokens.get_icon_button_size(), 34.0);
    assert_eq!(tokens.get_control_radius(), -1.0);
    assert_eq!(tokens.get_line_width(), 1.0);
    assert_eq!(ui.get_metric_override_count(), 0);

    let before = std::fs::read_to_string(store.path()).unwrap();
    assert!(crate::settings_search::reveal(
        ui,
        "metric:geometry.focus_width"
    ));
    settle(ui).await;
    assert_eq!(ui.get_settings_revealed_id(), "metric:geometry.focus_width");
    assert!(focused_control_has_label(
        ui,
        "Focus ring width in logical pixels, 1 to 6; Enter applies"
    ));
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);
    let focus = || store.load().unwrap().geometry.focus_width;

    ui.invoke_metric_step(1);
    settle(ui).await;
    assert_eq!(focus(), 3);
    assert_eq!(tokens.get_focus_width(), 3.0);
    ui.invoke_metric_value_edited("9".into());
    settle(ui).await;
    assert!(!ui.get_metric_editor().hint.is_empty());
    ui.invoke_metric_value_accepted("9".into());
    settle(ui).await;
    assert_eq!(focus(), 3, "an out-of-range value was saved");
    ui.invoke_metric_value_accepted("5".into());
    settle(ui).await;
    assert_eq!(focus(), 5);
    assert!(ui.get_metric_editor().can_undo && ui.get_metric_editor().overridden);
    ui.invoke_metric_undo();
    settle(ui).await;
    assert_eq!(focus(), 3);

    // Undo never overwrites a later edit made elsewhere.
    ui.invoke_metric_step(1);
    settle(ui).await;
    assert_eq!(focus(), 4);
    let mut external = store.load().unwrap();
    external
        .set_metric("geometry.focus_width", MetricValue::Set(6))
        .unwrap();
    store.save(&external).unwrap();
    let superseded = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_metric_undo();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), superseded);
    assert!(!ui.get_metric_editor().can_undo);
    assert!(!ui.get_metric_editor().hint.is_empty());

    // An invalid hand edit is never overwritten and its value never shown.
    let applied = tokens.get_focus_width();
    let broken = "[geometry]\nfocus_width = 0\n";
    std::fs::write(store.path(), broken).unwrap();
    ui.invoke_metric_step(1);
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), broken);
    assert_eq!(tokens.get_focus_width(), applied);
    assert!(!ui.get_config_error().is_empty());
    assert!(!ui.get_config_error().contains('0'));
    std::fs::write(store.path(), &superseded).unwrap();

    // An omitted metric steps from its built-in look; Use built-in omits it.
    assert!(crate::settings_search::reveal(
        ui,
        "metric:geometry.avatar_roundness"
    ));
    settle(ui).await;
    assert!(ui.get_metric_editor().inherited);
    ui.invoke_metric_step(1);
    settle(ui).await;
    assert_eq!(store.load().unwrap().geometry.avatar_roundness, Some(40));
    assert_eq!(tokens.get_avatar_roundness(), 40);
    ui.invoke_metric_reset();
    settle(ui).await;
    assert_eq!(store.load().unwrap().geometry.avatar_roundness, None);
    assert_eq!(tokens.get_avatar_roundness(), -1);

    // Reset all is armed first, then resets every metric, then restores.
    ui.invoke_metric_reset_all();
    settle(ui).await;
    assert!(!ui.get_metric_reset_all_notice().is_empty());
    assert_eq!(focus(), 6, "arming Reset all changed the file");
    ui.invoke_metric_reset_all();
    settle(ui).await;
    assert_eq!(store.load().unwrap().geometry, Config::default().geometry);
    assert_eq!(tokens.get_focus_width(), 2.0);
    ui.invoke_metric_reset_all();
    settle(ui).await;
    assert_eq!(focus(), 6);

    // The largest icon and clock with the smallest message text at 200%.
    let mut extreme = store.load().unwrap();
    extreme.typography.message_size = 10;
    for (key, value) in [
        ("geometry.icon_size", 28),
        ("typography.timestamp_size", 20),
    ] {
        extreme.set_metric(key, MetricValue::Set(value)).unwrap();
    }
    store.save(&extreme).unwrap();
    apply_config(ui, &extreme, None);
    assert_eq!(tokens.get_icon_button_size(), 44.0);
    ui.set_messages(ModelRc::new(VecModel::from(vec![role_message(
        "$metric-extreme",
        false,
        "Metric Author",
        0,
        "10:15 AM",
        Color::from_rgb_u8(0x11, 0x22, 0x33),
    )])));
    ui.set_thread_root_id("$metric-extreme".into());
    ui.set_thread_root_sender("Metric Author".into());
    ui.invoke_show_thread_panel();
    let panes = ui.get_rail_width_px() + ui.get_room_list_width() + ui.get_details_width();
    let narrow = (panes + 360).max(940);
    ui.window()
        .set_size(slint::PhysicalSize::new(narrow as u32, 760));
    ui.set_font_scale_percent(200);
    settle(ui).await;
    for (button, input) in [
        (
            "Attach files".to_owned(),
            format!("Message composer for {}", ui.get_active_room_name()),
        ),
        (
            "Open emoji picker for thread reply".to_owned(),
            "Reply in thread started by Metric Author".to_owned(),
        ),
    ] {
        let button_box = control_bounds(ui, &button);
        let input_box = control_bounds(ui, &input);
        assert_eq!((button_box.len(), input_box.len()), (1, 1), "{button}");
        let (button_box, input_box) = (button_box[0], input_box[0]);
        assert!(
            button_box.bottom - button_box.top >= 44.0 - 0.5,
            "{button} did not grow"
        );
        assert!(
            button_box.top >= input_box.top - 0.5 && button_box.bottom <= input_box.bottom + 0.5,
            "{button} {button_box:?} is clipped by its composer row {input_box:?}"
        );
    }
    // The narrow row's clock is 40 px; its actions start below it.
    let clocks = text_sizes_and_tops(ui, "· 10:15 AM");
    assert_eq!(clocks.len(), 1, "the narrow header shows one clock");
    let (clock_size, clock_top) = clocks[0];
    assert!((clock_size - 40.0).abs() < 0.01, "clock is {clock_size}");
    let reply = control_bounds(ui, "Reply to Metric Author");
    assert_eq!(reply.len(), 1);
    assert!(
        reply[0].top >= clock_top + clock_size * 1.2 - 0.5,
        "the reply action {:?} covers the clock at {clock_top}",
        reply[0]
    );

    // A download's cancel action follows the shared button metrics instead
    // of being squeezed into a fixed-height notice. No save is started.
    let previous_notice = ui.get_transfer_notice();
    let previous_download = ui.get_downloading_event_id();
    ui.set_transfer_notice("Preparing synthetic attachment".into());
    ui.set_downloading_event_id("$synthetic-download:example.org".into());
    for scale in [100, 200] {
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        let cancel = control_bounds(ui, "Cancel saving the attachment");
        assert_eq!(cancel.len(), 1);
        assert!(cancel[0].bottom - cancel[0].top >= 32.0 * scale as f32 / 100.0 - 0.5);
        assert!(inside_window(ui, cancel[0]));
    }
    ui.set_transfer_notice(previous_notice);
    ui.set_downloading_event_id(previous_download);

    ui.set_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    ui.set_thread_root_id("".into());
    ui.set_thread_root_sender("".into());
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_page(0);
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

/// Interaction states and the sender palette on the real generated window:
/// unset states keep every built-in color; config states reach the message
/// hover, disabled buttons and a distinct mention badge; the palette editor
/// hints on partial input, saves a complete value, reorders, undoes only its
/// own change, never overwrites an invalid file, reports low contrast and
/// returns to the theme palette; the timeline names use the saved palette.
async fn assert_interaction_states_and_sender_palette(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;
    use slint::private_unstable_api::re_exports::{AccessibleRole, AccessibleStringProperty};

    let store = ConfigStore::at(directory.join("states.toml"));
    let mut config = store.load_or_create().unwrap();
    crate::sender_palette_editor::register(ui, &store);
    apply_config(ui, &config, None);
    settle(ui).await;
    let transparent = Color::from_argb_u8(0, 0, 0, 0);
    let elevated = ui.invoke_token_color("elevated".into());
    assert_eq!(
        ui.invoke_row_surface(false, true, false, transparent),
        elevated
    );
    assert_eq!(
        ui.invoke_token_color("mention".into()),
        ui.invoke_token_color("destructive".into())
    );
    assert_eq!(ui.invoke_token_color("control_pressed".into()), transparent);

    // Config states reach real controls.
    let rgb = |red, green, blue| Color::from_rgb_u8(red, green, blue);
    for (id, value) in [
        ("message_hover", "#223344"),
        ("mention", "#AA3344"),
        ("mention_ink", "#FFFFFF"),
        ("control_disabled", "#5A6B7C"),
        ("composer_surface", "#334455"),
    ] {
        config.colors.insert(id.into(), value.into());
    }
    config.appearance.hover_duration_ms = 0;
    apply_config(ui, &config, None);
    ui.set_page(0);
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(vec![RoomItem {
        id: "!mentions:example.org".into(),
        name: "Mentions room".into(),
        unread: 3,
        mentions: 2,
        ..Default::default()
    }])));
    crate::navigation::apply_room_order(ui);
    settle(ui).await;
    assert_eq!(
        ui.invoke_row_surface(false, true, false, transparent),
        rgb(0x22, 0x33, 0x44)
    );
    assert_eq!(
        ui.invoke_token_color("composer_surface".into()),
        rgb(0x33, 0x44, 0x55)
    );
    assert_text_color(ui, "@2", rgb(0xff, 0xff, 0xff), 1);
    assert!(
        text_colors_of(ui, "3").is_empty(),
        "a mention badge replaces the unread count"
    );
    // The hover-fade decrease at 0 ms is disabled. Inspect its actual vector
    // stroke, not another control's text or the pre-migration minus glyph.
    ui.set_page(1);
    settle(ui).await;
    assert!(crate::settings_search::reveal(
        ui,
        "appearance.hover_duration"
    ));
    settle(ui).await;
    let label = "Decrease Hover fade duration";
    let decrease = control_with_label(ui, label);
    assert_eq!(decrease.accessible_role(), AccessibleRole::Button);
    assert_eq!(
        decrease
            .accessible_string_property(AccessibleStringProperty::Enabled)
            .as_deref(),
        Some("false"),
        "the exact hover-duration decrease is disabled at zero"
    );
    let (bounds, icon) = drawn_icon(ui, label);
    assert!(bounds.right > bounds.left && bounds.bottom > bounds.top);
    assert!(
        inside_window(ui, bounds),
        "the disabled control is shown in full"
    );
    assert_eq!(icon.commands, ["M 5 12 L 19 12"]);
    assert!(icon.texts.is_empty(), "the operator remains a vector icon");
    assert!(
        icon.bounds[0].right > icon.bounds[0].left && icon.bounds[0].bottom > icon.bounds[0].top
    );
    assert!(icon.stroke_widths[0] > 0.0);
    assert_eq!(icon.strokes, [rgb(0x5a, 0x6b, 0x7c)]);
    // Remove the color override while keeping this same control disabled.
    let mut plain = store.load().unwrap();
    plain.appearance.hover_duration_ms = 0;
    apply_config(ui, &plain, None);
    settle(ui).await;
    let decrease = control_with_label(ui, label);
    assert_eq!(
        decrease
            .accessible_string_property(AccessibleStringProperty::Enabled)
            .as_deref(),
        Some("false")
    );
    let (bounds, icon) = drawn_icon(ui, label);
    assert!(inside_window(ui, bounds));
    assert_eq!(icon.commands, ["M 5 12 L 19 12"]);
    assert_eq!(icon.strokes, [ui.invoke_token_color("ink".into())]);
    assert!(!icon.strokes.contains(&rgb(0x5a, 0x6b, 0x7c)));

    // The palette editor starts from the identity colors.
    let mut colored = store.load().unwrap();
    colored.chat.color_senders = true;
    store.save(&colored).unwrap();
    apply_config(ui, &colored, None);
    settle(ui).await;
    let rows = || ui.get_sender_color_rows();
    assert_eq!(rows().row_count(), 5);
    assert!(!ui.get_sender_palette_custom());
    let senders = || store.load().unwrap().senders.colors;
    ui.invoke_sender_color_edited(0, "#12345".into());
    settle(ui).await;
    assert!(!ui.get_sender_palette_hint().is_empty());
    assert_eq!(senders(), None, "a partial value was saved");
    ui.invoke_sender_color_edited(0, "#123456".into());
    settle(ui).await;
    let saved = senders().expect("a complete value saves");
    assert_eq!(saved.len(), 5);
    assert_eq!(saved[0], "#123456");
    assert!(ui.get_sender_palette_custom() && ui.get_sender_palette_can_undo());
    assert_eq!(
        ui.invoke_sender_ink(false, false, 0, transparent),
        rgb(0x12, 0x34, 0x56)
    );
    ui.invoke_sender_color_move(0, 1);
    settle(ui).await;
    assert_eq!(senders().unwrap()[1], "#123456");
    ui.invoke_sender_colors_undo();
    settle(ui).await;
    assert_eq!(senders().unwrap()[0], "#123456");

    // Undo never overwrites a later edit made elsewhere.
    ui.invoke_sender_color_edited(1, "#654321".into());
    settle(ui).await;
    let mut external = store.load().unwrap();
    external.senders.colors = Some(vec!["#ABCDEF".into()]);
    store.save(&external).unwrap();
    let superseded = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_sender_colors_undo();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), superseded);
    assert!(!ui.get_sender_palette_hint().is_empty());

    // An invalid hand edit is never overwritten.
    let broken = "[senders]\ncolors = []\n";
    std::fs::write(store.path(), broken).unwrap();
    ui.invoke_sender_color_add();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), broken);
    assert!(!ui.get_config_error().is_empty());
    std::fs::write(store.path(), &superseded).unwrap();

    // A one-color palette cannot lose its last color; low contrast is
    // reported, not corrected.
    let mut light = store.load().unwrap();
    light.colors.insert("canvas".into(), "#FFFFFF".into());
    store.save(&light).unwrap();
    apply_config(ui, &light, None);
    settle(ui).await;
    ui.invoke_sender_color_remove(0);
    settle(ui).await;
    assert_eq!(senders().unwrap().len(), 1);
    assert!(!ui.get_sender_palette_hint().is_empty());
    ui.invoke_sender_color_edited(0, "#FFFF00".into());
    settle(ui).await;
    assert_eq!(senders().unwrap(), vec!["#FFFF00".to_owned()]);
    assert!(!rows().row_data(0).unwrap().readable);
    // Alpha is blended over the background before measuring, and kept.
    ui.invoke_sender_color_edited(0, "#00000020".into());
    settle(ui).await;
    assert_eq!(senders().unwrap(), vec!["#00000020".to_owned()]);
    assert!(!rows().row_data(0).unwrap().readable);
    ui.invoke_sender_color_edited(0, "#000000".into());
    settle(ui).await;
    assert!(rows().row_data(0).unwrap().readable);
    // In the narrowest window at 200% text the hex field keeps a usable width
    // on its own line, and the row's actions stay inside the window.
    ui.set_page(1);
    ui.set_font_scale_percent(200);
    ui.window().set_size(slint::PhysicalSize::new(940, 900));
    settle(ui).await;
    let hex = control_bounds(ui, "Sender color 1 hex, #RRGGBB or #RRGGBBAA");
    let remove = control_bounds(ui, "Remove sender color 1");
    assert_eq!((hex.len(), remove.len()), (1, 1));
    assert!(hex[0].right - hex[0].left >= 140.0, "{:?}", hex[0]);
    assert!(
        remove[0].top >= hex[0].bottom,
        "{:?} {:?}",
        hex[0],
        remove[0]
    );
    assert!(remove[0].right <= window_size(ui).0, "{:?}", remove[0]);
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;
    // At the default size one line holds the field and its actions.
    let hex = control_bounds(ui, "Sender color 1 hex, #RRGGBB or #RRGGBBAA");
    let remove = control_bounds(ui, "Remove sender color 1");
    assert!(
        remove[0].top < hex[0].bottom,
        "{:?} {:?}",
        hex[0],
        remove[0]
    );
    ui.invoke_sender_colors_reset();
    settle(ui).await;
    assert_eq!(senders(), None);
    assert!(!ui.get_sender_palette_custom());

    ui.set_unsorted_channels(ModelRc::new(VecModel::<RoomItem>::default()));
    crate::navigation::apply_room_order(ui);
    ui.set_page(0);
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

/// Pane composition and motion on the real generated window: the details
/// panel moves to either side without moving the room list or the account
/// control, keyboard room order follows the displayed section order, each
/// pane keeps its own density, and the opt-in fades are zero under Reduce
/// motion.
async fn assert_pane_composition_and_motion(ui: &AppWindow) {
    use morpheus_config::{Density, DetailsSide};

    let mut config = Config::default();
    apply_config(ui, &config, None);
    ui.set_page(0);
    ui.set_show_side_panel(true);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;
    let details = || control_bounds(ui, "Conversation details");
    let (width, _) = window_size(ui);
    let right = details();
    assert_eq!(right.len(), 1);
    assert!((right[0].right - width).abs() < 1.0, "{:?}", right[0]);
    let account_label = format!(
        "Account menu for {}, {}",
        ui.get_active_account_name(),
        ui.get_active_account_handle()
    );
    let account = control_bounds(ui, &account_label);
    assert_eq!(account.len(), 1);
    config.layout.details_side = DetailsSide::Left;
    apply_config(ui, &config, None);
    settle(ui).await;
    let left = details();
    assert_eq!(left.len(), 1);
    let rail = ui.get_rail_width_px() as f32;
    let list = ui.get_room_list_width() as f32;
    assert!(
        (left[0].left - (rail + list)).abs() < 1.0,
        "the panel starts after the room list: {:?}",
        left[0]
    );
    assert_eq!(
        control_bounds(ui, &account_label),
        account,
        "the account control moved"
    );

    // Keyboard order follows the displayed sections.
    let room = |id: &str, unread| RoomItem {
        id: id.into(),
        name: id.into(),
        unread,
        ..Default::default()
    };
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(vec![room("!room", 1)])));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(vec![room("!direct", 1)])));
    crate::navigation::apply_room_order(ui);
    let order = || {
        crate::navigation::visible_rooms(ui)
            .into_iter()
            .map(|room| room.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(), ["!room", "!direct"]);
    config.layout.direct_messages_first = true;
    config.layout.room_density = Some(Density::Spacious);
    config.layout.message_density = Some(Density::Compact);
    apply_config(ui, &config, None);
    settle(ui).await;
    assert_eq!(order(), ["!direct", "!room"]);
    assert_eq!(ui.get_density_mode(), 1);
    assert_eq!(ui.get_room_density_mode(), 2);
    assert_eq!(ui.get_message_density_mode(), 0);
    let row = control_bounds(ui, "Open room !room");
    assert_eq!(row.len(), 1);
    assert!(
        (row[0].bottom - row[0].top - 44.0).abs() < 0.5,
        "spacious room rows: {:?}",
        row[0]
    );

    // With combined lists, Direct messages first is disabled; the search
    // destination is still a live control, the selected details-panel side.
    config.layout.group_direct_messages = false;
    apply_config(ui, &config, None);
    ui.set_settings_revealed_id("".into());
    settle(ui).await;
    assert!(crate::settings_search::reveal(ui, "layout.composition"));
    settle(ui).await;
    assert_eq!(ui.get_settings_revealed_id(), "layout.composition");
    assert!(focused_control_has_label(
        ui,
        "Details panel on the Left of the conversation, selected"
    ));
    config.layout.group_direct_messages = true;
    apply_config(ui, &config, None);
    ui.set_page(0);
    settle(ui).await;

    // Fades are opt-in, and Reduce motion makes them immediate.
    let tokens = ui.global::<crate::StyleTokens>();
    assert_eq!(tokens.get_popup_duration(), 0);
    config.appearance.popup_fade_ms = 160;
    config.appearance.panel_fade_ms = 240;
    apply_config(ui, &config, None);
    assert_eq!(tokens.get_popup_duration(), 160);
    assert_eq!(tokens.get_panel_duration(), 240);
    config.appearance.reduce_motion = true;
    apply_config(ui, &config, None);
    assert_eq!(tokens.get_popup_duration(), 0);
    assert_eq!(tokens.get_panel_duration(), 0);

    ui.set_unsorted_channels(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::<RoomItem>::default()));
    crate::navigation::apply_room_order(ui);
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

/// Account-scoped colors on the real generated window: an account's colors
/// appear only while the window publishes that account, a switch or detach
/// never leaves them on screen, the scoped editor writes only the active
/// account's entry, an undo step never crosses accounts, and removing the
/// account clears exactly its own appearance.
async fn assert_account_scoped_appearance(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;

    let store = ConfigStore::at(directory.join("accounts.toml"));
    let mut config = store.load_or_create().unwrap();
    config.colors.insert("canvas".into(), "#101010".into());
    config
        .account_overrides
        .entry("account-one".into())
        .or_default()
        .colors
        .insert("canvas".into(), "#203040".into());
    config
        .account_overrides
        .entry("account-two".into())
        .or_default()
        .colors
        .insert("accent".into(), "#405060".into());
    store.save(&config).unwrap();
    crate::appearance_editor::register(ui, &store);
    let canvas = || ui.invoke_token_color("canvas".into());
    let global = Color::from_rgb_u8(0x10, 0x10, 0x10);
    let own = Color::from_rgb_u8(0x20, 0x30, 0x40);
    ui.set_active_account_local_id("".into());
    apply_config(ui, &config, None);
    settle(ui).await;
    assert_eq!(canvas(), global);
    // The window publishes an account only after its activation succeeded.
    for (account, expected) in [
        ("account-one", own),
        ("account-two", global),
        ("account-one", own),
        ("", global),
        ("unknown-account", global),
    ] {
        ui.set_active_account_local_id(account.into());
        crate::appearance::apply_theme_mode(ui);
        settle(ui).await;
        assert_eq!(canvas(), expected, "{account}");
    }

    // The scoped editor writes only the active account's entry.
    ui.set_active_account_local_id("account-one".into());
    crate::appearance::apply_theme_mode(ui);
    ui.set_page(1);
    settle(ui).await;
    ui.invoke_color_scope_chosen(1);
    ui.invoke_color_token_chosen("link".into());
    ui.invoke_color_hex_edited("#ABCDEF".into());
    settle(ui).await;
    let saved = store.load().unwrap();
    assert_eq!(
        saved.account_overrides["account-one"]
            .colors
            .get("link")
            .map(String::as_str),
        Some("#ABCDEF")
    );
    assert!(
        !saved.colors.contains_key("link"),
        "the global table changed"
    );
    assert!(
        !saved.account_overrides["account-two"]
            .colors
            .contains_key("link")
    );
    assert!(ui.get_color_editor().can_undo);
    // A live refresh after every room update keeps a partial value.
    ui.set_color_hex_input("#AB".into());
    ui.invoke_color_hex_edited("#AB".into());
    crate::appearance::apply_theme_mode(ui);
    settle(ui).await;
    assert_eq!(ui.get_color_hex_input().as_str(), "#AB");
    // For all accounts, a color the active account sets itself shows and
    // edits the global value.
    ui.invoke_color_scope_chosen(0);
    ui.invoke_color_token_chosen("canvas".into());
    settle(ui).await;
    assert_eq!(ui.get_color_hex_input().as_str(), "#101010");
    assert_eq!(ui.get_color_swatch(), global);
    assert!(
        ui.get_color_editor()
            .source
            .contains("active account uses its own color")
    );
    ui.invoke_color_hex_edited("#203040".into());
    settle(ui).await;
    assert_eq!(
        store
            .load()
            .unwrap()
            .colors
            .get("canvas")
            .map(String::as_str),
        Some("#203040")
    );
    assert_eq!(ui.get_color_hex_input().as_str(), "#203040");
    ui.invoke_color_hex_edited("#101010".into());
    settle(ui).await;
    assert_eq!(store.load().unwrap().colors, saved.colors);
    assert_eq!(canvas(), own, "the account keeps its own color");
    // Switching accounts drops the undo step; Undo cannot reach another account.
    ui.set_active_account_local_id("account-two".into());
    crate::appearance::apply_theme_mode(ui);
    settle(ui).await;
    assert!(!ui.get_color_editor().can_undo);
    let before = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_color_undo();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);
    // With no active account, the editor returns to the colors for all.
    ui.set_active_account_local_id("".into());
    crate::appearance::apply_theme_mode(ui);
    settle(ui).await;
    assert_eq!(ui.get_color_scope(), 0);

    // Removal clears exactly the removed account's appearance.
    let voices = VoiceSpaces::new(ConfigStore::at(store.path()));
    voices.clear_logged_out_account("account-one").unwrap();
    let cleaned = store.load().unwrap();
    assert!(!cleaned.account_overrides.contains_key("account-one"));
    assert_eq!(
        cleaned.account_overrides["account-two"],
        saved.account_overrides["account-two"]
    );
    assert_eq!(cleaned.colors, saved.colors);

    ui.set_page(0);
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

/// Keyboard customization on the real generated window: every registry
/// action is listed; a recorded combination saves in portable form; typed
/// conflicts name the other action and save nothing; reserved keys are
/// refused; Disable, Default and Undo follow the file; recording pauses every
/// shortcut; and new actions dispatch to their real destinations.
async fn assert_shortcut_customization(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::{ConfigStore, ShortcutAction};
    use std::{cell::RefCell, rc::Rc};

    let store = ConfigStore::at(directory.join("shortcuts.toml"));
    let config = store.load_or_create().unwrap();
    crate::shortcuts::register(ui, store.clone());
    apply_config(ui, &config, None);
    ui.set_page(1);
    settle(ui).await;
    assert_eq!(
        ui.get_shortcut_items().row_count(),
        ShortcutAction::ALL.len()
    );
    let binding = |name: &str| store.load().unwrap().shortcuts.get(name).cloned();

    // A recorded combination saves at once, written with Mod.
    ui.invoke_shortcut_record("toggle_details_panel".into());
    assert_eq!(ui.get_shortcut_recording(), "toggle_details_panel");
    // A modifier on its own keeps waiting for the key.
    assert!(ui.invoke_shortcut_recorded(
        "toggle_details_panel".into(),
        slint::SharedString::from(Key::Shift),
        false,
        false,
        true,
        false
    ));
    assert_eq!(ui.get_shortcut_recording(), "toggle_details_panel");
    assert!(ui.invoke_shortcut_recorded(
        "toggle_details_panel".into(),
        "j".into(),
        true,
        false,
        true,
        false
    ));
    settle(ui).await;
    assert_eq!(
        binding("toggle_details_panel").as_deref(),
        Some("Mod+Shift+J")
    );
    assert!(ui.get_shortcut_recording().is_empty());
    assert!(ui.get_shortcut_can_undo());

    // A conflict names the other action; a reserved key is refused.
    let before = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_shortcut_typed("go_home".into(), "mod+k".into());
    settle(ui).await;
    assert!(ui.get_shortcut_message_error());
    assert!(ui.get_shortcut_message().contains("Switch room"));
    ui.invoke_shortcut_typed("go_home".into(), "Mod+C".into());
    settle(ui).await;
    assert!(ui.get_shortcut_message_error());
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);

    // Disable, Default and Undo.
    ui.invoke_shortcut_disable("open_emoji_picker".into());
    settle(ui).await;
    assert_eq!(binding("open_emoji_picker"), None);
    ui.invoke_shortcut_use_default("open_emoji_picker".into());
    settle(ui).await;
    assert_eq!(binding("open_emoji_picker").as_deref(), Some("Mod+E"));
    ui.invoke_shortcut_undo();
    settle(ui).await;
    assert_eq!(binding("open_emoji_picker"), None);
    // Undo never overwrites a later edit made elsewhere.
    ui.invoke_shortcut_use_default("open_emoji_picker".into());
    settle(ui).await;
    let mut external = store.load().unwrap();
    external.shortcuts.remove("go_home");
    store.save(&external).unwrap();
    let superseded = std::fs::read_to_string(store.path()).unwrap();
    ui.invoke_shortcut_undo();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), superseded);
    assert!(ui.get_shortcut_message_error());

    // Dispatch outside Settings; recording pauses every shortcut.
    let current = store.load().unwrap();
    apply_config(ui, &current, None);
    ui.invoke_show_thread_panel();
    ui.set_show_side_panel(false);
    settle(ui).await;
    let press = |text: &str, shift: bool| {
        ui.invoke_dispatch_shortcut(text.into(), true, false, shift, false)
    };
    ui.set_shortcut_recording("search".into());
    assert!(!press("p", true), "a shortcut ran while recording");
    ui.set_shortcut_recording("".into());
    assert!(press("p", true));
    assert!(ui.get_show_side_panel());
    assert_eq!(ui.get_side_panel_tab(), 0);
    ui.set_active_space_id("!space:example.org".into());
    ui.set_page(3);
    assert!(
        !press("1", false),
        "go_home was removed outside this window"
    );
    let mut restored = store.load().unwrap();
    restored.shortcuts.insert("go_home".into(), "Mod+1".into());
    store.save(&restored).unwrap();
    apply_config(ui, &restored, None);
    assert!(press("1", false));
    assert_eq!((ui.get_page(), ui.get_active_space_id().as_str()), (0, ""));
    let opened = Rc::new(RefCell::new(Vec::new()));
    let seen = opened.clone();
    ui.on_open_room(move |id| seen.borrow_mut().push(id.to_string()));
    let room = |id: &str| RoomItem {
        id: id.into(),
        name: id.into(),
        ..Default::default()
    };
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(vec![room("!a"), room("!b")])));
    crate::navigation::apply_room_order(ui);
    ui.set_active_room_id("!a".into());
    assert!(ui.invoke_dispatch_shortcut(
        slint::SharedString::from(Key::DownArrow),
        false,
        true,
        true,
        false
    ));
    assert_eq!(*opened.borrow(), ["!b"]);
    assert!(press("u", true));
    assert!(ui.get_account_menu_open());
    assert!(!press("p", true), "shortcuts pause while the menu is open");
    ui.set_account_menu_open(false);

    ui.on_open_room(|_| {});
    ui.set_unsorted_channels(ModelRc::new(VecModel::<RoomItem>::default()));
    crate::navigation::apply_room_order(ui);
    ui.set_active_room_id("".into());
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

/// Seen by is a real modal on the generated window: opening it hands focus to
/// its Close control, Tab and Shift+Tab stay there, shortcuts pause while it
/// is open, and Escape closes it and returns focus to the action that opened
/// it. The room-activity controller needs a signed-in account, so a stand-in
/// opens and closes the view the way the controller does.
async fn assert_seen_by_modal_guard(ui: &AppWindow) {
    use crate::RoomActivity;
    use slint::private_unstable_api::re_exports::AccessibilityAction;

    let activity = ui.global::<RoomActivity>();
    let weak = ui.as_weak();
    activity.on_show_seen_by(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            let activity = ui.global::<RoomActivity>();
            activity.set_seen_state(2);
            activity.set_seen_open(true);
        }
    });
    let weak = ui.as_weak();
    activity.on_close_seen_by(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<RoomActivity>().set_seen_open(false);
        }
    });
    let live = ui.get_live_session();
    let messages = ui.get_messages();
    ui.set_page(0);
    ui.set_live_session(true);
    let row = MessageItem {
        event_id: "$seen:example.org".into(),
        sender: "Robin".into(),
        body: "Seen-by modal check".into(),
        ..Default::default()
    };
    ui.set_messages(ModelRc::new(VecModel::from(vec![row])));
    settle(ui).await;
    let opener = "Seen by: who has read up to this message from Robin";
    let close = "Close read receipts";
    let press = |text: &str| ui.invoke_dispatch_shortcut(text.into(), true, false, true, false);
    control_with_label(ui, opener).accessible_action(&AccessibilityAction::Default);
    settle(ui).await;
    assert!(activity.get_seen_open());
    assert!(focused_control_has_label(ui, close));
    assert!(!press("d"), "a shortcut ran behind the Seen-by view");
    for tab in [Key::Tab, Key::Tab, Key::Backtab] {
        key(ui, tab);
        assert!(
            focused_control_has_label(ui, close),
            "focus left the Seen-by view"
        );
    }
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!activity.get_seen_open());
    assert!(
        focused_control_has_label(ui, opener),
        "focus did not return to the action that opened Seen by"
    );
    // Shortcuts run again once it is closed (the details panel, twice).
    assert!(press("d"));
    assert!(press("d"));
    activity.on_show_seen_by(|_, _| {});
    activity.on_close_seen_by(|| {});
    ui.set_messages(messages);
    ui.set_live_session(live);
    settle(ui).await;
}

/// Generated-window presentation only. Core admission/cancellation and sync
/// retention are exercised separately in room_leave's controller/core tests.
async fn assert_room_leave_modal_guard(ui: &AppWindow) {
    use crate::{RoomLeave, RoomLeaveView};
    use slint::private_unstable_api::re_exports::AccessibilityAction;
    use std::{cell::Cell, rc::Rc};

    let old_page = ui.get_page();
    let old_live = ui.get_live_session();
    let old_room = ui.get_active_room_id();
    let old_scale = ui.get_font_scale_percent();
    let old_size = ui.window().size();
    let closed = Rc::new(Cell::new(0));
    let observed = closed.clone();
    let leave = ui.global::<RoomLeave>();
    let weak = ui.as_weak();
    leave.on_close(move || {
        observed.set(observed.get() + 1);
        if let Some(ui) = weak.upgrade() {
            ui.global::<RoomLeave>().set_view(RoomLeaveView::default());
        }
    });
    let name = "Long synthetic room name ".repeat(5);
    let room = format!("!{}:example.org", "synthetic".repeat(24));
    let review = RoomLeaveView {
        open: true,
        stage: 1,
        title: "Leave this room?".into(),
        name: name.clone().into(),
        room_id: room.clone().into(),
        impact: "Only this exact room is left; a server response is not proof of local sync. "
            .repeat(16)
            .into(),
        ..Default::default()
    };
    let weak = ui.as_weak();
    let opened = review.clone();
    leave.on_start(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<RoomLeave>().set_view(opened.clone());
        }
    });
    ui.set_live_session(true);
    ui.set_page(0);
    ui.set_active_room_id("!leave-presentation:example.org".into());
    let cancel = format!("Cancel and stay in {name}");
    let confirm = format!("Leave {name} ({room})");
    for scale in [100, 200] {
        ui.window().set_size(slint::PhysicalSize::new(640, 620));
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        assert!(!ui.get_room_leave_blocked());
        control_with_label(ui, "Leave this room…").accessible_action(&AccessibilityAction::Default);
        settle(ui).await;
        assert!(leave.get_view().open);
        assert!(ui.get_matrix_link_blocked() && ui.get_sas_protected());
        assert!(focused_control_has_label(ui, &cancel));
        for label in [&cancel, &confirm] {
            assert!(
                inside_window(ui, item_bounds(&control_with_label(ui, label))),
                "the fixed Leave footer must fit at {scale}%"
            );
        }
        for (tab, label) in [(Key::Tab, &confirm), (Key::Backtab, &cancel)] {
            key(ui, tab);
            assert!(focused_control_has_label(ui, label));
        }
        assert!(!ui.invoke_dispatch_shortcut("d".into(), true, false, true, false));
        ui.invoke_open_account_menu();
        assert!(!ui.get_account_menu_open());
        let page = ui.get_page();
        ui.invoke_focus_settings_search();
        assert_eq!(ui.get_page(), page);
        key(ui, Key::PageDown);
        assert!(inside_window(
            ui,
            item_bounds(&control_with_label(ui, &cancel))
        ));

        let before = closed.get();
        let mut busy = review.clone();
        busy.stage = 2;
        leave.set_view(busy);
        settle(ui).await;
        assert!(focused_control_has_label(ui, "Close"));
        key(ui, Key::Escape);
        settle(ui).await;
        assert!(!leave.get_view().open);
        assert_eq!(closed.get(), before + 1, "busy Escape must withdraw");
        assert!(focused_control_has_label(ui, "Leave this room…"));
    }
    leave.on_start(|| {});
    leave.on_close(|| {});
    leave.set_view(RoomLeaveView::default());
    ui.set_active_room_id(old_room);
    ui.set_live_session(old_live);
    ui.set_page(old_page);
    ui.set_font_scale_percent(old_scale);
    ui.window().set_size(old_size);
    settle(ui).await;
}

/// Style profiles on the real generated window. Save current look stores the
/// look only. A preview changes the window and saves nothing; Cancel restores
/// the look exactly. A newer look, an account switch or a Settings save ends
/// the preview, and a save that would change the look during it is refused.
/// Apply saves the profile's look over the re-read file and keeps every other
/// setting; an invalid settings file is never overwritten; Delete asks first.
/// The Settings live preview draws synthetic samples that never take focus
/// or pointer input. Its advanced-editor links only reveal their editor, and
/// hiding it is session state.
async fn assert_live_preview(ui: &AppWindow) {
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_font_scale_percent(100);
    ui.set_settings_search_query("".into());
    ui.invoke_settings_search("".into());
    ui.set_page(1);
    settle(ui).await;
    assert!(
        ui.get_appearance_preview_shown(),
        "the preview starts shown"
    );
    // Samples sit between the header actions and the links in the tree; Tab
    // skips every one of them.
    tab_to_control(ui, "Hide the live preview");
    key(ui, Key::Tab);
    assert!(focused_control_has_label(
        ui,
        "Keep a compact preview above the settings while editing"
    ));
    key(ui, Key::Tab);
    assert!(
        focused_control_has_label(ui, "Go to the color editor"),
        "a preview sample took keyboard focus"
    );
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(ui.get_settings_revealed_id(), "appearance.colors");
    ui.set_page(0);
    settle(ui).await;
    ui.set_page(1);
    settle(ui).await;
    tab_to_control(ui, "Hide the live preview");
    key(ui, Key::Return);
    settle(ui).await;
    assert!(!ui.get_appearance_preview_shown());
    assert!(
        focused_control_has_label(ui, "Show the live preview"),
        "the toggle keeps focus"
    );
    key(ui, Key::Return);
    settle(ui).await;
    assert!(ui.get_appearance_preview_shown());
    // Pinning keeps a compact copy above the settings until its own Unpin.
    assert!(!ui.get_appearance_preview_pinned(), "pinning is opt-in");
    tab_to_control(
        ui,
        "Keep a compact preview above the settings while editing",
    );
    key(ui, Key::Return);
    settle(ui).await;
    assert!(ui.get_appearance_preview_pinned());
    assert!(focused_control_has_label(
        ui,
        "Stop keeping the preview above the settings"
    ));
    click_control_once(ui, "Unpin the compact preview");
    settle(ui).await;
    assert!(!ui.get_appearance_preview_pinned());

    // At the smallest window and 200% text the card keeps a bounded, stable
    // height (its samples scroll inside it), so it never fills the page.
    ui.window().set_size(slint::PhysicalSize::new(940, 620));
    for (scale, expected) in [(100, 340.0f32), (200, 408.0)] {
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        let card = control_with_label(ui, "Live preview with sample content");
        let height = card.geometry().size.height;
        assert!(
            (height - expected).abs() < 1.0,
            "{scale}%: the preview card is {height} px, not {expected}"
        );
    }
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_page(0);
    settle(ui).await;
}

/// The themed select keeps ComboBox's keyboard contract: Up and Down choose
/// directly, Return opens the list without choosing, keys in the list choose
/// live, Escape closes it with focus back on the select, and a disabled
/// select ignores keys.
async fn assert_themed_select(ui: &AppWindow) {
    use std::{cell::RefCell, rc::Rc};

    let chosen = Rc::new(RefCell::new(Vec::<String>::new()));
    let record = chosen.clone();
    ui.on_select_audio_microphone(move |value| record.borrow_mut().push(value.into()));
    // Choosing saves for new calls only; a call in progress switches on its
    // explicit Use for this call button, never on an arrow or wheel step.
    let switched = Rc::new(std::cell::Cell::new(0));
    let switches = switched.clone();
    ui.on_apply_audio_route_to_call(move || switches.set(switches.get() + 1));
    let live = ui.get_live_session();
    ui.set_live_session(true);
    ui.set_native_call_joined(true);
    let choices: Vec<slint::SharedString> = vec![
        "System default".into(),
        "Desk microphone".into(),
        "Headset".into(),
    ];
    ui.set_audio_microphone_choices(ModelRc::new(VecModel::from(choices)));
    ui.set_audio_microphone_index(0);
    ui.set_audio_devices_enabled(true);
    ui.set_audio_devices_busy(false);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_page(1);
    settle(ui).await;
    assert!(crate::settings_search::reveal(ui, "media.audio_devices"));
    settle(ui).await;
    let label = "Microphone for calls";
    assert!(focused_control_has_label(ui, label));
    assert_eq!(
        button_bounds(ui, "Use for this call").len(),
        1,
        "offered in a call"
    );

    key(ui, Key::DownArrow);
    assert_eq!(ui.get_audio_microphone_index(), 1);
    key(ui, Key::UpArrow);
    assert_eq!(ui.get_audio_microphone_index(), 0);
    assert_eq!(*chosen.borrow(), ["Desk microphone", "System default"]);

    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(chosen.borrow().len(), 2, "opening chose nothing");
    key(ui, Key::End);
    settle(ui).await;
    assert_eq!(ui.get_audio_microphone_index(), 2);
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(focused_control_has_label(ui, label), "focus returns");
    let last = chosen.borrow().last().cloned();
    assert_eq!(last.as_deref(), Some("Headset"));

    // The wheel moves the choice while the select has focus.
    let select = control_with_label(ui, label);
    let center = select.map_to_window(select.geometry().center());
    let position = slint::LogicalPosition::new(center.x, center.y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y: 10.0,
    });
    settle(ui).await;
    assert_eq!(ui.get_audio_microphone_index(), 1, "wheel up");

    // Long externally supplied labels stay within the settings panel rather
    // than increasing its minimum width. The visible caption may elide.
    ui.set_audio_microphone_choices(ModelRc::new(VecModel::from(vec![
        "System default".into(),
        slint::SharedString::from("Long device label ".repeat(128)),
        "Headset".into(),
    ])));
    let previous_scale = ui.get_font_scale_percent();
    ui.set_font_scale_percent(200);
    settle(ui).await;
    let bounds = control_bounds(ui, label);
    assert_eq!(bounds.len(), 1);
    let logical_width = ui.window().size().width as f32 / ui.window().scale_factor();
    assert!(bounds[0].left >= -0.5 && bounds[0].right <= logical_width + 0.5);
    ui.set_font_scale_percent(previous_scale);
    settle(ui).await;

    ui.set_audio_devices_busy(true);
    settle(ui).await;
    key(ui, Key::UpArrow);
    assert_eq!(ui.get_audio_microphone_index(), 1, "disabled");
    assert_eq!(switched.get(), 0, "choosing never switches the call");

    ui.set_audio_devices_busy(false);
    ui.set_native_call_joined(false);
    settle(ui).await;
    assert!(
        button_bounds(ui, "Use for this call").is_empty(),
        "no call, no button"
    );
    ui.set_audio_devices_enabled(false);
    ui.set_live_session(live);
    ui.on_apply_audio_route_to_call(|| {});
    ui.on_select_audio_microphone(|_| {});
    ui.set_page(0);
    settle(ui).await;
}

async fn assert_style_profiles(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::style_profiles::ProfileStore;
    use morpheus_config::{ConfigStore, LoadedConfig};

    let store = ConfigStore::at(directory.join("style-check").join("config.toml"));
    let mut config = store.load_or_create().unwrap();
    // Explicit synthetic opt-in: production message previews default off.
    // The later live-config regression must exercise a true -> false change.
    config.notifications.show_message_preview = true;
    config.notifications.highlight_words = vec!["private-word".into()];
    config.typography.message_size = 15;
    store.save(&config).unwrap();
    apply_config(ui, &config, None);
    crate::style_profiles::register(ui, &store);
    // Search must focus the real filter even when Settings does not exist
    // until this request. Revealing a section must not create or save a look.
    ui.set_page(0);
    ui.set_settings_revealed_id("".into());
    settle(ui).await;
    let before_reveal = std::fs::read(store.path()).unwrap();
    assert!(crate::settings_search::reveal(
        ui,
        "appearance.style_profiles"
    ));
    settle(ui).await;
    assert_eq!(ui.get_settings_revealed_id(), "appearance.style_profiles");
    assert!(focused_control_has_label(
        ui,
        "Filter style profiles by name or look"
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), before_reveal);
    let profiles = ProfileStore::for_config(&store).unwrap();
    assert!(profiles.list().unwrap().entries.is_empty());

    // Save current look: the look only, sparse, never other settings.
    ui.set_style_profile_name_input("Reading room".into());
    ui.invoke_style_profile_save_current();
    settle(ui).await;
    assert_eq!(profiles.list().unwrap().entries.len(), 1);
    let raw = std::fs::read_to_string(profiles.dir().join("reading-room.toml")).unwrap();
    assert!(raw.contains("message_size = 15"), "{raw}");
    for absent in [
        "private-word",
        "highlight",
        "notifications",
        "shortcuts",
        "scale_percent",
    ] {
        assert!(!raw.contains(absent), "{absent} leaked into a profile");
    }
    let items = ui.get_style_profile_items();
    assert_eq!(items.row_count(), 1);
    let item = items.row_data(0).unwrap();
    assert!(item.valid && item.current);
    assert!(!item.key.contains("reading"), "list keys are opaque");
    // The same name asks before replacing.
    ui.invoke_style_profile_save_current();
    settle(ui).await;
    assert_eq!(ui.get_style_profile_state().confirm, 2);
    ui.invoke_style_profile_dismiss();
    settle(ui).await;
    assert_eq!(profiles.list().unwrap().entries.len(), 1);

    // A new accepted look, then a preview of the saved one.
    let mut larger = store.load().unwrap();
    larger.typography.message_size = 20;
    store.save(&larger).unwrap();
    apply_config(ui, &larger, None);
    settle(ui).await;
    assert!(!ui.get_style_profile_items().row_data(0).unwrap().current);
    ui.invoke_style_profile_chosen(item.key.clone());
    let preview = |ui: &AppWindow| {
        ui.invoke_style_profile_preview();
        assert!(ui.get_style_preview_active());
        assert_eq!(ui.get_style_preview_name(), "Reading room");
    };
    preview(ui);
    settle(ui).await;
    assert_eq!(ui.get_message_font_size(), 15);
    assert_eq!(store.load().unwrap(), larger, "a preview saves nothing");
    ui.invoke_style_profile_cancel_preview();
    settle(ui).await;
    assert!(!ui.get_style_preview_active());
    assert_eq!(ui.get_message_font_size(), 20);

    // A save that would change the look is refused and ends the preview.
    preview(ui);
    let before = std::fs::read_to_string(store.path()).unwrap();
    let refused = crate::save_config_edit(ui, &store, |config| {
        config.typography.message_size = 18;
        true
    });
    assert!(refused.is_none());
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);
    assert!(!ui.get_style_preview_active());
    assert!(ui.get_style_profile_message_error());
    assert_eq!(ui.get_message_font_size(), 20);
    // The outcome is also announced on every page until dismissed.
    assert!(ui.get_style_profile_notice_error());
    assert!(!ui.get_style_profile_notice().is_empty());
    ui.invoke_style_profile_notice_dismiss();
    assert!(ui.get_style_profile_notice().is_empty());
    // A save that keeps the look ends the preview and is saved.
    preview(ui);
    let kept = crate::save_config_edit(ui, &store, |config| {
        config.notifications.sounds = false;
        true
    });
    assert!(kept.is_some());
    assert!(!ui.get_style_preview_active());
    assert!(!store.load().unwrap().notifications.sounds);
    assert_eq!(ui.get_message_font_size(), 20);

    // Re-reading the same look keeps the preview and shows its other
    // settings at once; a different look ends it.
    preview(ui);
    let mut same = LoadedConfig {
        config: store.load().unwrap(),
        theme: None,
    };
    same.config.appearance.scale_percent = 150;
    // A live preview must not hold back an external privacy change. These
    // settings are deliberately excluded from profile imports and rollback.
    assert!(ui.get_notifications_enabled());
    assert!(ui.get_notification_show_preview());
    same.config.notifications.enabled = false;
    same.config.notifications.show_message_preview = false;
    crate::apply_loaded_config(ui, &same);
    assert!(!ui.get_notifications_enabled());
    assert!(!ui.get_notification_show_preview());
    assert!(ui.get_style_preview_active());
    assert_eq!(ui.get_message_font_size(), 15);
    assert_eq!(ui.get_font_scale_percent(), 150);
    ui.invoke_style_profile_cancel_preview();
    assert!(!ui.get_style_preview_active());
    assert!(!ui.get_notifications_enabled());
    assert!(!ui.get_notification_show_preview());
    preview(ui);
    let mut external = store.load().unwrap();
    external.typography.message_size = 22;
    store.save(&external).unwrap();
    crate::apply_loaded_config(
        ui,
        &LoadedConfig {
            config: external.clone(),
            theme: None,
        },
    );
    assert!(!ui.get_style_preview_active());
    assert_eq!(ui.get_message_font_size(), 22);

    // An account switch cancels a preview started for another account.
    let account = ui.get_active_account_local_id();
    preview(ui);
    ui.set_active_account_local_id("another-account".into());
    crate::appearance::apply_theme_mode(ui);
    assert!(!ui.get_style_preview_active());
    assert_eq!(ui.get_message_font_size(), 22);
    assert!(ui.get_style_profile_notice().contains("Switching accounts"));
    ui.set_active_account_local_id(account.clone());
    crate::appearance::apply_theme_mode(ui);

    // Apply from the bar saves exactly the previewed look, even when the
    // profile's file changed meanwhile, and keeps other settings.
    preview(ui);
    let previewed = profiles.list().unwrap().entries.remove(0);
    let original = previewed.profile.unwrap();
    let mut changed = original.clone();
    changed.typography.message_size = 19;
    profiles.replace(&previewed.file, &changed).unwrap();
    ui.invoke_style_preview_apply();
    settle(ui).await;
    profiles.replace(&previewed.file, &original).unwrap();
    ui.invoke_style_profiles_refresh();
    let applied = store.load().unwrap();
    assert_eq!(applied.typography.message_size, 15);
    assert_eq!(applied.notifications.highlight_words, ["private-word"]);
    assert!(!applied.notifications.sounds);
    assert!(!ui.get_style_preview_active());
    assert_eq!(ui.get_message_font_size(), 15);
    assert!(ui.get_style_profile_items().row_data(0).unwrap().current);

    // An invalid settings file is never overwritten by Apply.
    let broken = "schema_version = 1\n[typography]\nmessage_size = 999\n";
    std::fs::write(store.path(), broken).unwrap();
    ui.invoke_style_profile_apply();
    settle(ui).await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), broken);
    assert!(ui.get_style_profile_message_error());
    store.save(&applied).unwrap();
    ui.set_config_error("".into());

    // Delete asks first and removes only the profile file.
    ui.invoke_style_profile_delete();
    settle(ui).await;
    assert_eq!(ui.get_style_profile_state().confirm, 1);
    assert_eq!(profiles.list().unwrap().entries.len(), 1);
    ui.invoke_style_profile_confirm();
    settle(ui).await;
    assert!(profiles.list().unwrap().entries.is_empty());
    assert_eq!(store.load().unwrap(), applied);
    assert_eq!(ui.get_style_profile_items().row_count(), 0);

    ui.set_page(0);
    apply_config(ui, &Config::default(), None);
    settle(ui).await;
}

// Uses the existing generated-window event loop and actual registered close/
// context callbacks. No sign-in, SDK request or credential vault is opened.
async fn assert_room_addresses_bounded_close_and_context(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    runtime: &Handle,
) {
    use crate::{RoomAddressEntry, RoomAddresses};
    use slint::SharedString;
    crate::room_addresses::register(ui, live, runtime.clone());
    let old_page = ui.get_page();
    let old_live = ui.get_live_session();
    let old_scale = ui.get_font_scale_percent();
    let old_size = ui.window().size();
    ui.set_show_sign_in(false);
    ui.set_page(0);
    for scale in [100, 200] {
        ui.window().set_size(slint::PhysicalSize::new(640, 620));
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        let state = ui.global::<RoomAddresses>();
        state.set_room_name("A long synthetic room name for address review".into());
        state.set_own_server("long-synthetic-homeserver.example.org".into());
        state.set_new_local("synthetic-address".into());
        state.set_summary("A bounded list with a long homeserver and more addresses than can be shown. The homeserver did not list its addresses; only published addresses are known. ".repeat(3).into());
        state.set_error(
            "Synthetic fixed error shown with the observed state. "
                .repeat(4)
                .into(),
        );
        state.set_notice(
            "Reading current state is not proof that an earlier unanswered write finished. "
                .repeat(4)
                .into(),
        );
        state.set_can_publish(true);
        state.set_publish_editable(true);
        state.set_rows(ModelRc::new(VecModel::from(vec![RoomAddressEntry {
            alias: "#synthetic:example.org".into(),
            shown: "#synthetic:example.org".into(),
            detail: "A long synthetic published address detail".into(),
            local: true,
            listed: true,
            main: false,
            alternative: false,
        }])));
        state.set_open(true);
        settle(ui).await;
        let whole = |label: &str| {
            let size = ui.window().size().to_logical(ui.window().scale_factor());
            let bounds = button_bounds(ui, label);
            assert_eq!(bounds.len(), 1);
            let b = bounds[0];
            assert!(
                b.left >= 0.0
                    && b.top >= 0.0
                    && b.right <= size.width + 0.5
                    && b.bottom <= size.height + 0.5
                    && b.right > b.left
                    && b.bottom > b.top,
                "address fixed action must remain wholly inside its generated window"
            );
        };
        assert!(
            ui.get_sas_protected(),
            "the address panel protects verification"
        );
        whole("Close the addresses of this room");
        whole("Done with the addresses of this room");
        whole("Read this room's addresses again");
        tab_to_control(ui, "Review adding this address on your homeserver");
        settle(ui).await;
        whole("Review adding this address on your homeserver");
        assert!(focused_control_has_label(
            ui,
            "Review adding this address on your homeserver"
        ));
        state.set_review_title(
            "Review a long exact synthetic address before making the change".into(),
        );
        state.set_review_lines(ModelRc::new(VecModel::from(vec![
            SharedString::from("A long synthetic review explains an exact room and alias without claiming a compare-and-swap or rollback. ".repeat(14)),
        ])));
        state.set_confirm_label("Save published addresses".into());
        state.set_reviewing(true);
        settle(ui).await;
        whole("Cancel this change");
        whole(
            "Save published addresses: Review a long exact synthetic address before making the change",
        );
        key(ui, Key::End);
        settle(ui).await;
        whole("Cancel this change");
        whole("Done with the addresses of this room");
        state.set_busy(true);
        settle(ui).await;
        whole("Close and withdraw this address change");
        // This is the actual Escape path while busy, not callback invocation.
        key(ui, Key::Escape);
        settle(ui).await;
        assert!(!state.get_open() && !state.get_busy() && !state.get_reviewing());
        state.set_open(true);
        state.set_busy(true);
        settle(ui).await;
        // A protected presentation cancels before the address view hides;
        // context clearing must not increment focus-return to its old opener.
        let closed = state.get_closed();
        ui.set_access_open(true);
        settle(ui).await;
        assert!(!state.get_open() && !state.get_busy());
        assert_eq!(state.get_closed(), closed);
        ui.set_access_open(false);
        settle(ui).await;
    }
    crate::room_addresses::clear(ui);
    ui.set_font_scale_percent(old_scale);
    ui.set_live_session(old_live);
    ui.set_page(old_page);
    ui.window().set_size(old_size);
    settle(ui).await;
}

async fn exercise_account_ui(runtime: Handle, phase: std::rc::Rc<AccountUiProgress>) {
    #[cfg(not(morpheus_timeline_acceptance))]
    exercise_startup_recovery();
    let directory = tempfile::tempdir().unwrap();
    let manager =
        Arc::new(morpheus_matrix::AccountManager::open(directory.path().join("profile")).unwrap());
    let ui = AppWindow::new().unwrap();
    assert_live_font_projections(&ui);
    ui.set_show_sign_in(false);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.show().unwrap();
    ui.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    phase.set(1);
    assert_sign_in_keyboard(&ui).await;
    settle(&ui).await;
    phase.set(2);
    assert_navigation_projection_and_focus(&ui).await;
    assert_header_menu_capture_and_detach(&ui).await;
    phase.set(3);
    assert_accessible_action_focus_and_first_click(&ui).await;
    phase.set(4);
    assert_native_motion_and_preference(&ui, &directory.path().join("appearance.toml")).await;
    phase.set(5);
    assert_lazy_timeline_view(&ui).await;
    phase.set(6);
    assert_palette_and_density_keyboard(&ui).await;
    phase.set(7);
    assert_scale_keyboard_and_focus_bounds(&ui).await;
    phase.set(8);
    assert_message_role_precedence_and_thread_metadata(&ui).await;
    phase.set(9);
    assert_appearance_workspace_and_search(&ui, directory.path()).await;
    phase.set(10);
    assert_metric_workspace_and_extremes(&ui, directory.path()).await;
    phase.set(11);
    assert_interaction_states_and_sender_palette(&ui, directory.path()).await;
    phase.set(12);
    assert_pane_composition_and_motion(&ui).await;
    phase.set(13);
    assert_account_scoped_appearance(&ui, directory.path()).await;
    phase.set(14);
    assert_shortcut_customization(&ui, directory.path()).await;
    phase.set(30);
    assert_seen_by_modal_guard(&ui).await;
    phase.set(88);
    assert_room_leave_modal_guard(&ui).await;
    phase.set(18);
    assert_style_profiles(&ui, directory.path()).await;
    phase.set(19);
    assert_live_preview(&ui).await;
    phase.set(20);
    assert_themed_select(&ui).await;
    phase.set(26);
    assert_layout_navigation_recovery(&ui, directory.path()).await;
    phase.set(27);
    assert_vector_icons(&ui).await;
    phase.set(28);
    assert_public_profile_projection(&ui).await;
    phase.set(21);
    assert_room_notification_panel(&ui).await;
    // The whole invitations scenario: the center's gating, then the Home
    // cards' outcome ownership (INVDA02-HOME).
    phase.set(90);
    assert_invitations_center_gating(&ui).await;
    crate::invitations::tests::home_outcomes_belong_to_their_account(&ui);
    phase.set(50);
    assert_theme_studio_draft(&ui, directory.path()).await;
    assert_behavior_saves_with_broken_theme(&ui, directory.path()).await;
    phase.set(51);
    assert_theme_studio_publish_race(&ui, directory.path()).await;
    phase.set(52);
    assert_theme_pack_keeps_installed_file(&ui, directory.path()).await;
    phase.set(53);
    assert_pack_scope_fences(&ui, directory.path()).await;
    phase.set(54);
    assert_style_profile_scope_fences(&ui, directory.path()).await;
    phase.set(55);
    assert_room_appearance_scope(&ui, directory.path()).await;
    phase.set(56);
    assert_font_browser(&ui, directory.path()).await;
    phase.set(57);
    assert_people_window_privacy(&ui).await;
    phase.set(72);
    assert_member_row_actions_fit(&ui).await;
    phase.set(75);
    assert_sas_values_leave_on_close_and_protection(&ui).await;
    #[cfg(feature = "native-calls")]
    {
        phase.set(85);
        assert_call_bar(&ui, &runtime).await;
    }
    phase.set(15);
    let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
        directory.path().join("config.toml"),
    )));
    let live = LiveUi::new(manager.clone(), runtime.clone(), ui.as_weak(), voices);
    phase.set(80);
    assert_room_addresses_bounded_close_and_context(&ui, live.clone(), &runtime).await;
    phase.set(15);
    assert_pane_navigation_epoch(&ui, &live, directory.path()).await;
    phase.set(73);
    assert_moderation_stops_with_its_context(&ui, &live, &runtime).await;
    phase.set(74);
    assert_room_media_scope_and_privacy(&ui, &live, &runtime).await;
    phase.set(86);
    assert_identity_warnings_presentation(&ui, &live, &runtime).await;
    phase.set(76);
    assert_discovery_join_withdrawal(&ui, &live, &runtime).await;
    phase.set(82);
    assert_room_permissions_editor(&ui, &live, &runtime).await;
    phase.set(83);
    crate::matrix_links::tests::assert_link_card(&ui, &live, &runtime).await;
    // The invitations case again, now that the link controller is registered.
    phase.set(90);
    assert_invitations_and_links_exclude_each_other(&ui, &live, &runtime).await;
    phase.set(89);
    crate::thread_browser::register(&ui, live.clone(), runtime.clone());
    crate::thread_browser::tests::assert_thread_browser_panel(&ui, &live, &runtime).await;
    phase.set(92);
    assert_responsive_header(&ui).await;
    #[cfg(feature = "native-calls")]
    crate::incoming_ring::ui::assert_answered_call_withdraws_leave(&ui).await;
    #[cfg(feature = "native-calls")]
    crate::voice_rosters::assert_unowned_presentation_is_cleared(&ui, live.clone(), &runtime);
    phase.set(91);
    crate::room_creation::tests::assert_creation_protected_entries(&ui).await;
    // This suite tests the actual dialog/controller boundary, not credentials.
    // A denied account-change guard also prevents any mistaken key sequence
    // from starting policy writes, account activation, or local deletion.
    let access = AccountAccessUi::register(&ui, live.clone(), runtime.clone(), || false);
    phase.set(16);
    let pinned = crate::pinned_messages::PinnedMessagesUi::register(&ui, live.clone(), &runtime);
    assert_switch_readiness(&ui, &live, &access).await;
    phase.set(17);
    ui.set_composer_text("untouched conversation draft".into());
    settle(&ui).await;

    // Exercise the unlock presentation without a core authorization challenge.
    // Keyboard effects prove focus placement without debug-only element IDs or
    // a test-only focused-property exposed by the production component.
    access.open(
        Action::Unlock("presentation-only".into()),
        None,
        "@casey:example.org".into(),
    );
    settle(&ui).await;
    assert!(ui.get_access_open());
    assert_eq!(ui.get_access_mode(), 0);
    key(&ui, Key::Tab); // Cancel -> Unlock
    key(&ui, Key::Tab); // Unlock -> current password (modal wrap)
    type_text(&ui, "cedar");
    assert_eq!(ui.get_access_current_password().as_str(), "cedar");
    // Reloading navigation must also leave a focused password field in place.
    let mut sidebar = Config::default();
    sidebar.layout.navigation = NavigationMode::Sidebar;
    apply_config(&ui, &sidebar, None);
    settle(&ui).await;
    type_text(&ui, "x");
    assert_eq!(ui.get_access_current_password().as_str(), "cedarx");
    key(&ui, Key::Backspace);
    apply_config(&ui, &Config::default(), None);
    settle(&ui).await;
    type_text(&ui, "y");
    assert_eq!(ui.get_access_current_password().as_str(), "cedary");
    key(&ui, Key::Backspace);
    assert!(ui.get_access_new_password().is_empty());
    key(&ui, Key::Backtab); // first input -> Unlock
    key(&ui, Key::Tab); // Unlock -> first input again
    ui.set_access_current_password("".into());
    type_text(&ui, "birch");
    assert_eq!(ui.get_access_current_password().as_str(), "birch");
    assert_eq!(
        ui.get_composer_text().as_str(),
        "untouched conversation draft"
    );
    // Cancelling an unlock remains available while the prompt shows work.
    ui.set_access_busy(true);
    key(&ui, Key::Escape);
    assert!(!ui.get_access_open());
    assert_passwords_empty(&ui);
    assert!(access.pending.lock().unwrap().is_none());
    key(&ui, Key::Return); // restored account trigger
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);

    // Click an unfocused real action once. Account controls use a full-size
    // forwarded focus rectangle so ScrollView can reveal the whole button;
    // that rectangle must not consume the press before the existing TouchArea.
    ui.set_manage_accounts_open(true);
    settle(&ui).await;
    key(&ui, Key::Tab); // Done -> Add, so Done is not already focused
    let logical_size = ui.window().size().to_logical(ui.window().scale_factor());
    let text_scale = ui.get_font_scale_percent() as f32 / 100.0;
    let dialog_height = (680.0 * text_scale).min(logical_size.height - 32.0);
    // Center of the fixed Done footer: centered dialog, 20px outer padding,
    // 40px scaled action height. No font metrics or caption width assumptions.
    let position = slint::LogicalPosition::new(
        logical_size.width / 2.0,
        (logical_size.height + dialog_height) / 2.0 - 20.0 - 20.0 * text_scale,
    );
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    settle(&ui).await;
    assert!(
        !ui.get_manage_accounts_open(),
        "one click must activate Done"
    );
    assert!(!ui.get_account_menu_open());
    assert!(!ui.get_access_open());

    // The real registered submit callback clears every password property even
    // when confirmation is invalid, before it can start any asynchronous work.
    ui.set_manage_accounts_open(true);
    settle(&ui).await;
    ui.invoke_open_app_password(1);
    settle(&ui).await;
    key(&ui, Key::Tab); // Cancel -> Save
    key(&ui, Key::Tab); // Save -> new password
    type_text(&ui, "different first value");
    key(&ui, Key::Tab); // new -> confirmation
    type_text(&ui, "different second value");
    assert_eq!(
        ui.get_access_new_password().as_str(),
        "different first value"
    );
    assert_eq!(
        ui.get_access_confirm_password().as_str(),
        "different second value"
    );
    ui.set_access_current_password("stale property must also clear".into());
    ui.invoke_submit_account_access();
    assert_passwords_empty(&ui);
    assert!(!ui.get_access_error().is_empty());
    assert!(ui.get_access_open());
    assert!(!access.busy.load(Ordering::Acquire));
    // Policy/removal prompts block Escape while a mutation is presented busy.
    ui.set_access_busy(true);
    key(&ui, Key::Escape);
    assert!(ui.get_access_open());
    key(&ui, Key::Tab);
    type_text(&ui, "must not reach a disabled input");
    assert_passwords_empty(&ui);
    ui.set_access_busy(false);
    key(&ui, Key::Escape);
    assert!(!ui.get_access_open());
    key(&ui, Key::Return); // restored Manage accounts Done button
    assert!(!ui.get_manage_accounts_open());
    key(&ui, Key::Return); // closing Manage accounts restores account trigger
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);

    // Logout must enter the Rust callback even with a connected call. Rust
    // owns the captured selection and can preserve it through confirmation;
    // Slint must not bypass capture by opening the numeric -3 dialog directly.
    let logout_clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let clicks = logout_clicks.clone();
    ui.on_log_out(move || clicks.set(clicks.get() + 1));
    ui.set_account_options(ModelRc::new(VecModel::default()));
    ui.set_call_connected(true);
    ui.set_account_menu_open(true);
    settle(&ui).await;
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
    key(&ui, Key::Tab); // Add account -> Manage accounts
    key(&ui, Key::Tab); // Manage accounts -> Log out
    key(&ui, Key::Return);
    assert_eq!(logout_clicks.get(), 1);
    assert!(!ui.get_leave_call_dialog()); // the Rust stub intentionally does not open it
    assert!(!ui.get_account_menu_open());
    ui.set_call_connected(false);

    // Every dismissal path drops the captured logout continuation. Exercise
    // the actual Escape route and the focused Cancel button.
    let dismissed = std::rc::Rc::new(std::cell::Cell::new(0));
    let count = dismissed.clone();
    ui.on_cancel_pending_log_out(move || count.set(count.get() + 1));
    for escape in [true, false] {
        ui.set_pending_account_index(-3);
        ui.set_leave_call_busy(false);
        ui.set_leave_call_dialog(true);
        settle(&ui).await;
        key(&ui, if escape { Key::Escape } else { Key::Return });
        assert!(!ui.get_leave_call_dialog());
        assert_eq!(ui.get_pending_account_index(), -1);
    }
    assert_eq!(dismissed.get(), 2);

    // Slint can retain focus when a custom action becomes disabled. Enter,
    // Space and Escape must not activate or cancel the in-flight operation.
    let confirmations = std::rc::Rc::new(std::cell::Cell::new(0));
    let count = confirmations.clone();
    ui.on_confirm_leave_call(move |_| count.set(count.get() + 1));
    for confirm_focus in [false, true] {
        ui.set_pending_account_index(-3);
        ui.set_leave_call_busy(false);
        ui.set_leave_call_dialog(true);
        settle(&ui).await;
        if confirm_focus {
            key(&ui, Key::Tab);
        }
        ui.set_leave_call_busy(true);
        let before = dismissed.get();
        key(&ui, Key::Return);
        key(&ui, Key::Space);
        key(&ui, Key::Escape);
        assert!(ui.get_leave_call_dialog());
        assert_eq!(ui.get_pending_account_index(), -3);
        assert_eq!(dismissed.get(), before);
        assert_eq!(confirmations.get(), 0);
        ui.set_leave_call_busy(false);
        key(&ui, Key::Escape);
        assert!(!ui.get_leave_call_dialog());
        assert_eq!(dismissed.get(), before + 1);
    }

    // Direct removal uses the saved row's identity in the confirmation. Tab
    // wraps between its two actions; Enter on Cancel never submits deletion.
    ui.set_saved_accounts(ModelRc::new(VecModel::from(vec![SavedAccountItem {
        id: "presentation-only".into(),
        handle: "@jordan:example.org".into(),
        active: false,
        removal_pending: true,
        ..Default::default()
    }])));
    ui.invoke_remove_saved_account("presentation-only".into());
    settle(&ui).await;
    assert_eq!(ui.get_access_mode(), 4);
    assert_eq!(ui.get_access_target().as_str(), "@jordan:example.org");
    assert_passwords_empty(&ui);
    key(&ui, Key::Tab); // Cancel -> Remove
    key(&ui, Key::Tab); // Remove -> Cancel
    key(&ui, Key::Return);
    assert!(!ui.get_access_open());
    assert!(access.pending.lock().unwrap().is_none());
    assert!(!access.busy.load(Ordering::Acquire));
    assert_eq!(ui.get_saved_accounts().row_count(), 1);
    assert!(manager.accounts().await.is_empty());
    key(&ui, Key::Return);
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);

    // Sign-in covers the normal sidebar account control. Dismissing an account
    // prompt must instead focus its visible saved-account shortcut. Enter must
    // open Manage accounts, never the obscured account menu beneath sign-in.
    ui.set_show_sign_in(true);
    settle(&ui).await;
    ui.invoke_remove_saved_account("presentation-only".into());
    settle(&ui).await;
    key(&ui, Key::Escape);
    assert!(!ui.get_access_open());
    key(&ui, Key::Return);
    assert!(ui.get_manage_accounts_open());
    assert!(!ui.get_account_menu_open());
    // The real Manage callback refreshes this empty core profile. Wait until
    // its UI result arrives: the synthetic row above was presentation only.
    tokio::time::timeout(Duration::from_secs(2), async {
        while ui.get_saved_accounts().row_count() != 0 {
            settle(&ui).await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the empty profile must refresh the saved-account rows");
    ui.set_login_homeserver("".into());
    key(&ui, Key::Return); // Manage accounts Done -> visible homeserver input
    assert!(!ui.get_manage_accounts_open());
    type_text(&ui, "https://matrix.example.org");
    assert_eq!(
        ui.get_login_homeserver().as_str(),
        "https://matrix.example.org"
    );
    assert!(!ui.get_account_menu_open());
    // Direct prompt cancellation takes the same no-saved-account fallback.
    ui.invoke_open_app_password(1);
    settle(&ui).await;
    key(&ui, Key::Escape);
    ui.set_login_homeserver("".into());
    type_text(&ui, "https://other.example.org");
    assert_eq!(
        ui.get_login_homeserver().as_str(),
        "https://other.example.org"
    );
    assert!(!ui.get_account_menu_open());

    // If a final removal refresh arrives after dialog dismissal, the saved
    // shortcut itself vanishes. Focus must move onto the surviving sign-in
    // input rather than retain a deleted conditional item.
    ui.set_saved_accounts(ModelRc::new(VecModel::from(vec![SavedAccountItem {
        id: "presentation-only".into(),
        handle: "@jordan:example.org".into(),
        active: false,
        removal_pending: false,
        ..Default::default()
    }])));
    ui.set_manage_accounts_open(true);
    settle(&ui).await;
    key(&ui, Key::Return); // Done -> saved-account shortcut
    assert!(!ui.get_manage_accounts_open());
    ui.set_saved_accounts(ModelRc::new(VecModel::<SavedAccountItem>::default()));
    settle(&ui).await;
    ui.set_login_homeserver("".into());
    type_text(&ui, "https://after-removal.example.org");
    assert_eq!(
        ui.get_login_homeserver().as_str(),
        "https://after-removal.example.org"
    );
    assert!(!ui.get_account_menu_open());

    assert_account_theme_follows_accepted_snapshot(&ui);

    // Exercise the real viewer's modal tab ring using only one synthetic
    // pixel. Do not activate file pickers, navigation or network callbacks.
    ui.set_show_sign_in(false);
    ui.set_active_account_local_id("viewer-fixture".into());
    ui.set_active_room_id("!viewer:example.org".into());
    ui.set_image_preview_account_id("viewer-fixture".into());
    ui.set_image_preview_room_id("!viewer:example.org".into());
    ui.set_image_preview_title("synthetic viewer".into());
    ui.set_image_preview_event_id("$viewer:example.org".into());
    ui.set_image_preview_has_previous(true);
    ui.set_image_preview_has_next(true);
    ui.set_image_preview_loaded(true);
    ui.set_image_preview_image(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
        slint::Rgba8Pixel,
    >::new(1, 1)));
    ui.set_image_preview_open(true);
    settle(&ui).await;
    assert!(focused_control_has_label(&ui, "Close image preview"));
    for label in [
        "Save synthetic viewer to a location you choose",
        "Previous image",
        "Next image",
        "Zoom in",
    ] {
        key(&ui, Key::Tab);
        assert!(focused_control_has_label(&ui, label), "viewer: {label}");
    }
    key(&ui, Key::Return); // Local zoom only: enables Out and Fit.
    key(&ui, Key::Backtab);
    assert!(focused_control_has_label(&ui, "Fit image to the window"));
    key(&ui, Key::Backtab);
    assert!(focused_control_has_label(&ui, "Zoom out"));
    key(&ui, Key::Tab);
    assert!(focused_control_has_label(&ui, "Fit image to the window"));
    key(&ui, Key::Tab);
    assert!(focused_control_has_label(&ui, "Zoom in"));
    key(&ui, Key::Tab);
    assert!(focused_control_has_label(&ui, "Close image preview"));
    // Actions are sized from their own caption: the built-in 30 px at the
    // defaults, and at 200% text with the largest icon size (glyph captions
    // scale by both) every action still holds its caption and stays inside
    // the window.
    let close = control_bounds(&ui, "Close image preview");
    assert_eq!(close.len(), 1);
    let height = close[0].bottom - close[0].top;
    assert!((height - 30.0).abs() < 0.5, "default Close is {height} px");
    let tokens = ui.global::<crate::StyleTokens>();
    ui.set_font_scale_percent(200);
    tokens.set_icon_size(28.0);
    settle(&ui).await;
    let glyph_font = 12.0 * 2.0 * 28.0 / 18.0;
    for (label, glyph) in [
        ("Close image preview", false),
        ("Save synthetic viewer to a location you choose", false),
        ("Fit image to the window", false),
        ("Zoom in", true),
        ("Zoom out", true),
        ("Previous image", true),
        ("Next image", true),
    ] {
        let bounds = control_bounds(&ui, label);
        assert_eq!(bounds.len(), 1, "{label}");
        let height = bounds[0].bottom - bounds[0].top;
        let caption = if glyph { glyph_font } else { 24.0 };
        assert!(
            height >= caption + 11.5,
            "{label}: {height} px at 200% text and 28 px icons"
        );
        assert!(
            inside_window(&ui, bounds[0]),
            "{label} at 200% text and 28 px icons"
        );
    }
    tokens.set_icon_size(18.0);
    ui.set_font_scale_percent(100);
    settle(&ui).await;
    ui.set_image_preview_open(false);
    ui.set_image_preview_loaded(false);
    ui.set_image_preview_has_previous(false);
    ui.set_image_preview_has_next(false);
    ui.set_image_preview_event_id("".into());
    ui.set_image_preview_image(slint::Image::default());

    // Measure the queue's real captions at large text too. These are inert
    // in-memory presentation rows; no file, upload or action is submitted.
    let previous_page = ui.get_page();
    ui.set_page(0);
    ui.set_font_scale_percent(200);
    ui.set_attachment_items(ModelRc::new(VecModel::from(vec![crate::AttachmentItem {
        id: 1,
        name: "synthetic queued file".into(),
        detail: "1 KiB".into(),
        status: "Ready".into(),
        progress: -1.0,
        can_remove: true,
        ..Default::default()
    }])));
    ui.set_attachment_ready_count(1);
    settle(&ui).await;
    let row = control_bounds(&ui, "synthetic queued file, 1 KiB, Ready");
    assert_eq!(row.len(), 1);
    assert!(row[0].bottom - row[0].top >= 49.5);
    for label in ["Send the ready attachment", "Remove synthetic queued file"] {
        let bounds = control_bounds(&ui, label);
        assert_eq!(bounds.len(), 1, "{label}");
        assert!(bounds[0].bottom - bounds[0].top >= 29.5, "{label}");
        assert!(inside_window(&ui, bounds[0]), "{label} at 200%");
    }
    ui.set_attachment_items(ModelRc::new(VecModel::default()));
    ui.set_attachment_ready_count(0);
    ui.set_font_scale_percent(100);
    ui.set_page(previous_page);
    ui.set_show_sign_in(true);
    settle(&ui).await;

    // A queued production attachment refresh cannot restore old private rows
    // while presentation IDs survive a revoked/detached live context.
    ui.set_active_account_local_id("old-account".into());
    ui.set_active_room_id("!private:example.org".into());
    assert!(!live.is_live.load(std::sync::atomic::Ordering::Acquire));
    for shown_live in [false, true] {
        ui.set_live_session(shown_live);
        crate::attachment_workbench::Workbench::queue_private_projection_for_test(
            &ui,
            live.clone(),
            runtime.clone(),
        );
        assert_eq!(ui.get_attachment_items().row_count(), 1);
        assert_eq!(ui.get_transfer_items().row_count(), 1);
        settle(&ui).await;
        assert_eq!(ui.get_active_account_local_id().as_str(), "old-account");
        assert_eq!(ui.get_active_room_id().as_str(), "!private:example.org");
        assert_eq!(ui.get_attachment_items().row_count(), 0);
        assert_eq!(ui.get_attachment_ready_count(), 0);
        assert!(!ui.get_attachment_can_add());
        assert!(ui.get_attachment_status().is_empty());
        assert_eq!(ui.get_transfer_items().row_count(), 0);
        assert_eq!(ui.get_transfer_active_count(), 0);
        assert!(ui.get_transfer_summary().is_empty());
    }

    // Seed private information in distinct user-visible surfaces, then use the
    // production detach adapter with a real Weak<AppWindow>, not Weak::default.
    ui.set_live_session(true);
    ui.set_active_account_local_id("old-account".into());
    assert_eq!(ui.get_theme_mode(), 1, "the themed account is still shown");
    ui.set_active_account_handle("@casey:example.org".into());
    ui.set_accounts(ModelRc::new(VecModel::from(vec![AccountItem {
        handle: "@casey:example.org".into(),
        ..Default::default()
    }])));
    ui.set_active_room_id("!private:example.org".into());
    ui.set_active_room_name("Private room".into());
    ui.set_channels(ModelRc::new(VecModel::from(vec![RoomItem {
        id: "!private:example.org".into(),
        ..Default::default()
    }])));
    ui.set_combined_rooms(ModelRc::new(VecModel::from(vec![
        crate::NavigationRoomItem {
            room: RoomItem {
                id: "!private:example.org".into(),
                ..Default::default()
            },
            direct: true,
        },
    ])));
    ui.set_messages(ModelRc::new(VecModel::from(vec![MessageItem {
        body: "private message".into(),
        ..Default::default()
    }])));
    ui.set_thread_messages(ModelRc::new(VecModel::from(vec![MessageItem {
        body: "private reply".into(),
        ..Default::default()
    }])));
    ui.set_members(ModelRc::new(VecModel::from(vec![MemberItem {
        name: "Jordan".into(),
        ..Default::default()
    }])));
    ui.set_thread_root_body("private thread root".into());
    ui.set_thread_root_is_own(true);
    ui.set_thread_root_palette_index(3);
    ui.set_thread_root_accent(Color::from_rgb_u8(0x09, 0x09, 0x09));
    ui.set_thread_composer_text("private reply draft".into());
    ui.set_room_search_query("private search".into());
    ui.set_room_search_results(ModelRc::new(VecModel::from(vec![
        MessageSearchItem::default(),
    ])));
    ui.set_room_search_selected_body("private search result".into());
    ui.set_member_confirm_name("Jordan".into());
    ui.set_room_admin_topic_input("private topic".into());
    ui.set_attachment_status("private upload name".into());
    assert_recovery_strip_projection(&ui).await;
    assert_sticker_picker_projection(&ui).await;
    // The real picker controller over a recording fake core: admission,
    // persistence, single-flight commands, held sends and exact reconciliation.
    crate::stickers::controller_tests::run(&ui, runtime.clone()).await;
    // A departed account's call stage: tiles, a pinned featured tile, decoded
    // pictures and roster rows. No video watcher runs in this test, so only
    // the detach path itself can clear them.
    ui.set_native_call_tiles(ModelRc::new(VecModel::from(vec![CallTileItem {
        id: 7,
        name: "@private:example.org".into(),
        live: true,
        frame_slot: 1,
        ..Default::default()
    }])));
    ui.set_native_call_featured(CallTileItem {
        id: 8,
        name: "@pinned:example.org".into(),
        live: true,
        pinned: true,
        ..Default::default()
    });
    ui.set_native_call_featured_visible(true);
    let picture =
        || slint::Image::from_rgb8(slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(2, 2));
    ui.set_native_call_frames(ModelRc::new(VecModel::from(vec![picture(), picture()])));
    ui.set_native_call_participants(ModelRc::new(VecModel::from(vec![CallParticipantItem {
        name: "@private:example.org".into(),
        detail: "Camera on".into(),
        is_local: false,
    }])));
    ui.set_native_call_page(1);
    ui.set_native_call_pages(2);
    ui.set_native_call_tile_count(6);
    ui.set_native_call_media_degraded(true);
    // Exercise chat controls before opening the one-time-key modal. Then
    // render that private surface as well before testing the real detach.
    ui.set_security_recovery_input("synthetic recovery input".into());
    ui.set_security_recovery_key("synthetic recovery output".into());
    // A poll draft, notice and dialog draft are private presentation too.
    let polls = slint::ComponentHandle::global::<crate::PollUiState>(&ui);
    polls.set_draft_event_id("$private-poll".into());
    polls.set_notice_event_id("$private-poll".into());
    polls.set_notice("private poll notice".into());
    polls.set_dialog_question("private poll question".into());
    // The account's session list and an open sign-out step are private too.
    ui.set_device_sessions_query("private session search".into());
    let session = crate::DeviceSessionRow {
        key: "PRIVATEDEVICE".into(),
        name: "Private laptop".into(),
        device_id: "PRIVATEDEVICE".into(),
        ..crate::DeviceSessionRow::default()
    };
    ui.set_device_session_rows(ModelRc::new(VecModel::from(vec![session])));
    ui.set_device_sessions_view(crate::DeviceSessionsView {
        panel_id: "1".into(),
        notice: "private session notice".into(),
        stage: 2,
        stage_title: "Confirm with your password to sign out Private laptop".into(),
        ..crate::DeviceSessionsView::default()
    });
    settle(&ui).await;
    assert_eq!(control_bounds(&ui, "One-time Matrix recovery key").len(), 1);
    // A membership-denied refresh withdraws old decrypted rows and targets
    // immediately; an error banner alone must not leave the old pane usable.
    ui.set_pinned_items(ModelRc::new(VecModel::from(vec![
        crate::PinnedMessageItem {
            event_id: "$withdrawn:example.org".into(),
            text: "withdrawn private pin".into(),
            can_open: true,
            can_unpin: true,
            ..Default::default()
        },
    ])));
    ui.set_pinned_filter("withdrawn filter".into());
    ui.set_pinned_can_edit(true);
    ui.set_pinned_state_loaded(true);
    crate::pinned_messages::seed_row_pins_for_test(&["$withdrawn:example.org"]);
    crate::pinned_messages::clear_unjoined_presentation(&ui);
    assert_eq!(ui.get_pinned_items().row_count(), 0);
    assert!(ui.get_pinned_filter().is_empty());
    assert!(!ui.get_pinned_can_edit() && !ui.get_pinned_state_loaded());
    assert!(!crate::pinned_messages::row_pinned(Some(
        "$withdrawn:example.org"
    )));
    // Reseed every field below, preserving the independent detach check.
    // Seeded after the last settle, so only the detach path can clear it.
    ui.set_pinned_items(ModelRc::new(VecModel::from(vec![
        crate::PinnedMessageItem {
            text: "private pinned text".into(),
            ..Default::default()
        },
    ])));
    ui.set_pinned_filter("private pinned search".into());
    ui.set_pinned_summary("3 pinned messages".into());
    ui.set_pinned_notice("Message pinned.".into());
    ui.set_pinned_can_edit(true);
    ui.set_pinned_state_loaded(true);
    crate::pinned_messages::seed_row_pins_for_test(&["$pinned:example.org"]);
    // Reseed after the last settle so the independent real detach path, not
    // the queued-refresh check above, must clear this private surface.
    ui.set_attachment_items(ModelRc::new(VecModel::from(vec![crate::AttachmentItem {
        name: "private attachment".into(),
        ..Default::default()
    }])));
    ui.set_attachment_ready_count(1);
    ui.set_attachment_can_add(true);
    ui.set_attachment_status("private upload name".into());
    // Seed directly after the final settle: the real detach adapter must
    // clear the transfer pane without relying on Workbench refresh.
    ui.set_transfer_items(ModelRc::new(VecModel::from(vec![crate::TransferItem {
        id: 7,
        name: "private transfer filename".into(),
        room: "private transfer room".into(),
        ..Default::default()
    }])));
    ui.set_transfer_summary("private transfer summary".into());
    ui.set_transfer_active_count(1);
    ui.set_transfer_focus_pending(true);
    live.account_detached(ui.as_weak()).await;
    settle(&ui).await;
    assert!(!ui.get_live_session());
    assert_eq!(ui.get_pinned_items().row_count(), 0);
    assert!(ui.get_pinned_filter().is_empty() && ui.get_pinned_summary().is_empty());
    assert!(ui.get_pinned_notice().is_empty());
    assert!(!ui.get_pinned_can_edit() && !ui.get_pinned_state_loaded());
    assert!(!crate::pinned_messages::row_pinned(Some(
        "$pinned:example.org"
    )));
    assert_eq!(ui.get_attachment_items().row_count(), 0);
    assert_eq!(ui.get_attachment_ready_count(), 0);
    assert!(!ui.get_attachment_can_add());
    assert_eq!(ui.get_transfer_items().row_count(), 0);
    assert_eq!(ui.get_transfer_active_count(), 0);
    assert!(ui.get_transfer_summary().is_empty());
    assert!(!ui.get_transfer_focus_pending());
    // Both saved-send strips are cleared by the real detach path; no departed
    // scope's records, flags or notices survive.
    assert_eq!(ui.get_composer_pending_items().row_count(), 0);
    assert_eq!(ui.get_thread_pending_items().row_count(), 0);
    assert!(!ui.get_thread_send_pending());
    assert!(!ui.get_composer_unconfirmed() && !ui.get_thread_unconfirmed());
    assert!(!ui.get_composer_pending_blocked() && !ui.get_thread_pending_blocked());
    assert_eq!(ui.get_composer_pending_count(), 0);
    assert!(ui.get_composer_blocked_notice().is_empty());
    assert!(ui.get_thread_blocked_notice().is_empty());
    assert!(ui.get_show_sign_in());
    assert!(ui.get_active_account_local_id().is_empty());
    assert!(ui.get_active_account_handle().is_empty());
    assert_eq!(ui.get_active_account_index(), -1);
    assert_eq!(
        ui.get_theme_mode(),
        2,
        "detaching returns to the global theme, not the departed account's override"
    );
    assert_eq!(ui.get_global_theme_mode(), 2);
    assert!(ui.get_account_theme_notice().is_empty());
    apply_config(&ui, &Config::default(), None);
    assert_eq!(ui.get_theme_mode(), 0);
    assert_eq!(ui.get_global_theme_mode(), 0);
    assert!(ui.get_active_room_id().is_empty());
    assert!(ui.get_active_room_name().is_empty());
    assert_eq!(ui.get_accounts().row_count(), 0);
    assert_eq!(ui.get_channels().row_count(), 0);
    assert_eq!(ui.get_combined_rooms().row_count(), 0);
    assert_eq!(ui.get_messages().row_count(), 0);
    assert_eq!(ui.get_thread_messages().row_count(), 0);
    assert_eq!(ui.get_members().row_count(), 0);
    assert!(ui.get_composer_text().is_empty());
    assert!(ui.get_thread_root_body().is_empty());
    assert!(!ui.get_thread_root_is_own());
    assert_eq!(ui.get_thread_root_palette_index(), 0);
    assert_eq!(ui.get_thread_root_accent(), Color::default());
    assert!(ui.get_thread_composer_text().is_empty());
    assert!(ui.get_room_search_query().is_empty());
    assert_eq!(ui.get_room_search_results().row_count(), 0);
    assert!(ui.get_room_search_selected_body().is_empty());
    assert!(ui.get_member_confirm_name().is_empty());
    assert!(ui.get_room_admin_topic_input().is_empty());
    assert!(ui.get_attachment_status().is_empty());
    assert!(ui.get_security_recovery_input().is_empty());
    assert!(ui.get_security_recovery_key().is_empty());
    assert!(polls.get_draft_event_id().is_empty());
    assert!(polls.get_notice().is_empty() && polls.get_notice_event_id().is_empty());
    assert!(polls.get_dialog_question().is_empty());
    assert!(!ui.get_poll_dialog_open());
    assert!(ui.get_device_sessions_query().is_empty());
    assert_eq!(ui.get_device_session_rows().row_count(), 0);
    let cleared = crate::DeviceSessionsView::default();
    assert_eq!(ui.get_device_sessions_view(), cleared);
    // The call stage clears synchronously with the account, not after an
    // asynchronous video update: no tile, pinned row, picture or roster row.
    assert_eq!(ui.get_native_call_tiles().row_count(), 0);
    assert!(!ui.get_native_call_featured_visible());
    assert_eq!(ui.get_native_call_featured(), CallTileItem::default());
    let frames = ui.get_native_call_frames();
    assert_eq!(frames.row_count(), 2);
    assert!(frames.iter().all(|frame| frame.size().width == 0));
    assert_eq!(ui.get_native_call_participants().row_count(), 0);
    assert_eq!(
        (
            ui.get_native_call_page(),
            ui.get_native_call_pages(),
            ui.get_native_call_tile_count()
        ),
        (0, 1, 0)
    );
    assert!(!ui.get_native_call_media_degraded());
    assert!(manager.active_account().await.is_none());

    assert_removed_account_menu_projection(&ui);
    pinned.shutdown(&ui);
    ui.hide().unwrap();
    drop(ui); // Drops the registered controller callbacks.
    drop(pinned); // Release the explicit controller owner before checking profile teardown.
    drop(access);
    drop(live);
    tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(&manager) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("draft worker must release the profile before cleanup");
    drop(manager);
    directory.close().unwrap();
}

/// Whether any instantiated item's accessible label contains `needle`.
#[cfg(feature = "native-calls")]
fn any_label_contains(ui: &AppWindow, needle: &str) -> bool {
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut found = false;
    root.visit_descendants(|item| {
        let label = item.accessible_string_property(AccessibleStringProperty::Label);
        if label.as_deref().is_some_and(|label| label.contains(needle)) {
            found = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    found
}

/// The call controller as phase 85's bar sees it: the current call as the
/// controller would observe it, every action it admitted, and how many it
/// refused. Like the controller, it admits an action at its hand-off only for
/// exactly the observed call (media: exactly its shown join) and only while
/// the click's own context holds; `hold` keeps the next hand-off waiting, as
/// the controller's slot lock can. `fail` makes the next admitted action
/// fail in the worker instead (counted in `failed`), as a capture worker's
/// Stop can after it published Failed. `holds` keeps the next hand-off of
/// one exact action waiting, so several can wait at once and be answered in
/// a chosen order.
#[cfg(feature = "native-calls")]
struct FakeCallBarControls {
    current: Mutex<crate::call_bar::CallView>,
    hold: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    holds: Mutex<
        Vec<(
            crate::call_bar::BarAction,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    acted: Mutex<Vec<(crate::call_bar::BarAction, crate::calls::ObservedCall)>>,
    refused: std::sync::atomic::AtomicUsize,
    fail: Mutex<Option<crate::calls::CallControllerError>>,
    failed: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "native-calls")]
impl crate::call_bar::BarControls for Arc<FakeCallBarControls> {
    fn current(&self) -> Option<crate::call_bar::CallView> {
        Some(self.current.lock().unwrap().clone())
    }

    fn act(
        &self,
        action: crate::call_bar::BarAction,
        observed: crate::calls::ObservedCall,
        admit: crate::call_bar::Admit,
    ) -> crate::call_bar::Acting {
        let fake = self.clone();
        Box::pin(async move {
            let hold = fake.hold.lock().unwrap().take().or_else(|| {
                let mut holds = fake.holds.lock().unwrap();
                let at = holds.iter().position(|(held, _)| *held == action)?;
                Some(holds.remove(at).1)
            });
            if let Some(hold) = hold {
                let _ = hold.await;
            }
            let current = fake.current.lock().unwrap().call.clone();
            let now = current.and_then(|call| call.observed);
            let exact = now.is_some_and(|now| match action {
                crate::call_bar::BarAction::Leave => {
                    (now.call, &now.account_id, &now.room_id)
                        == (observed.call, &observed.account_id, &observed.room_id)
                }
                _ => now == observed,
            });
            if exact && admit() {
                if let Some(error) = fake.fail.lock().unwrap().take() {
                    fake.failed.fetch_add(1, Ordering::SeqCst);
                    return Err(error);
                }
                fake.acted.lock().unwrap().push((action, observed));
                Ok(())
            } else {
                fake.refused.fetch_add(1, Ordering::SeqCst);
                Err(crate::calls::CallControllerError::StaleCall)
            }
        })
    }
}

/// Settle until `done` holds: a bar action finishes on the runtime, then on
/// the UI thread.
#[cfg(feature = "native-calls")]
async fn settle_until(ui: &AppWindow, runtime: &Handle, done: impl Fn() -> bool) {
    for _ in 0..400 {
        settle(ui).await;
        if done() {
            return;
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = sender.send(());
        });
        let _ = receiver.await;
    }
    panic!("a call bar action did not finish");
}

/// Window-relative bounds of every text item showing exactly `text`.
#[cfg(feature = "native-calls")]
fn text_item_bounds(ui: &AppWindow, text: &str) -> Vec<Bounds> {
    use slint::private_unstable_api::re_exports::{ComplexText, ItemRc, SimpleText, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut found = Vec::new();
    root.visit_descendants(|item| {
        let shown = if let Some(simple) = item.downcast::<SimpleText>() {
            simple.as_pin_ref().text().as_str() == text
        } else if let Some(complex) = item.downcast::<ComplexText>() {
            complex.as_pin_ref().text().as_str() == text
        } else {
            false
        };
        if shown {
            found.push(item_bounds(item));
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// Phase 85: the persistent call bar on the real window. It shows only
/// outside the call page and never on a protected surface, where only the
/// generic badge (no room name, no control) remains. Each control hands the
/// exact call it was shown for, with the click's own context, to the
/// controller (never the call page's unbound handlers): a replaced call, even
/// in the same room and account, is refused both at the click and at the
/// hand-off, as is a click whose security context changed while it waited.
/// Mute and Deafen act only while the window's applied audio generation is
/// the shown join, and Stop only while its applied capture generation is.
/// Every surface in the window's protected list hides the bar, and a real
/// entry withdraws a waiting click synchronously, even when the surface
/// closes again before any change handler runs. Without an exact call Leave
/// is disabled; Return only shows the call page and moves keyboard focus to
/// its stage (a tile, else the participant list's control, else the stage
/// itself on a narrow stage); and at the smallest window and 200% text its
/// controls and indicators stay inside the window, in the top half.
#[cfg(feature = "native-calls")]
async fn assert_call_bar(ui: &AppWindow, runtime: &Handle) {
    use crate::CallBar;
    use crate::call_bar::{
        self, Admit, BarAction,
        tests::{clear_for_test, joined_view_for_test, observed_for_test, present_for_test},
    };
    use crate::calls::CallStatus;
    use crate::local_video::CaptureKind;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    const CHANGED: &str = "The call or account changed before this was sent; nothing was done.";

    let room = "!planning:example.org";
    // Two calls of the same account and room: only their generations differ.
    let first = observed_for_test("work", room, 7, 7);
    let second = observed_for_test("work", room, 8, 8);
    let fake = Arc::new(FakeCallBarControls {
        current: Mutex::new(joined_view_for_test(&first)),
        hold: Mutex::new(None),
        holds: Mutex::new(Vec::new()),
        acted: Mutex::new(Vec::new()),
        refused: Default::default(),
        fail: Mutex::new(None),
        failed: Default::default(),
    });
    let acted = || fake.acted.lock().unwrap().clone();
    let refused = || fake.refused.load(Ordering::SeqCst);
    let failed = || fake.failed.load(Ordering::SeqCst);
    let set_current = |view| *fake.current.lock().unwrap() = view;
    let hold = || {
        let (release, hold) = tokio::sync::oneshot::channel();
        *fake.hold.lock().unwrap() = Some(hold);
        release
    };
    // Each click's own hand-off, as send() starts it: a wait is for that
    // hand-off's actual finish() publication, which only its own finish
    // closure acknowledges, so no earlier delayed finish can satisfy it.
    let handed_off = |click: &dyn Fn()| -> u64 {
        let ids = call_bar::handoffs_for_test(click);
        assert_eq!(ids.len(), 1, "the click handed off exactly once");
        ids[0]
    };
    let not_handed_off = |click: &dyn Fn()| {
        assert!(
            call_bar::handoffs_for_test(click).is_empty(),
            "nothing was handed off"
        );
    };
    let finished = |id: u64| move || call_bar::handoff_finished_for_test(id);
    // The click's own security context, as the live session's admission
    // captures it: a security step started since the click refuses it, even
    // after that step ended.
    let security = Arc::new(AtomicU64::new(0));
    let context = security.clone();
    let admission = move |account_id: &str| {
        let generation = context.load(Ordering::SeqCst);
        let context = context.clone();
        let admit = move || context.load(Ordering::SeqCst) == generation;
        (account_id == "work").then(|| Arc::new(admit) as Admit)
    };
    // The audio and capture generations whose state the window shows, as
    // main.rs's audio and capture watchers apply them: the first call's join
    // for now.
    let applied_audio = Arc::new(AtomicU64::new(first.presentation));
    let applied_capture = Arc::new(AtomicU64::new(first.presentation));
    call_bar::register(
        ui,
        fake.clone(),
        runtime.clone(),
        applied_audio.clone(),
        applied_capture.clone(),
        admission,
    );
    // The call page's own handlers: the bar never uses them.
    let toggled = Rc::new(Cell::new(0));
    let stopped = Rc::new(RefCell::new(Vec::new()));
    let ended = Rc::new(Cell::new(0));
    let counter = toggled.clone();
    ui.on_toggle_native_microphone(move || counter.set(counter.get() + 1));
    let kinds = stopped.clone();
    ui.on_stop_native_capture(move |kind| kinds.borrow_mut().push(kind));
    let counter = ended.clone();
    ui.on_end_native_call(move || counter.set(counter.get() + 1));
    let saved = (ui.get_live_session(), ui.get_page(), ui.get_show_sign_in());
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_font_scale_percent(100);
    ui.set_show_sign_in(false);
    ui.set_live_session(true);
    ui.set_page(0);
    // The published call state, as the status and capture watchers set it.
    ui.set_call_connected(true);
    ui.set_native_call_joined(true);
    ui.set_native_call_room_display("Planning room".into());
    ui.set_native_audio_ready(true);
    ui.set_native_microphone_muted(false);
    ui.set_native_camera_active(true);
    present_for_test(ui, joined_view_for_test(&first));
    settle(ui).await;
    assert!(ui.get_call_bar_visible());
    let bar = ui.global::<CallBar>();
    assert_eq!(bar.get_status_text().as_str(), "Connected");
    assert_eq!(bar.get_elapsed().as_str(), "0:00");
    assert!(bar.get_can_leave() && bar.get_can_control());
    // Indicators are the published capture state.
    assert!(!control_bounds(ui, "Camera on").is_empty());
    assert!(!control_bounds(ui, "Mic on").is_empty());
    // A control hands its action for exactly the call it shows.
    let op = handed_off(&|| click_control_once(ui, "Mute microphone"));
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 1);
    assert_eq!(acted(), [(BarAction::Mute(true), first.clone())]);
    // Another call in the same room and account replaced it before the bar
    // was told: nothing reaches the controller, and Return does not follow.
    set_current(joined_view_for_test(&second));
    not_handed_off(&|| {
        click_control_once(ui, "Stop camera");
        click_control_once(ui, "Leave call");
        click_control_once(ui, "Return to the call page");
        bar.invoke_mute();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused(), ui.get_page()), (1, 0, 0));
    assert!(!ui.get_native_camera_busy());
    // The replacement arrives after the click, while its hand-off waits:
    // the controller refuses Leave and Stop, and the bar says nothing was
    // done.
    set_current(joined_view_for_test(&first));
    let release = hold();
    let op = handed_off(&|| bar.invoke_leave());
    settle(ui).await;
    set_current(joined_view_for_test(&second));
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(ui.get_session_error().as_str(), CHANGED);
    assert_eq!((acted().len(), refused()), (1, 1));
    ui.set_session_error("".into());
    set_current(joined_view_for_test(&first));
    let release = hold();
    let op = handed_off(&|| bar.invoke_stop_camera());
    settle(ui).await;
    assert!(ui.get_native_camera_busy());
    set_current(joined_view_for_test(&second));
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert!(!ui.get_native_camera_busy());
    assert_eq!(ui.get_native_capture_error().as_str(), CHANGED);
    assert_eq!((acted().len(), refused()), (1, 2));
    ui.set_native_capture_error("".into());
    // The click's security context changes while it waits (a step starts
    // and ends): refused, even though the call is still the same.
    set_current(joined_view_for_test(&first));
    let release = hold();
    let op = handed_off(&|| bar.invoke_leave());
    settle(ui).await;
    security.fetch_add(1, Ordering::SeqCst);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(ui.get_session_error().as_str(), CHANGED);
    assert_eq!((acted().len(), refused()), (1, 3));
    ui.set_session_error("".into());
    // A protected surface is shown and closed while a click waits: refused.
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    ui.set_show_sign_in(true);
    settle(ui).await;
    ui.set_show_sign_in(false);
    settle(ui).await;
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 4);
    assert_eq!(acted().len(), 1);
    // The matching call: Leave and Stop reach it.
    let op = handed_off(&|| bar.invoke_leave());
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 2);
    let op = handed_off(&|| bar.invoke_stop_camera());
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 3);
    assert_eq!(
        acted()[1..],
        [
            (BarAction::Leave, first.clone()),
            (BarAction::Stop(CaptureKind::Camera), first.clone()),
        ]
    );
    ui.set_native_camera_busy(false);
    // Presented for the current call: Stop stops only that capture, never
    // opens a picker, and Return shows the call page, where the bar is not.
    present_for_test(ui, joined_view_for_test(&second));
    set_current(joined_view_for_test(&second));
    ui.set_native_screen_active(true);
    settle(ui).await;
    // The status shows the second call while the window still shows the
    // first join's camera and screen state: Stop acts on nothing, since what
    // it read is not that join's.
    assert_eq!(applied_capture.load(Ordering::SeqCst), first.presentation);
    not_handed_off(&|| {
        bar.invoke_stop_camera();
        bar.invoke_stop_share();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused()), (3, 4));
    let idle = !ui.get_native_camera_busy() && !ui.get_native_screen_busy();
    assert!(idle, "nothing was sent");
    // The second join's capture state is applied: each Stop reaches that
    // join with its own kind.
    applied_capture.store(second.presentation, Ordering::SeqCst);
    // Each successful Stop gives up its ownership in its own finish(): both
    // are acknowledged before any later Stop starts.
    let op = handed_off(&|| bar.invoke_stop_camera());
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 4);
    let op = handed_off(&|| bar.invoke_stop_share());
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 5);
    assert_eq!(
        acted()[3..],
        [
            (BarAction::Stop(CaptureKind::Camera), second.clone()),
            (BarAction::Stop(CaptureKind::Screen), second.clone()),
        ]
    );
    assert!(!ui.get_native_capture_picker_open());
    ui.set_native_camera_busy(false);
    ui.set_native_screen_busy(false);
    ui.set_native_screen_active(false);
    // A system screen picker is open (busy and selecting) when Stop share is
    // pressed, and the worker's Stop fails. The worker publishes Failed
    // before it answers, so the capture watcher applies that first (busy and
    // selecting off) while the presentation still matches. The late failure
    // must not put the click's old busy flag back.
    {
        use crate::local_video::{CapturePhase, CaptureState, LocalVideoStatus};
        ui.set_native_screen_busy(true);
        ui.set_native_screen_selecting(true);
        *fake.fail.lock().unwrap() = Some(crate::calls::CallControllerError::WorkerStopped);
        let release = hold();
        let op = handed_off(&|| bar.invoke_stop_share());
        settle(ui).await;
        assert!(ui.get_native_screen_busy());
        let failure = CaptureState {
            phase: CapturePhase::Failed("synthetic stop failure".into()),
            source: String::new(),
        };
        crate::apply_capture_status(
            ui,
            LocalVideoStatus {
                token: second.presentation,
                screen: failure,
                ..Default::default()
            },
        );
        assert!(!ui.get_native_screen_busy() && !ui.get_native_screen_selecting());
        // Wait for this Stop's own finish() publication.
        release.send(()).unwrap();
        settle_until(ui, runtime, finished(op)).await;
        assert_eq!(failed(), 1);
        settle(ui).await;
        assert!(!ui.get_native_screen_busy(), "no stale ShareStarting");
        assert!(!ui.get_native_screen_selecting());
        assert_eq!((acted().len(), refused()), (5, 4));
        // Positive control: the picker is still selecting and nothing wrote
        // the capture state while the Stop waited; refused at the hand-off,
        // the click's busy flag (true) is put back, with the fixed text.
        ui.set_native_screen_busy(true);
        ui.set_native_screen_selecting(true);
        let release = hold();
        let op = handed_off(&|| bar.invoke_stop_share());
        settle(ui).await;
        set_current(joined_view_for_test(&first));
        // Wait for this Stop's own finish() publication.
        release.send(()).unwrap();
        settle_until(ui, runtime, finished(op)).await;
        assert_eq!(refused(), 5);
        settle(ui).await;
        assert!(ui.get_native_screen_busy() && ui.get_native_screen_selecting());
        assert_eq!(ui.get_native_capture_error().as_str(), CHANGED);
        ui.set_native_capture_error("".into());
        set_current(joined_view_for_test(&second));
        // Negative: the join was replaced (the watcher applied another
        // presentation and reset its capture state) while the Stop waited;
        // its refusal touches nothing of the new join.
        let release = hold();
        let op = handed_off(&|| bar.invoke_stop_share());
        settle(ui).await;
        applied_capture.store(second.presentation + 1, Ordering::SeqCst);
        ui.set_native_screen_busy(false);
        ui.set_native_screen_selecting(false);
        set_current(joined_view_for_test(&first));
        // Wait for this Stop's own finish() publication.
        release.send(()).unwrap();
        settle_until(ui, runtime, finished(op)).await;
        assert_eq!(refused(), 6);
        settle(ui).await;
        assert!(!ui.get_native_screen_busy() && !ui.get_native_screen_selecting());
        assert!(ui.get_native_capture_error().is_empty());
        applied_capture.store(second.presentation, Ordering::SeqCst);
        set_current(joined_view_for_test(&second));
        // A Stop camera refused while it waited, after a newer click took the
        // camera's busy flag (the call page's start writes it): the refusal
        // never clears that newer flag.
        ui.set_native_camera_active(true);
        let release = hold();
        let op = handed_off(&|| bar.invoke_stop_camera());
        settle(ui).await;
        let shown = ui.get_native_camera_busy();
        let _newer = crate::call_bar::capture_owner_begin(CaptureKind::Camera, shown);
        ui.set_native_camera_busy(true);
        set_current(joined_view_for_test(&first));
        // Wait for this Stop's own finish() publication.
        release.send(()).unwrap();
        settle_until(ui, runtime, finished(op)).await;
        assert_eq!(refused(), 7);
        settle(ui).await;
        assert!(ui.get_native_camera_busy(), "the newer busy flag stays");
        set_current(joined_view_for_test(&second));
        crate::apply_capture_status(
            ui,
            LocalVideoStatus {
                token: second.presentation,
                ..Default::default()
            },
        );
        ui.set_native_screen_error("".into());
        ui.set_native_capture_error("".into());
        assert!(!ui.get_native_camera_busy() && !ui.get_native_screen_busy());
        // CB-CAP-01: a Stop camera and a Stop share wait at once, the real
        // account menu opens and closes, and both are refused before they
        // were queued (no capture update). Answered in either order, each
        // puts back its own kind's flag: the other kind never takes it.
        let hold_action = |action: BarAction| {
            let (release, hold) = tokio::sync::oneshot::channel::<()>();
            fake.holds.lock().unwrap().push((action, hold));
            release
        };
        let stop_camera = BarAction::Stop(CaptureKind::Camera);
        let stop_share = BarAction::Stop(CaptureKind::Screen);
        ui.set_native_camera_active(true);
        ui.set_native_screen_active(true);
        settle(ui).await;
        for camera_first in [true, false] {
            let camera = hold_action(stop_camera);
            let screen = hold_action(stop_share);
            let camera_op = handed_off(&|| bar.invoke_stop_camera());
            let screen_op = handed_off(&|| bar.invoke_stop_share());
            settle(ui).await;
            assert!(ui.get_native_camera_busy() && ui.get_native_screen_busy());
            ui.invoke_open_account_menu();
            ui.set_account_menu_open(false);
            let before = refused();
            let ((first_out, first_op), (second_out, second_op)) = if camera_first {
                ((camera, camera_op), (screen, screen_op))
            } else {
                ((screen, screen_op), (camera, camera_op))
            };
            // Each Stop's own finish() publication, the first acknowledged
            // before the second is released.
            first_out.send(()).unwrap();
            settle_until(ui, runtime, finished(first_op)).await;
            assert!(!finished(second_op)(), "the second is still held");
            assert_eq!(refused(), before + 1);
            second_out.send(()).unwrap();
            settle_until(ui, runtime, finished(second_op)).await;
            assert_eq!(refused(), before + 2);
            settle(ui).await;
            let stuck = (ui.get_native_camera_busy(), ui.get_native_screen_busy());
            assert_eq!(stuck, (false, false), "camera first: {camera_first}");
            assert_eq!(ui.get_native_capture_error().as_str(), CHANGED);
            ui.set_native_capture_error("".into());
        }
        // Duplicate same-kind Stops (Stop share stays enabled while the screen
        // is shared): the second click saw the first one's optimistic busy
        // flag. Both refused, answered in either order: the flag stays busy
        // while one is still pending, then goes back to the authoritative
        // false, never to the optimistic true.
        for first_out in [true, false] {
            let one = hold_action(stop_share);
            let two = hold_action(stop_share);
            let one_op = handed_off(&|| bar.invoke_stop_share());
            let two_op = handed_off(&|| bar.invoke_stop_share());
            settle(ui).await;
            assert!(ui.get_native_screen_busy());
            ui.invoke_open_account_menu();
            ui.set_account_menu_open(false);
            let before = refused();
            let ((early, early_op), (late, late_op)) = if first_out {
                ((one, one_op), (two, two_op))
            } else {
                ((two, two_op), (one, one_op))
            };
            // Each Stop's own finish() publication, the first acknowledged
            // before the second is released.
            early.send(()).unwrap();
            settle_until(ui, runtime, finished(early_op)).await;
            assert!(!finished(late_op)(), "the second is still held");
            assert_eq!(refused(), before + 1);
            settle(ui).await;
            assert!(ui.get_native_screen_busy(), "the other click still owns it");
            late.send(()).unwrap();
            settle_until(ui, runtime, finished(late_op)).await;
            assert_eq!(refused(), before + 2);
            settle(ui).await;
            assert!(!ui.get_native_screen_busy(), "first out: {first_out}");
            ui.set_native_capture_error("".into());
        }
        // A newer full snapshot while both wait (the watcher applies both
        // kinds): neither refusal puts back its old flag, not even the
        // picker's selecting busy flag.
        ui.set_native_screen_active(false);
        ui.set_native_screen_busy(true);
        ui.set_native_screen_selecting(true);
        let camera = hold_action(stop_camera);
        let screen = hold_action(stop_share);
        let camera_op = handed_off(&|| bar.invoke_stop_camera());
        let screen_op = handed_off(&|| bar.invoke_stop_share());
        settle(ui).await;
        crate::apply_capture_status(
            ui,
            LocalVideoStatus {
                token: second.presentation,
                camera: CaptureState {
                    phase: CapturePhase::Active,
                    source: String::new(),
                },
                ..Default::default()
            },
        );
        set_current(joined_view_for_test(&first));
        let before = refused();
        // Each Stop's own finish() publication, the camera's acknowledged
        // before the screen's is released.
        camera.send(()).unwrap();
        settle_until(ui, runtime, finished(camera_op)).await;
        assert!(!finished(screen_op)(), "the screen Stop is still held");
        assert_eq!(refused(), before + 1);
        screen.send(()).unwrap();
        settle_until(ui, runtime, finished(screen_op)).await;
        assert_eq!(refused(), before + 2);
        settle(ui).await;
        assert!(!ui.get_native_camera_busy() && ui.get_native_camera_active());
        assert!(!ui.get_native_screen_busy() && !ui.get_native_screen_selecting());
        assert!(
            ui.get_native_capture_error().is_empty(),
            "no stale error text"
        );
        set_current(joined_view_for_test(&second));
        // Back to the state the checks below expect: the camera on, and this
        // regression's thirteen refusals left out of the totals they count.
        ui.set_native_camera_active(true);
        ui.set_native_screen_active(false);
        fake.refused.fetch_sub(13, Ordering::SeqCst);
    }
    // The status shows the second call while the window still shows the
    // first join's audio state: Mute and Deafen act on nothing, since the
    // values they read are not that join's.
    assert_eq!(applied_audio.load(Ordering::SeqCst), first.presentation);
    not_handed_off(&|| {
        bar.invoke_mute();
        bar.invoke_deafen();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused()), (5, 4));
    // The second join's audio state is applied: now they act on it.
    applied_audio.store(second.presentation, Ordering::SeqCst);
    let op = handed_off(&|| bar.invoke_mute());
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 6);
    let op = handed_off(&|| bar.invoke_deafen());
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), 7);
    assert_eq!(
        acted()[5..],
        [
            (BarAction::Mute(true), second.clone()),
            (BarAction::Deafen(true), second.clone()),
        ]
    );
    // Return shows the call page, where the bar is not, and keyboard focus
    // goes to the stage's tile.
    ui.set_native_call_tiles(ModelRc::new(VecModel::from(vec![CallTileItem {
        id: 3,
        name: "Ada Lovelace".into(),
        status: "joined".into(),
        frame_slot: -1,
        ..Default::default()
    }])));
    // Admitted, Return also ends an automatic open a click still owns (an
    // accepted invitation's) before the page changes.
    let owned = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(owned.clone());
    bar.invoke_return_to_call();
    assert!(owned.withdrawn(), "Return's entry ended the owned open");
    settle(ui).await;
    assert_eq!(ui.get_page(), 2);
    assert!(!ui.get_call_bar_visible());
    assert!(control_bounds(ui, "Return to the call page").is_empty());
    // The stage and its tile are built as the page shows.
    let tile = "Ada Lovelace, joined";
    settle_until(ui, runtime, || focused_control_has_label(ui, tile)).await;
    assert!(!ui.get_call_stage_focus_pending());
    ui.set_page(0);
    ui.set_native_call_tiles(ModelRc::new(VecModel::from(Vec::<CallTileItem>::new())));
    settle(ui).await;
    // With no tile, focus goes to the participant list's own control on the
    // stage (never the call page's Leave), inside the window.
    bar.invoke_return_to_call();
    settle(ui).await;
    assert_eq!(ui.get_page(), 2);
    let people = ui.get_native_call_participants().row_count();
    let fallback = [
        "Hide participant list".to_owned(),
        format!("Show participant list, {people} in call"),
    ];
    let shown = fallback
        .iter()
        .any(|label| !control_bounds(ui, label).is_empty());
    assert!(shown, "the stage keeps its participant list control");
    let focused_fallback = || {
        fallback
            .iter()
            .any(|label| focused_control_has_label(ui, label))
    };
    settle_until(ui, runtime, focused_fallback).await;
    assert!(!focused_control_has_label(ui, "Leave native call"));
    let window = ui.window().size().to_logical(ui.window().scale_factor());
    let focused = focused_control_bounds(ui).expect("a focused control");
    assert!(
        focused.left >= 0.0
            && focused.right <= window.width + 0.5
            && focused.top >= 0.0
            && focused.bottom <= window.height + 0.5,
        "{focused:?}"
    );
    assert!(!ui.get_call_stage_focus_pending());
    ui.set_page(0);
    settle(ui).await;
    // No exact call (an incomplete cleanup): Leave and the media controls
    // are disabled and say so; Return still leads to the call page.
    let cleanup = call_bar::view_of(
        &CallStatus::CleanupFailed {
            account_id: "work".into(),
            room_id: room.into(),
            reason: "leave status unknown".into(),
        },
        Some(second.clone()),
    );
    ui.set_native_leave_unconfirmed(true);
    present_for_test(ui, cleanup.clone());
    set_current(cleanup);
    settle(ui).await;
    assert!(!bar.get_can_leave() && !bar.get_can_control());
    let unavailable = "Leave unavailable here: review the incomplete call cleanup on the call page";
    assert!(!control_bounds(ui, unavailable).is_empty());
    not_handed_off(&|| {
        bar.invoke_leave();
        bar.invoke_mute();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused()), (7, 4));
    bar.invoke_return_to_call();
    settle(ui).await;
    assert_eq!(ui.get_page(), 2);
    ui.set_page(0);
    ui.set_native_leave_unconfirmed(false);
    present_for_test(ui, joined_view_for_test(&second));
    set_current(joined_view_for_test(&second));
    settle(ui).await;
    // A protected surface: no bar, no room name, no action; only the
    // generic badge, with what this device captures.
    ui.set_show_sign_in(true);
    settle(ui).await;
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    let badge = control_bounds(ui, "Call active · Mic on · Camera on");
    assert!(!badge.is_empty());
    assert!(!any_label_contains(ui, "Planning room"));
    not_handed_off(&|| {
        bar.invoke_leave();
        bar.invoke_mute();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused()), (7, 4));
    ui.set_show_sign_in(false);
    settle(ui).await;
    // Every surface that takes the window over protects the bar: the whole
    // list in call-bar-protected (one list with matrix-link-blocked).
    let surfaces: [(&str, fn(&AppWindow, bool)); 33] = [
        ("no live session", |ui, on| ui.set_live_session(!on)),
        ("link card", |ui, on| {
            ui.global::<crate::MatrixLink>().set_open(on);
        }),
        ("sign-in", |ui, on| ui.set_show_sign_in(on)),
        ("account access", |ui, on| ui.set_access_open(on)),
        ("Manage accounts", |ui, on| ui.set_manage_accounts_open(on)),
        ("account menu", |ui, on| ui.set_account_menu_open(on)),
        ("account switch", |ui, on| ui.set_account_switch_pending(on)),
        ("security step", |ui, on| ui.set_security_busy(on)),
        ("recovery key", |ui, on| {
            let key = if on {
                "synthetic-key-not-a-credential"
            } else {
                ""
            };
            ui.set_security_recovery_key(key.into());
        }),
        ("verification busy", |ui, on| ui.set_sas_busy(on)),
        ("verification stage", |ui, on| {
            ui.set_sas_stage(if on { 4 } else { 0 })
        }),
        ("account profile", |ui, on| ui.set_account_profile_open(on)),
        ("People", |ui, on| ui.set_people_open(on)),
        ("member details", |ui, on| {
            let inspector = crate::MemberInspectorView {
                open: on,
                ..Default::default()
            };
            ui.global::<crate::MemberPrivacy>().set_inspector(inspector);
        }),
        ("Space explorer", |ui, on| {
            ui.global::<crate::SpaceExplorer>().set_open(on);
        }),
        ("Space rooms", |ui, on| {
            ui.global::<crate::SpaceRooms>().set_open(on)
        }),
        ("room addresses", |ui, on| {
            ui.global::<crate::RoomAddresses>().set_open(on);
        }),
        ("room notifications", |ui, on| {
            ui.global::<crate::RoomNotificationState>().set_open(on);
        }),
        ("Seen by", |ui, on| {
            ui.global::<crate::RoomActivity>().set_seen_open(on);
        }),
        ("Quick Switcher", |ui, on| ui.set_quick_switcher_open(on)),
        ("room actions", |ui, on| ui.set_room_actions_open(on)),
        ("room admin", |ui, on| ui.set_room_admin_open(on)),
        ("member confirmation", |ui, on| {
            ui.set_member_confirm_open(on)
        }),
        ("capture picker", |ui, on| {
            ui.set_native_capture_picker_open(on)
        }),
        ("leave dialog", |ui, on| ui.set_leave_call_dialog(on)),
        ("image preview", |ui, on| ui.set_image_preview_open(on)),
        ("poll dialog", |ui, on| ui.set_poll_dialog_open(on)),
        ("thread browser", |ui, on| {
            ui.global::<crate::ThreadBrowser>().set_open(on);
        }),
        ("Leave dialog", |ui, on| {
            let leave = ui.global::<crate::RoomLeave>();
            let mut view = leave.get_view();
            view.open = on;
            leave.set_view(view);
        }),
        // The two Settings states (the bar is otherwise available there).
        ("ignored-users review", |ui, on| {
            let privacy = ui.global::<crate::MemberPrivacy>();
            let mut ignored = privacy.get_ignored();
            ignored.stage = if on { 1 } else { 0 };
            privacy.set_ignored(ignored);
        }),
        ("ignored-users change sending", |ui, on| {
            let privacy = ui.global::<crate::MemberPrivacy>();
            let mut ignored = privacy.get_ignored();
            ignored.busy = on;
            privacy.set_ignored(ignored);
        }),
        ("shortcut recording", |ui, on| {
            let recording = if on { "toggle_details_panel" } else { "" };
            ui.set_shortcut_recording(recording.into());
        }),
        ("the invitations center", |ui, on| {
            ui.global::<crate::InvitationsState>().set_open(on)
        }),
    ];
    for (name, set) in surfaces {
        assert!(
            !ui.get_call_bar_protected(),
            "nothing protected before {name}"
        );
        set(ui, true);
        let hidden = ui.get_call_bar_protected() && !ui.get_call_bar_visible();
        assert!(hidden, "{name} protects the bar");
        set(ui, false);
    }
    assert!(ui.get_call_bar_visible());
    settle(ui).await;
    // A modal opened and closed again while a click waits, with no change
    // handler run in between (no settle): its real entry withdrew the click
    // synchronously, so it stays refused although nothing is open now.
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    ui.invoke_open_account_menu();
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 5);
    assert_eq!(acted().len(), 7);
    settle(ui).await;
    // The same through the actual account trigger (pointer), not the public
    // function: opening it withdraws the waiting click synchronously, so an
    // immediate close with no settle cannot revive it.
    let trigger = format!(
        "Account menu for {}, {}",
        ui.get_active_account_name(),
        ui.get_active_account_handle()
    );
    // Focus elsewhere first, so the click reaches an unfocused trigger.
    request_bridge_focus(ui, "Mute microphone");
    settle(ui).await;
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    click_control_once(ui, &trigger);
    assert!(
        ui.get_account_menu_open(),
        "the actual trigger opens the menu"
    );
    ui.set_account_menu_open(false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 6);
    assert_eq!(acted().len(), 7);
    // The trigger's close toggle is kept (keyboard, on the focused trigger).
    request_bridge_focus(ui, "Mute microphone");
    settle(ui).await;
    click_control_once(ui, &trigger);
    assert!(ui.get_account_menu_open());
    key(ui, Key::Return);
    assert!(
        !ui.get_account_menu_open(),
        "the actual trigger closes the menu"
    );
    settle(ui).await;
    // This case's refusal is left out of the totals the checks below count.
    fake.refused.fetch_sub(1, Ordering::SeqCst);
    // Shortcut recording in Settings protects the bar too, and its real
    // entry withdraws a waiting click synchronously: opened and cancelled
    // again with no change handler run in between, the click stays refused.
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    ui.invoke_shortcut_record("toggle_details_panel".into());
    assert!(!ui.get_shortcut_recording().is_empty());
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    ui.invoke_shortcut_cancel_recording();
    assert!(ui.get_shortcut_recording().is_empty());
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 6);
    assert_eq!(acted().len(), 7);
    settle(ui).await;
    // The inline ignored-users review, entered as its real entry does
    // (withdraw first, then the stage) and closed again with no settle:
    // the waiting click stays refused.
    let review = |ui: &AppWindow, stage: i32, busy: bool| {
        let privacy = ui.global::<crate::MemberPrivacy>();
        let mut ignored = privacy.get_ignored();
        ignored.stage = stage;
        ignored.busy = busy;
        privacy.set_ignored(ignored);
    };
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    crate::withdraw_call_bar(ui);
    review(ui, 1, false);
    assert!(ui.get_call_bar_protected());
    review(ui, 0, false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 7);
    assert_eq!(acted().len(), 7);
    settle(ui).await;
    // While each of the three states is active: only the generic badge, no
    // private bar, and no direct Return, Leave or media action.
    let states: [(&str, fn(&AppWindow, bool)); 3] = [
        ("ignored-users review", |ui, on| {
            let privacy = ui.global::<crate::MemberPrivacy>();
            let mut ignored = privacy.get_ignored();
            ignored.stage = if on { 1 } else { 0 };
            privacy.set_ignored(ignored);
        }),
        ("ignored-users change sending", |ui, on| {
            let privacy = ui.global::<crate::MemberPrivacy>();
            let mut ignored = privacy.get_ignored();
            ignored.stage = if on { 2 } else { 0 };
            ignored.busy = on;
            privacy.set_ignored(ignored);
        }),
        ("shortcut recording", |ui, on| {
            let recording = if on { "toggle_details_panel" } else { "" };
            ui.set_shortcut_recording(recording.into());
        }),
    ];
    for (name, set) in states {
        set(ui, true);
        settle(ui).await;
        assert!(
            ui.get_call_bar_protected() && !ui.get_call_bar_visible(),
            "{name}"
        );
        assert!(
            !control_bounds(ui, "Call active · Mic on · Camera on").is_empty(),
            "{name}"
        );
        assert!(!any_label_contains(ui, "Planning room"), "{name}");
        not_handed_off(&|| {
            bar.invoke_return_to_call();
            bar.invoke_leave();
            bar.invoke_mute();
            bar.invoke_stop_camera();
        });
        settle(ui).await;
        assert_eq!(
            (acted().len(), refused(), ui.get_page()),
            (7, 7, 0),
            "{name}"
        );
        set(ui, false);
        settle(ui).await;
    }
    assert!(ui.get_call_bar_visible());
    // The thread browser protects the bar too. Its entry (thread_browser::
    // show, registered only from phase 89) withdraws the bar before its
    // flags change; entered in that order and closed with no settle, a
    // waiting click stays refused, and while it is open no direct Return,
    // Leave or media action runs.
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    crate::withdraw_call_bar(ui);
    ui.global::<crate::ThreadBrowser>().set_open(true);
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    ui.global::<crate::ThreadBrowser>().set_open(false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 8);
    assert_eq!(acted().len(), 7);
    ui.global::<crate::ThreadBrowser>().set_open(true);
    settle(ui).await;
    assert!(!control_bounds(ui, "Call active · Mic on · Camera on").is_empty());
    not_handed_off(&|| {
        bar.invoke_return_to_call();
        bar.invoke_leave();
        bar.invoke_mute();
        bar.invoke_stop_camera();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused(), ui.get_page()), (7, 8, 0));
    ui.global::<crate::ThreadBrowser>().set_open(false);
    settle(ui).await;
    assert!(ui.get_call_bar_visible());
    // The Leave dialog protects the bar too. Its admitted start (room_leave,
    // replaced by phase 88's own handler here) withdraws the bar before its
    // view opens; entered in that order and closed with no settle, a waiting
    // click stays refused, and while it is open no direct action runs.
    let leave_view = |ui: &AppWindow, open: bool| {
        let leave = ui.global::<crate::RoomLeave>();
        let mut view = leave.get_view();
        view.open = open;
        leave.set_view(view);
    };
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    crate::withdraw_call_bar(ui);
    leave_view(ui, true);
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    leave_view(ui, false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(refused(), 9);
    assert_eq!(acted().len(), 7);
    leave_view(ui, true);
    settle(ui).await;
    assert!(!control_bounds(ui, "Call active · Mic on · Camera on").is_empty());
    not_handed_off(&|| {
        bar.invoke_return_to_call();
        bar.invoke_leave();
        bar.invoke_mute();
        bar.invoke_stop_camera();
    });
    settle(ui).await;
    assert_eq!((acted().len(), refused(), ui.get_page()), (7, 9, 0));
    leave_view(ui, false);
    settle(ui).await;
    assert!(ui.get_call_bar_visible());
    // The bar stays available in ordinary Settings.
    ui.set_page(1);
    settle(ui).await;
    assert!(!ui.get_call_bar_protected() && ui.get_call_bar_visible());
    ui.set_page(0);
    settle(ui).await;
    // The keyboard: Tab and Backtab walk the bar's controls in order (Deafen
    // only where the bar is wide), and Tab leaves the bar after Leave.
    let wide = !control_bounds(ui, "Deafen speaker").is_empty();
    let order: Vec<&str> = [
        "Mute microphone",
        "Deafen speaker",
        "Stop camera",
        "Return to the call page",
        "Leave call",
    ]
    .into_iter()
    .filter(|label| wide || *label != "Deafen speaker")
    .collect();
    request_bridge_focus(ui, order[0]);
    for label in &order[1..] {
        key(ui, Key::Tab);
        assert!(focused_control_has_label(ui, label), "Tab reaches {label}");
    }
    for label in order[..order.len() - 1].iter().rev() {
        key(ui, Key::Backtab);
        assert!(
            focused_control_has_label(ui, label),
            "Backtab reaches {label}"
        );
    }
    request_bridge_focus(ui, "Leave call");
    key(ui, Key::Tab);
    let left = order
        .iter()
        .all(|label| !focused_control_has_label(ui, label));
    assert!(left, "Tab leaves the bar after Leave");
    key(ui, Key::Backtab);
    assert!(focused_control_has_label(ui, "Leave call"));
    settle(ui).await;
    // The smallest windows, including the actual minimum, at 200% text with
    // every capture indicator shown: every control and indicator is inside
    // the window, in its top half, so the composer keeps its place.
    ui.set_native_screen_active(true);
    ui.set_font_scale_percent(200);
    for (width, height) in [(940, 620), (640, 620)] {
        let size = slint::PhysicalSize::new(width, height);
        ui.window().set_size(size);
        settle(ui).await;
        let window = ui.window().size().to_logical(ui.window().scale_factor());
        for label in [
            "Mute microphone",
            "Stop camera",
            "Stop screen sharing",
            "Return to the call page",
            "Leave call",
            "Mic on",
            "Camera on",
            "Sharing screen",
        ] {
            let bounds = control_bounds(ui, label);
            assert!(
                !bounds.is_empty(),
                "{label} is shown at {width}x{height}, 200%"
            );
            for bounds in bounds {
                assert!(
                    bounds.left >= 0.0
                        && bounds.right <= window.width + 0.5
                        && bounds.top >= 0.0
                        && bounds.bottom <= window.height / 2.0,
                    "{label} at {width}x{height}, 200%: {bounds:?}"
                );
            }
        }
    }
    // Return at the minimum window and 200% text with no tile: the stage is
    // too narrow for its participant list, so the stage itself takes focus,
    // visibly inside the window, and only then does the request end.
    bar.invoke_return_to_call();
    settle(ui).await;
    assert_eq!(ui.get_page(), 2);
    let people = ui.get_native_call_participants().row_count();
    let stage = format!("Call stage, {people} in call");
    settle_until(ui, runtime, || focused_control_has_label(ui, &stage)).await;
    assert!(!focused_control_has_label(ui, "Leave native call"));
    let window = ui.window().size().to_logical(ui.window().scale_factor());
    let focused = focused_control_bounds(ui).expect("a focused control");
    assert!(
        focused.right > focused.left
            && focused.bottom > focused.top
            && focused.left >= 0.0
            && focused.right <= window.width + 0.5
            && focused.top >= 0.0
            && focused.bottom <= window.height + 0.5,
        "stage focus at 640x620, 200%: {focused:?}"
    );
    assert!(!ui.get_call_stage_focus_pending());
    ui.set_page(0);
    settle(ui).await;
    // The protected badge at the minimum window and 200% text: inside the
    // window, with its whole status shown (its words wrap, never elide).
    ui.set_show_sign_in(true);
    settle(ui).await;
    let window = ui.window().size().to_logical(ui.window().scale_factor());
    let summary = "Call active · Mic on · Camera on · Sharing screen";
    let badges = control_bounds(ui, summary);
    assert_eq!(badges.len(), 1, "one badge carries the summary");
    let texts = text_item_bounds(ui, summary);
    assert_eq!(texts.len(), 1, "the badge shows the whole summary");
    let (badge, text) = (badges[0], texts[0]);
    assert!(
        badge.left >= 0.0 && badge.right <= window.width + 0.5 && badge.top >= 0.0,
        "badge at 640x620, 200%: {badge:?}"
    );
    assert!(
        text.left >= badge.left
            && text.right <= badge.right + 0.5
            && text.top >= badge.top - 0.5
            && text.bottom <= badge.bottom + 0.5,
        "badge text {text:?} inside {badge:?}"
    );
    ui.set_show_sign_in(false);
    ui.set_native_screen_active(false);
    // The call page's unbound handlers were never used by the bar.
    assert!(stopped.borrow().is_empty());
    assert_eq!((toggled.get(), ended.get()), (0, 0));
    // The responsive header's actual More opener is another protected
    // entry. Open and close by its accessibility action without a change
    // handler between them: the held command must stay withdrawn. While it
    // is open the private bar and direct media/navigation actions are held.
    // This extends the existing fixture; all earlier command IDs are drained.
    // Identity details use the same admitted-entry order: withdrawal before
    // the visible flag, closed again without a change handler. The actual
    // scoped showDetails/ViewMember choke points are checked in call_bar's
    // existing entry-order test; this case exercises the real bar controller.
    let identity = ui.global::<crate::IdentityWarnings>();
    let identity_room = ui.get_active_room_id();
    ui.set_active_room_id(room.into());
    settle(ui).await;
    assert!(ui.get_identity_warnings_allowed());
    let acted_before_identity = acted().len();
    let refused_before_identity = refused();
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    crate::withdraw_call_bar(ui);
    identity.set_open(true);
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    not_handed_off(&|| {
        bar.invoke_return_to_call();
        bar.invoke_leave();
        bar.invoke_mute();
        bar.invoke_stop_camera();
    });
    identity.set_open(false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), acted_before_identity);
    assert_eq!(refused(), refused_before_identity + 1);
    ui.set_active_room_id(identity_room);
    // More's real opening must also retire the old identity focus intent.
    settle(ui).await;
    assert!(ui.get_header_overflow());
    let acted_before_more = acted().len();
    let refused_before_more = refused();
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    let more = control_with_label(ui, "More room actions");
    identity.set_return_focus(true);
    identity.set_awaiting_inspector(true);
    assert!(identity.get_return_focus() && identity.get_awaiting_inspector());
    more.accessible_action(&slint::private_unstable_api::re_exports::AccessibilityAction::Default);
    assert!(ui.get_header_more_open());
    assert!(!identity.get_return_focus() && !identity.get_awaiting_inspector());
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    not_handed_off(&|| {
        bar.invoke_return_to_call();
        bar.invoke_leave();
        bar.invoke_mute();
        bar.invoke_stop_camera();
    });
    more.accessible_action(&slint::private_unstable_api::re_exports::AccessibilityAction::Default);
    assert!(!ui.get_header_more_open());
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), acted_before_more);
    assert_eq!(refused(), refused_before_more + 1);
    assert!(stopped.borrow().is_empty());
    assert_eq!((toggled.get(), ended.get()), (0, 0));
    // The invitations center's actual Home entry is another protected entry:
    // admitted, it withdraws the held command before its flag, and closed
    // again without a change handler the command stays withdrawn. While the
    // center is open the bar and Return-to-call are held.
    let center = ui.global::<crate::InvitationsState>();
    let acted_before_center = acted().len();
    let refused_before_center = refused();
    let release = hold();
    let op = handed_off(&|| bar.invoke_mute());
    settle(ui).await;
    ui.invoke_open_invitations();
    assert!(center.get_open(), "the center's real entry opened it");
    assert!(ui.get_call_bar_protected() && !ui.get_call_bar_visible());
    not_handed_off(&|| {
        bar.invoke_return_to_call();
        bar.invoke_leave();
        bar.invoke_mute();
        bar.invoke_stop_camera();
    });
    assert_eq!(ui.get_page(), 0, "Return is held under the center");
    center.set_open(false);
    release.send(()).unwrap();
    settle_until(ui, runtime, finished(op)).await;
    assert_eq!(acted().len(), acted_before_center);
    assert_eq!(refused(), refused_before_center + 1);
    // The call ends: the bar and its timer go.
    clear_for_test(ui);
    settle(ui).await;
    assert!(!bar.get_active() && bar.get_elapsed().is_empty());
    assert!(!bar.get_can_leave() && !bar.get_can_control());
    ui.set_call_connected(false);
    ui.set_native_call_joined(false);
    ui.set_native_call_room_display("".into());
    ui.set_native_audio_ready(false);
    ui.set_native_microphone_muted(true);
    ui.set_native_camera_active(false);
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_live_session(saved.0);
    ui.set_page(saved.1);
    ui.set_show_sign_in(saved.2);
    settle(ui).await;
}

/// Window-relative bounds of one real generated control: left, top, right,
/// bottom.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// Window-relative bounds of one real item. Pinned Slint's `map_to_window`
/// maps a point given in the item's parent coordinate system and does not add
/// the item's own x and y, so the item's `geometry()` (which is in that parent
/// system) is mapped by its origin, never by the zero point: mapping zero
/// would report the parent's origin and under-report every offset control.
fn item_bounds(item: &slint::private_unstable_api::re_exports::ItemRc) -> Bounds {
    let geometry = item.geometry();
    let origin = item.map_to_window(geometry.origin);
    Bounds {
        left: origin.x,
        top: origin.y,
        right: origin.x + geometry.size.width,
        bottom: origin.y + geometry.size.height,
    }
}

/// Where the real generated controls with a given accessible label sit, as
/// window-relative bounds. Empty when none is instantiated.
fn control_bounds(ui: &AppWindow, label: &str) -> Vec<Bounds> {
    labelled_bounds(ui, label, None)
}

fn button_bounds(ui: &AppWindow, label: &str) -> Vec<Bounds> {
    use slint::private_unstable_api::re_exports::AccessibleRole;

    labelled_bounds(ui, label, Some(AccessibleRole::Button))
}

fn labelled_bounds(
    ui: &AppWindow,
    label: &str,
    role: Option<slint::private_unstable_api::re_exports::AccessibleRole>,
) -> Vec<Bounds> {
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut bounds = Vec::new();
    let _ = root.visit_descendants(|item| {
        if role.is_none_or(|role| item.accessible_role() == role)
            && item
                .accessible_string_property(AccessibleStringProperty::Label)
                .as_deref()
                == Some(label)
        {
            bounds.push(item_bounds(item));
        }
        ControlFlow::<()>::Continue(())
    });
    bounds
}

/// Proof that the bounds helpers map the item's own origin: for every
/// labelled control whose own x or y is nonzero, the reported bounds differ
/// from a zero-point mapping by exactly that offset, and at least one such
/// control exists among the given labels (otherwise the check proves nothing).
fn assert_bounds_use_item_origin(ui: &AppWindow, labels: &[&str]) {
    use slint::private_unstable_api::re_exports::{
        AccessibleStringProperty, ItemRc, LogicalPoint, WindowInner,
    };
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut offset_controls = 0;
    let _ = root.visit_descendants(|item| {
        let label = item.accessible_string_property(AccessibleStringProperty::Label);
        if !labels.iter().any(|wanted| label.as_deref() == Some(wanted)) {
            return ControlFlow::<()>::Continue(());
        }
        let geometry = item.geometry();
        let naive = item.map_to_window(LogicalPoint::default());
        let bounds = item_bounds(item);
        assert!((bounds.left - (naive.x + geometry.origin.x)).abs() < 0.01);
        assert!((bounds.top - (naive.y + geometry.origin.y)).abs() < 0.01);
        if geometry.origin.x.abs() > 0.5 || geometry.origin.y.abs() > 0.5 {
            offset_controls += 1;
            assert!(
                (bounds.left - naive.x).abs() > 0.5 || (bounds.top - naive.y).abs() > 0.5,
                "a control with its own offset must not report its parent's origin: {bounds:?}"
            );
        }
        ControlFlow::<()>::Continue(())
    });
    assert!(
        offset_controls > 0,
        "at least one checked control must have a nonzero own x or y"
    );
}

/// Left and right edges only, for callers that check the horizontal axis.
fn control_extents(ui: &AppWindow, label: &str) -> Vec<(f32, f32)> {
    control_bounds(ui, label)
        .into_iter()
        .map(|bounds| (bounds.left, bounds.right))
        .collect()
}

/// Failure-only, bounded geometry diagnostics. Read the target's ancestor
/// chain without changing focus, scrolling, callbacks or widget properties.
/// Only numeric geometry and static compiler element names/IDs are reported.
fn clipped_control_geometry(ui: &AppWindow, label: &str) -> Vec<String> {
    use slint::private_unstable_api::re_exports::AccessibleStringProperty;

    let nodes = geometry_nodes(ui);
    let (width, _) = window_size(ui);
    let target = nodes.iter().find(|item| {
        item.accessible_string_property(AccessibleStringProperty::Label)
            .as_deref()
            == Some(label)
            && {
                let bounds = item_bounds(item);
                bounds.left < -0.5 || bounds.right > width + 0.5
            }
    });
    geometry_trace(ui, &nodes, target)
}

/// The same failure-only trace for the control that has keyboard focus,
/// whichever way its bounds failed.
fn focused_control_geometry(ui: &AppWindow) -> Vec<String> {
    use slint::private_unstable_api::re_exports::WindowInner;

    let nodes = geometry_nodes(ui);
    let focused = {
        let window = WindowInner::from_pub(ui.window());
        let item = window.focus_item.borrow().upgrade();
        item
    };
    geometry_trace(ui, &nodes, focused.as_ref())
}

const GEOMETRY_NODE_LIMIT: usize = 4096;

fn geometry_nodes(ui: &AppWindow) -> Vec<slint::private_unstable_api::re_exports::ItemRc> {
    use slint::private_unstable_api::re_exports::{ItemRc, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut nodes = vec![root.clone()];
    root.visit_descendants(|item| {
        if nodes.len() >= GEOMETRY_NODE_LIMIT {
            return ControlFlow::Break(());
        }
        nodes.push(item.clone());
        ControlFlow::Continue(())
    });
    nodes
}

fn geometry_trace(
    ui: &AppWindow,
    nodes: &[slint::private_unstable_api::re_exports::ItemRc],
    target: Option<&slint::private_unstable_api::re_exports::ItemRc>,
) -> Vec<String> {
    use slint::private_unstable_api::re_exports::{
        BorderRectangle, Clip, Empty, Flickable, FocusScope, Opacity, Rectangle,
    };
    use std::ops::ControlFlow;

    const NODE_LIMIT: usize = GEOMETRY_NODE_LIMIT;
    const PATH_LIMIT: usize = 24;
    let root = &nodes[0];
    let (width, height) = window_size(ui);
    let mut trace = vec![format!(
        "window={width}x{height}; root={:?}; panes={}/{}/{}; scale={}; nodes={}; node_cap_reached={}",
        item_bounds(root),
        ui.get_rail_width_px(),
        ui.get_room_list_width(),
        ui.get_details_width(),
        ui.get_font_scale_percent(),
        nodes.len(),
        nodes.len() >= NODE_LIMIT
    )];
    let Some(target) = target else {
        trace.push("no matching target found".into());
        return trace;
    };
    for node in nodes {
        if trace.len() >= PATH_LIMIT {
            trace.push("ancestor trace capped".into());
            break;
        }
        let mut contains = node == target;
        if !contains {
            let mut visited = 0;
            node.visit_descendants(|child| {
                visited += 1;
                if child == target {
                    contains = true;
                    return ControlFlow::Break(());
                }
                if visited >= NODE_LIMIT {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            });
        }
        if contains {
            let role = if node.downcast::<Flickable>().is_some() {
                "flickable"
            } else if node.downcast::<FocusScope>().is_some() {
                "focus"
            } else if node.downcast::<Clip>().is_some() {
                "clip"
            } else if node.downcast::<BorderRectangle>().is_some() {
                "border"
            } else if node.downcast::<Rectangle>().is_some() {
                "rectangle"
            } else if node.downcast::<Opacity>().is_some() {
                "opacity"
            } else if node.downcast::<Empty>().is_some() {
                "empty"
            } else {
                "other"
            };
            let names: Vec<_> = node
                .element_type_names_and_ids(0)
                .unwrap_or_default()
                .into_iter()
                .take(2)
                .map(|(kind, id)| {
                    format!(
                        "{}:{}",
                        kind.chars().take(32).collect::<String>(),
                        id.chars().take(32).collect::<String>()
                    )
                })
                .collect();
            let geometry = node.geometry();
            trace.push(format!(
                "{role} {:?}; local=({},{},{},{}); window={:?}",
                names,
                geometry.origin.x,
                geometry.origin.y,
                geometry.size.width,
                geometry.size.height,
                item_bounds(node)
            ));
            if let Some(flickable) = node.downcast::<Flickable>() {
                let flickable = flickable.as_pin_ref();
                trace.push(format!(
                    "canvas=({},{},{},{})",
                    flickable.content_x().0,
                    flickable.content_y().0,
                    flickable.content_width().0,
                    flickable.content_height().0
                ));
            }
        }
    }
    trace
}

/// Failure-only geometry: root bounds, then at most sixteen scroll viewports
/// with their content origins and extents. No labels, IDs, text or paths.
fn motion_layout_geometry(ui: &AppWindow) -> Vec<[f32; 8]> {
    use slint::private_unstable_api::re_exports::Flickable;
    let nodes = geometry_nodes(ui);
    let root = item_bounds(&nodes[0]);
    let mut rows = vec![[
        root.left,
        root.top,
        root.right,
        root.bottom,
        0.0,
        0.0,
        0.0,
        0.0,
    ]];
    for node in &nodes {
        if let Some(scroll) = node.downcast::<Flickable>() {
            let bounds = item_bounds(node);
            let scroll = scroll.as_pin_ref();
            rows.push([
                bounds.left,
                bounds.top,
                bounds.right,
                bounds.bottom,
                scroll.content_x().0,
                scroll.content_y().0,
                scroll.content_width().0,
                scroll.content_height().0,
            ]);
            if rows.len() == 17 {
                break;
            }
        }
    }
    rows
}

/// Bounds of the control that currently has keyboard focus, if any.
fn focused_control_bounds(ui: &AppWindow) -> Option<Bounds> {
    use slint::private_unstable_api::re_exports::WindowInner;

    let window = WindowInner::from_pub(ui.window());
    let focused = window.focus_item.borrow().upgrade()?;
    Some(item_bounds(&focused))
}

/// Weak identity distinguishes each record's control without retaining a
/// removed repeated item across keyboard dispatches.
fn focused_control_identity(
    ui: &AppWindow,
) -> Option<slint::private_unstable_api::re_exports::ItemWeak> {
    use slint::private_unstable_api::re_exports::WindowInner;
    let window = WindowInner::from_pub(ui.window());
    let identity = window
        .focus_item
        .borrow()
        .upgrade()
        .map(|item| item.downgrade());
    identity
}

fn window_size(ui: &AppWindow) -> (f32, f32) {
    let size = ui.window().size().to_logical(ui.window().scale_factor());
    (size.width, size.height)
}

fn inside_window(ui: &AppWindow, bounds: Bounds) -> bool {
    let (width, height) = window_size(ui);
    bounds.right > bounds.left
        && bounds.bottom > bounds.top
        && bounds.left >= -0.5
        && bounds.top >= -0.5
        && bounds.right <= width + 0.5
        && bounds.bottom <= height + 0.5
}

fn assert_details_pane_inside_window(ui: &AppWindow) {
    let bounds = control_bounds(ui, "Conversation details");
    assert_eq!(bounds.len(), 1, "one visible details pane");
    assert!(
        inside_window(ui, bounds[0]),
        "the complete details pane must fit: {:?}; path={:?}",
        bounds[0],
        clipped_control_geometry(ui, "Conversation details")
    );
}

/// Every saved action must lie within its scrollable content, including
/// wrapped captions/statuses below the initial viewport. A window-bounds check
/// alone cannot detect a scroll range shorter than its laid-out children.
fn assert_record_scroll_range(ui: &AppWindow, labels: &[&str], threaded: bool) {
    use slint::private_unstable_api::re_exports::{
        AccessibleStringProperty, Flickable, ItemRc, WindowInner,
    };
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let midpoint = window_size(ui).0 / 2.0;
    let mut targets = Vec::new();
    let mut visited = 0;
    root.visit_descendants(|item| {
        visited += 1;
        let label = item.accessible_string_property(AccessibleStringProperty::Label);
        if labels.iter().any(|wanted| label.as_deref() == Some(wanted))
            && (item_bounds(item).left >= midpoint) == threaded
        {
            targets.push(item.clone());
        }
        if visited >= 4096 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    assert_eq!(
        targets.len(),
        28,
        "eight records retain all expected actions"
    );
    for target in targets {
        let bounds = item_bounds(&target);
        let mut ancestors = 0;
        root.visit_descendants(|item| {
            if let Some(flickable) = item.downcast::<Flickable>() {
                let contains = item.visit_descendants(|child| {
                    if child == &target { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
                }).is_some();
                if contains {
                    ancestors += 1;
                    let f = flickable.as_pin_ref();
                    let viewport = item_bounds(item);
                    let content_bottom = viewport.top + f.content_y().0 + f.content_height().0;
                    assert!(bounds.bottom <= content_bottom + 0.5,
                        "saved action exceeds its scroll range: action={bounds:?}, content_bottom={content_bottom}, threaded={threaded}");
                }
            }
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(
            ancestors, 1,
            "each action belongs to one saved-send viewport"
        );
    }
}

/// Walk the real tab order to a saved-send control with a budget sized for a
/// strip of eight records. The account and appearance walks keep their own
/// `tab_to_control` budget; this helper does not change it.
fn tab_to_record_control(ui: &AppWindow, label: &str) {
    for _ in 0..400 {
        if focused_control_has_label(ui, label) {
            return;
        }
        key(ui, Key::Tab);
    }
    panic!("Saved-send control is not keyboard reachable: {label}");
}

const CANCEL_LABEL: &str =
    "Cancel sending this message; a message that already reached the server stays sent";
const KEEP_LABEL: &str = "Not sent: keep this message's text";
const SENT_LABEL: &str = "It was sent: drop this saved message without resending";
const RESTORE_LABEL: &str = "Restore this message's text into the empty composer";
const COPY_LABEL: &str = "Copy this message's text to the system clipboard";
const STICKER_SEARCH: &str = "Search stickers by description, shortcode or pack";
const STICKER_CONFIRM: &str = "I checked the conversation: allow sending stickers again";
const DISCARD_UNREADABLE_LABEL: &str =
    "Discard saved send status this version cannot read; your typed text stays";

/// The real sticker popup and send-status strip. Opaque keys route through
/// the generated callbacks; the search field owns arrows, Return and Escape;
/// consent is an explicit keyboard-reachable action; no image element exists
/// before consent and Return cannot send while sending is not allowed.
async fn assert_sticker_picker_projection(ui: &AppWindow) {
    use crate::StickerItem;
    use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    ui.set_show_sign_in(false);
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("presentation-only".into());
    ui.set_active_room_id("!presentation:example.org".into());
    ui.set_composer_mode(0);
    ui.set_composer_busy(false);
    let navigated: Rc<RefCell<Vec<i32>>> = Rc::default();
    let queries: Rc<RefCell<Vec<String>>> = Rc::default();
    let sent: Rc<RefCell<Vec<String>>> = Rc::default();
    let consents: Rc<RefCell<Vec<bool>>> = Rc::default();
    let dismissed = Rc::new(Cell::new(0));
    let cancelled = Rc::new(Cell::new(0));
    let acknowledged = Rc::new(Cell::new(0));
    let sink = navigated.clone();
    ui.on_sticker_navigate(move |offset| sink.borrow_mut().push(offset));
    let sink = queries.clone();
    ui.on_sticker_query(move |query| sink.borrow_mut().push(query.to_string()));
    let sink = sent.clone();
    ui.on_sticker_send(move |key| sink.borrow_mut().push(key.to_string()));
    let sink = consents.clone();
    ui.on_sticker_consent(move |allowed| sink.borrow_mut().push(allowed));
    let sink = dismissed.clone();
    ui.on_sticker_dismiss(move || sink.set(sink.get() + 1));
    let sink = cancelled.clone();
    ui.on_sticker_send_cancel(move || sink.set(sink.get() + 1));
    let sink = acknowledged.clone();
    ui.on_sticker_send_dismiss(move || sink.set(sink.get() + 1));

    ui.set_sticker_items(ModelRc::new(VecModel::from(vec![
        StickerItem {
            key: "41:0".into(),
            label: "Waving fox".into(),
            pack: "Forest friends".into(),
        },
        StickerItem {
            key: "41:1".into(),
            label: "Sleeping owl".into(),
            pack: "Forest friends".into(),
        },
    ])));
    ui.set_sticker_packs(ModelRc::new(VecModel::from(vec![
        SharedString::from("All packs"),
        SharedString::from("Forest friends"),
    ])));
    ui.set_sticker_index(0);
    ui.set_sticker_phase(1);
    ui.set_sticker_status("2 stickers in 1 pack.".into());
    ui.set_sticker_needs_consent(true);
    ui.set_sticker_can_send(false);
    ui.set_sticker_preview_status("Images appear after you allow sticker images here.".into());
    ui.set_sticker_threaded(false);
    ui.set_sticker_open(true);
    settle(ui).await;
    assert!(
        ui.get_mention_ui_allowed(),
        "fixture is a visible idle room composer"
    );
    assert_eq!(control_bounds(ui, "Sticker picker").len(), 1);
    assert_eq!(
        control_bounds(ui, "Sticker Waving fox from Forest friends, selected").len(),
        1
    );
    assert_eq!(
        control_bounds(ui, "Sticker Sleeping owl from Forest friends").len(),
        1
    );
    // TextAction's caption is a separate accessible text node with this
    // same label. Require exactly one actionable button, not a caption.
    assert_eq!(button_bounds(ui, "Allow sticker images here").len(), 1);
    // A placed action keeps a real height: an action sized by a zero
    // preferred height is still focusable but has no visible target.
    for label in [
        "Allow sticker images here",
        "Cancel and close the sticker picker",
    ] {
        let bounds = button_bounds(ui, label);
        assert_eq!(bounds.len(), 1, "{label}");
        assert!(
            bounds[0].bottom - bounds[0].top >= 31.5 && bounds[0].right > bounds[0].left,
            "{label} has no visible target: {:?}",
            bounds[0]
        );
    }
    assert!(
        control_bounds(ui, "Sticker preview: Waving fox").is_empty(),
        "no image is presented before consent"
    );

    // The search field takes focus when the popup opens.
    key(ui, Key::DownArrow);
    key(ui, Key::UpArrow);
    assert_eq!(navigated.borrow().as_slice(), [1, -1]);
    key(ui, Key::Return);
    assert!(
        sent.borrow().is_empty(),
        "Return cannot send before it is allowed"
    );
    type_text(ui, "owl");
    assert_eq!(ui.get_sticker_query_text().as_str(), "owl");
    assert_eq!(queries.borrow().last().map(String::as_str), Some("owl"));
    key(ui, Key::Escape);
    assert_eq!(
        dismissed.get(),
        1,
        "Escape in the search field closes the picker"
    );

    tab_to_control(ui, "Allow sticker images here");
    key(ui, Key::Return);
    assert_eq!(consents.borrow().as_slice(), [true]);
    tab_to_control(ui, "Sticker Sleeping owl from Forest friends");
    key(ui, Key::Escape);
    assert_eq!(
        dismissed.get(),
        2,
        "Escape on a sticker row closes the picker"
    );

    // The controller publishes permission and the decoded preview. Move focus
    // to Send before the consent prompt disappears from the tree.
    ui.set_sticker_can_send(true);
    ui.set_sticker_preview_image(Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(
        2, 2,
    )));
    ui.set_sticker_preview_loaded(true);
    settle(ui).await;
    tab_to_control(ui, "Send sticker");
    ui.set_sticker_needs_consent(false);
    settle(ui).await;
    assert!(button_bounds(ui, "Allow sticker images here").is_empty());
    assert_eq!(control_bounds(ui, "Sticker preview: Waving fox").len(), 1);
    key(ui, Key::Return);
    assert_eq!(sent.borrow().as_slice(), ["41:0"]);

    // After enqueue the popup closes and the exact request's controls remain.
    ui.set_sticker_open(false);
    ui.set_sticker_send_threaded(false);
    ui.set_sticker_send_status("Sticker could not be sent. Retry it or cancel it.".into());
    ui.set_sticker_send_can_cancel(true);
    ui.set_sticker_send_can_retry(true);
    ui.set_sticker_send_visible(true);
    settle(ui).await;
    assert!(control_bounds(ui, "Sticker picker").is_empty());
    assert_eq!(button_bounds(ui, "Retry this sticker").len(), 1);
    tab_to_control(ui, "Cancel this sticker");
    key(ui, Key::Return);
    assert_eq!(cancelled.get(), 1);
    assert_eq!(
        sent.borrow().len(),
        1,
        "status controls never send another sticker"
    );

    // At 200% text the room composer and the narrow thread pane both keep
    // every picker control keyboard reachable and inside the window; the
    // thread pane stacks them in a scrolling area instead of squeezing a row.
    let previous_scale = ui.get_font_scale_percent();
    ui.set_font_scale_percent(200);
    ui.set_sticker_send_visible(false);
    for threaded in [false, true] {
        let scope = if threaded { "thread" } else { "room" };
        if threaded {
            ui.set_thread_root_id("$sticker-root:example.org".into());
            ui.invoke_show_thread_panel();
        }
        ui.set_sticker_threaded(threaded);
        ui.set_sticker_needs_consent(true);
        ui.set_sticker_can_send(false);
        ui.set_sticker_preview_loaded(false);
        for label in [
            STICKER_SEARCH,
            "Sticker pack",
            "Close sticker picker",
            "Allow sticker images here",
            "Cancel and close the sticker picker",
        ] {
            // Reopen so the walk starts at the search field every time.
            ui.set_sticker_open(false);
            settle(ui).await;
            ui.set_sticker_open(true);
            settle(ui).await;
            tab_to_control(ui, label);
            let bounds = focused_control_bounds(ui).expect("a focused sticker control");
            // The trace is built only on failure and holds static element
            // names and numeric geometry.
            assert!(
                inside_window(ui, bounds),
                "{scope}: {label} is outside the window: {bounds:?}; {:?}",
                focused_control_geometry(ui)
            );
            if threaded {
                let pane = control_bounds(ui, "Conversation details")[0];
                assert!(
                    bounds.left >= pane.left - 0.5 && bounds.right <= pane.right + 0.5,
                    "{scope}: {label} leaves the thread pane: {bounds:?} of {pane:?}"
                );
            }
        }
        // Consent accepted and the controller allows sending: Send is
        // reachable and sends; Close is reachable and closes.
        ui.set_sticker_needs_consent(false);
        ui.set_sticker_can_send(true);
        ui.set_sticker_open(false);
        settle(ui).await;
        ui.set_sticker_open(true);
        settle(ui).await;
        tab_to_control(ui, "Send sticker");
        let bounds = focused_control_bounds(ui).expect("a focused Send action");
        assert!(
            inside_window(ui, bounds),
            "{scope}: Send is outside the window: {bounds:?}; {:?}",
            focused_control_geometry(ui)
        );
        let sends = sent.borrow().len();
        key(ui, Key::Return);
        assert_eq!(sent.borrow().len(), sends + 1, "{scope}: Send");
        ui.set_sticker_open(false);
        settle(ui).await;
        ui.set_sticker_open(true);
        settle(ui).await;
        tab_to_control(ui, "Close sticker picker");
        let closes = dismissed.get();
        key(ui, Key::Return);
        assert_eq!(dismissed.get(), closes + 1, "{scope}: Close");
        // An uncertain send offers only the explicit confirmation.
        ui.set_sticker_open(false);
        ui.set_sticker_send_threaded(threaded);
        ui.set_sticker_send_status("This sticker was not found in the queue.".into());
        ui.set_sticker_send_can_cancel(false);
        ui.set_sticker_send_can_retry(false);
        ui.set_sticker_send_uncertain(true);
        ui.set_sticker_send_visible(true);
        settle(ui).await;
        assert!(control_bounds(ui, "Dismiss sticker status").is_empty());
        tab_to_record_control(ui, STICKER_CONFIRM);
        let bounds = focused_control_bounds(ui).expect("a focused confirmation");
        assert!(
            inside_window(ui, bounds),
            "{scope}: confirmation is outside"
        );
        let confirmations = acknowledged.get();
        key(ui, Key::Return);
        assert_eq!(acknowledged.get(), confirmations + 1);
        ui.set_sticker_send_visible(false);
        ui.set_sticker_send_uncertain(false);
        settle(ui).await;
    }
    ui.set_font_scale_percent(previous_scale);
    ui.set_thread_root_id("".into());
    ui.set_sticker_threaded(false);
    ui.set_sticker_open(false);
    settle(ui).await;

    ui.set_sticker_send_visible(false);
    ui.set_sticker_send_status("".into());
    ui.set_sticker_send_can_cancel(false);
    ui.set_sticker_send_can_retry(false);
    ui.set_sticker_items(ModelRc::new(VecModel::from(Vec::<StickerItem>::new())));
    ui.set_sticker_packs(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_sticker_index(-1);
    ui.set_sticker_phase(0);
    ui.set_sticker_status("".into());
    ui.set_sticker_preview_image(Image::default());
    ui.set_sticker_preview_loaded(false);
    ui.set_sticker_preview_status("".into());
    ui.set_sticker_query_text("".into());
    ui.set_sticker_can_send(false);
    settle(ui).await;
}

/// The per-record saved-send surface in the real generated window: one block
/// per record with its own exact controls, keyboard reachable, routing the
/// record's opaque key and never another record's; the narrow thread pane at
/// 200% text stacks the controls inside the window instead of clipping them.
async fn assert_recovery_strip_projection(ui: &AppWindow) {
    use crate::draft_recovery;
    use crate::drafts::{PendingItem, PendingStatus, PendingSummary};
    use morpheus_matrix::Generation;
    use std::{cell::RefCell, rc::Rc};

    let previous_scale = ui.get_font_scale_percent();
    // Earlier account scenarios leave the sign-in surface open. This fixture
    // represents a selected room, not keyboard navigation behind a dialog.
    ui.set_show_sign_in(false);
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_show_side_panel(true);
    ui.set_active_account_local_id("presentation-only".into());
    ui.set_active_room_id("!presentation:example.org".into());
    ui.set_composer_text("".into());
    let waiting = Generation::fresh();
    let unconfirmed = Generation::fresh();
    let kept = Generation::fresh();
    let items = vec![
        PendingItem {
            nonce: waiting.clone(),
            status: PendingStatus::Queued,
            preview: "first fictional message".into(),
            has_request: true,
        },
        PendingItem {
            nonce: unconfirmed.clone(),
            status: PendingStatus::Unconfirmed,
            preview: "second fictional message".into(),
            has_request: false,
        },
        PendingItem {
            nonce: kept.clone(),
            status: PendingStatus::Kept,
            preview: "third fictional message".into(),
            has_request: false,
        },
    ];
    let summary = PendingSummary {
        count: 2,
        kept: 1,
        blocked: false,
        unconfirmed: true,
        quarantined: false,
        read_only: false,
    };
    draft_recovery::publish_room(ui, &summary, &items);
    settle(ui).await;
    assert_eq!(ui.get_composer_pending_items().row_count(), 3);
    assert!(ui.get_composer_unconfirmed());
    // Exactly one record offers Cancel, exactly one offers the review pair,
    // every record offers Copy, and Restore appears only for an empty composer.
    assert_eq!(control_extents(ui, CANCEL_LABEL).len(), 1);
    assert_eq!(control_extents(ui, KEEP_LABEL).len(), 1);
    assert_eq!(control_extents(ui, SENT_LABEL).len(), 1);
    assert_eq!(control_extents(ui, COPY_LABEL).len(), 3);
    assert_eq!(control_extents(ui, RESTORE_LABEL).len(), 3);
    assert!(control_extents(ui, DISCARD_UNREADABLE_LABEL).is_empty());
    ui.set_composer_text("typing something newer".into());
    settle(ui).await;
    assert!(
        control_extents(ui, RESTORE_LABEL).is_empty(),
        "restore never offers to overwrite typed text"
    );
    ui.set_composer_text("".into());
    settle(ui).await;

    // Controls route the exact record key of the block they belong to.
    let cancelled: Rc<RefCell<Vec<(bool, String)>>> = Rc::default();
    let reviewed: Rc<RefCell<Vec<(bool, String, bool)>>> = Rc::default();
    let sink = cancelled.clone();
    ui.on_cancel_pending_send(move |threaded, key| {
        sink.borrow_mut().push((threaded, key.to_string()))
    });
    let sink = reviewed.clone();
    ui.on_review_unconfirmed_send(move |threaded, key, sent| {
        sink.borrow_mut().push((threaded, key.to_string(), sent))
    });
    tab_to_control(ui, CANCEL_LABEL);
    key(ui, Key::Return);
    assert_eq!(
        cancelled.borrow().as_slice(),
        [(false, waiting.as_key().to_owned())]
    );
    tab_to_control(ui, KEEP_LABEL);
    key(ui, Key::Return);
    assert_eq!(
        reviewed.borrow().as_slice(),
        [(false, unconfirmed.as_key().to_owned(), false)]
    );
    assert_eq!(
        cancelled.borrow().len(),
        1,
        "the review control never cancels"
    );

    // The fixed notice for unreadable saved data offers only the explicit discard.
    let blocked = PendingSummary {
        quarantined: true,
        ..PendingSummary::default()
    };
    draft_recovery::publish_room(ui, &blocked, &[]);
    settle(ui).await;
    assert!(!ui.get_composer_blocked_notice().is_empty());
    assert_eq!(control_extents(ui, DISCARD_UNREADABLE_LABEL).len(), 1);
    assert!(control_extents(ui, CANCEL_LABEL).is_empty());
    let discards: Rc<RefCell<Vec<bool>>> = Rc::default();
    let sink = discards.clone();
    ui.on_discard_unreadable_status(move |threaded| sink.borrow_mut().push(threaded));
    tab_to_control(ui, DISCARD_UNREADABLE_LABEL);
    key(ui, Key::Return);
    assert_eq!(discards.borrow().as_slice(), [false]);
    draft_recovery::publish_room(ui, &PendingSummary::default(), &[]);
    settle(ui).await;
    assert!(ui.get_composer_blocked_notice().is_empty());

    // Narrow thread pane at 200% text: the same records, stacked controls,
    // every control inside the window and still keyboard reachable.
    ui.set_font_scale_percent(200);
    ui.set_thread_root_id("$presentation-root:example.org".into());
    ui.set_thread_root_sender("@jules:example.org".into());
    ui.set_thread_root_body("Fictional thread root".into());
    ui.set_thread_composer_text("".into());
    tab_to_control(ui, "Show thread");
    key(ui, Key::Return);
    let thread_summary = PendingSummary {
        count: 1,
        kept: 1,
        blocked: false,
        unconfirmed: true,
        quarantined: false,
        read_only: false,
    };
    draft_recovery::publish_thread(ui, &thread_summary, &items[1..]);
    settle(ui).await;
    assert_eq!(ui.get_thread_pending_items().row_count(), 2);
    assert!(ui.get_thread_send_pending());
    assert_details_pane_inside_window(ui);
    let window_width = ui
        .window()
        .size()
        .to_logical(ui.window().scale_factor())
        .width;
    let mut seen = 0;
    for label in [KEEP_LABEL, SENT_LABEL, COPY_LABEL, RESTORE_LABEL] {
        let extents = control_extents(ui, label);
        assert!(
            !extents.is_empty(),
            "{label} must be instantiated in the thread pane"
        );
        for (left, right) in extents {
            assert!(right > left, "{label} has no width");
            assert!(
                left >= 0.0 && right <= window_width + 0.5,
                "{label} is clipped: {left}..{right} of {window_width}; path={:?}",
                clipped_control_geometry(ui, label)
            );
            seen += 1;
        }
    }
    assert!(seen >= 6);
    // Only the first keep-control appears for the only unconfirmed record.
    assert_eq!(control_extents(ui, KEEP_LABEL).len(), 1);
    let thread_reviews: Rc<RefCell<Vec<(bool, String, bool)>>> = Rc::default();
    let sink = thread_reviews.clone();
    ui.on_review_unconfirmed_send(move |threaded, key, sent| {
        sink.borrow_mut().push((threaded, key.to_string(), sent))
    });
    tab_to_control(ui, SENT_LABEL);
    key(ui, Key::Return);
    assert_eq!(
        thread_reviews.borrow().as_slice(),
        [(true, unconfirmed.as_key().to_owned(), true)]
    );
    assert_bounded_strip_with_eight_records(ui).await;
    // The layout must honor the supported window minimum with the same
    // large text and saved pane widths; content cannot move the pane outside it.
    let previous_size = ui.window().size();
    let rail = if ui.get_navigation_mode() == 0 {
        ui.get_rail_width_px()
    } else {
        0
    };
    let minimum_width = (rail + ui.get_room_list_width() + ui.get_details_width() + 360).max(940);
    ui.window()
        .set_size(slint::PhysicalSize::new(minimum_width as u32, 620));
    settle(ui).await;
    assert_details_pane_inside_window(ui);
    ui.window().set_size(previous_size);
    ui.set_font_scale_percent(previous_scale);
    settle(ui).await;
}

/// The strip is a bounded viewport: with the maximum eight records at 200%
/// text, in the wide room composer and in the narrow thread pane, every
/// record and action is retained, the composer input and the conversation
/// keep their space, and each control is scrolled fully into the window on
/// both axes when it receives keyboard focus.
async fn assert_bounded_strip_with_eight_records(ui: &AppWindow) {
    use crate::draft_recovery;
    use crate::drafts::{PendingItem, PendingStatus, PendingSummary};
    use morpheus_matrix::Generation;

    let (window_width, window_height) = window_size(ui);
    let records: Vec<PendingItem> = (0..morpheus_matrix::MAX_PENDING_RECORDS)
        .map(|index| PendingItem {
            nonce: Generation::fresh(),
            status: match index % 4 {
                0 => PendingStatus::Queued,
                1 => PendingStatus::Paused { recoverable: false },
                2 => PendingStatus::Unconfirmed,
                _ => PendingStatus::Kept,
            },
            preview: format!("fictional record {index} with a long enough preview to wrap"),
            has_request: index % 4 < 2,
        })
        .collect();
    let summary = PendingSummary {
        count: 6,
        kept: 2,
        blocked: true,
        unconfirmed: true,
        quarantined: false,
        read_only: false,
    };
    assert_eq!(ui.get_font_scale_percent(), 200);

    // Room composer, wide layout.
    ui.set_composer_text("".into());
    let composer_label = format!("Message composer for {}", ui.get_active_room_name());
    draft_recovery::publish_room(ui, &summary, &records);
    settle(ui).await;
    assert_eq!(ui.get_composer_pending_items().row_count(), 8);
    // Every record keeps its actions: eight Copy controls exist in the tree.
    assert_eq!(
        control_bounds(ui, COPY_LABEL).len(),
        8 + 2,
        "room and thread strips both hold their records"
    );
    // The viewport is bounded: the composer input is still fully visible,
    // leaving conversation space above the strip.
    let composer = control_bounds(ui, &composer_label);
    assert_eq!(
        composer.len(),
        1,
        "the room composer input is instantiated once"
    );
    assert!(
        inside_window(ui, composer[0]),
        "composer input clipped: {:?}",
        composer[0]
    );
    assert!(
        composer[0].top > window_height / 3.0,
        "the strip must not push the composer out of the lower part of the window: {:?}",
        composer[0]
    );
    // Focus walks every control of all eight records down to the composer
    // input; each strip control is scrolled fully into view on both axes
    // when it receives focus, so the deepest record is reachable by keyboard
    // alone. Without a bounded viewport the lower records would sit below the
    // window.
    // The thread strip still shows the projection fixture's two records
    // (unconfirmed and kept, neither owning a request), so only the four room
    // records with a request offer Cancel.
    let cancels = control_bounds(ui, CANCEL_LABEL);
    assert_eq!(
        cancels.len(),
        4,
        "four room records own a request; the inherited thread records do not"
    );
    // The bounds used for every clipping and focus check below map each
    // control's own origin; a control offset inside its row or stack must not
    // report its parent's position.
    assert_bounds_use_item_origin(
        ui,
        &[CANCEL_LABEL, COPY_LABEL, RESTORE_LABEL, &composer_label],
    );
    let strip_labels = [
        KEEP_LABEL,
        SENT_LABEL,
        CANCEL_LABEL,
        COPY_LABEL,
        RESTORE_LABEL,
        "Retry sending this message",
        "Discard this kept text",
    ];
    assert_record_scroll_range(ui, &strip_labels, false);
    tab_to_record_control(ui, CANCEL_LABEL);
    let mut room_strip_controls = 0;
    let mut room_control_identities = Vec::new();
    for _ in 0..400 {
        if focused_control_has_label(ui, &composer_label) {
            break;
        }
        for label in strip_labels {
            if focused_control_has_label(ui, label) {
                let focused = focused_control_bounds(ui).expect("a focused saved-send control");
                assert!(
                    inside_window(ui, focused),
                    "{label} clipped in the room strip: {focused:?}"
                );
                assert!(
                    focused.left < window_width / 2.0,
                    "{label} must sit in the room composer: {focused:?}"
                );
                let identity = focused_control_identity(ui).expect("a focused room action");
                if !room_control_identities.contains(&identity) {
                    room_control_identities.push(identity);
                }
                room_strip_controls += 1;
            }
        }
        key(ui, Key::Tab);
    }
    assert!(
        room_strip_controls >= 28,
        "the walk must traverse all eight room records, saw {room_strip_controls}"
    );
    assert!(
        focused_control_has_label(ui, &composer_label),
        "the tab walk must leave the strip and reach the composer input"
    );
    assert_eq!(
        room_control_identities.len(),
        28,
        "all eight room records must contribute their distinct actions; repeated cycles do not count"
    );
    let focused = focused_control_bounds(ui).unwrap();
    assert!(
        inside_window(ui, focused),
        "composer input not revealed: {focused:?}"
    );

    // Narrow thread pane at 200%: the same eight records, stacked controls,
    // bounded viewport, thread input still visible.
    ui.set_thread_composer_text("".into());
    let thread_label = format!("Reply in thread started by {}", ui.get_thread_root_sender());
    draft_recovery::publish_thread(ui, &summary, &records);
    settle(ui).await;
    assert_eq!(ui.get_thread_pending_items().row_count(), 8);
    let thread_input = control_bounds(ui, &thread_label);
    assert_eq!(
        thread_input.len(),
        1,
        "the thread input is instantiated once"
    );
    assert!(
        inside_window(ui, thread_input[0]),
        "thread input clipped: {:?}",
        thread_input[0]
    );
    assert!(
        thread_input[0].top > window_height / 3.0,
        "the thread strip must leave room above: {:?}",
        thread_input[0]
    );
    assert_record_scroll_range(ui, &strip_labels, true);
    // Walk from the thread tab into the thread strip: every focused control
    // is fully inside the window on both axes, including stacked ones.
    tab_to_control(ui, "Show thread");
    let mut seen_strip_controls = 0;
    let mut thread_control_identities = Vec::new();
    for _ in 0..400 {
        key(ui, Key::Tab);
        if focused_control_has_label(ui, &thread_label) {
            break;
        }
        for label in strip_labels {
            if focused_control_has_label(ui, label) {
                let focused = focused_control_bounds(ui).unwrap();
                assert!(
                    inside_window(ui, focused),
                    "{label} clipped in the thread pane: {focused:?}"
                );
                assert!(
                    focused.left >= window_width / 2.0,
                    "{label} must sit in the side pane: {focused:?}"
                );
                let identity = focused_control_identity(ui).expect("a focused thread action");
                if !thread_control_identities.contains(&identity) {
                    thread_control_identities.push(identity);
                }
                seen_strip_controls += 1;
            }
        }
    }
    assert!(
        seen_strip_controls >= 28,
        "the walk must traverse all eight thread records, saw {seen_strip_controls}"
    );
    assert!(
        focused_control_has_label(ui, &thread_label),
        "the walk must end at the thread input"
    );
    assert_eq!(
        thread_control_identities.len(),
        28,
        "all eight thread records must contribute their distinct actions; repeated cycles do not count"
    );
    draft_recovery::publish_thread(ui, &PendingSummary::default(), &[]);
    draft_recovery::publish_room(ui, &PendingSummary::default(), &[]);
    settle(ui).await;
}

/// The icon a real control draws: its vector paths (outline commands,
/// stroke color and width, window bounds) and any Text it shows.
struct DrawnIcon {
    commands: Vec<String>,
    strokes: Vec<slint::Color>,
    stroke_widths: Vec<f32>,
    bounds: Vec<Bounds>,
    texts: Vec<String>,
}

/// The vector icon inside the first laid-out control labelled `label`.
fn drawn_icon(ui: &AppWindow, label: &str) -> (Bounds, DrawnIcon) {
    use slint::private_unstable_api::re_exports::{
        AccessibleStringProperty, ComplexText, ItemRc, Path, PathData, SimpleText, WindowInner,
    };
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut control = None;
    root.visit_descendants(|item| {
        if item
            .accessible_string_property(AccessibleStringProperty::Label)
            .as_deref()
            == Some(label)
            && item.geometry().size.width > 0.0
        {
            control = Some(item.clone());
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    let control = control.unwrap_or_else(|| panic!("{label} must be shown"));
    let mut drawn = DrawnIcon {
        commands: Vec::new(),
        strokes: Vec::new(),
        stroke_widths: Vec::new(),
        bounds: Vec::new(),
        texts: Vec::new(),
    };
    control.visit_descendants(|item| {
        if let Some(path) = item.downcast::<Path>() {
            let path = path.as_pin_ref();
            if let PathData::Commands(commands) = path.elements() {
                drawn.commands.push(commands.to_string());
            }
            drawn.strokes.push(path.stroke().color());
            drawn.stroke_widths.push(path.stroke_width().get());
            drawn.bounds.push(item_bounds(item));
        } else if let Some(text) = item.downcast::<SimpleText>() {
            drawn.texts.push(text.as_pin_ref().text().to_string());
        } else if let Some(text) = item.downcast::<ComplexText>() {
            drawn.texts.push(text.as_pin_ref().text().to_string());
        }
        ControlFlow::<()>::Continue(())
    });
    (item_bounds(&control), drawn)
}

/// The original vector icons on the real generated window: navigation,
/// header, composer, details tabs and call controls draw one stroked path in
/// the control's current ink (no glyph text), centred inside the control;
/// optical size and stroke weight follow the configured icon size, the 16,
/// 20, 24 and 32 px tab sizes and high contrast; a state change swaps the
/// drawn outline while the control, label and action stay the same.
async fn assert_vector_icons(ui: &AppWindow) {
    let page = ui.get_page();
    let live = ui.get_live_session();
    let space = ui.get_active_space_id();
    let side = ui.get_show_side_panel();
    let tab = ui.get_side_panel_tab();
    let reveal = ui.get_details_reveal_open();
    ui.set_page(0);
    ui.invoke_show_members_panel();
    settle(ui).await;
    let tokens = ui.global::<crate::StyleTokens>();
    let stroke_for = |size: f32, high_contrast: bool| {
        (size * 0.0625 + 0.5).clamp(1.25, 2.75) + if high_contrast { 0.5 } else { 0.0 }
    };
    for icon_size in [18.0_f32, 28.0] {
        tokens.set_icon_size(icon_size);
        settle(ui).await;
        for label in [
            "Home and all rooms",
            "Explore public Matrix rooms",
            "Open call stage",
            "Edit room settings",
            "Hide details panel",
            "Attach files",
            "Open emoji picker for message",
        ] {
            let (control, icon) = drawn_icon(ui, label);
            assert_eq!(icon.commands.len(), 1, "{label} draws one vector icon");
            assert!(!icon.commands[0].is_empty(), "{label} has an outline");
            assert!(icon.texts.is_empty(), "{label} shows no glyph text");
            let bounds = icon.bounds[0];
            let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
            assert!((width - height).abs() < 0.01, "{label} icon is square");
            // IconAction's glyph rule: the configured size, kept inside an
            // explicitly smaller button, never below the built-in 18 px.
            let room = (control.right - control.left).min(control.bottom - control.top) - 6.0;
            let expected = icon_size.min(room.max(18.0));
            assert!(
                (width - expected).abs() < 0.5,
                "{label}: {width} px, expected {expected}"
            );
            assert!(
                (icon.stroke_widths[0] - stroke_for(width, false)).abs() < 0.01,
                "{label} stroke"
            );
            let centre_x =
                (bounds.left + bounds.right) / 2.0 - (control.left + control.right) / 2.0;
            let centre_y =
                (bounds.top + bounds.bottom) / 2.0 - (control.top + control.bottom) / 2.0;
            assert!(
                centre_x.abs() < 0.5 && centre_y.abs() < 0.5,
                "{label} icon is centred"
            );
            assert!(
                bounds.left >= control.left - 0.5 && bounds.right <= control.right + 0.5,
                "{label} icon stays inside its control"
            );
        }
    }
    tokens.set_icon_size(18.0);
    // The selected Home route draws in the active icon ink; Explore does not.
    ui.set_active_space_id("".into());
    settle(ui).await;
    let (_, home) = drawn_icon(ui, "Home and all rooms");
    let (_, explore) = drawn_icon(ui, "Explore public Matrix rooms");
    assert_ne!(home.strokes[0], explore.strokes[0], "selected ink differs");
    assert_ne!(home.commands[0], explore.commands[0], "distinct outlines");
    // High contrast adds half a pixel of stroke to every icon.
    tokens.set_high_contrast(true);
    settle(ui).await;
    let (_, contrast) = drawn_icon(ui, "Home and all rooms");
    assert!((contrast.stroke_widths[0] - stroke_for(18.0, true)).abs() < 0.01);
    tokens.set_high_contrast(false);
    // Details tabs size their icons with the text scale: 16, 20, 24, 32 px.
    for (scale, size) in [(100, 16.0_f32), (125, 20.0), (150, 24.0), (200, 32.0)] {
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        let (control, members) = drawn_icon(ui, "Show members");
        assert_eq!(members.commands.len(), 1);
        let bounds = members.bounds[0];
        assert!(
            (bounds.right - bounds.left - size).abs() < 0.5,
            "{size} px tab icon"
        );
        assert!(
            (bounds.bottom - bounds.top - size).abs() < 0.5,
            "{size} px tab icon"
        );
        assert!((members.stroke_widths[0] - stroke_for(size, false)).abs() < 0.01);
        assert!(bounds.top >= control.top - 0.5 && bounds.bottom <= control.bottom + 0.5);
    }
    ui.set_font_scale_percent(100);
    // A preview call control swaps its outline with its state and keeps its
    // ink; nothing is captured or published.
    // Use an explicit synthetic preview state; earlier phases may have
    // left a live-session presentation or a muted preview on the window.
    ui.set_live_session(false);
    ui.set_page(2);
    settle(ui).await;
    let was_muted = !control_bounds(ui, "Unmute microphone preview").is_empty();
    if was_muted {
        click_control_once(ui, "Unmute microphone preview");
        settle(ui).await;
        key(ui, Key::Tab);
        settle(ui).await;
    }
    let (_, unmuted) = drawn_icon(ui, "Mute microphone preview");
    click_control_once(ui, "Mute microphone preview");
    settle(ui).await;
    let (_, muted) = drawn_icon(ui, "Unmute microphone preview");
    assert_eq!((unmuted.commands.len(), muted.commands.len()), (1, 1));
    assert_ne!(
        unmuted.commands[0], muted.commands[0],
        "muted outline differs"
    );
    assert_eq!(unmuted.strokes[0], muted.strokes[0]);
    let (_, back) = drawn_icon(ui, "Return to chat");
    assert_eq!(back.commands.len(), 1);
    assert!(back.texts.is_empty(), "no legacy R caption");
    // Both preview controls draw in the window's ink.
    assert_eq!(muted.strokes[0], back.strokes[0]);
    // IconAction keeps focus while its label toggles. Move focus before
    // another first-click check; Tab does not activate a preview control.
    key(ui, Key::Tab);
    settle(ui).await;
    click_control_once(ui, "Unmute microphone preview");
    if was_muted {
        settle(ui).await;
        key(ui, Key::Tab);
        settle(ui).await;
        click_control_once(ui, "Mute microphone preview");
    }
    ui.set_live_session(live);
    ui.set_active_space_id(space);
    // Restore through the window's real navigation seam; internal tab and
    // preview-microphone properties need no new Rust write access for tests.
    match tab {
        0 => ui.invoke_show_members_panel(),
        1 => ui.invoke_show_thread_panel(),
        2 => ui.invoke_focus_room_search(),
        3 => ui.invoke_show_pinned_panel(),
        4 => ui.invoke_show_transfer_panel(),
        _ => panic!("unexpected details tab"),
    }
    ui.set_page(page);
    ui.set_show_side_panel(side);
    ui.set_details_reveal_open(reveal);
    settle(ui).await;
}

/// Real generated controls remain visible and usable when only the Space
/// rail is hidden; the full room list does not require an account column.
/// The public-profile editor's presentation on the real generated window,
/// with synthetic identities only and no network: a server-confirmed name
/// relabels exactly that account's switcher rows (never another account's),
/// the editor names the identity it changes, and clearing the editor drops
/// its draft, preview, status and error synchronously.
async fn assert_public_profile_projection(ui: &AppWindow) {
    // The name and picture caches are process-wide; unit tests that prune
    // them wait while this phase uses them.
    let _shared = crate::account_identity::shared_cache_test_guard();
    let saved = (
        ui.get_account_options(),
        ui.get_accounts(),
        ui.get_active_account_local_id(),
        ui.get_active_account_index(),
        ui.get_active_account_name(),
        ui.get_active_account_initials(),
    );
    let option = |local: &str, name: &str| crate::AccountOption {
        local_id: local.into(),
        name: name.into(),
        handle: format!("@{local}:example.org").as_str().into(),
        initials: "SY".into(),
        ..Default::default()
    };
    ui.set_account_options(ModelRc::new(VecModel::from(vec![
        option("synthetic-a", "synthetic-a"),
        option("synthetic-b", "synthetic-b"),
    ])));
    ui.set_accounts(ModelRc::new(VecModel::from(vec![
        crate::AccountItem {
            name: "synthetic-a".into(),
            ..Default::default()
        },
        crate::AccountItem {
            name: "synthetic-b".into(),
            ..Default::default()
        },
    ])));
    ui.set_active_account_local_id("synthetic-a".into());
    ui.set_active_account_index(0);
    ui.set_active_account_name("synthetic-a".into());
    settle(ui).await;

    let confirmed = morpheus_matrix::OwnProfile::synthetic(
        "synthetic-a",
        "@synthetic-a:example.org",
        "https://example.org",
        Some("Public Alias"),
    );
    crate::account_identity::apply_server(ui, &confirmed, true);
    settle(ui).await;
    assert_eq!(ui.get_active_account_name(), "Public Alias");
    assert_eq!(ui.get_active_account_initials(), "PU");
    assert_eq!(
        ui.get_account_options().row_data(0).unwrap().name,
        "Public Alias"
    );
    assert_eq!(
        ui.get_account_options().row_data(1).unwrap().name,
        "synthetic-b"
    );
    assert_eq!(ui.get_accounts().row_data(0).unwrap().name, "Public Alias");
    assert_eq!(ui.get_accounts().row_data(1).unwrap().name, "synthetic-b");
    assert_eq!(ui.get_account_profile_user_id(), "@synthetic-a:example.org");
    assert_eq!(ui.get_account_profile_homeserver(), "https://example.org");
    assert_eq!(ui.get_account_profile_name_input(), "Public Alias");
    assert_eq!(
        crate::account_identity::confirmed_name("synthetic-a").as_deref(),
        Some("Public Alias")
    );

    // Another account's confirmed profile never relabels the visible one.
    let other = morpheus_matrix::OwnProfile::synthetic(
        "synthetic-b",
        "@synthetic-b:example.org",
        "https://example.org",
        Some("Other Alias"),
    );
    crate::account_identity::apply_server(ui, &other, false);
    settle(ui).await;
    assert_eq!(ui.get_active_account_name(), "Public Alias");
    assert_eq!(
        ui.get_account_options().row_data(1).unwrap().name,
        "Other Alias"
    );
    assert_eq!(ui.get_accounts().row_data(1).unwrap().name, "synthetic-b");

    // A server profile without a name falls back to the user ID localpart.
    let unnamed = morpheus_matrix::OwnProfile::synthetic(
        "synthetic-a",
        "@synthetic-a:example.org",
        "https://example.org",
        None,
    );
    crate::account_identity::apply_server(ui, &unnamed, false);
    settle(ui).await;
    assert_eq!(ui.get_active_account_name(), "synthetic-a");
    assert_eq!(crate::account_identity::confirmed_name("synthetic-a"), None);

    // The editor stays inside the smallest window at 200% text, its body
    // scrolls, and Tab and Shift+Tab stay inside it.
    let size = ui.window().size();
    ui.window().set_size(slint::PhysicalSize::new(900, 620));
    ui.set_font_scale_percent(200);
    ui.set_account_profile_open(true);
    ui.set_account_profile_loaded(true);
    ui.set_account_profile_busy(false);
    ui.set_account_profile_name_dirty(false);
    ui.set_account_profile_avatar_state(0);
    settle(ui).await;
    for label in ["Close", "Read the public profile again from the homeserver"] {
        let bounds = control_bounds(ui, label);
        assert_eq!(bounds.len(), 1, "{label}");
        assert!(inside_window(ui, bounds[0]), "{label} stays in the window");
    }
    // Save is disabled with nothing to save, so Close is the last control.
    for _ in 0..12 {
        if focused_control_has_label(ui, "Close") {
            break;
        }
        key(ui, Key::Tab);
    }
    assert!(focused_control_has_label(ui, "Close"));
    key(ui, Key::Tab);
    assert!(
        focused_control_has_label(ui, "Choose a new public avatar image"),
        "Tab from the last control returns to the first"
    );
    key(ui, Key::Backtab);
    assert!(
        focused_control_has_label(ui, "Close"),
        "Shift+Tab from the first control goes to the last"
    );
    ui.set_font_scale_percent(100);
    ui.window().set_size(size);
    settle(ui).await;

    // An account picture shows only in the rows of its exact local account
    // and the active profile, and a lock or detach drops every one at once.
    crate::account_identity::hold_test_avatar(ui, "synthetic-a", 128);
    settle(ui).await;
    assert!(ui.get_active_account_has_avatar());
    assert!(ui.get_account_options().row_data(0).unwrap().has_avatar);
    assert!(!ui.get_account_options().row_data(1).unwrap().has_avatar);
    assert!(ui.get_accounts().row_data(0).unwrap().has_avatar);
    assert!(!ui.get_accounts().row_data(1).unwrap().has_avatar);
    let picture = ui.get_active_account_avatar().size();
    assert!(picture.width <= 64 && picture.height <= 64, "{picture:?}");
    crate::account_identity::forget_all_avatars(ui);
    settle(ui).await;
    assert!(!ui.get_active_account_has_avatar());
    assert!(!ui.get_account_options().row_data(0).unwrap().has_avatar);
    assert!(!ui.get_accounts().row_data(0).unwrap().has_avatar);
    assert!(crate::account_identity::avatar_image("synthetic-a").is_none());

    // Detach clears the editor's private presentation at once.
    ui.set_account_profile_open(true);
    ui.set_account_profile_name_input("Unsaved Synthetic".into());
    ui.set_account_profile_avatar_state(1);
    ui.set_account_profile_image_visible(true);
    ui.set_account_profile_status("synthetic status".into());
    ui.set_account_profile_error("synthetic error".into());
    crate::account_identity::clear_presentation(ui);
    assert!(!ui.get_account_profile_open());
    assert_eq!(ui.get_account_profile_name_input(), "");
    assert_eq!(ui.get_account_profile_user_id(), "");
    assert_eq!(ui.get_account_profile_avatar_state(), 0);
    assert!(!ui.get_account_profile_image_visible());
    assert_eq!(ui.get_account_profile_status(), "");
    assert_eq!(ui.get_account_profile_error(), "");
    crate::account_identity::retain_confirmed(&[]);

    let (options, accounts, local, index, name, initials) = saved;
    ui.set_account_options(options);
    ui.set_accounts(accounts);
    ui.set_active_account_local_id(local);
    ui.set_active_account_index(index);
    ui.set_active_account_name(name);
    ui.set_active_account_initials(initials);
    settle(ui).await;
}

/// Start a conversation on the real generated window with synthetic people
/// only: Up/Down stay within the shown rows, the selected row is revealed,
/// and a detach clears every row, name and notice synchronously.
async fn assert_people_window_privacy(ui: &AppWindow) {
    let live = ui.get_live_session();
    let sign_in = ui.get_show_sign_in();
    ui.set_live_session(true);
    ui.set_show_sign_in(false);
    let weak = ui.as_weak();
    ui.on_close_people(move || {
        if let Some(ui) = weak.upgrade() {
            crate::people_directory::clear_presentation(&ui);
        }
    });
    let rows: Vec<crate::PersonItem> = (0..30)
        .map(|index| crate::PersonItem {
            user_id: format!("@synthetic{index}:example.org").as_str().into(),
            name: format!("Synthetic {index}").as_str().into(),
            exact: index == 0,
        })
        .collect();
    ui.set_people_items(ModelRc::new(VecModel::from(rows)));
    ui.set_people_index(0);
    ui.set_people_open(true);
    settle(ui).await;
    key(ui, Key::UpArrow);
    assert_eq!(ui.get_people_index(), 0, "Up stops at the first row");
    for _ in 0..40 {
        key(ui, Key::DownArrow);
    }
    assert_eq!(ui.get_people_index(), 29, "Down stops at the last row");
    settle(ui).await;
    let last = control_bounds(ui, "Choose Synthetic 29, @synthetic29:example.org");
    assert_eq!(last.len(), 1, "the selected row is instantiated");
    assert!(inside_window(ui, last[0]), "the selected row is revealed");
    // The window's own debounce Timer and Enter, counted at the callbacks
    // (no LiveUi here; the opt-in live People scenario drives the Rust side).
    // Not busy: a typing pause searches once.
    let searches = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let chosen = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let counted = searches.clone();
    ui.on_people_search(move || counted.set(counted.get() + 1));
    let counted = chosen.clone();
    ui.on_people_choose(move |_| counted.set(counted.get() + 1));
    let query = ui.get_people_query();
    key(ui, "s");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
    settle(ui).await;
    assert_eq!(searches.get(), 1, "a typing pause searches when not busy");
    // Armed, then a creation is admitted before the pause ends: the Timer
    // never searches, and Enter neither searches nor chooses.
    key(ui, "t");
    ui.set_people_busy(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1_000));
    settle(ui).await;
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(
        (searches.get(), chosen.get()),
        (1, 0),
        "nothing searches or chooses while busy"
    );
    ui.set_people_busy(false);
    ui.set_people_query(query);
    settle(ui).await;
    // While a creation runs the search field is disabled, and Tab and
    // Shift+Tab cycle inside the dialog.
    ui.set_people_busy(true);
    settle(ui).await;
    for _ in 0..12 {
        key(ui, Key::Tab);
        assert!(
            focused_control_has_label(ui, "Close")
                || focused_control_has_label(
                    ui,
                    "Search people; arrows select, Enter chooses, Escape closes"
                )
                || {
                    let index = usize::try_from(ui.get_people_index()).unwrap_or(0);
                    focused_control_has_label(
                        ui,
                        &format!("Choose Synthetic {index}, @synthetic{index}:example.org"),
                    )
                },
            "Tab left the dialog while busy"
        );
    }
    ui.set_people_busy(false);
    settle(ui).await;
    // From Close, Tab returns to the search field; Shift+Tab from the
    // search field returns to Close.
    for _ in 0..40 {
        if focused_control_has_label(ui, "Close") {
            break;
        }
        key(ui, Key::Tab);
    }
    assert!(focused_control_has_label(ui, "Close"));
    key(ui, Key::Tab);
    assert!(focused_control_has_label(
        ui,
        "Search people; arrows select, Enter chooses, Escape closes"
    ));
    key(ui, Key::Backtab);
    assert!(
        focused_control_has_label(ui, "Close"),
        "Shift+Tab cycles to Close"
    );

    // At the smallest window and 200% text a chosen person's decision stays
    // reachable: the actions are fixed below the scrolling body.
    let size = ui.window().size();
    ui.window().set_size(slint::PhysicalSize::new(900, 620));
    ui.set_font_scale_percent(200);
    ui.set_people_chosen_id("@synthetic3:example.org".into());
    ui.set_people_chosen_name("Synthetic 3".into());
    ui.set_people_known(2);
    ui.set_people_status("synthetic status".into());
    settle(ui).await;
    for label in [
        "Start an encrypted direct conversation with @synthetic3:example.org",
        "Close",
    ] {
        let bounds = control_bounds(ui, label);
        assert_eq!(bounds.len(), 1, "{label}");
        assert!(
            inside_window(ui, bounds[0]),
            "{label} is reachable at 620 px and 200%"
        );
    }
    ui.set_people_confirm(true);
    settle(ui).await;
    let confirm = control_bounds(
        ui,
        "Confirm starting an encrypted direct conversation with @synthetic3:example.org",
    );
    assert_eq!(confirm.len(), 1);
    assert!(
        inside_window(ui, confirm[0]),
        "the confirmation action is reachable"
    );
    // With the widest valid control padding (24 px) in a 560 px dialog at
    // 200% text, the confirmation, Cancel and Close all fit inside the
    // dialog, one per row, without overlapping, in decision-Cancel-Close
    // order.
    let tokens = ui.global::<crate::StyleTokens>();
    let padding = tokens.get_control_padding();
    tokens.set_control_padding(24.0);
    settle(ui).await;
    let dialog = control_bounds(ui, "Start a conversation");
    assert_eq!(dialog.len(), 1);
    let footer: Vec<_> = [
        "Confirm starting an encrypted direct conversation with @synthetic3:example.org",
        "Cancel",
        "Close",
    ]
    .into_iter()
    .map(|label| {
        let bounds = control_bounds(ui, label);
        assert_eq!(bounds.len(), 1, "{label}");
        let bounds = bounds[0];
        assert!(
            bounds.left >= dialog[0].left - 0.5 && bounds.right <= dialog[0].right + 0.5,
            "{label} fits inside the 560 px dialog"
        );
        assert!(inside_window(ui, bounds), "{label} stays reachable");
        bounds
    })
    .collect();
    for pair in footer.windows(2) {
        assert!(
            pair[0].bottom <= pair[1].top + 0.5,
            "the footer actions do not overlap"
        );
    }
    tokens.set_control_padding(padding);
    ui.set_people_confirm(false);
    ui.set_font_scale_percent(100);
    ui.window().set_size(size);
    settle(ui).await;

    // Detach: nothing about the people searched stays on screen.
    ui.set_people_chosen_id("@synthetic3:example.org".into());
    ui.set_people_chosen_name("Synthetic 3".into());
    ui.set_people_status("synthetic status".into());
    ui.set_people_error("synthetic error".into());
    ui.set_people_query("synthetic".into());
    ui.set_people_confirm(true);
    // The actual window context fence closes the dialog on account access;
    // the registered callback uses the production private-state clearing path.
    ui.set_access_open(true);
    settle(ui).await;
    assert!(!ui.get_people_open());
    assert_eq!(ui.get_people_items().row_count(), 0);
    assert_eq!(ui.get_people_index(), -1);
    assert_eq!(ui.get_people_query(), "");
    assert_eq!(ui.get_people_chosen_id(), "");
    assert_eq!(ui.get_people_chosen_name(), "");
    assert_eq!(ui.get_people_status(), "");
    assert_eq!(ui.get_people_error(), "");
    assert!(!ui.get_people_confirm());
    ui.set_access_open(false);
    ui.set_live_session(live);
    ui.set_show_sign_in(sign_in);
    settle(ui).await;
}

async fn assert_layout_navigation_recovery(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::{ConfigStore, PaneId};
    let store = ConfigStore::at(directory.join("layout-recovery.toml"));
    crate::workspace_layout::register(ui, &store);
    let page = ui.get_page();
    for hidden in [vec![PaneId::Spaces], vec![PaneId::Spaces, PaneId::Rooms]] {
        let mut config = Config::default();
        config.layout.hidden_panes = hidden.clone();
        store.save(&config).unwrap();
        apply_config(ui, &config, None);
        ui.set_page(0);
        settle(ui).await;
        let column = hidden.contains(&PaneId::Rooms);
        assert_eq!(ui.get_account_column(), column);
        let (settings, restore) = if column {
            ("Open settings", "Restore the default pane layout")
        } else {
            (
                "Open settings from account controls",
                "Restore layout from account controls",
            )
        };
        // In the full-list case the original hidden rail still has a Settings
        // item, so the account toolbar deliberately uses distinct labels.
        if !column {
            for label in [settings, restore] {
                let item = control_with_label(ui, label);
                assert!(
                    item.is_visible(),
                    "{label} must be visible without the rail"
                );
                assert!(
                    inside_window(ui, item_bounds(&item)),
                    "{label} must remain in the window"
                );
            }
            click_control_once(ui, settings);
            settle(ui).await;
            assert_eq!(ui.get_page(), 1);
            ui.set_page(0);
            settle(ui).await;
            click_control_once(ui, restore);
        } else {
            // The compact column's restore is distinct from the Settings
            // action, which can also exist in the hidden rail.
            let item = control_with_label(ui, restore);
            assert!(item.is_visible());
            assert!(inside_window(ui, item_bounds(&item)));
            click_control_once(ui, restore);
        }
        settle(ui).await;
        assert!(store.load().unwrap().layout.hidden_panes.is_empty());
        assert!(!ui.get_layout_customized());
    }
    // Explicit panel navigation must still reach a details pane hidden by
    // TOML, including the responsive fallback at the minimum window width.
    let size = ui.window().size();
    let was_live = ui.get_live_session();
    for (width, available) in [
        (1310.0, false),
        (1310.0, true),
        (640.0, false),
        (640.0, true),
    ] {
        ui.set_live_session(available);
        let mut config = Config::default();
        config.layout.hidden_panes = vec![PaneId::Details];
        store.save(&config).unwrap();
        let saved = std::fs::read(store.path()).unwrap();
        apply_config(ui, &config, None);
        ui.window().set_size(slint::LogicalSize::new(width, 820.0));
        ui.set_page(0);
        settle(ui).await;
        assert_eq!(ui.get_pane_details_w(), 0.0);
        ui.invoke_show_thread_panel();
        settle(ui).await;
        assert!(ui.get_pane_details_w() >= 200.0);
        assert!(ui.get_pane_conversation_w() >= 360.0);
        ui.set_show_side_panel(false);
        settle(ui).await;
        assert!(!ui.get_details_reveal_open());
        assert_eq!(ui.get_pane_details_w(), 0.0);
        ui.invoke_show_transfer_panel();
        settle(ui).await;
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        settle(ui).await;
        assert!(ui.get_pane_details_w() >= 200.0);
        let transfer_heading = if available {
            format!("Transfers: {}", ui.get_transfer_summary())
        } else {
            "Transfers: Sign in to see this account's transfers".to_owned()
        };
        assert!(
            focused_control_has_label(ui, &transfer_heading),
            "explicitly opening Transfers must focus its actual available/signed-out heading"
        );
        assert_eq!(
            std::fs::read(store.path()).unwrap(),
            saved,
            "explicit navigation must not rewrite the saved layout"
        );
        // A new accepted configuration ends the temporary reveal too.
        apply_config(ui, &config, None);
        settle(ui).await;
        assert!(!ui.get_details_reveal_open());
        assert_eq!(ui.get_pane_details_w(), 0.0);
    }
    ui.set_live_session(was_live);
    ui.window().set_size(size);
    apply_config(ui, &Config::default(), None);
    assert_pane_pointer_cancellation(ui, &store).await;
    ui.set_page(page);
    settle(ui).await;
}

/// Exercise real generated TouchAreas: Slint sends Cancel with button Other.
/// Neither a cancelled grab nor its late release may persist a layout edit.
async fn assert_pane_pointer_cancellation(ui: &AppWindow, store: &morpheus_config::ConfigStore) {
    use i_slint_backend_testing::ElementHandle;
    use slint::platform::PointerEventButton;
    let was_live = ui.get_live_session();
    let was_preview = ui.get_preview_session();
    ui.set_live_session(false);
    ui.set_preview_session(true);
    ui.set_show_sign_in(false);
    ui.set_page(0);
    let config = Config::default();
    store.save(&config).unwrap();
    apply_config(ui, &config, None);
    settle(ui).await;
    let saved = std::fs::read(store.path()).unwrap();
    for component in ["PaneResizeEdge", "PaneDragGrip"] {
        let handle = ElementHandle::find_by_element_type_name(ui, component)
            .find(|item| item.size().width > 0.0 && item.size().height > 0.0)
            .expect("the generated pane pointer control must be present");
        let origin = handle.absolute_position();
        let size = handle.size();
        let start = slint::LogicalPosition::new(
            origin.x + size.width / 2.0,
            origin.y + 6.0_f32.min(size.height / 2.0),
        );
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position: start });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: start,
            button: PointerEventButton::Left,
        });
        settle(ui).await;
        let token = ui.get_pane_gesture();
        assert!(
            token > 0,
            "the actual pointer press must begin a gesture: {component}"
        );
        let moved = slint::LogicalPosition::new(start.x + 40.0, start.y);
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position: moved });
        settle(ui).await;
        assert_eq!(
            std::fs::read(store.path()).unwrap(),
            saved,
            "pointer preview must not save"
        );
        ui.window().dispatch_event(WindowEvent::PointerExited);
        settle(ui).await;
        assert_eq!(
            ui.get_pane_gesture(),
            0,
            "Cancel/Other must clear the gesture: {component}"
        );
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: moved,
            button: PointerEventButton::Left,
        });
        settle(ui).await;
        assert_eq!(
            std::fs::read(store.path()).unwrap(),
            saved,
            "cancelled pointer release must not save"
        );
        // A late commit token cannot be reused, including after a new gesture.
        let newer = ui.invoke_begin_pane_gesture();
        assert!(newer > token);
        ui.set_pane_gesture(newer);
        ui.invoke_commit_pane_width(token, "rooms".into(), 350);
        assert_eq!(ui.get_pane_gesture(), newer);
        ui.invoke_cancel_pane_preview();
        assert_eq!(std::fs::read(store.path()).unwrap(), saved);
    }
    ui.set_live_session(was_live);
    ui.set_preview_session(was_preview);
    settle(ui).await;
}

/// Use the production LiveUi epoch even when a rapid A-B-A has coalesced to
/// the original UI strings. A preview session uses no real account or vault.
async fn assert_pane_navigation_epoch(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    directory: &std::path::Path,
) {
    let store = morpheus_config::ConfigStore::at(directory.join("pane-epoch.toml"));
    let config = Config::default();
    store.save(&config).unwrap();
    let saved = std::fs::read(store.path()).unwrap();
    let was_live = ui.get_live_session();
    let was_preview = ui.get_preview_session();
    ui.set_live_session(false);
    ui.set_preview_session(true);
    ui.set_show_sign_in(false);
    ui.set_page(0);
    crate::workspace_layout::register_live(ui, &store, live);
    apply_config(ui, &config, None);
    settle(ui).await;
    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    let token = ui.invoke_begin_pane_gesture();
    assert!(token > 0);
    ui.set_pane_gesture(token);
    live.invalidate_navigation();
    ui.set_active_account_local_id("synthetic-other-account".into());
    ui.set_active_room_id("!synthetic-other-room:example.invalid".into());
    live.invalidate_navigation();
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    // Deliberately do not settle: identical final strings are not an epoch.
    ui.invoke_commit_pane_width(token, "rooms".into(), 350);
    assert_eq!(std::fs::read(store.path()).unwrap(), saved);
    assert!(!ui.get_layout_notice().is_empty());
    assert_eq!(ui.get_pane_gesture(), 0);
    let token = ui.invoke_begin_pane_gesture();
    assert!(token > 0);
    ui.set_pane_gesture(token);
    ui.invoke_commit_pane_width(token, "rooms".into(), 350);
    assert_eq!(store.load().unwrap().layout.room_list_width, 350);
    ui.invoke_commit_pane_width(token, "rooms".into(), 400);
    assert_eq!(
        store.load().unwrap().layout.room_list_width,
        350,
        "commit is single-use"
    );
    ui.invoke_cancel_pane_preview();
    apply_config(ui, &config, None);
    ui.set_live_session(was_live);
    ui.set_preview_session(was_preview);
    settle(ui).await;
}

/// The media browser through its registered callbacks and the window's own
/// hooks (header entry, scope and tab handlers, the live reset and Close),
/// with the core's load held: a page is shown only for the scope and the
/// navigation and security epochs its load started in; a security step in
/// progress refuses a load; the reset and Close clear every row at once;
/// View and Save act on a listed row only. No account, network or vault.
async fn assert_room_media_scope_and_privacy(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    use crate::RoomMedia;
    use crate::room_media_browser::{HeldLoader, RoomMediaBrowser};
    use morpheus_matrix::{RoomMediaItem, RoomMediaKind, RoomMediaPage};
    use slint::private_unstable_api::re_exports::{AccessibilityAction, Flickable};
    use std::{cell::RefCell, rc::Rc, sync::atomic::Ordering};

    async fn within(wait: impl std::future::Future<Output = ()>) {
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the held media core answers");
    }
    async fn until(ui: &AppWindow, done: impl Fn(&AppWindow) -> bool) {
        let wait = async {
            while !done(ui) {
                settle(ui).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the media browser reaches the window");
    }
    fn rows(ui: &AppWindow) -> usize {
        ui.global::<RoomMedia>().get_rows().row_count()
    }
    fn idle(ui: &AppWindow) -> bool {
        !ui.global::<RoomMedia>().get_loading()
    }

    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    let was_session = ui.get_live_session();
    let was_page = ui.get_page();
    ui.set_page(0);
    settle(ui).await;
    let was_tab = ui.get_side_panel_tab();
    let was_panel = ui.get_show_side_panel();
    let shown = "!media:example.org";
    ui.set_live_session(true);
    ui.set_active_account_local_id("media-account".into());
    ui.set_active_room_id(shown.into());
    settle(ui).await;
    let was_live = live.is_live.swap(true, Ordering::AcqRel);
    let photo = RoomMediaItem {
        event_id: "$photo:example.org".into(),
        kind: RoomMediaKind::Image,
        filename: "beach.png".into(),
        mime_type: "image/png".into(),
        size: Some(2_048),
        timestamp_ms: 0,
        sender_id: "@ana:example.org".into(),
        sender_name: "Ana".into(),
        thread_root: None,
        encrypted: false,
        authenticity_warning: false,
    };
    let page = RoomMediaPage {
        room_id: shown.into(),
        items: vec![photo],
        next: Some("t1".into()),
        examined: 50,
        ..RoomMediaPage::default()
    };
    let core = Arc::new(HeldLoader::new(page));
    let _browser = RoomMediaBrowser::register(ui, live.clone(), runtime, core.clone());
    let chosen = Rc::new(RefCell::new(Vec::<String>::new()));
    let views = chosen.clone();
    ui.on_preview_image_attachment(move |event, _| {
        views.borrow_mut().push(format!("view {event}"));
    });
    let saves = chosen.clone();
    ui.on_save_attachment(move |event, _| {
        saves.borrow_mut().push(format!("save {event}"));
    });
    let media = ui.global::<RoomMedia>();

    // The header entry shows the tab; its first load is held while the room
    // changes and comes back, unsettled: equal final strings fire no change,
    // and the navigation epoch taken at the load drops what it read.
    ui.invoke_show_room_media();
    settle(ui).await;
    within(core.wait_loads(1)).await;
    assert!(media.get_loading());
    live.invalidate_navigation();
    ui.set_active_room_id("!elsewhere:example.org".into());
    live.invalidate_navigation();
    ui.set_active_room_id(shown.into());
    core.release();
    within(core.wait_answered(1)).await;
    until(ui, idle).await;
    assert_eq!(rows(ui), 0);
    assert!(media.get_can_retry());

    // A security step that begins and ends while the retried load is held.
    media.invoke_retry();
    within(core.wait_loads(2)).await;
    let gate = &live.security_gate;
    drop(gate.try_account_access().expect("the gate is idle"));
    core.release();
    within(core.wait_answered(2)).await;
    until(ui, idle).await;
    assert_eq!(rows(ui), 0);
    // One in progress refuses a load at once.
    let lease = gate.try_account_access().expect("the gate is idle");
    media.invoke_retry();
    assert!(!media.get_loading());
    assert!(media.get_error().contains("security step"));
    drop(lease);

    // Nothing changed: the page is shown, and says what it covers.
    media.invoke_retry();
    within(core.wait_loads(3)).await;
    core.release();
    within(core.wait_answered(3)).await;
    until(ui, |ui| rows(ui) == 1).await;
    assert!(media.get_summary().contains("1 item from the 50 messages"));
    assert!(media.get_can_load_older());
    // Long coverage/error text must scroll without displacing the actual
    // first row or fixed actions at the minimum height and 200% text.
    let layout_before = (
        ui.window().size(),
        ui.get_font_scale_percent(),
        ui.get_details_width(),
        media.get_summary(),
        media.get_error(),
    );
    ui.window().set_size(slint::PhysicalSize::new(1280, 620));
    ui.set_font_scale_percent(200);
    ui.set_details_width(200);
    media.set_summary("Synthetic coverage describes the loaded messages and the older media that has not been read. ".repeat(12).into());
    media.set_error(
        "A synthetic read error leaves the loaded row available. "
            .repeat(8)
            .into(),
    );
    settle(ui).await;
    assert_eq!(ui.window().size(), slint::PhysicalSize::new(1280, 620));
    let pane = item_bounds(&control_with_label(ui, "Conversation details"));
    let panel_label = format!("Media in {}", ui.get_active_room_name());
    let panel = item_bounds(&control_with_label(ui, &panel_label));
    let list = item_bounds(&control_with_label(
        ui,
        &format!("{panel_label}, newest first"),
    ));
    let whole = |label: &str| {
        let found = control_bounds(ui, label);
        assert_eq!(found.len(), 1, "one actual media item: {label}");
        let bounds = found[0];
        assert!(
            inside_window(ui, bounds)
                && bounds.left >= pane.left - 0.5
                && bounds.right <= pane.right + 0.5
                && bounds.top >= panel.top - 0.5
                && bounds.bottom <= pane.bottom + 0.5
                && bounds.bottom <= panel.bottom + 0.5,
            "media item {label} is whole inside its allocated pane: {bounds:?}, panel={panel:?}, pane={pane:?}"
        );
        bounds
    };
    assert!(inside_window(ui, pane) && inside_window(ui, panel));
    assert!(panel.top >= pane.top - 0.5 && panel.bottom <= pane.bottom + 0.5);
    let first = whole("beach.png, Image · 2.0 KB · image/png, Ana");
    assert!(
        first.top >= list.top - 0.5 && first.bottom <= list.bottom + 0.5,
        "the complete first row is inside the real media list: {first:?}, list={list:?}"
    );
    for label in [
        "Show beach.png in the conversation".to_owned(),
        "View beach.png".to_owned(),
        "Save beach.png".to_owned(),
        format!("Load older media of {}", ui.get_active_room_name()),
        "Close the media list".to_owned(),
    ] {
        let action = whole(&label);
        assert!(
            first.bottom <= action.top + 0.5,
            "the first row does not overlap {label}"
        );
    }
    let search_label = "Search loaded media; arrows select, Enter shows it in the conversation, Escape clears or closes";
    tab_to_control(ui, search_label);
    settle(ui).await;
    let search = control_with_label(ui, search_label);
    let mut header_viewports = Vec::new();
    for node in geometry_nodes(ui) {
        if node.downcast::<Flickable>().is_none() {
            continue;
        }
        let mut contains_search = false;
        let _ = node.visit_descendants(|item| {
            if item == &search {
                contains_search = true;
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        });
        if contains_search {
            header_viewports.push(item_bounds(&node));
        }
    }
    assert_eq!(
        header_viewports.len(),
        1,
        "the header shares one real scroll viewport"
    );
    let header_viewport = header_viewports[0];
    let fixed_close = whole("Close the media list");
    let scroll_position = slint::LogicalPosition::new(
        (header_viewport.left + header_viewport.right) / 2.0,
        header_viewport.bottom - 10.0,
    );
    for label in ["Kind of media shown", search_label] {
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: scroll_position,
            delta_x: 0.0,
            delta_y: -5_000.0,
        });
        settle(ui).await;
        assert!(
            item_bounds(&search).bottom < header_viewport.top,
            "the actual body wheel scroll moved the header out of view"
        );
        tab_to_control(ui, label);
        settle(ui).await;
        assert!(focused_control_has_label(ui, label));
        let header = whole(label);
        assert!(
            header.top >= header_viewport.top - 0.5
                && header.bottom <= header_viewport.bottom + 0.5,
            "keyboard focus reveals the whole header control: {header:?}, viewport={header_viewport:?}"
        );
        assert_eq!(whole("Close the media list"), fixed_close);
        assert_eq!(whole("beach.png, Image · 2.0 KB · image/png, Ana"), first);
    }
    media.set_summary(layout_before.3);
    media.set_error(layout_before.4);
    ui.set_details_width(layout_before.2);
    ui.set_font_scale_percent(layout_before.1);
    ui.window().set_size(layout_before.0);
    settle(ui).await;
    // View and Save hand on a listed row only.
    media.invoke_view("$photo:example.org".into());
    media.invoke_save("$photo:example.org".into());
    media.invoke_save("$unlisted:example.org".into());
    let expected = ["view $photo:example.org", "save $photo:example.org"];
    assert_eq!(*chosen.borrow(), expected);

    // The live reset hook clears every row before it returns; the tab then
    // reads again for what is shown.
    crate::room_media_browser::reset_window(ui);
    assert_eq!(rows(ui), 0);
    assert!(media.get_summary().is_empty());
    settle(ui).await;
    within(core.wait_loads(4)).await;
    // Close gives the panel back as it was and ends the held load unshown.
    let close = control_with_label(ui, "Close the media list");
    close.accessible_action(&AccessibilityAction::Default);
    settle(ui).await;
    assert_eq!(ui.get_side_panel_tab(), was_tab);
    assert_eq!(ui.get_show_side_panel(), was_panel);
    core.release();
    within(core.wait_answered(4)).await;
    settle(ui).await;
    assert_eq!(rows(ui), 0);

    // Real Tab focus on each footer action: Escape bubbles from the
    // focused control and closes, without activating Open/View/Save.
    for (index, label) in [
        "Show beach.png in the conversation",
        "View beach.png",
        "Save beach.png",
    ]
    .into_iter()
    .enumerate()
    {
        let count = index + 5;
        ui.invoke_show_room_media();
        settle(ui).await;
        within(core.wait_loads(count)).await;
        core.release();
        within(core.wait_answered(count)).await;
        until(ui, |ui| rows(ui) == 1).await;
        tab_to_control(ui, label);
        assert!(focused_control_has_label(ui, label));
        key(ui, Key::Escape);
        settle(ui).await;
        assert_eq!(ui.get_side_panel_tab(), was_tab);
        assert_eq!(rows(ui), 0);
    }
    assert_eq!(
        *chosen.borrow(),
        expected,
        "Escape never activates a focused action"
    );

    // A selector popup gets its own Escape first. Once it is closed, the
    // same key on the focused selector closes the media panel.
    ui.invoke_show_room_media();
    settle(ui).await;
    within(core.wait_loads(8)).await;
    tab_to_control(ui, "Kind of media shown");
    key(ui, Key::Return);
    settle(ui).await;
    key(ui, Key::Escape);
    settle(ui).await;
    assert_eq!(ui.get_side_panel_tab(), crate::room_media_browser::PANE_TAB);
    assert!(focused_control_has_label(ui, "Kind of media shown"));
    key(ui, Key::Escape);
    settle(ui).await;
    assert_eq!(ui.get_side_panel_tab(), was_tab);
    core.release();
    within(core.wait_answered(8)).await;

    // Search still clears first, then closes, through actual key input.
    ui.invoke_show_room_media();
    settle(ui).await;
    within(core.wait_loads(9)).await;
    tab_to_control(
        ui,
        "Search loaded media; arrows select, Enter shows it in the conversation, Escape clears or closes",
    );
    type_text(ui, "beach");
    settle(ui).await;
    assert_eq!(media.get_query().as_str(), "beach");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(media.get_query().is_empty());
    assert_eq!(ui.get_side_panel_tab(), crate::room_media_browser::PANE_TAB);
    key(ui, Key::Escape);
    settle(ui).await;
    assert_eq!(ui.get_side_panel_tab(), was_tab);
    core.release();
    within(core.wait_answered(9)).await;
    settle(ui).await;
    assert_eq!(rows(ui), 0);

    live.is_live.store(was_live, Ordering::Release);
    ui.on_preview_image_attachment(|_, _| {});
    ui.on_save_attachment(|_, _| {});
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    ui.set_live_session(was_session);
    ui.set_page(was_page);
    settle(ui).await;
}

/// The room header's bell opens its panel only for the live account and the
/// room open now. The panel keeps Tab inside, pauses shortcuts, closes on
/// Escape with focus back on the bell, and closes and clears on another room,
/// another account or an access screen. Stand-ins replace the controller, so
/// nothing reads a homeserver or writes configuration.
async fn assert_room_notification_panel(ui: &AppWindow) {
    use crate::RoomNotificationState;
    use std::{cell::Cell, rc::Rc};

    let global = ui.global::<RoomNotificationState>();
    let opens = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let weak = ui.as_weak();
    let counted = opens.clone();
    global.on_request_open(move |account_id, room_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        counted.set(counted.get() + 1);
        let global = ui.global::<RoomNotificationState>();
        global.set_account_id(account_id);
        global.set_room_id(room_id);
        global.set_room_name("Team room".into());
        global.set_effective(2);
        global.set_open(true);
    });
    let weak = ui.as_weak();
    let counted = closes.clone();
    global.on_close(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        counted.set(counted.get() + 1);
        let global = ui.global::<RoomNotificationState>();
        global.set_open(false);
        global.set_account_id("".into());
        global.set_room_id("".into());
        global.set_room_name("".into());
    });
    let before = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_active_account_local_id(),
        ui.get_active_room_id(),
    );
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("work".into());
    ui.set_active_room_id("!room:example.org".into());
    settle(ui).await;

    click_control_once(ui, "Room notifications");
    settle(ui).await;
    assert_eq!(opens.get(), 1);
    assert!(global.get_open());
    assert!(focused_control_has_label(ui, "Close room notifications"));
    key(ui, Key::Backtab);
    assert!(focused_control_has_label(
        ui,
        "Open notification settings for every room"
    ));
    key(ui, Key::Tab);
    assert!(focused_control_has_label(ui, "Close room notifications"));
    assert!(
        !ui.invoke_dispatch_shortcut("p".into(), true, false, false, false),
        "shortcuts pause while the room panel is open"
    );
    assert!(!ui.get_quick_switcher_open());
    key(ui, Key::Escape);
    settle(ui).await;
    assert_eq!(closes.get(), 1);
    assert!(!global.get_open());
    assert!(focused_control_has_label(ui, "Room notifications"));

    // At the smallest window and 200% text the panel stays inside the
    // window, its body scrolls, and a control reached by keyboard is shown.
    ui.window().set_size(slint::PhysicalSize::new(940, 620));
    ui.set_font_scale_percent(200);
    settle(ui).await;
    global.invoke_request_open("work".into(), "!room:example.org".into());
    settle(ui).await;
    let panel = item_bounds(&control_with_label(ui, "Notifications for Team room"));
    assert!(
        inside_window(ui, panel),
        "the panel fits the window: {panel:?}"
    );
    key(ui, Key::Backtab);
    settle(ui).await;
    assert!(focused_control_has_label(
        ui,
        "Open notification settings for every room"
    ));
    let focused = focused_control_bounds(ui).expect("a focused panel control");
    assert!(
        focused.top >= panel.top && focused.bottom <= panel.bottom,
        "the last control is scrolled into the panel: {focused:?} in {panel:?}"
    );
    // Every body control, the device switches with wrapped details included,
    // is shown whole below the header when focus reaches it: the reveal uses
    // each control's real laid-out height, not a guess.
    global.set_previews_on(true);
    global.set_highlights_set(true);
    global.set_no_highlights(true);
    settle(ui).await;
    key(ui, Key::Tab);
    settle(ui).await;
    assert!(focused_control_has_label(ui, "Close room notifications"));
    let header = item_bounds(&control_with_label(ui, "Close room notifications"));
    let mut reached_last = false;
    for step in 0..16 {
        key(ui, Key::Tab);
        settle(ui).await;
        let focused = focused_control_bounds(ui).expect("a focused panel control");
        assert!(
            focused.top >= header.bottom - 0.5 && focused.bottom <= panel.bottom + 0.5,
            "body control {step} is shown whole: {focused:?} below {header:?} in {panel:?}"
        );
        if focused_control_has_label(ui, "Open notification settings for every room") {
            reached_last = true;
            break;
        }
    }
    assert!(reached_last, "Tab reached every body control");
    key(ui, Key::Escape);
    settle(ui).await;
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;

    for change in 0..3 {
        global.invoke_request_open("work".into(), "!room:example.org".into());
        settle(ui).await;
        assert!(global.get_open());
        match change {
            0 => ui.set_active_room_id("!other:example.org".into()),
            1 => ui.set_active_account_local_id("home".into()),
            _ => ui.set_access_open(true),
        }
        settle(ui).await;
        assert!(!global.get_open(), "context change {change} kept the panel");
        assert!(global.get_room_name().is_empty() && global.get_room_id().is_empty());
        ui.set_access_open(false);
        ui.set_active_account_local_id("work".into());
        ui.set_active_room_id("!room:example.org".into());
        settle(ui).await;
    }
    assert_eq!(closes.get(), 5);

    let (page, live, account, room) = before;
    ui.set_page(page);
    ui.set_live_session(live);
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    settle(ui).await;
}

/// The invitations center opens from its Home entry and from Settings
/// search, takes focus in its search field, closes on Escape with focus back
/// on the entry, and closes when Home, the listed account or the live session
/// is left. No controller is registered, so nothing is read or answered.
async fn assert_invitations_center_gating(ui: &AppWindow) {
    use crate::{InvitationItem, InvitationsState};

    let global = ui.global::<InvitationsState>();
    let before = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_active_account_local_id(),
    );
    let invite = RoomItem {
        id: "!invite:example.org".into(),
        name: "Invited room".into(),
        ..Default::default()
    };
    ui.set_invitations(ModelRc::new(VecModel::from(vec![invite.clone()])));
    ui.set_home_invitations(ModelRc::new(VecModel::from(vec![invite])));
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("work".into());
    settle(ui).await;

    click_control_once(ui, "Open all 1 invitations");
    settle(ui).await;
    assert!(global.get_open());
    assert!(focused_control_has_label(ui, "Search invitations"));
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());
    assert!(focused_control_has_label(ui, "Open all 1 invitations"));

    // Settings search opens it on Home and reports its destination.
    ui.set_settings_revealed_id("".into());
    ui.set_page(1);
    settle(ui).await;
    assert!(crate::settings_search::reveal(ui, "rooms.invitations"));
    settle(ui).await;
    assert_eq!(ui.get_page(), 0);
    assert!(global.get_open());
    assert_eq!(ui.get_settings_revealed_id(), "rooms.invitations");
    // Leaving Home closes it.
    ui.set_page(1);
    settle(ui).await;
    assert!(!global.get_open());

    // A rebuilt list never strands keyboard focus: focus on a removed row's
    // button goes back to the search field.
    let row = |room: &str, name: &str| InvitationItem {
        room_id: room.into(),
        generation: 1,
        name: name.into(),
        detail: "Room".into(),
        ..Default::default()
    };
    ui.set_page(0);
    global.set_index(-1);
    global.set_items(ModelRc::new(VecModel::from(vec![
        row("!a:example.org", "Alpha"),
        row("!b:example.org", "Beta"),
    ])));
    global.set_open(true);
    settle(ui).await;
    request_bridge_focus(ui, "Accept invitation to Alpha");
    settle(ui).await;
    assert_eq!(global.get_index(), 0, "focusing a row's answer chooses it");
    global.set_items(ModelRc::new(VecModel::from(vec![row(
        "!b:example.org",
        "Beta",
    )])));
    global.set_focus_request(global.get_focus_request() + 1);
    settle(ui).await;
    assert!(focused_control_has_label(ui, "Search invitations"));
    global.set_items(ModelRc::new(VecModel::<InvitationItem>::default()));
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());

    // Modal: with focus on one of its controls, a real Mod+K chord reaches
    // nothing behind the center, and window shortcuts are paused.
    let modifier = if cfg!(target_os = "macos") {
        Key::Meta
    } else {
        Key::Control
    };
    global.set_open(true);
    settle(ui).await;
    request_bridge_focus(ui, "Close invitations");
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: modifier.into(),
    });
    key(ui, "k");
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: modifier.into(),
    });
    settle(ui).await;
    assert!(
        !ui.get_quick_switcher_open(),
        "a window shortcut ran behind the center"
    );
    assert!(global.get_open());
    assert!(
        !ui.invoke_dispatch_shortcut("k".into(), true, false, false, false),
        "shortcuts pause while the center is open"
    );

    // At the smallest window and 200% text, with a long notice and its
    // manual Check and open, and a pending bulk confirmation, every action is
    // reachable: focusing one scrolls the center's body until it is whole
    // inside the center, and the confirmation's buttons bring its text.
    ui.window().set_size(slint::PhysicalSize::new(940, 620));
    ui.set_font_scale_percent(200);
    global.set_items(ModelRc::new(VecModel::from(vec![
        row("!a:example.org", "Alpha"),
        row("!b:example.org", "Beta"),
    ])));
    global.set_matching(2);
    global.set_total(2);
    global.set_read(2);
    global.set_bulk_count(2);
    let notice = concat!(
        "Alpha: Joined, but sync has not brought the room here yet. ",
        "Choose Check and open again later."
    );
    global.set_notice(notice.into());
    global.set_notice_room_id("!a:example.org".into());
    global.set_notice_room_ready(false);
    settle(ui).await;
    let label = format!("Invitations for {}", ui.get_active_account_handle());
    let center = item_bounds(&control_with_label(ui, &label));
    assert!(
        inside_window(ui, center),
        "the center fits the window: {center:?}"
    );
    let whole = |bounds: Bounds| {
        bounds.top >= center.top
            && bounds.bottom <= center.bottom
            && bounds.left >= center.left
            && bounds.right <= center.right
    };
    let disclosure = concat!(
        "Decline these 2 invitations? Each one is declined on its own and ",
        "cannot be taken back here."
    );
    for action in [
        "Open the joined room if sync has brought it here",
        "Accept invitation to Alpha",
        "Confirm: decline 2 invitations",
        "Keep these invitations",
        "Read this account's invitations again",
    ] {
        request_bridge_focus(ui, action);
        settle(ui).await;
        let bounds = item_bounds(&control_with_label(ui, action));
        assert!(
            whole(bounds),
            "{action} is whole inside the center once focused: {bounds:?} in {center:?}"
        );
        if action == "Confirm: decline 2 invitations" {
            let text = labelled_bounds(ui, disclosure, None);
            assert!(
                !text.is_empty() && text.iter().all(|bounds| whole(*bounds)),
                "the confirmation's text shows with its buttons: {text:?}"
            );
        }
    }
    request_bridge_focus(ui, "Close invitations");
    let mut reached = false;
    for _ in 0..16 {
        key(ui, Key::Tab);
        settle(ui).await;
        if focused_control_has_label(ui, "Confirm: decline 2 invitations") {
            reached = true;
            break;
        }
    }
    assert!(reached, "Tab reaches the confirmation");
    global.set_notice("".into());
    global.set_notice_room_id("".into());
    global.set_bulk_count(0);
    global.set_items(ModelRc::new(VecModel::<InvitationItem>::default()));
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;
    // The confirmation's buttons are gone; Escape from the center's own
    // close action still closes it.
    request_bridge_focus(ui, "Close invitations");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());

    // Another account on screen than the one listed, or no live session,
    // closes it too.
    ui.set_page(0);
    global.set_open(true);
    global.set_account_id("work".into());
    settle(ui).await;
    assert!(global.get_open());
    ui.set_active_account_local_id("home".into());
    settle(ui).await;
    assert!(!global.get_open(), "another account closed the center");
    ui.set_active_account_local_id("work".into());
    global.set_account_id("".into());
    global.set_open(true);
    settle(ui).await;
    ui.set_live_session(false);
    settle(ui).await;
    assert!(!global.get_open(), "a detached session closed the center");

    let (page, live, account) = before;
    ui.set_invitations(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_home_invitations(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_page(page);
    ui.set_live_session(live);
    ui.set_active_account_local_id(account);
    settle(ui).await;
}

/// The invitations center and a Matrix link exclude each other, through
/// their real entries, once the link controller is registered: the center's
/// Home entry and its Settings-search route each end a joined link's message
/// still to come synchronously, before the center opens, and closing the
/// center never revives it; while the center is open a link click shows
/// nothing and a link follow-up admitted under it ends for good.
async fn assert_invitations_and_links_exclude_each_other(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
) {
    use crate::InvitationsState;

    let global = ui.global::<InvitationsState>();
    let before = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_active_account_local_id(),
    );
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("work".into());
    settle(ui).await;
    assert!(!ui.get_matrix_link_protected());

    // The Home entry ends the held follow-up at the call, before the
    // center's flag changes anything.
    let held = crate::matrix_links::tests::hold_follow_up();
    assert!(held.admits(ui));
    ui.invoke_open_invitations();
    assert!(held.withdrawn(), "ended at the entry");
    settle(ui).await;
    assert!(global.get_open());
    assert!(focused_control_has_label(ui, "Search invitations"));
    // While it is open, links are protected: a click shows nothing, and a
    // follow-up admitted under it ends for good.
    assert!(ui.get_matrix_link_protected());
    let link = "https://matrix.to/#/@alice:example.org";
    assert!(crate::matrix_links::open(ui, live, runtime, link));
    settle(ui).await;
    assert!(!crate::matrix_links::is_open(ui));
    let missed = crate::matrix_links::tests::hold_follow_up();
    assert!(!missed.admits(ui), "admitted under the center: ended");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());
    assert!(!held.admits(ui), "closing the center never revives it");
    assert!(!missed.admits(ui), "and it stays ended");

    // The Settings-search route ends a held follow-up first too.
    let routed = crate::matrix_links::tests::hold_follow_up();
    assert!(routed.admits(ui));
    assert!(crate::settings_search::reveal(ui, "rooms.invitations"));
    assert!(routed.withdrawn(), "ended at the route");
    settle(ui).await;
    assert!(global.get_open());
    assert!(focused_control_has_label(ui, "Search invitations"));
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());
    assert!(!routed.admits(ui));

    // INV-OPEN-01: an automatic open a click still owns (an accepted
    // invitation's) ends at every protected surface's own entry,
    // synchronously, before its flag changes, and stays ended once that
    // surface closes, even closed before any deferred handler ran; a newer
    // owned open replaces and ends the older.
    let menu = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(menu.clone());
    ui.invoke_open_account_menu();
    ui.set_account_menu_open(false);
    assert!(menu.withdrawn(), "ended at the account menu's entry");
    settle(ui).await;
    assert!(menu.withdrawn(), "closing it never revives it");
    // The account control's own activation opens the menu through that entry.
    let trigger = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(trigger.clone());
    let label = format!(
        "Account menu for {}, {}",
        ui.get_active_account_name(),
        ui.get_active_account_handle()
    );
    request_bridge_focus(ui, &label);
    key(ui, Key::Return);
    assert!(ui.get_account_menu_open());
    ui.set_account_menu_open(false);
    assert!(
        trigger.withdrawn(),
        "ended at the account control's activation"
    );
    settle(ui).await;
    assert!(trigger.withdrawn());
    let entry = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(entry.clone());
    ui.invoke_open_invitations();
    assert!(entry.withdrawn(), "ended at the center's Home entry");
    settle(ui).await;
    assert!(global.get_open());
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());
    assert!(entry.withdrawn());
    let older = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(older.clone());
    let newer = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(newer.clone());
    assert!(
        older.withdrawn() && !newer.withdrawn(),
        "only the last click opens"
    );
    crate::matrix_links::clear(ui);
    assert!(newer.withdrawn());

    // Under anything that withdraws a link (a verification still pending on
    // Home here) both entries are refused, not deferred: no flag is left
    // behind to block links, shortcuts or mentions, or to open the center
    // by itself once it ends.
    ui.set_sas_stage(1);
    settle(ui).await;
    assert!(ui.get_matrix_link_blocked());
    ui.invoke_open_invitations();
    settle(ui).await;
    assert!(!global.get_open(), "refused at the Home entry");
    assert!(crate::settings_search::reveal(ui, "rooms.invitations"));
    settle(ui).await;
    assert!(!global.get_open(), "refused at the route");
    ui.set_sas_stage(0);
    settle(ui).await;
    assert!(!global.get_open(), "never opens by itself later");
    assert!(ui.get_mention_ui_allowed());

    // The thread browser, Leave and the center exclude each other: an open
    // Leave dialog or browser refuses both of the center's entries before
    // anything is withdrawn (an automatic open a click owns still stands),
    // and an open center holds Leave's and the browser's own admission.
    // Each surface is set and reset with no settle between, so no change
    // handler runs.
    let leave = ui.global::<crate::RoomLeave>();
    let browser = ui.global::<crate::ThreadBrowser>();
    let kept = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(kept.clone());
    leave.set_view(crate::RoomLeaveView {
        open: true,
        ..Default::default()
    });
    ui.invoke_open_invitations();
    assert!(!crate::settings_search::reveal(ui, "rooms.invitations"));
    assert!(!global.get_open(), "refused over Leave");
    assert!(leave.get_view().open, "Leave stays");
    assert!(!kept.withdrawn(), "refused before any withdrawal");
    leave.set_view(crate::RoomLeaveView::default());
    browser.set_open(true);
    ui.invoke_open_invitations();
    assert!(!global.get_open(), "refused over the thread browser");
    assert!(browser.get_open(), "the browser stays");
    assert!(!kept.withdrawn(), "refused before any withdrawal");
    browser.set_open(false);
    settle(ui).await;
    ui.invoke_open_invitations();
    assert!(kept.withdrawn(), "an admitted entry ends it");
    settle(ui).await;
    assert!(global.get_open());
    assert!(ui.get_room_leave_blocked(), "the center holds Leave");
    assert!(!ui.get_thread_browser_allowed(), "and the browser");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());
    assert!(!ui.get_room_leave_blocked() && ui.get_thread_browser_allowed());

    // The Settings-search route from the actual Settings page, where the
    // page itself never refuses it: refused under a surface up there (a
    // verification here), it withdraws nothing and Settings stays; admitted,
    // it ends an automatic open a click owns before the center's flag. The
    // open center then holds a jump too.
    ui.set_page(1);
    settle(ui).await;
    let routed_open = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(routed_open.clone());
    ui.set_sas_stage(1);
    assert!(crate::settings_search::reveal(ui, "rooms.invitations"));
    assert!(!routed_open.withdrawn(), "refused before any withdrawal");
    assert_eq!(ui.get_page(), 1, "and Settings stays");
    assert!(!global.get_open());
    ui.set_sas_stage(0);
    settle(ui).await;
    assert!(crate::settings_search::reveal(ui, "rooms.invitations"));
    assert!(routed_open.withdrawn(), "the route ended it first");
    assert_eq!(ui.get_page(), 0);
    settle(ui).await;
    assert!(global.get_open());
    assert!(!ui.get_unread_jump_allowed(), "the center holds a jump");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!global.get_open());

    let (page, live_session, account) = before;
    ui.set_settings_reveal_id("".into());
    ui.set_settings_revealed_id("".into());
    ui.set_page(page);
    ui.set_live_session(live_session);
    ui.set_active_account_local_id(account);
    settle(ui).await;
}

const HEADER_MORE: &str = "More room actions";
const HEADER_MENU: &str = "Room actions that do not fit";
const HEADER_ROOM: &str = "A long synthetic room name that the responsive header keeps readable";
const HEADER_TOPIC: &str = "A synthetic topic that gives way before the room name";
const HEADER_LEAVE: &str = "Leave this room…";
const HEADER_JUMP: &str = "Jump to first unread message";

/// The eight header actions that may fold, in header order, with the labels
/// the header gives them now.
fn header_foldable_labels(ui: &AppWindow) -> Vec<String> {
    let muted = ui
        .global::<crate::RoomNotificationState>()
        .get_header_muted();
    let transfers = ui.get_transfer_active_count();
    vec![
        if muted {
            "Room notifications, muted"
        } else {
            "Room notifications"
        }
        .to_owned(),
        "Edit room settings".to_owned(),
        "Show pinned messages".to_owned(),
        "Browse threads in this room".to_owned(),
        "Browse this room's media".to_owned(),
        if transfers > 0 {
            format!("Show transfers, {transfers} in progress")
        } else {
            "Show transfers".to_owned()
        },
        "Open current room call stage".to_owned(),
        if ui.get_show_side_panel() {
            "Hide details panel"
        } else {
            "Show details panel"
        }
        .to_owned(),
    ]
}

fn header_organize_label(ui: &AppWindow) -> String {
    let state = ui.global::<crate::RoomOrganizationState>();
    let suffix = if state.get_active_favourite() {
        ", in Favorites"
    } else if state.get_active_low_priority() {
        ", low priority"
    } else {
        ""
    };
    format!("Organize {}{suffix}", ui.get_active_room_name())
}

fn bounds_within(inner: Bounds, outer: Bounds) -> bool {
    inner.left >= outer.left - 0.5
        && inner.right <= outer.right + 0.5
        && inner.top >= outer.top - 0.5
        && inner.bottom <= outer.bottom + 0.5
}

/// The items on the conversation header's row: centred on the room name and
/// clear of the details pane, whose tabs reuse some labels.
fn on_header_row(ui: &AppWindow, items: Vec<Bounds>) -> Vec<Bounds> {
    let name = control_bounds(ui, &format!("Current room: {}", ui.get_active_room_name()));
    assert_eq!(name.len(), 1, "one room name in the header");
    let centre = (name[0].top + name[0].bottom) / 2.0;
    let details = control_bounds(ui, "Conversation details");
    items
        .into_iter()
        .filter(|item| ((item.top + item.bottom) / 2.0 - centre).abs() < 1.0)
        .filter(|item| {
            !details
                .iter()
                .any(|pane| item.right > pane.left + 0.5 && item.left < pane.right - 0.5)
        })
        .collect()
}

fn header_buttons(ui: &AppWindow, label: &str) -> Vec<Bounds> {
    on_header_row(ui, button_bounds(ui, label))
}

/// Buttons labelled `label` in the open "More room actions" menu.
fn header_menu_buttons(ui: &AppWindow, label: &str) -> Vec<Bounds> {
    let menus = control_bounds(ui, HEADER_MENU);
    button_bounds(ui, label)
        .into_iter()
        .filter(|item| menus.iter().any(|menu| bounds_within(*item, *menu)))
        .collect()
}

/// One real pointer click at the centre of drawn bounds.
fn click_bounds(ui: &AppWindow, bounds: Bounds) {
    let position = slint::LogicalPosition::new(
        (bounds.left + bounds.right) / 2.0,
        (bounds.top + bounds.bottom) / 2.0,
    );
    assert!(
        inside_window(ui, bounds),
        "click inside the window: {bounds:?}"
    );
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
}

/// The header row as drawn: a readable room name, then Organization, the
/// drawn foldable actions with Search and More, each at most once, in header
/// order, apart, inside the window and within the conversation canvas (never
/// under the details pane). Returns the labels of the foldable actions drawn.
fn assert_header_row(ui: &AppWindow, case: &str) -> Vec<String> {
    let scale = ui.get_font_scale_percent() as f32 / 100.0;
    let name = control_bounds(ui, &format!("Current room: {}", ui.get_active_room_name()))[0];
    assert!(inside_window(ui, name), "{case}: room name {name:?}");
    assert!(
        name.right - name.left >= 48.0 * scale - 0.5,
        "{case}: the room name stays readable: {name:?}"
    );
    let labels = header_foldable_labels(ui);
    let mut row = vec![header_organize_label(ui)];
    row.extend(labels[..2].iter().cloned());
    // Leave sits between room settings and Search while it is shown.
    row.push(HEADER_LEAVE.to_owned());
    row.push("Search messages".to_owned());
    // Jump to first unread sits between Search and pins while it is shown.
    row.push(HEADER_JUMP.to_owned());
    row.extend(labels[2..].iter().cloned());
    row.push(HEADER_MORE.to_owned());
    let mut previous = name;
    let mut shown = Vec::new();
    for label in &row {
        let drawn = header_buttons(ui, label);
        assert!(drawn.len() <= 1, "{case}: {label} is drawn once");
        let Some(&bounds) = drawn.first() else {
            continue;
        };
        assert!(inside_window(ui, bounds), "{case}: {label} {bounds:?}");
        assert!(
            bounds.left >= previous.right - 0.5,
            "{case}: {label} {bounds:?} after {previous:?}"
        );
        if labels.contains(label) {
            shown.push(label.clone());
        }
        previous = bounds;
    }
    for fixed in [header_organize_label(ui), "Search messages".to_owned()] {
        assert_eq!(
            header_buttons(ui, &fixed).len(),
            1,
            "{case}: {fixed} never folds"
        );
    }
    // The canvas starts 24 px, the glyph and an 11 px gap before the name and
    // keeps a 16 px edge: the row ends inside it.
    let canvas = ui.get_pane_conversation_w();
    assert!(
        previous.right <= name.left - 51.0 + canvas + 0.5,
        "{case}: the row {previous:?} ends inside the {canvas} px canvas from {name:?}"
    );
    shown
}

async fn open_header_more(ui: &AppWindow) {
    let more = header_buttons(ui, HEADER_MORE);
    assert_eq!(more.len(), 1, "one More control");
    click_bounds(ui, more[0]);
    settle(ui).await;
    // The first available item takes focus a tick after the menu exists.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
    settle(ui).await;
    assert!(ui.get_header_more_open(), "More opens its menu");
}

/// Phase 92: the conversation header at the width it actually has, with no
/// account, core or network. Comfortable, every action is one click in the
/// header. Cramped (the minimum window, 200 % text, the largest configured
/// icons, an open details panel), Organization and Search stay, every other
/// action is reachable exactly once, in the header or in one flat "More room
/// actions" menu, and a menu item runs its icon's action after the menu has
/// closed. Folding while focused moves focus to More without acting; Escape
/// and every context change close the menu.
async fn assert_responsive_header(ui: &AppWindow) {
    use slint::private_unstable_api::re_exports::AccessibilityAction;
    use std::{cell::Cell, rc::Rc};

    let tokens = ui.global::<crate::StyleTokens>();
    let threads = ui.global::<crate::ThreadBrowser>();
    let saved_header_state = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_show_sign_in(),
        ui.get_active_account_local_id(),
        ui.get_active_room_id(),
        ui.get_active_room_name(),
        ui.get_active_room_topic(),
        ui.get_show_side_panel(),
        ui.get_side_panel_tab(),
        ui.get_details_reveal_open(),
        ui.get_font_scale_percent(),
        ui.window().size(),
        tokens.get_icon_size(),
    );
    ui.set_show_sign_in(false);
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("header-a".into());
    ui.set_active_room_id("!header:example.org".into());
    ui.set_active_room_name(HEADER_ROOM.into());
    ui.set_active_room_topic(HEADER_TOPIC.into());
    ui.set_show_side_panel(false);
    ui.set_details_reveal_open(false);
    ui.set_font_scale_percent(100);
    tokens.set_icon_size(18.0);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;

    // Comfortable: every action and the topic, one click each, no More.
    assert!(ui.get_header_full());
    assert_eq!(assert_header_row(ui, "comfortable").len(), 8);
    assert!(header_buttons(ui, HEADER_MORE).is_empty());
    assert_eq!(on_header_row(ui, control_bounds(ui, HEADER_TOPIC)).len(), 1);
    assert_eq!(header_buttons(ui, HEADER_JUMP).len(), 1, "Jump is drawn");
    let pins = header_buttons(ui, "Show pinned messages");
    click_bounds(ui, pins[0]);
    settle(ui).await;
    assert!(ui.get_show_side_panel() && ui.get_side_panel_tab() == 3);
    // Beside the open details panel with the largest configured icons the
    // eleven actions no longer fit beside a readable name; the icons the
    // vector icon phase draws there (room settings and the details panel)
    // and Leave stay in the header.
    tokens.set_icon_size(28.0);
    settle(ui).await;
    let shown = assert_header_row(ui, "beside details, large icons");
    for label in ["Edit room settings", "Hide details panel"] {
        assert!(
            shown.contains(&label.to_owned()),
            "{label} stays drawn beside details"
        );
    }
    assert_eq!(header_buttons(ui, HEADER_LEAVE).len(), 1);
    tokens.set_icon_size(18.0);

    for (case, width, scale, icon, side) in [
        ("minimum window", 640, 100, 18.0, false),
        ("minimum window at 200 %", 640, 200, 18.0, false),
        ("cramped pane, large icons", 1100, 100, 28.0, true),
        ("cramped pane, large icons at 200 %", 1100, 200, 28.0, true),
    ] {
        ui.window().set_size(slint::PhysicalSize::new(width, 620));
        ui.set_font_scale_percent(scale);
        tokens.set_icon_size(icon);
        ui.set_show_side_panel(side);
        settle(ui).await;
        settle(ui).await;
        assert!(!ui.get_header_full(), "{case}: actions fold");
        let shown = assert_header_row(ui, case);
        assert_eq!(header_buttons(ui, HEADER_MORE).len(), 1, "{case}: one More");
        assert_eq!(shown.len() as i32, ui.get_header_capacity(), "{case}");
        assert!(on_header_row(ui, control_bounds(ui, HEADER_TOPIC)).is_empty());
        let labels = header_foldable_labels(ui);
        let folded: Vec<String> = labels
            .iter()
            .filter(|label| !shown.contains(label))
            .cloned()
            .collect();
        assert!(
            folded.contains(&"Edit room settings".to_owned()),
            "{case}: settings folds first"
        );
        assert!(ui.get_header_leave_shown(), "{case}: Leave keeps its place");
        assert_eq!(
            header_buttons(ui, HEADER_LEAVE).len(),
            1,
            "{case}: Leave is drawn once"
        );
        assert!(ui.get_header_jump_shown(), "{case}: Jump keeps its place");
        assert_eq!(
            header_buttons(ui, HEADER_JUMP).len(),
            1,
            "{case}: Jump is drawn once"
        );

        open_header_more(ui).await;
        let menu = control_bounds(ui, HEADER_MENU);
        assert_eq!(menu.len(), 1, "{case}: one menu");
        assert!(inside_window(ui, menu[0]), "{case}: menu {:?}", menu[0]);
        for label in &labels {
            let listed = header_menu_buttons(ui, label);
            assert_eq!(
                header_buttons(ui, label).len() + listed.len(),
                1,
                "{case}: {label} is reachable exactly once"
            );
            assert_eq!(
                !listed.is_empty(),
                folded.contains(label),
                "{case}: {label} listed"
            );
            for item in listed {
                assert!(inside_window(ui, item), "{case}: {label} {item:?}");
            }
        }
        assert!(
            header_menu_buttons(ui, HEADER_LEAVE).is_empty(),
            "{case}: Leave not listed"
        );
        assert!(
            header_menu_buttons(ui, HEADER_JUMP).is_empty(),
            "{case}: Jump not listed"
        );
        // Keyboard: the first listed action has focus; Down, End, Tab (which
        // wraps inside the menu) and Backtab move through the listed ones.
        let last = folded.last().unwrap();
        assert!(
            focused_control_has_label(ui, &folded[0]),
            "{case}: first item"
        );
        key(ui, Key::DownArrow);
        settle(ui).await;
        assert!(focused_control_has_label(
            ui,
            &folded[1.min(folded.len() - 1)]
        ));
        key(ui, Key::End);
        settle(ui).await;
        assert!(focused_control_has_label(ui, last), "{case}: End");
        key(ui, Key::Tab);
        settle(ui).await;
        assert!(
            focused_control_has_label(ui, &folded[0]),
            "{case}: Tab wraps"
        );
        key(ui, Key::Backtab);
        settle(ui).await;
        assert!(focused_control_has_label(ui, last), "{case}: Backtab wraps");
        assert!(
            !ui.invoke_dispatch_shortcut("p".into(), true, false, false, false),
            "{case}: shortcuts pause while the menu is open"
        );
        assert!(!ui.get_quick_switcher_open());
        // Escape closes it, focus back on More, nothing run.
        let state = (
            ui.get_page(),
            ui.get_show_side_panel(),
            ui.get_side_panel_tab(),
        );
        key(ui, Key::Escape);
        settle(ui).await;
        assert!(!ui.get_header_more_open(), "{case}: Escape closes");
        assert!(control_bounds(ui, HEADER_MENU).is_empty());
        assert!(
            focused_control_has_label(ui, HEADER_MORE),
            "{case}: focus on More"
        );
        assert_eq!(
            state,
            (
                ui.get_page(),
                ui.get_show_side_panel(),
                ui.get_side_panel_tab()
            )
        );
        // Return on More reopens it. A listed action runs as its icon does,
        // after the menu has closed: pins and transfers keep their focus
        // flags and details tabs (opening the panel only narrows the canvas,
        // so what was folded stays folded).
        key(ui, Key::Return);
        settle(ui).await;
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
        settle(ui).await;
        assert!(ui.get_header_more_open(), "{case}: Return opens");
        let mut ran = 0;
        for (label, tab) in [(&labels[2], 3), (&labels[5], 4)] {
            if !folded.contains(label) {
                continue;
            }
            if !ui.get_header_more_open() {
                open_header_more(ui).await;
            }
            let item = header_menu_buttons(ui, label);
            assert_eq!(item.len(), 1, "{case}: {label} is listed");
            click_bounds(ui, item[0]);
            settle(ui).await;
            assert!(!ui.get_header_more_open(), "{case}: the menu closes first");
            assert!(
                ui.get_show_side_panel() && ui.get_side_panel_tab() == tab,
                "{case}: {label}"
            );
            assert_eq!(ui.get_page(), 0);
            ran += 1;
        }
        assert!(ran > 0, "{case}: a listed action ran from More");
    }

    // An action folded while focused hands focus to More and runs nothing;
    // once nothing is folded, More hands it to Search.
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_font_scale_percent(100);
    tokens.set_icon_size(18.0);
    ui.set_show_side_panel(false);
    settle(ui).await;
    assert!(ui.get_header_full());
    request_bridge_focus(ui, "Edit room settings");
    let state = (
        ui.get_page(),
        ui.get_show_side_panel(),
        ui.get_room_admin_open(),
    );
    ui.window().set_size(slint::PhysicalSize::new(640, 620));
    settle(ui).await;
    settle(ui).await;
    assert!(!ui.get_header_full());
    assert!(
        focused_control_has_label(ui, HEADER_MORE),
        "folded focus moves to More"
    );
    assert!(header_buttons(ui, "Edit room settings").is_empty());
    assert!(!ui.get_header_more_open(), "folding opens nothing");
    assert_eq!(
        state,
        (
            ui.get_page(),
            ui.get_show_side_panel(),
            ui.get_room_admin_open()
        )
    );
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;
    settle(ui).await;
    assert!(ui.get_header_full());
    assert!(
        focused_control_has_label(ui, "Search messages"),
        "then to Search"
    );

    // The thread browser's focus return reaches More while threads is folded.
    ui.window().set_size(slint::PhysicalSize::new(640, 620));
    ui.set_font_scale_percent(200);
    settle(ui).await;
    settle(ui).await;
    assert!(header_buttons(ui, "Browse threads in this room").is_empty());
    ui.invoke_focus_composer(false);
    settle(ui).await;
    threads.set_return_focus(threads.get_return_focus() + 1);
    settle(ui).await;
    assert!(
        focused_control_has_label(ui, HEADER_MORE),
        "thread return to More"
    );

    // The call stage from More: the menu closes, then the icon's protected
    // withdraw and page change run.
    open_header_more(ui).await;
    let call = header_menu_buttons(ui, "Open current room call stage");
    click_bounds(ui, call[0]);
    settle(ui).await;
    assert!(!ui.get_header_more_open());
    assert_eq!(ui.get_page(), 2);
    assert!(!threads.get_open());
    ui.set_page(0);
    settle(ui).await;

    // The menu closes with its context: another page, account or room, a
    // surface that owns the window, or sign-in; a blocked More cannot open.
    for change in 0..5 {
        ui.invoke_focus_composer(false);
        settle(ui).await;
        open_header_more(ui).await;
        match change {
            0 => ui.set_page(1),
            1 => ui.set_active_account_local_id("header-b".into()),
            2 => ui.set_active_room_id("!header-other:example.org".into()),
            3 => ui.set_account_menu_open(true),
            _ => ui.set_show_sign_in(true),
        }
        settle(ui).await;
        assert!(
            !ui.get_header_more_open(),
            "context change {change} closes the menu"
        );
        assert!(control_bounds(ui, HEADER_MENU).is_empty());
        if change == 1 || change == 2 {
            assert!(
                focused_control_has_label(ui, HEADER_MORE),
                "change {change}"
            );
        }
        if change == 3 {
            request_bridge_focus(ui, HEADER_MORE);
            key(ui, Key::Return);
            settle(ui).await;
            assert!(!ui.get_header_more_open(), "a blocked More stays closed");
        }
        ui.set_account_menu_open(false);
        ui.set_show_sign_in(false);
        ui.set_page(0);
        ui.set_active_account_local_id("header-a".into());
        ui.set_active_room_id("!header:example.org".into());
        settle(ui).await;
    }

    // Opening More is a protected entry. Refused while another surface is
    // up (a Leave dialog, the thread browser, a link card), it does nothing:
    // that surface stays as it was and no Leave owner is withdrawn for More.
    // The accessibility action is the same entry as a click or Return.
    let leave = ui.global::<crate::RoomLeave>();
    let resets = Rc::new(Cell::new(0));
    let counted = resets.clone();
    leave.on_reset(move || counted.set(counted.get() + 1));
    let activate_more = |ui: &AppWindow| {
        control_with_label(ui, HEADER_MORE).accessible_action(&AccessibilityAction::Default);
    };
    for surface in 0..3 {
        match surface {
            0 => leave.set_view(crate::RoomLeaveView {
                open: true,
                ..Default::default()
            }),
            1 => threads.set_open(true),
            _ => ui.global::<crate::MatrixLink>().set_open(true),
        }
        settle(ui).await;
        let before = resets.get();
        activate_more(ui);
        settle(ui).await;
        assert!(!ui.get_header_more_open(), "surface {surface} refuses More");
        assert_eq!(resets.get(), before, "a refused More withdraws nothing");
        let kept = match surface {
            0 => leave.get_view().open,
            1 => threads.get_open(),
            _ => crate::matrix_links::is_open(ui),
        };
        assert!(kept, "surface {surface} stays open after the refusal");
        leave.set_view(crate::RoomLeaveView::default());
        crate::thread_browser::clear(ui);
        crate::matrix_links::clear(ui);
        settle(ui).await;
    }
    // Identity details are a visible peer too: More refuses before any
    // withdrawal, including the identity dialog's own focus-return intent.
    let identity = ui.global::<crate::IdentityWarnings>();
    identity.set_open(true);
    identity.set_return_focus(true);
    identity.set_awaiting_inspector(true);
    let before_identity = resets.get();
    activate_more(ui);
    assert!(!ui.get_header_more_open());
    assert!(identity.get_open());
    assert!(identity.get_return_focus() && identity.get_awaiting_inspector());
    assert_eq!(
        resets.get(),
        before_identity,
        "visible identity details refuse first"
    );
    crate::identity_warnings::withdraw(ui);
    settle(ui).await;
    // An open invitations center is a visible peer too: More refuses before
    // any withdrawal, and an automatic open a click owns still stands.
    let center = ui.global::<crate::InvitationsState>();
    let owned = crate::live::NavigationAuthority::default();
    crate::matrix_links::own_automatic_open(owned.clone());
    center.set_open(true);
    let before_center = resets.get();
    activate_more(ui);
    assert!(!ui.get_header_more_open() && center.get_open());
    assert!(!owned.withdrawn(), "the open center refuses More first");
    assert_eq!(resets.get(), before_center);
    center.set_open(false);
    // Admitted, it withdraws first: a joined link's message still to come
    // and a Leave owner whose view already hid. While it is open links and
    // the browser are held (a late admission ends for good) and Leave stays
    // available in the menu; closing, even at once, revives nothing.
    let follow_up = crate::matrix_links::tests::install_follow_up();
    let before = resets.get();
    activate_more(ui);
    assert!(ui.get_header_more_open());
    assert!(owned.withdrawn(), "More's entry ended the owned open");
    assert!(
        !ui.get_identity_warnings_allowed(),
        "More owns identity admission"
    );
    assert!(
        follow_up.withdrawn(),
        "More's entry ended the link's follow-up"
    );
    assert_eq!(
        resets.get(),
        before + 1,
        "and withdrew a latent Leave owner"
    );
    settle(ui).await;
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
    settle(ui).await;
    assert!(ui.get_matrix_link_blocked() && crate::thread_browser::held(ui));
    assert!(
        !ui.get_room_leave_blocked(),
        "Leave's own admission is unchanged"
    );
    let late = crate::matrix_links::tests::install_follow_up();
    assert!(!late.admits(ui), "no late navigation over the menu");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!ui.get_header_more_open());
    assert!(
        !late.admits(ui) && follow_up.withdrawn(),
        "closing revives nothing"
    );
    let quick = crate::matrix_links::tests::install_follow_up();
    activate_more(ui);
    activate_more(ui);
    assert!(!ui.get_header_more_open() && quick.withdrawn());
    settle(ui).await;
    assert!(
        !quick.admits(ui),
        "an immediate open and close revives nothing"
    );

    // Leave folds into More only when the room name's shortest readable
    // width leaves it no room (here icons far beyond the configured range).
    // Its item keeps Leave's availability, withdraws before the dialog flag,
    // opens the dialog once the menu has closed, and the dialog's end gives
    // focus back to More.
    let starts = Rc::new(Cell::new(0));
    let started = starts.clone();
    let weak = ui.as_weak();
    leave.on_start(move || {
        started.set(started.get() + 1);
        if let Some(ui) = weak.upgrade() {
            ui.global::<crate::RoomLeave>()
                .set_view(crate::RoomLeaveView {
                    open: true,
                    stage: 1,
                    title: "Leave this room?".into(),
                    name: HEADER_ROOM.into(),
                    room_id: "!header:example.org".into(),
                    ..Default::default()
                });
        }
    });
    let weak = ui.as_weak();
    leave.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<crate::RoomLeave>()
                .set_view(crate::RoomLeaveView::default());
        }
    });
    tokens.set_icon_size(60.0);
    settle(ui).await;
    settle(ui).await;
    assert!(!ui.get_header_leave_shown() && header_buttons(ui, HEADER_LEAVE).is_empty());
    let _ = assert_header_row(ui, "Leave folded");
    ui.set_live_session(false);
    settle(ui).await;
    ui.invoke_focus_composer(false);
    settle(ui).await;
    open_header_more(ui).await;
    let item = header_menu_buttons(ui, HEADER_LEAVE);
    assert_eq!(item.len(), 1, "an unavailable Leave is still listed");
    click_bounds(ui, item[0]);
    settle(ui).await;
    assert!(
        ui.get_header_more_open() && starts.get() == 0,
        "and does nothing"
    );
    key(ui, Key::Escape);
    settle(ui).await;
    ui.set_live_session(true);
    settle(ui).await;
    let before = resets.get();
    open_header_more(ui).await;
    let item = header_menu_buttons(ui, HEADER_LEAVE);
    assert_eq!(item.len(), 1, "Leave is listed once");
    click_bounds(ui, item[0]);
    settle(ui).await;
    assert!(
        !ui.get_header_more_open(),
        "the menu closes before Leave starts"
    );
    assert!(leave.get_view().open && starts.get() == 1);
    assert_eq!(
        resets.get(),
        before + 2,
        "More's entry and Leave's own withdraw ran"
    );
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!leave.get_view().open);
    assert!(
        focused_control_has_label(ui, HEADER_MORE),
        "Leave gives focus back to More"
    );
    // The header owns the focus return of a dialog it opened. Opened from the
    // drawn icon, then folded by a resize while the dialog has focus (28 px
    // icons at 200 %, a 360 px canvas beside the revealed details panel),
    // Escape returns focus to More. Opened from More and drawn again by the
    // time it ends, focus returns to the Leave icon. A protected context that
    // ends the dialog keeps its own focus, and nothing returns later.
    tokens.set_icon_size(28.0);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.invoke_show_transfer_panel();
    settle(ui).await;
    settle(ui).await;
    assert!(ui.get_header_leave_shown());
    let icon = header_buttons(ui, HEADER_LEAVE);
    assert_eq!(icon.len(), 1, "Leave is drawn beside the details panel");
    click_bounds(ui, icon[0]);
    settle(ui).await;
    assert!(leave.get_view().open && !focused_control_has_label(ui, HEADER_LEAVE));
    ui.window().set_size(slint::PhysicalSize::new(640, 620));
    settle(ui).await;
    settle(ui).await;
    assert!(!ui.get_header_leave_shown() && header_buttons(ui, HEADER_LEAVE).is_empty());
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!leave.get_view().open);
    assert!(
        focused_control_has_label(ui, HEADER_MORE),
        "the folded opener's return"
    );
    open_header_more(ui).await;
    let item = header_menu_buttons(ui, HEADER_LEAVE);
    assert_eq!(item.len(), 1, "the folded Leave is listed");
    click_bounds(ui, item[0]);
    settle(ui).await;
    assert!(leave.get_view().open && !ui.get_header_more_open());
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    settle(ui).await;
    settle(ui).await;
    let icon = header_buttons(ui, HEADER_LEAVE);
    assert_eq!(icon.len(), 1, "Leave is drawn again");
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!leave.get_view().open);
    assert!(
        focused_control_has_label(ui, HEADER_LEAVE),
        "the drawn icon takes the return"
    );
    ui.invoke_focus_composer(false);
    settle(ui).await;
    click_bounds(ui, icon[0]);
    settle(ui).await;
    assert!(leave.get_view().open);
    ui.set_show_sign_in(true);
    settle(ui).await;
    assert!(!leave.get_view().open, "sign-in ends the dialog");
    for label in [HEADER_LEAVE, HEADER_MORE] {
        assert!(
            !focused_control_has_label(ui, label),
            "sign-in keeps its focus"
        );
    }
    ui.set_show_sign_in(false);
    settle(ui).await;
    assert!(
        !focused_control_has_label(ui, HEADER_LEAVE),
        "nothing returns later"
    );
    ui.window().set_size(slint::PhysicalSize::new(640, 620));
    ui.set_show_side_panel(false);
    settle(ui).await;
    leave.on_start(|| {});
    leave.on_close(|| {});
    leave.on_reset(|| {});
    leave.set_view(crate::RoomLeaveView::default());
    tokens.set_icon_size(18.0);
    settle(ui).await;
    settle(ui).await;

    // Jump to first unread is the twelfth action. Drawn (comfortable, all
    // twelve inside the window and Tab in header order), as the drawn icon's
    // accessibility action and in overflow (a More item, by keyboard and by
    // its accessibility action), it runs the window's actual jump callback;
    // a running jump relabels and disables it. An account-menu entry that
    // opens and closes at once, and an admitted Thread or Leave entry, each
    // withdraw a held jump; a refused Thread entry leaves every peer alone.
    let unread = ui.global::<crate::UnreadNavigation>();
    let jumps = Rc::new(Cell::new(0));
    let counted = jumps.clone();
    unread.on_jump(move || counted.set(counted.get() + 1));
    let withdrawn = Rc::new(Cell::new(0));
    let counted = withdrawn.clone();
    let weak = ui.as_weak();
    unread.on_withdraw_jump(move || {
        counted.set(counted.get() + 1);
        // As unread_navigation's own withdraw_jump: the held jump ends.
        if let Some(ui) = weak.upgrade() {
            ui.global::<crate::UnreadNavigation>().set_jumping(false);
        }
    });
    let latent = Rc::new(Cell::new(0));
    let counted = latent.clone();
    leave.on_reset(move || counted.set(counted.get() + 1));
    let weak = ui.as_weak();
    leave.on_start(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<crate::RoomLeave>()
                .set_view(crate::RoomLeaveView {
                    open: true,
                    stage: 1,
                    title: "Leave this room?".into(),
                    name: HEADER_ROOM.into(),
                    room_id: "!header:example.org".into(),
                    ..Default::default()
                });
        }
    });
    let weak = ui.as_weak();
    leave.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<crate::RoomLeave>()
                .set_view(crate::RoomLeaveView::default());
        }
    });
    unread.set_can_jump(true);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_font_scale_percent(100);
    settle(ui).await;
    settle(ui).await;
    assert!(ui.get_header_full() && ui.get_header_jump_shown());
    assert_eq!(assert_header_row(ui, "twelve actions").len(), 8);
    let labels = header_foldable_labels(ui);
    request_bridge_focus(ui, &labels[0]);
    let order = [
        labels[1].as_str(),
        HEADER_LEAVE,
        "Search messages",
        HEADER_JUMP,
        labels[2].as_str(),
        labels[3].as_str(),
        labels[4].as_str(),
        labels[5].as_str(),
        labels[6].as_str(),
        labels[7].as_str(),
    ];
    for label in order {
        key(ui, Key::Tab);
        settle(ui).await;
        assert!(focused_control_has_label(ui, label), "Tab reaches {label}");
    }
    key(ui, Key::Backtab);
    settle(ui).await;
    assert!(
        focused_control_has_label(ui, &labels[6]),
        "Backtab goes back"
    );
    let icon = header_buttons(ui, HEADER_JUMP);
    click_bounds(ui, icon[0]);
    settle(ui).await;
    assert_eq!(jumps.get(), 1, "the drawn Jump runs the jump");
    control_with_label(ui, HEADER_JUMP).accessible_action(&AccessibilityAction::Default);
    settle(ui).await;
    assert_eq!(jumps.get(), 2, "and so does its accessibility action");
    unread.set_jumping(true);
    settle(ui).await;
    let loading = "Loading earlier messages to your read position";
    assert!(header_buttons(ui, HEADER_JUMP).is_empty());
    let running = header_buttons(ui, loading);
    assert_eq!(running.len(), 1, "a running jump relabels Jump");
    click_bounds(ui, running[0]);
    settle(ui).await;
    assert_eq!(jumps.get(), 2, "and a running jump disables it");

    // The account menu's guarded entry, opened and closed at once: the held
    // jump, a joined link's follow-up and a latent Leave owner end with it.
    let follow_up = crate::matrix_links::tests::install_follow_up();
    let (before, owners) = (withdrawn.get(), latent.get());
    ui.invoke_open_account_menu();
    ui.set_account_menu_open(false);
    assert!(withdrawn.get() > before && !unread.get_jumping());
    assert!(follow_up.withdrawn() && latent.get() == owners + 1);
    settle(ui).await;
    assert!(
        !follow_up.admits(ui) && !unread.get_jumping(),
        "nothing revives"
    );
    // An admitted Thread entry withdraws a held jump before its own flags.
    unread.set_jumping(true);
    let before = withdrawn.get();
    threads.invoke_show();
    assert!(
        withdrawn.get() > before && !unread.get_jumping(),
        "Thread entry"
    );
    crate::thread_browser::clear(ui);
    settle(ui).await;
    // So does an admitted Leave entry, from its drawn icon.
    unread.set_jumping(true);
    let before = withdrawn.get();
    let icon = header_buttons(ui, HEADER_LEAVE);
    click_bounds(ui, icon[0]);
    settle(ui).await;
    assert!(leave.get_view().open, "Leave opens");
    assert!(
        withdrawn.get() > before && !unread.get_jumping(),
        "Leave entry"
    );
    // Over that visible Leave a Thread entry is refused before it touches a
    // held jump, a link's follow-up or a latent Leave owner.
    unread.set_jumping(true);
    let follow_up = crate::matrix_links::tests::install_follow_up();
    let (before, owners) = (withdrawn.get(), latent.get());
    threads.invoke_show();
    assert_eq!(withdrawn.get(), before, "a refused entry withdraws no jump");
    assert!(unread.get_jumping() && !follow_up.withdrawn());
    assert!(leave.get_view().open && latent.get() == owners && !threads.get_open());
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!leave.get_view().open);
    crate::matrix_links::clear(ui);
    unread.set_jumping(false);
    settle(ui).await;

    // In overflow (far larger icons fold Leave and Jump at the minimum
    // window and 200 %): Jump is listed after Leave, reached by Down and run
    // by Return or by its accessibility action, each after the menu closed.
    ui.window().set_size(slint::PhysicalSize::new(640, 620));
    ui.set_font_scale_percent(200);
    tokens.set_icon_size(60.0);
    settle(ui).await;
    settle(ui).await;
    assert!(!ui.get_header_jump_shown() && header_buttons(ui, HEADER_JUMP).is_empty());
    let _ = assert_header_row(ui, "Jump folded");
    open_header_more(ui).await;
    assert_eq!(header_menu_buttons(ui, HEADER_JUMP).len(), 1, "Jump listed");
    assert!(focused_control_has_label(ui, &labels[0]), "first item");
    for label in [labels[1].as_str(), HEADER_LEAVE, HEADER_JUMP] {
        key(ui, Key::DownArrow);
        settle(ui).await;
        assert!(focused_control_has_label(ui, label), "Down reaches {label}");
    }
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(jumps.get(), 3, "the Jump item runs the jump");
    assert!(!ui.get_header_more_open() && focused_control_has_label(ui, HEADER_MORE));
    open_header_more(ui).await;
    control_with_label(ui, HEADER_JUMP).accessible_action(&AccessibilityAction::Default);
    settle(ui).await;
    assert_eq!(jumps.get(), 4, "and so does its accessibility action");
    assert!(!ui.get_header_more_open());
    unread.on_jump(|| {});
    unread.on_withdraw_jump(|| {});
    unread.set_can_jump(false);
    unread.set_jumping(false);
    leave.on_start(|| {});
    leave.on_close(|| {});
    leave.on_reset(|| {});
    leave.set_view(crate::RoomLeaveView::default());
    tokens.set_icon_size(18.0);
    settle(ui).await;
    settle(ui).await;

    // Without a live session the room actions are unavailable in the menu as
    // in the header: a click on one neither acts nor closes the menu.
    ui.set_show_side_panel(false);
    ui.set_live_session(false);
    settle(ui).await;
    settle(ui).await;
    if focused_control_has_label(ui, HEADER_MORE) {
        key(ui, Key::Return);
        settle(ui).await;
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
        settle(ui).await;
    } else {
        open_header_more(ui).await;
    }
    assert!(ui.get_header_more_open());
    assert!(focused_control_has_label(
        ui,
        "Open current room call stage"
    ));
    let pins = header_menu_buttons(ui, "Show pinned messages");
    assert_eq!(pins.len(), 1, "an unavailable action is still listed");
    click_bounds(ui, pins[0]);
    settle(ui).await;
    assert!(ui.get_header_more_open() && !ui.get_show_side_panel());
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!ui.get_header_more_open());
    assert!(focused_control_has_label(ui, HEADER_MORE));

    // Signed out at a medium width the enabled call stage and details panel
    // stay drawn and every folded action is unavailable. Opened by a click or
    // by the accessibility action (focus elsewhere), the menu still lists
    // them all, More holds focus, and Tab and Shift+Tab stay on More.
    ui.window().set_size(slint::PhysicalSize::new(940, 620));
    ui.set_font_scale_percent(100);
    settle(ui).await;
    settle(ui).await;
    assert!(!ui.get_header_full() && ui.get_header_capacity() >= 4);
    let shown = assert_header_row(ui, "signed out, medium width");
    for drawn in ["Open current room call stage", "Show details panel"] {
        assert!(shown.contains(&drawn.to_owned()), "{drawn} stays drawn");
    }
    let folded: Vec<String> = header_foldable_labels(ui)
        .into_iter()
        .filter(|label| !shown.contains(label))
        .collect();
    assert!(!folded.is_empty(), "actions fold at this width");
    for by_click in [true, false] {
        if by_click {
            open_header_more(ui).await;
        } else {
            request_bridge_focus(ui, "Open current room call stage");
            control_with_label(ui, HEADER_MORE).accessible_action(&AccessibilityAction::Default);
            settle(ui).await;
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
            settle(ui).await;
            assert!(
                ui.get_header_more_open(),
                "the accessibility action opens More"
            );
        }
        assert!(
            focused_control_has_label(ui, HEADER_MORE),
            "More holds focus"
        );
        for label in &folded {
            assert_eq!(
                header_menu_buttons(ui, label).len(),
                1,
                "{label} stays listed"
            );
        }
        for tab in [Key::Tab, Key::Backtab, Key::Tab] {
            key(ui, tab);
            settle(ui).await;
            assert!(ui.get_header_more_open(), "Tab never leaves the open menu");
            assert!(
                focused_control_has_label(ui, HEADER_MORE),
                "focus stays on More"
            );
        }
        key(ui, Key::Escape);
        settle(ui).await;
        assert!(!ui.get_header_more_open() && focused_control_has_label(ui, HEADER_MORE));
    }

    let (page, live, sign_in, account, room, name, topic, side, tab, reveal, scale, size, icon) =
        saved_header_state;
    tokens.set_icon_size(icon);
    ui.set_font_scale_percent(scale);
    ui.window().set_size(size);
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    ui.set_active_room_name(name);
    ui.set_active_room_topic(topic);
    ui.set_live_session(live);
    match tab {
        0 => ui.invoke_show_members_panel(),
        1 => ui.invoke_show_thread_panel(),
        2 => ui.invoke_focus_room_search(),
        3 => ui.invoke_show_pinned_panel(),
        4 => ui.invoke_show_transfer_panel(),
        5 => ui.invoke_show_room_media(),
        _ => panic!("unexpected details tab"),
    }
    ui.set_page(page);
    ui.set_show_side_panel(side);
    ui.set_details_reveal_open(reveal);
    ui.set_show_sign_in(sign_in);
    ui.invoke_focus_composer(false);
    settle(ui).await;
}

/// Phase 86: identity-change warnings. First the row and details through
/// stand-in callbacks: the row under the room header, the details' focus trap
/// (member list, View member, Close), paused shortcuts, Escape with focus
/// back on Details, View member routing only the exact listed ID through the
/// production routing and guarded inspector entry, focus back on Details
/// after the person closes the inspector, the bounds at 940x620 and 200% text
/// with a long list (also across a text-scale change), a row that stays when
/// every followed warning cleared but some could not be followed, protected
/// withdrawal of the details and of the inspector that never pulls focus
/// from the protected surface, the exact View member gate, and rescoping on
/// room and account changes. Then the registered production controller with
/// no account (nothing is ever checked): a reset clears every row
/// synchronously, Details stays closed without a checked scope, and an
/// unlisted ID is never routed. No account, network or vault is used.
async fn assert_identity_warnings_presentation(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
) {
    use crate::{IdentityWarningItem, IdentityWarnings, MemberInspectorView, MemberPrivacy};
    use morpheus_matrix::{IdentityWarning, IdentityWarningKind, IdentityWarnings as Warnings};
    use slint::private_unstable_api::re_exports::WindowInner;
    use slint::{Model, ModelRc, VecModel};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    let focused_item = |ui: &AppWindow| {
        WindowInner::from_pub(ui.window())
            .focus_item
            .borrow()
            .upgrade()
    };
    // The account access surface (either of its titles) holds focus.
    let in_access_dialog = |ui: &AppWindow| {
        focused_control_has_label(ui, "Local app password")
            || focused_control_has_label(ui, "Remove saved account")
    };
    let warnings = ui.global::<IdentityWarnings>();
    let privacy = ui.global::<MemberPrivacy>();
    let routed = Rc::new(RefCell::new(Vec::<String>::new()));
    let scopes = Rc::new(Cell::new(0));
    let weak = ui.as_weak();
    warnings.on_show_details(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<IdentityWarnings>().set_open(true);
        }
    });
    let weak = ui.as_weak();
    warnings.on_close_details(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<IdentityWarnings>().set_open(false);
        }
    });
    // View member uses the production routing: the details, the window's
    // admission and the security gate are read when it is pressed, then the
    // inspector's guarded entry. Only the scope's account check, which needs
    // a real account, is left to the registered controller below.
    let weak = ui.as_weak();
    let sink = routed.clone();
    let gate = live.security_gate.clone();
    warnings.on_view_member(move |user| {
        sink.borrow_mut().push(user.to_string());
        if let Some(ui) = weak.upgrade() {
            crate::identity_warnings::route_member(&ui, &gate, user.as_str());
        }
    });
    let counted = scopes.clone();
    warnings.on_context_changed(move || counted.set(counted.get() + 1));
    // The inspector opens synchronously, as the production entry does; no
    // member is read.
    let inspected = Rc::new(RefCell::new(Vec::<String>::new()));
    let sink = inspected.clone();
    let weak = ui.as_weak();
    privacy.on_inspect(move |user| {
        sink.borrow_mut().push(user.to_string());
        if let Some(ui) = weak.upgrade() {
            ui.global::<MemberPrivacy>()
                .set_inspector(MemberInspectorView {
                    open: true,
                    loading: true,
                    user_id: user,
                    ..Default::default()
                });
        }
    });
    let weak = ui.as_weak();
    privacy.on_close_inspector(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<MemberPrivacy>()
                .set_inspector(MemberInspectorView::default());
        }
    });

    let before = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_active_account_local_id(),
        ui.get_active_room_id(),
    );
    ui.set_page(0);
    ui.set_live_session(true);
    ui.set_active_account_local_id("work".into());
    ui.set_active_room_id("!room:example.org".into());
    settle(ui).await;

    // The production text for what the SDK reported.
    let reported = Warnings {
        members: vec![
            IdentityWarning {
                user_id: "@sam:example.org".into(),
                name: Some("Sam".into()),
                kind: IdentityWarningKind::VerifiedChanged,
            },
            IdentityWarning {
                user_id: "@pat:example.org".into(),
                name: Some("Pat".into()),
                kind: IdentityWarningKind::Changed,
            },
        ],
        omitted: 0,
        at_least: false,
    };
    let seed = |rows: Vec<IdentityWarningItem>, summary: &str| {
        warnings.set_rows(ModelRc::new(VecModel::from(rows)));
        warnings.set_summary(summary.into());
        warnings.set_overflow("".into());
    };
    let two = || {
        vec![
            IdentityWarningItem {
                user_id: "@sam:example.org".into(),
                display_id: "@sam:example.org".into(),
                label: "Sam".into(),
                detail: "Identity changed after you verified it".into(),
            },
            IdentityWarningItem {
                user_id: "@pat:example.org".into(),
                display_id: "@pat:example.org".into(),
                label: "Pat".into(),
                detail: "Identity changed since it was first seen".into(),
            },
        ]
    };
    seed(two(), &crate::identity_warnings::summary(&reported));
    settle(ui).await;
    assert_eq!(
        button_bounds(ui, "Show identity change details").len(),
        1,
        "one quiet row with Details"
    );

    let sam = "Member 1 of 2: Sam, @sam:example.org, Identity changed after you verified it";
    let pat = "Member 2 of 2: Pat, @pat:example.org, Identity changed since it was first seen";
    click_control_once(ui, "Show identity change details");
    settle(ui).await;
    assert!(warnings.get_open());
    assert!(
        focused_control_has_label(ui, sam),
        "focus starts on the list"
    );
    key(ui, Key::DownArrow);
    settle(ui).await;
    assert!(focused_control_has_label(ui, pat));
    key(ui, Key::Tab);
    assert!(focused_control_has_label(ui, "View member details for Pat"));
    key(ui, Key::Tab);
    assert!(focused_control_has_label(ui, "Close identity changes"));
    key(ui, Key::Tab);
    assert!(focused_control_has_label(ui, pat), "Tab stays inside");
    key(ui, Key::Backtab);
    assert!(focused_control_has_label(ui, "Close identity changes"));
    key(ui, Key::Backtab);
    assert!(focused_control_has_label(ui, "View member details for Pat"));
    key(ui, Key::Backtab);
    assert!(focused_control_has_label(ui, pat), "Shift+Tab stays inside");
    assert!(
        !ui.invoke_dispatch_shortcut("p".into(), true, false, false, false),
        "shortcuts pause while the details are open"
    );
    assert!(!ui.get_quick_switcher_open());
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!warnings.get_open());
    assert!(focused_control_has_label(
        ui,
        "Show identity change details"
    ));

    // View member routes only the selected row's exact ID through the
    // guarded inspector entry and closes the details; when the person closes
    // the inspector, focus returns to the row. Details has focus again, so
    // the keyboard opens it.
    key(ui, Key::Return);
    settle(ui).await;
    assert!(warnings.get_open());
    key(ui, Key::End);
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(*routed.borrow(), ["@pat:example.org"]);
    assert_eq!(*inspected.borrow(), ["@pat:example.org"]);
    assert!(!warnings.get_open());
    assert!(privacy.get_inspector().open);
    assert!(!focused_control_has_label(
        ui,
        "Show identity change details"
    ));
    privacy.invoke_close_inspector();
    settle(ui).await;
    assert!(focused_control_has_label(
        ui,
        "Show identity change details"
    ));

    // The smallest window at 200% text: the dialog fits, its member list
    // scrolls to the selected row, and the fixed Close stays inside it.
    ui.window().set_size(slint::PhysicalSize::new(940, 620));
    ui.set_font_scale_percent(200);
    let many: Vec<IdentityWarningItem> = (0..30)
        .map(|index| IdentityWarningItem {
            user_id: format!("@m{index}:example.org").into(),
            display_id: format!("@m{index}:example.org").into(),
            label: format!("Member {index}").into(),
            detail: "Identity changed since it was first seen".into(),
        })
        .collect();
    seed(many, "30 members' identities changed.");
    warnings.set_overflow("And 4 more members.".into());
    settle(ui).await;
    warnings.invoke_show_details();
    settle(ui).await;
    let panel = item_bounds(&control_with_label(ui, "Identity change details"));
    assert!(
        inside_window(ui, panel),
        "the dialog fits the window: {panel:?}"
    );
    let close = item_bounds(&control_with_label(ui, "Close identity changes"));
    assert!(
        close.top >= panel.top - 0.5 && close.bottom <= panel.bottom + 0.5,
        "Close stays inside the dialog: {close:?} in {panel:?}"
    );
    key(ui, Key::End);
    settle(ui).await;
    let last_row = "Member 29, @m29:example.org, Identity changed since it was first seen";
    let last = item_bounds(&control_with_label(ui, last_row));
    assert!(
        last.top >= panel.top - 0.5 && last.bottom <= close.top + 0.5,
        "the selected last row is scrolled into view: {last:?} above {close:?}"
    );
    // The list's own scroll area (the focused list, labelled with the
    // selection) contains the whole selected row: the reveal uses the same
    // row pitch as the layout, also after a text-scale change.
    for percent in [200, 150, 200] {
        ui.set_font_scale_percent(percent);
        settle(ui).await;
        let list = item_bounds(&control_with_label(
            ui,
            &format!("Member 30 of 30: {last_row}"),
        ));
        let close = item_bounds(&control_with_label(ui, "Close identity changes"));
        let last = item_bounds(&control_with_label(ui, last_row));
        assert!(
            last.top >= list.top - 0.5 && last.bottom <= list.bottom + 0.5,
            "{percent}%: the selected last row is inside the list: {last:?} in {list:?}"
        );
        assert!(
            last.bottom <= close.top + 0.5,
            "{percent}%: the selected last row is above Close: {last:?} above {close:?}"
        );
    }
    key(ui, Key::Escape);
    settle(ui).await;
    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));

    // Every followed warning cleared but some could not be followed: the
    // row stays, with no count, and the details say only that.
    let unfollowed = Warnings {
        members: Vec::new(),
        omitted: 0,
        at_least: true,
    };
    seed(Vec::new(), &crate::identity_warnings::summary(&unfollowed));
    warnings.set_overflow(crate::identity_warnings::overflow(&unfollowed).into());
    settle(ui).await;
    assert_eq!(
        button_bounds(ui, "Show identity change details").len(),
        1,
        "the row stays while some warnings could not be followed"
    );
    warnings.invoke_show_details();
    settle(ui).await;
    assert!(warnings.get_open());
    assert!(
        focused_control_has_label(ui, "Close identity changes"),
        "nothing to list: focus starts on Close"
    );
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!warnings.get_open());
    assert!(focused_control_has_label(
        ui,
        "Show identity change details"
    ));
    seed(two(), &crate::identity_warnings::summary(&reported));
    settle(ui).await;

    // A protected surface closes the details at once and keeps its own
    // focus: nothing is pulled back to the row behind it, then or later.
    warnings.invoke_show_details();
    settle(ui).await;
    assert!(warnings.get_open());
    ui.set_access_open(true);
    settle(ui).await;
    assert!(
        !warnings.get_open(),
        "a protected surface closed the details"
    );
    assert!(in_access_dialog(ui), "the protected surface has focus");
    let replaced = focused_item(ui);
    settle(ui).await;
    assert!(focused_item(ui) == replaced, "and keeps it");
    ui.set_access_open(false);
    settle(ui).await;
    assert!(
        !focused_control_has_label(ui, "Show identity change details"),
        "no deferred return to the row"
    );

    // The same for the inspector opened from View member.
    warnings.invoke_show_details();
    settle(ui).await;
    key(ui, Key::Return);
    settle(ui).await;
    assert_eq!(
        inspected.borrow().last().map(String::as_str),
        Some("@sam:example.org")
    );
    assert!(privacy.get_inspector().open && warnings.get_awaiting_inspector());
    ui.set_access_open(true);
    settle(ui).await;
    assert!(
        !privacy.get_inspector().open,
        "a protected surface closed the inspector"
    );
    assert!(!warnings.get_awaiting_inspector());
    assert!(in_access_dialog(ui), "the protected surface has focus");
    let replaced = focused_item(ui);
    settle(ui).await;
    assert!(focused_item(ui) == replaced, "and keeps it");
    ui.set_access_open(false);
    settle(ui).await;
    assert!(
        !focused_control_has_label(ui, "Show identity change details"),
        "no deferred return to the row"
    );

    // The exact View member gate, read when it is pressed: closed details, a
    // protected surface the window's handlers have not applied yet, a held
    // security gate, an inspector already open, and the inspector's own
    // admission each refuse without a read; with nothing in the way exactly
    // the one listed ID is routed.
    let gate = &live.security_gate;
    let route = |user: &str| crate::identity_warnings::route_member(ui, gate, user);
    let reads = inspected.borrow().len();
    warnings.set_open(false);
    assert!(!route("@pat:example.org"), "closed details");
    warnings.set_open(true);
    ui.set_access_open(true);
    assert!(
        !route("@pat:example.org"),
        "a protected surface, read at once"
    );
    ui.set_access_open(false);
    let held = gate
        .try_account_access()
        .expect("the security gate is idle");
    assert!(!route("@pat:example.org"), "a held security gate");
    drop(held);
    privacy.set_inspector(MemberInspectorView {
        open: true,
        ..Default::default()
    });
    assert!(!route("@pat:example.org"), "an inspector already open");
    privacy.set_inspector(MemberInspectorView::default());
    warnings.set_open(false);
    ui.set_security_recovery_key("synthetic-key-not-a-credential".into());
    assert!(
        !crate::member_privacy::inspect_admitted(ui, gate, "@pat:example.org"),
        "the inspector's own admission"
    );
    ui.set_security_recovery_key("".into());
    assert_eq!(
        inspected.borrow().len(),
        reads,
        "nothing was opened or read"
    );
    warnings.set_open(true);
    assert!(route("@pat:example.org"));
    assert_eq!(inspected.borrow()[reads..], ["@pat:example.org"]);
    assert!(!warnings.get_open() && privacy.get_inspector().open);
    settle(ui).await;
    privacy.invoke_close_inspector();
    settle(ui).await;
    assert!(!warnings.get_awaiting_inspector());
    assert!(focused_control_has_label(
        ui,
        "Show identity change details"
    ));
    // IWCOMP01: accepted IDs with a line break, a direction override, more
    // than 255 bytes, non-ASCII and a tab, as production rows (no, blank or
    // unreadable names are all `None` there; one safe name). The row's
    // summary and every label read aloud carry only the bounded, escaped ID;
    // View member hands the inspector the exact ID.
    let long = format!("@{}:example.org", "a".repeat(300));
    let hostile = [
        ("@pat\nadmin:example.org", None),
        ("@a\u{202e}gro.x:evil.org", Some("Ari")),
        (long.as_str(), None),
        ("@\u{fc}\u{2066}\tx:example.org", None),
    ];
    let members: Vec<IdentityWarning> = hostile
        .iter()
        .map(|&(user, name)| {
            let typed = matrix_sdk::ruma::UserId::parse(user);
            assert!(typed.is_ok(), "accepted as the SDK parses it: {user:?}");
            IdentityWarning {
                user_id: user.into(),
                name: name.map(str::to_owned),
                kind: IdentityWarningKind::Changed,
            }
        })
        .collect();
    let shown: Vec<String> = members.iter().map(IdentityWarning::display_id).collect();
    let rows = members.iter().map(crate::identity_warnings::item).collect();
    let alone = Warnings {
        members: vec![members[0].clone()],
        ..Warnings::default()
    };
    seed(rows, &crate::identity_warnings::summary(&alone));
    settle(ui).await;
    let check = "Check with them before sharing anything sensitive.";
    let expected = format!("{}'s identity changed. {check}", shown[0]);
    assert_eq!(warnings.get_summary().as_str(), expected);
    for (index, row) in warnings.get_rows().iter().enumerate() {
        assert_eq!(row.user_id.as_str(), hostile[index].0, "the exact key");
        assert_eq!(row.display_id.as_str(), shown[index]);
        let label = hostile[index].1.unwrap_or(shown[index].as_str());
        assert_eq!(row.label.as_str(), label);
    }
    warnings.invoke_show_details();
    settle(ui).await;
    let detail = "Identity changed since it was first seen";
    let first = format!("Member 1 of 4: {0}, {0}, {detail}", shown[0]);
    assert!(focused_control_has_label(ui, &first), "{first:?}");
    key(ui, Key::Tab);
    let view_first = format!("View member details for {}", shown[0]);
    assert!(focused_control_has_label(ui, &view_first));
    key(ui, Key::Backtab);
    key(ui, Key::DownArrow);
    settle(ui).await;
    let second = format!("Member 2 of 4: Ari, {}, {detail}", shown[1]);
    assert!(focused_control_has_label(ui, &second));
    for (index, id) in shown.iter().enumerate() {
        let label = hostile[index].1.unwrap_or(id.as_str());
        let row = format!("{label}, {id}, {detail}");
        assert_eq!(labelled_bounds(ui, &row, None).len(), 1, "{row:?}");
    }
    let spoken = {
        use slint::private_unstable_api::re_exports::{
            AccessibleStringProperty, ItemRc, WindowInner,
        };
        use std::ops::ControlFlow;

        let window = WindowInner::from_pub(ui.window());
        let root = ItemRc::new_root(window.component());
        let mut spoken = Vec::new();
        let _ = root.visit_descendants(|item| {
            let label = item.accessible_string_property(AccessibleStringProperty::Label);
            if let Some(label) = label {
                spoken.push(label.to_string());
            }
            ControlFlow::<()>::Continue(())
        });
        spoken
    };
    for raw in [
        "pat\nadmin",
        "a\u{202e}gro",
        "\u{fc}\u{2066}\tx",
        long.as_str(),
    ] {
        assert!(!spoken.iter().any(|label| label.contains(raw)), "{raw:?}");
    }
    for id in shown.iter().map(String::as_str) {
        assert!(spoken.iter().any(|label| label.contains(id)), "{id:?}");
    }
    key(ui, Key::Return);
    settle(ui).await;
    let exact = Some(hostile[1].0);
    assert_eq!(routed.borrow().last().map(String::as_str), exact);
    assert_eq!(inspected.borrow().last().map(String::as_str), exact);
    assert!(!warnings.get_open() && privacy.get_inspector().open);
    privacy.invoke_close_inspector();
    settle(ui).await;
    seed(two(), &crate::identity_warnings::summary(&reported));
    settle(ui).await;

    // A room or account change rescopes the warnings (production resets them).
    let scoped = scopes.get();
    ui.set_active_room_id("!other:example.org".into());
    settle(ui).await;
    ui.set_active_account_local_id("home".into());
    settle(ui).await;
    assert_eq!(scopes.get(), scoped + 2, "each change rescopes");
    ui.set_active_account_local_id("work".into());
    ui.set_active_room_id("!room:example.org".into());
    settle(ui).await;

    // The current window's modal lists include the details both ways: while
    // they are open a Matrix link, discovery, the member inspector and
    // mentions are blocked, and opening room addresses closes them.
    warnings.set_open(true);
    assert!(ui.get_identity_warnings_allowed());
    let waiting = ui.get_matrix_link_blocked() && !ui.get_discovery_context_allowed();
    assert!(waiting, "a Matrix link and discovery wait");
    let refused = !ui.get_member_inspector_allowed() && !ui.get_mention_ui_allowed();
    assert!(refused, "so do the member inspector and mentions");
    assert!(!ui.get_thread_browser_allowed() && ui.get_room_leave_blocked());
    let browser = ui.global::<crate::ThreadBrowser>();
    warnings.set_open(false);
    browser.set_open(true);
    assert!(
        !ui.get_identity_warnings_allowed(),
        "Threads hold identity details admission"
    );
    browser.set_open(false);
    let leave = ui.global::<crate::RoomLeave>();
    leave.set_view(crate::RoomLeaveView {
        open: true,
        ..Default::default()
    });
    assert!(
        !ui.get_identity_warnings_allowed(),
        "Leave holds identity details admission"
    );
    leave.set_view(crate::RoomLeaveView::default());
    // The invitations center too, both ways: an open center holds identity
    // details' and the member inspector's admission (the inspector's own
    // guarded entry refuses at once), and open details refuse the center's
    // Home entry before anything is withdrawn.
    let center = ui.global::<crate::InvitationsState>();
    center.set_open(true);
    assert!(
        !ui.get_identity_warnings_allowed(),
        "the center holds identity details admission"
    );
    assert!(!ui.get_member_inspector_allowed(), "and the inspector's");
    assert!(
        !crate::member_privacy::inspect_admitted(ui, &live.security_gate, "@pat:example.org"),
        "the inspector's own admission refuses"
    );
    center.set_open(false);
    warnings.set_open(true);
    ui.invoke_open_invitations();
    assert!(!center.get_open(), "open details refuse the center");
    assert!(warnings.get_open(), "and stay");
    let addresses = ui.global::<crate::RoomAddresses>();
    addresses.set_open(true);
    let allowed = ui.get_identity_warnings_allowed();
    assert!(!allowed, "room addresses are another modal");
    settle(ui).await;
    assert!(!warnings.get_open(), "and they close the details");
    addresses.set_open(false);
    settle(ui).await;

    // The actual protected entry withdraws details and old focus intent
    // synchronously, before Slint change handlers. The room warning row stays.
    warnings.set_open(true);
    warnings.set_return_focus(true);
    warnings.set_awaiting_inspector(true);
    let summary_before_entry = warnings.get_summary();
    ui.invoke_open_account_menu();
    assert!(ui.get_account_menu_open());
    assert!(!warnings.get_open() && !warnings.get_return_focus());
    assert!(!warnings.get_awaiting_inspector());
    assert_eq!(warnings.get_summary(), summary_before_entry);
    ui.set_account_menu_open(false);
    settle(ui).await;
    assert!(
        !warnings.get_open(),
        "closing the protected surface never reopens details"
    );

    // The production controller, with no account behind it.
    let controller =
        crate::identity_warnings::IdentityWarningsUi::register(ui, live.clone(), runtime);
    seed(two(), &crate::identity_warnings::summary(&reported));
    warnings.set_open(true);
    warnings.set_overflow("And 1 more member.".into());
    crate::identity_warnings::reset(ui);
    // Synchronous: nothing of the previous scope survives this call.
    assert!(!warnings.get_open());
    assert_eq!(warnings.get_rows().row_count(), 0);
    assert!(warnings.get_summary().is_empty() && warnings.get_overflow().is_empty());
    settle(ui).await;
    // Not checked (no live account): Details never opens, and View member
    // never routes an ID that is not listed, even with the details forced
    // open.
    let reads = inspected.borrow().len();
    seed(two(), "unchecked");
    warnings.invoke_show_details();
    warnings.invoke_view_member("@pat:example.org".into());
    settle(ui).await;
    assert!(!warnings.get_open());
    warnings.set_open(true);
    warnings.invoke_view_member("@pat:example.org".into());
    assert_eq!(
        inspected.borrow().len(),
        reads,
        "nothing routed without a scope"
    );
    assert!(warnings.get_open() && !privacy.get_inspector().open);
    crate::identity_warnings::reset(ui);
    settle(ui).await;

    controller.shutdown(ui);
    assert_eq!(warnings.get_rows().row_count(), 0);
    ui.global::<MemberPrivacy>().on_inspect(|_| {});
    let (page, live_session, account, room) = before;
    ui.set_page(page);
    ui.set_live_session(live_session);
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    settle(ui).await;
}

/// MOD01 through the registered production callbacks and Slint change
/// hooks, with the core's permission read held: a redaction whose dialog
/// closed, a member action whose room changed and came back (A-B-A, no
/// change fired) and one across a security step that began and ended are
/// never handed off; with nothing changed each kind is sent once. The
/// held core stands in for the account (no account, network or vault).
async fn assert_moderation_stops_with_its_context(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
) {
    use crate::moderation_ui::{HeldModerator, ModerationUi};
    use std::sync::atomic::Ordering;

    async fn within(wait: impl std::future::Future<Output = ()>) {
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the held moderation core answers");
    }
    async fn until(ui: &AppWindow, done: impl Fn(&AppWindow) -> bool) {
        let wait = async {
            while !done(ui) {
                settle(ui).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the moderation completion reaches the window");
    }

    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    let shown = "!moderated:example.org";
    let event = "$moderated:example.org";
    let dana = "@dana:example.org";
    ui.set_active_account_local_id("moderation-account".into());
    ui.set_active_room_id(shown.into());
    ui.set_room_admin_account_id("moderation-account".into());
    ui.set_room_admin_room_id(shown.into());
    ui.set_room_admin_event_id(event.into());
    ui.set_room_admin_mode(2);
    ui.set_room_admin_open(true);
    settle(ui).await;
    let was_live = live.is_live.swap(true, Ordering::AcqRel);
    let core = Arc::new(HeldModerator::new());
    let moderation = ModerationUi::register(ui, live.clone(), runtime, core.clone(), |_, _, _| {});

    // Actual Escape closes while busy, cancelling before the held read is
    // released. No direct property setter bypasses the visible control.
    ui.invoke_submit_room_admin(2, event.into(), "".into());
    assert!(ui.get_room_admin_busy());
    within(core.wait_reads(1)).await;
    key(ui, Key::Escape);
    assert!(!ui.get_room_admin_open());
    assert!(
        !ui.get_room_admin_busy(),
        "closing the dialog ends its redaction"
    );
    core.release();
    within(core.wait_answered(1)).await;
    assert!(core.sent().is_empty());

    // The visible Cancel button stays focused and enabled during the next
    // held read. Return activates that button through normal key routing.
    ui.set_room_admin_open(true);
    settle(ui).await;
    ui.invoke_submit_room_admin(2, event.into(), "".into());
    assert!(ui.get_room_admin_busy());
    within(core.wait_reads(2)).await;
    key(ui, Key::Return);
    assert!(!ui.get_room_admin_open());
    assert!(!ui.get_room_admin_busy());
    core.release();
    within(core.wait_answered(2)).await;
    assert!(core.sent().is_empty());

    // The room changes and comes back before the hand-off, unsettled:
    // whether or not equal final strings fire a change, the navigation
    // epoch captured at confirmation refuses it.
    ui.invoke_submit_member_action(1, dana.into(), "".into());
    assert!(ui.get_member_action_busy());
    within(core.wait_reads(3)).await;
    live.invalidate_navigation();
    ui.set_active_room_id("!elsewhere:example.org".into());
    live.invalidate_navigation();
    ui.set_active_room_id(shown.into());
    core.release();
    within(core.wait_answered(3)).await;
    assert!(core.sent().is_empty());
    until(ui, |ui| !ui.get_member_action_busy()).await;
    assert!(!ui.get_member_action_error().is_empty());

    // A security step in progress refuses at once; one that began and ended
    // while the read was held refuses at the hand-off.
    let gate = &live.security_gate;
    let lease = gate.try_account_access().expect("the gate is idle");
    ui.invoke_submit_member_action(2, dana.into(), "".into());
    assert!(!ui.get_member_action_busy());
    assert!(ui.get_member_action_error().contains("Nothing was sent"));
    drop(lease);
    ui.invoke_submit_member_action(2, dana.into(), "".into());
    assert!(ui.get_member_action_busy());
    within(core.wait_reads(4)).await;
    drop(gate.try_account_access().expect("the gate is idle"));
    assert!(!gate.blocked());
    core.release();
    within(core.wait_answered(4)).await;
    assert!(core.sent().is_empty());
    until(ui, |ui| !ui.get_member_action_busy()).await;

    // Nothing changed: each kind is handed off exactly once.
    ui.set_room_admin_open(true);
    settle(ui).await;
    ui.invoke_submit_room_admin(2, event.into(), "".into());
    core.release();
    within(core.wait_answered(5)).await;
    until(ui, |ui| !ui.get_room_admin_busy()).await;
    assert_eq!(ui.get_room_admin_notice().as_str(), "Message redacted");
    ui.invoke_submit_member_action(0, dana.into(), "".into());
    core.release();
    within(core.wait_answered(6)).await;
    assert_eq!(core.sent(), ["redact", "member"]);
    // Ends the member refresh wait; the completion still clears busy.
    live.invalidate_navigation();
    until(ui, |ui| !ui.get_member_action_busy()).await;
    assert!(ui.get_member_action_notice().contains("completed"));

    moderation.shutdown();
    live.is_live.store(was_live, Ordering::Release);
    ui.on_submit_member_action(|_, _, _| {});
    ui.on_submit_room_admin(|_, _, _| {});
    ui.on_moderation_context_changed(|_| {});
    ui.set_room_admin_open(false);
    ui.set_room_admin_mode(0);
    ui.set_room_admin_account_id("".into());
    ui.set_room_admin_room_id("".into());
    ui.set_room_admin_event_id("".into());
    ui.set_room_admin_error("".into());
    ui.set_room_admin_notice("".into());
    ui.set_member_action_error("".into());
    ui.set_member_action_notice("".into());
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    settle(ui).await;
}

/// The installed font browser fills the existing font roles through the
/// production controller (a test catalog stands in for the system fonts):
/// - coalesced search and a monospaced filter;
/// - a sample-only preview;
/// - choose and inherit writing only the role's key;
/// - a hand-typed missing family kept and shown as not found;
/// - the browser's scope fenced against A -> B -> A and against an account
///   change no hook reported.
async fn assert_font_browser(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;

    let store = ConfigStore::at(directory.join("fonts.toml"));
    let initial = store.load_or_create().unwrap();
    let switch = |ui: &AppWindow, account: &str| {
        ui.set_active_account_local_id(account.into());
        crate::appearance::apply_theme_mode(ui);
    };
    apply_config(ui, &initial, None);
    switch(ui, "font-a");
    crate::theme_studio::register(ui, &store);
    crate::theme_studio::font_catalog_for_test(vec![
        ("Inter", false, vec![400, 700]),
        ("JetBrains Mono", true, vec![400]),
        ("Literata", false, vec![400]),
    ]);
    // Direct saves, so the file shows each choice.
    ui.set_page(0);
    settle(ui).await;
    let names = |ui: &AppWindow| -> Vec<String> {
        ui.get_font_browser_results()
            .iter()
            .filter(|item| !item.generic)
            .map(|item| item.name.to_string())
            .collect()
    };

    ui.invoke_font_browser_show(1);
    assert!(ui.get_font_browser_open());
    assert!(names(ui).contains(&"JetBrains Mono".to_owned()));
    assert!(
        ui.get_font_browser_current()
            .contains("Follows the interface font")
    );
    // Typing is searched once it settles.
    ui.set_font_browser_query("lit".into());
    ui.invoke_font_browser_query_changed("lit".into());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(names(ui), ["Literata"]);
    ui.set_font_browser_query("".into());
    ui.invoke_font_browser_query_changed("".into());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    ui.invoke_font_browser_monospace_toggled();
    assert_eq!(names(ui), ["JetBrains Mono"]);
    ui.invoke_font_browser_monospace_toggled();

    // Previewing changes only the sample; nothing is written.
    let before = std::fs::read(store.path()).unwrap();
    ui.invoke_font_browser_highlight("Literata".into());
    assert_eq!(ui.get_font_browser_sample_family(), "Literata");
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    // Choosing writes the role's key; the composer follows messages.
    ui.invoke_font_browser_use("Literata".into());
    let saved = store.load().unwrap();
    assert_eq!(
        saved.typography.message_font_family.as_deref(),
        Some("Literata")
    );
    assert_eq!(saved.appearance.font_family, initial.appearance.font_family);
    assert_eq!(ui.get_message_font_family(), "Literata");
    assert_eq!(ui.get_composer_font_family(), "Literata");
    // Following the inherited font removes only that role's own value.
    ui.invoke_font_browser_inherit();
    assert_eq!(store.load().unwrap().typography.message_font_family, None);

    // A family typed by hand that is not installed is kept as typed and
    // shown as not found.
    let mut typed = store.load().unwrap();
    typed.typography.composer_font_family = Some("Not An Installed Face".into());
    store.save(&typed).unwrap();
    apply_config(ui, &typed, None);
    ui.invoke_font_browser_show(2);
    assert!(ui.get_font_browser_current_missing());
    assert_eq!(ui.get_composer_font_family(), "Not An Installed Face");

    // The controller samples the body weight that unweighted interface and
    // rich-message text inherit from the window, and the composers set
    // explicitly. The regular monospaced sample remains independent.
    typed.typography.body_weight = Some(600);
    store.save(&typed).unwrap();
    apply_config(ui, &typed, None);
    for (role, weight) in [(0, 600), (1, 600), (2, 600), (3, 400)] {
        ui.invoke_font_browser_show(role);
        assert_eq!(ui.get_font_browser_sample_weight(), weight);
    }

    // A -> B -> A while the browser is open closes it; nothing is chosen.
    switch(ui, "font-b");
    switch(ui, "font-a");
    assert!(!ui.get_font_browser_open());
    assert!(ui.get_font_browser_notice().contains("account changed"));
    // An account change no hook reported refuses the choice itself.
    ui.invoke_font_browser_show(1);
    ui.set_active_account_local_id("font-b".into());
    let before = std::fs::read(store.path()).unwrap();
    ui.invoke_font_browser_use("JetBrains Mono".into());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert!(!ui.get_font_browser_open());

    crate::theme_studio::reset_for_test();
    switch(ui, "");
    apply_config(ui, &initial, None);
    settle(ui).await;
}

/// The color editor's "This room" level: global -> account -> room
/// precedence on screen, writes only to the open room of the active
/// account, Reset of that room only, and an edit made after the open room
/// changed (before the editor showed it) refused rather than moved.
async fn assert_room_appearance_scope(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;

    let room = "!harbor:example.org";
    let other = "!quay:example.org";
    let store = ConfigStore::at(directory.join("room-scope.toml"));
    let mut config = store.load_or_create().unwrap();
    config.colors.insert("link".into(), "#101010".into());
    config
        .account_overrides
        .entry("room-a".into())
        .or_default()
        .colors
        .insert("link".into(), "#202020".into());
    store.save(&config).unwrap();
    crate::appearance_editor::register(ui, &store);
    crate::appearance::register_room_hook(ui);
    // Direct saves (not drafted), so the file shows each write.
    ui.set_page(0);
    let open = |ui: &AppWindow, account: &str, room: &str| {
        ui.set_active_account_local_id(account.into());
        ui.set_active_room_id(room.into());
        crate::appearance::apply_theme_mode(ui);
    };
    let link = |ui: &AppWindow| ui.invoke_token_color("link".into());
    let grey = |value: u8| Color::from_rgb_u8(value, value, value);
    let room_link = |config: &morpheus_config::Config, account: &str, room: &str| {
        config
            .room_appearance(Some(account), Some(room))
            .and_then(|appearance| appearance.colors.get("link").cloned())
    };
    apply_config(ui, &config, None);
    open(ui, "room-a", room);
    settle(ui).await;
    assert_eq!(
        link(ui),
        grey(0x20),
        "the room inherits its account's color"
    );

    // A color set for this room is written to it alone and shows only while
    // it is open in this account.
    ui.invoke_color_scope_chosen(2);
    assert_eq!(ui.get_color_scope(), 2);
    ui.invoke_color_token_chosen("link".into());
    ui.invoke_color_hex_edited("#303030".into());
    settle(ui).await;
    let saved = store.load().unwrap();
    assert_eq!(
        room_link(&saved, "room-a", room).as_deref(),
        Some("#303030")
    );
    assert_eq!(saved.account_overrides["room-a"].colors["link"], "#202020");
    assert_eq!(saved.colors["link"], "#101010");
    assert_eq!(link(ui), grey(0x30));
    open(ui, "room-a", other);
    settle(ui).await;
    assert_eq!(link(ui), grey(0x20), "another room inherits the account");
    open(ui, "room-b", room);
    settle(ui).await;
    assert_eq!(link(ui), grey(0x10), "the same room ID in another account");

    // The open room changes and the editor has not shown it yet: the edit
    // is refused, and neither room changes.
    open(ui, "room-a", room);
    ui.invoke_color_scope_chosen(2);
    settle(ui).await;
    ui.set_active_room_id(other.into());
    let before = std::fs::read(store.path()).unwrap();
    ui.invoke_color_hex_edited("#404040".into());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert!(ui.get_color_editor().hint.contains("open room changed"));
    // Once the editor shows that room, the edit goes to it.
    crate::appearance::apply_theme_mode(ui);
    ui.invoke_color_hex_edited("#404040".into());
    let saved = store.load().unwrap();
    assert_eq!(
        room_link(&saved, "room-a", other).as_deref(),
        Some("#404040")
    );
    assert_eq!(
        room_link(&saved, "room-a", room).as_deref(),
        Some("#303030")
    );

    // Reset removes only this room's color; it inherits again.
    ui.invoke_color_reset();
    let saved = store.load().unwrap();
    assert!(saved.room_appearance(Some("room-a"), Some(other)).is_none());
    assert_eq!(
        room_link(&saved, "room-a", room).as_deref(),
        Some("#303030")
    );

    // Without an open room, "This room" edits nothing.
    open(ui, "room-a", "");
    let before = std::fs::read(store.path()).unwrap();
    ui.invoke_color_hex_edited("#505050".into());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);

    ui.invoke_color_scope_chosen(0);
    open(ui, "", "");
    settle(ui).await;
}

/// Style profile import, preview, Apply and export carry the same scope
/// fences as theme packs, through the production controller.
async fn assert_style_profile_scope_fences(ui: &AppWindow, directory: &std::path::Path) {
    use crate::style_profiles::{finish_export_for_test, finish_import_for_test};
    use crate::theme_studio::scope;
    use morpheus_config::ConfigStore;
    use morpheus_config::style_profiles::StyleProfile;

    let folder = directory.join("profile-scopes");
    let store = ConfigStore::at(folder.join("config.toml"));
    let initial = store.load_or_create().unwrap();
    let switch = |ui: &AppWindow, account: &str| {
        ui.set_active_account_local_id(account.into());
        crate::appearance::apply_theme_mode(ui);
    };
    apply_config(ui, &initial, None);
    switch(ui, "profile-a");
    crate::style_profiles::register(ui, &store);
    settle(ui).await;
    let listed = |ui: &AppWindow| {
        ui.get_style_profile_items()
            .iter()
            .any(|item| item.name.as_str() == "Harbor look")
    };

    // An import answered after A -> B -> A adds nothing.
    let opened = scope::begin_dialog(ui);
    switch(ui, "profile-b");
    switch(ui, "profile-a");
    finish_import_for_test(ui, opened, StyleProfile::named("Harbor look"));
    assert!(!listed(ui));
    assert!(ui.get_style_profile_message().contains("not imported"));
    // In its own scope it is added (and selected, never applied).
    let opened = scope::begin_dialog(ui);
    finish_import_for_test(ui, opened, StyleProfile::named("Harbor look"));
    assert!(listed(ui));

    // The preview bar's Apply checks the preview's scope itself: an account
    // change that no hook reported stops it before anything is written.
    let before = std::fs::read(store.path()).unwrap();
    ui.invoke_style_profile_preview();
    assert!(crate::style_profiles::previewing());
    ui.set_active_account_local_id("profile-b".into());
    ui.invoke_style_preview_apply();
    assert!(!crate::style_profiles::previewing());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    switch(ui, "profile-a");

    // An export answered after A -> B -> A is not admitted: nothing written.
    let target = folder.join("harbor-look.toml");
    let opened = scope::begin_dialog(ui);
    switch(ui, "profile-b");
    switch(ui, "profile-a");
    assert!(finish_export_for_test(ui, opened, target.clone()));
    assert!(!target.exists());
    assert!(
        ui.get_style_profile_message()
            .contains("Nothing was written")
    );
    // In scope, the admitted write runs on a worker and is reported.
    let opened = scope::begin_dialog(ui);
    assert!(finish_export_for_test(ui, opened, target.clone()));
    wait_for(ui, |ui| !ui.get_style_profile_busy()).await;
    assert!(target.exists());
    assert!(ui.get_style_profile_message().contains("Exported"));

    switch(ui, "");
    apply_config(ui, &initial, None);
    settle(ui).await;
}

/// Settle the window until `done` holds, for results posted from a worker
/// thread (an admitted file write).
async fn wait_for(ui: &AppWindow, done: impl Fn(&AppWindow) -> bool) {
    for _ in 0..400 {
        settle(ui).await;
        if done(ui) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("a worker result did not arrive");
}

/// Theme pack work is fenced by the scope captured before its file dialog
/// opened: the appearance epoch, the account and the operation. Each case
/// drives the production controller through the entry points the dialog
/// threads use, with the scope from the production helper.
async fn assert_pack_scope_fences(ui: &AppWindow, directory: &std::path::Path) {
    use crate::theme_studio::{finish_pack_export_for_test, finish_pack_import_for_test, scope};
    use morpheus_config::ConfigStore;
    use morpheus_config::style_profiles::install_theme_pack;

    let folder = directory.join("studio-scopes");
    let store = ConfigStore::at(folder.join("config.toml"));
    let initial = store.load_or_create().unwrap();
    // An account switch as live.rs publishes one: the ID, then the hook.
    let switch = |ui: &AppWindow, account: &str| {
        ui.set_active_account_local_id(account.into());
        crate::appearance::apply_theme_mode(ui);
    };
    apply_config(ui, &initial, None);
    switch(ui, "scope-a");
    crate::theme_studio::register(ui, &store);
    settle(ui).await;
    let mut pack = BuiltInStylePreset::Terminal.theme().unwrap();
    pack.name = "Harbor".into();
    let previewed = |ui: &AppWindow| ui.get_studio_pack_preview_name().to_string();

    // A picker opened in A that answers after A -> B -> A starts nothing in
    // the new context, although the account ID matches again.
    let opened = scope::begin_dialog(ui);
    switch(ui, "scope-b");
    switch(ui, "scope-a");
    finish_pack_import_for_test(ui, opened, pack.clone());
    assert_eq!(previewed(ui), "");
    assert!(ui.get_studio_pack_message().contains("not opened"));
    // The same answer in an unchanged scope previews.
    let opened = scope::begin_dialog(ui);
    finish_pack_import_for_test(ui, opened, pack.clone());
    assert_eq!(previewed(ui), "Harbor");

    // Apply checks the preview's exact scope itself: an account change that
    // no hook reported still stops it before anything is written.
    ui.set_active_account_local_id("scope-b".into());
    ui.invoke_studio_pack_apply();
    assert_eq!(previewed(ui), "");
    assert!(!folder.join("themes").join("harbor.toml").exists());
    assert_ne!(
        store.load().unwrap().appearance.theme_pack.as_deref(),
        Some("harbor")
    );
    switch(ui, "scope-a");

    // An export answer after A -> B -> A is not admitted: nothing written.
    install_theme_pack(&store, "harbor", &pack, None).unwrap();
    let mut using = store.load().unwrap();
    using.appearance.theme_pack = Some("harbor".into());
    store.save(&using).unwrap();
    apply_config(ui, &using, Some(&pack));
    let target = folder.join("harbor-export.toml");
    let opened = scope::begin_dialog(ui);
    switch(ui, "scope-b");
    switch(ui, "scope-a");
    finish_pack_export_for_test(ui, opened, "harbor".into(), pack.clone(), target.clone());
    assert!(!target.exists());
    assert!(ui.get_studio_pack_message().contains("Nothing was written"));
    // Admitted in its own scope, the write runs on a worker (never the UI
    // thread) and is reported when it ends.
    let opened = scope::begin_dialog(ui);
    finish_pack_export_for_test(ui, opened, "harbor".into(), pack.clone(), target.clone());
    assert!(ui.get_studio_pack_busy(), "admitted and writing");
    wait_for(ui, |ui| !ui.get_studio_pack_busy()).await;
    assert!(target.exists());
    assert!(ui.get_studio_pack_message().contains("Exported"));
    // An admitted write still finishes when the account changes before it
    // reports. The result is told without naming the pack, and never as
    // "nothing written".
    let admitted_target = folder.join("harbor-admitted.toml");
    let opened = scope::begin_dialog(ui);
    finish_pack_export_for_test(
        ui,
        opened,
        "harbor".into(),
        pack.clone(),
        admitted_target.clone(),
    );
    switch(ui, "scope-b");
    wait_for(ui, |ui| !ui.get_studio_pack_busy()).await;
    assert!(admitted_target.exists());
    let told = ui.get_studio_pack_message();
    assert!(told.contains("the file was written"), "{told}");
    assert!(!told.contains("Harbor"));
    switch(ui, "scope-a");
    // A pack in use that changed while the dialog was open is not exported.
    let stale_target = folder.join("harbor-stale.toml");
    let mut older = pack.clone();
    older.name = "Older".into();
    let opened = scope::begin_dialog(ui);
    finish_pack_export_for_test(ui, opened, "harbor".into(), older, stale_target.clone());
    assert!(!stale_target.exists());

    // Detaching to offline settings ends a scope; offline settings work in
    // their own.
    let opened = scope::begin_dialog(ui);
    switch(ui, "");
    finish_pack_import_for_test(ui, opened, pack.clone());
    assert_eq!(previewed(ui), "");
    let opened = scope::begin_dialog(ui);
    finish_pack_import_for_test(ui, opened, pack.clone());
    assert_eq!(previewed(ui), "Harbor");
    ui.invoke_studio_pack_cancel();
    assert_eq!(previewed(ui), "");
    // A new settings context ends what the previous one started.
    let opened = scope::begin_dialog(ui);
    crate::theme_studio::register(ui, &store);
    finish_pack_import_for_test(ui, opened, pack);
    assert_eq!(previewed(ui), "");

    crate::theme_studio::reset_for_test();
    apply_config(ui, &initial, None);
    settle(ui).await;
}

/// An installed name collision is preview-only. Both a stale and a fresh
/// snapshot are refused; importing under a new name still installs and
/// applies without changing the original file.
async fn assert_theme_pack_keeps_installed_file(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;
    use morpheus_config::style_profiles::{install_theme_pack, read_theme_pack};

    let folder = directory.join("studio-packs");
    let store = ConfigStore::at(folder.join("config.toml"));
    let initial = store.load_or_create().unwrap();
    apply_config(ui, &initial, None);
    ui.set_active_account_local_id("".into());
    crate::theme_studio::register(ui, &store);
    settle(ui).await;
    let mut installed = BuiltInStylePreset::Paper.theme().unwrap();
    installed.name = "Dusk".into();
    install_theme_pack(&store, "dusk", &installed, None).unwrap();
    let mut incoming = BuiltInStylePreset::Terminal.theme().unwrap();
    incoming.name = "Dusk".into();

    crate::theme_studio::preview_pack_for_test(ui, incoming.clone());
    assert_eq!(ui.get_studio_pack_preview_name(), "Dusk");
    assert!(ui.get_studio_pack_replaces());

    // Another program edits the installed pack after the offer.
    let file = folder.join("themes").join("dusk.toml");
    let edited = format!(
        "{}\n# edited elsewhere\n",
        std::fs::read_to_string(&file).unwrap()
    );
    std::fs::write(&file, &edited).unwrap();
    ui.invoke_studio_pack_apply();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        edited,
        "nothing is replaced"
    );
    assert_ne!(
        store.load().unwrap().appearance.theme_pack.as_deref(),
        Some("dusk")
    );
    assert!(ui.get_studio_pack_message_error());
    // The preview stays, bound to the file as it is now, and asks again.
    assert_eq!(ui.get_studio_pack_preview_name(), "Dusk");
    assert!(ui.get_studio_pack_replaces());

    // Trying again cannot turn the new snapshot into overwrite permission.
    ui.invoke_studio_pack_apply();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);
    assert_ne!(
        store.load().unwrap().appearance.theme_pack.as_deref(),
        Some("dusk")
    );
    assert!(
        ui.get_studio_pack_message().contains("different")
            || ui.get_studio_pack_message().contains("Change the name")
    );
    ui.invoke_studio_pack_cancel();

    // A different name keeps both packs. It can already be selected in the
    // file while that theme file is missing; installing must still reload it.
    incoming.name = "NewDusk".into();
    let mut selecting_missing = store.load().unwrap();
    selecting_missing.appearance.theme_pack = Some("newdusk".into());
    store.save(&selecting_missing).unwrap();
    crate::theme_studio::preview_pack_for_test(ui, incoming.clone());
    assert!(!ui.get_studio_pack_replaces());
    ui.invoke_studio_pack_apply();
    assert_eq!(read_theme_pack(&store, "newdusk").unwrap(), incoming);
    assert_eq!(
        store.load().unwrap().appearance.theme_pack.as_deref(),
        Some("newdusk")
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);
    assert_eq!(ui.get_studio_pack_preview_name(), "");

    crate::theme_studio::reset_for_test();
    apply_config(ui, &initial, None);
    settle(ui).await;
}

/// A broken palette file cannot hold notification privacy changes hostage.
/// Exercise the actual routed save/controller with a real pending draft,
/// both a missing and malformed theme, and a callback that tries to mix a
/// behavior change with an appearance change.
async fn assert_behavior_saves_with_broken_theme(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::style_profiles::install_theme_pack;
    use morpheus_config::{ConfigStore, RoomNotificationLocal, ThemeMode};

    for (case, malformed) in [("missing", false), ("malformed", true)] {
        let store = ConfigStore::at(directory.join(format!("behavior-{case}/config.toml")));
        let mut initial = store.load_or_create().unwrap();
        initial.appearance.theme_pack = Some("behavior-palette".into());
        initial.notifications.enabled = true;
        initial.notifications.show_message_preview = true;
        initial.activity.send_typing = true;
        initial.activity.public_read_receipts = true;
        initial.activity.read_tracking = true;
        initial.set_room_notification_local(
            "last-notification",
            "!last:example.org",
            RoomNotificationLocal {
                quiet: true,
                ..Default::default()
            },
        );
        initial
            .account_overrides
            .entry("appearance-account".into())
            .or_default()
            .theme = Some(ThemeMode::Light);
        let pack = BuiltInStylePreset::Paper.theme().unwrap();
        install_theme_pack(&store, "behavior-palette", &pack, None).unwrap();
        store.save(&initial).unwrap();
        ui.set_active_account_local_id("".into());
        apply_config(ui, &initial, Some(&pack));
        crate::theme_studio::register(ui, &store);
        ui.set_page(1);
        settle(ui).await;

        let radius = if initial.geometry.corner_radius == 4 {
            5
        } else {
            4
        };
        crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = radius)
            .unwrap();
        assert_eq!(ui.get_studio_pending(), 1);
        let shown_accent = ui.invoke_token_color("accent".into());
        let theme_file = store.theme_path("behavior-palette").unwrap();
        if malformed {
            std::fs::write(&theme_file, "not = [valid").unwrap();
        } else {
            std::fs::remove_file(&theme_file).unwrap();
        }

        let saved = crate::save_config_change(ui, &store, |config| {
            config.notifications.enabled = false;
            config.notifications.show_message_preview = false;
            config.activity = Default::default();
            config
                .account_overrides
                .entry("new-notifications".into())
                .or_default()
                .notifications_enabled = Some(false);
            config.set_room_notification_local(
                "new-notifications",
                "!quiet:example.org",
                RoomNotificationLocal {
                    quiet: true,
                    hide_preview: true,
                    no_highlights: true,
                },
            );
            config
                .account_overrides
                .get_mut("appearance-account")
                .unwrap()
                .notifications_enabled = Some(false);
            config.set_room_notification_local(
                "last-notification",
                "!last:example.org",
                Default::default(),
            );
        });
        assert!(saved.is_some(), "{case}: behavior edit must save");
        let written = store.load().unwrap();
        assert!(!written.notifications.enabled);
        assert!(!written.notifications.show_message_preview);
        assert!(!ui.get_notifications_enabled());
        assert!(!ui.get_notification_show_preview());
        assert_eq!(written.activity, Default::default());
        let activity = ui.global::<crate::RoomActivity>();
        assert!(!activity.get_send_typing());
        assert!(!activity.get_public_receipts());
        assert!(!activity.get_read_tracking());
        assert!(!written.account_overrides.contains_key("last-notification"));
        assert_eq!(
            written.account_overrides["new-notifications"].notifications_enabled,
            Some(false)
        );
        assert!(
            written
                .room_notification_local("new-notifications", "!quiet:example.org")
                .hide_preview
        );
        assert_eq!(
            written.account_overrides["appearance-account"].theme,
            Some(ThemeMode::Light)
        );
        assert_eq!(
            written.account_overrides["appearance-account"].notifications_enabled,
            Some(false)
        );
        assert_eq!(
            written.geometry.corner_radius,
            initial.geometry.corner_radius
        );
        assert_eq!(
            ui.get_studio_pending(),
            1,
            "the appearance draft stays pending"
        );
        assert_eq!(ui.invoke_token_color("accent".into()), shown_accent);
        assert!(
            !ui.get_config_error().is_empty(),
            "the theme error remains visible"
        );

        let before = std::fs::read(store.path()).unwrap();
        let mixed_account = crate::save_config_change(ui, &store, |config| {
            let account = config
                .account_overrides
                .get_mut("appearance-account")
                .unwrap();
            account.notifications_enabled = Some(true);
            account.theme = Some(ThemeMode::Dark);
        });
        assert!(
            mixed_account.is_none(),
            "account theme and privacy cannot share the fallback"
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(ui.get_studio_pending(), 1);
        let refused = crate::save_config_change(ui, &store, |config| {
            config.notifications.enabled = true;
            config.geometry.corner_radius = radius + 1;
        });
        assert!(
            refused.is_none(),
            "mixed edits must not enter the behavior fallback"
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(ui.get_studio_pending(), 1);
        ui.invoke_studio_apply();
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            ui.get_studio_pending(),
            1,
            "Apply cannot accept the broken theme"
        );
        ui.invoke_studio_discard();
        assert_eq!(ui.get_studio_pending(), 0);
        assert_eq!(ui.invoke_token_color("accent".into()), shown_accent);
        assert!(!ui.get_notifications_enabled());
        assert!(!ui.get_notification_show_preview());
        // Discard redisplays the accepted last-valid look. Its behavior
        // metadata must include every successful fallback save, including
        // account entry creation/deletion, while the pending radius was
        // never accepted or written.
        let accepted = crate::appearance::remembered_theme_source().unwrap().0;
        assert_eq!(accepted.activity, written.activity);
        assert!(!accepted.account_overrides.contains_key("last-notification"));
        for account in ["new-notifications", "appearance-account"] {
            assert_eq!(
                accepted.account_overrides.get(account),
                written.account_overrides.get(account)
            );
        }
        assert_eq!(
            accepted.geometry.corner_radius,
            initial.geometry.corner_radius
        );

        // The same path works without a pending appearance draft too.
        crate::save_config_change(ui, &store, |config| config.notifications.sounds = false)
            .unwrap();
        assert!(!store.load().unwrap().notifications.sounds);
        assert!(!ui.get_notification_sounds());
        assert_eq!(ui.get_studio_pending(), 0);
        crate::theme_studio::reset_for_test();
        ui.set_config_error("".into());
        ui.invoke_studio_notice_dismiss();
        apply_config(ui, &Config::default(), None);
        ui.set_page(0);
        settle(ui).await;
    }
}

/// Apply and routed behavior saves commit only over the revision of the
/// file they were decided on. Each case places another program's edit at
/// the real pre-publish boundary (staged and locked, immediately before the
/// comparison and the rename). That version stays in the file byte for
/// byte, the draft stays pending, and the person then chooses.
async fn assert_theme_studio_publish_race(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;

    let store = ConfigStore::at(directory.join("studio-race.toml"));
    let initial = store.load_or_create().unwrap();
    apply_config(ui, &initial, None);
    ui.set_active_account_local_id("".into());
    crate::theme_studio::register(ui, &store);
    ui.set_page(1);
    settle(ui).await;
    let on_disk = || std::fs::read_to_string(store.path()).unwrap();
    // The other program's version is rendered beforehand by a scratch store
    // and written with std::fs at the boundary, where no ConfigStore may
    // write.
    let scratch = ConfigStore::at(directory.join("studio-race-external.toml"));
    let other_version = |config: &Config| {
        scratch.save(config).unwrap();
        let text = std::fs::read_to_string(scratch.path()).unwrap();
        let edit = text.clone();
        crate::theme_studio::before_next_publish_for_test(move |path| {
            std::fs::write(path, edit).unwrap();
        });
        text
    };

    // The other version also changed the drafted setting: nothing is
    // written, the draft is kept, and the choice is offered at once.
    crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = 4).unwrap();
    assert_eq!(ui.get_studio_pending(), 1);
    let mut other = store.load().unwrap();
    other.geometry.corner_radius = 11;
    other.typography.message_size += 1;
    let other_text = other_version(&other);
    ui.invoke_studio_apply();
    assert_eq!(on_disk(), other_text, "the intervening version is kept");
    assert_eq!(ui.get_studio_pending(), 1, "the draft is kept");
    assert_eq!(ui.get_studio_conflicts(), 1);
    assert!(ui.get_studio_notice_error());
    // Keeping mine writes the drafted value over the version now on disk
    // and keeps that version's other change.
    ui.invoke_studio_resolve(1);
    let saved = store.load().unwrap();
    assert_eq!(saved.geometry.corner_radius, 4);
    assert_eq!(saved.typography.message_size, other.typography.message_size);
    assert_eq!(ui.get_studio_pending(), 0);

    // The other version changed only an unrelated setting: nothing is
    // written and no choice is needed; Apply again saves both.
    crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = 6).unwrap();
    let mut other = store.load().unwrap();
    other.chat.show_avatars = !other.chat.show_avatars;
    let other_text = other_version(&other);
    ui.invoke_studio_apply();
    assert_eq!(on_disk(), other_text);
    assert_eq!(ui.get_studio_pending(), 1);
    assert_eq!(ui.get_studio_conflicts(), 0);
    ui.invoke_studio_apply();
    let saved = store.load().unwrap();
    assert_eq!(saved.geometry.corner_radius, 6);
    assert_eq!(saved.chat.show_avatars, other.chat.show_avatars);
    assert_eq!(ui.get_studio_pending(), 0);

    // A routed behavior save while drafting: the other version stays, the
    // behavior change is reported unsaved, and the draft is untouched.
    crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = 8).unwrap();
    let mut other = store.load().unwrap();
    other.typography.message_size += 1;
    let other_text = other_version(&other);
    let sounds = !other.notifications.sounds;
    let refused =
        crate::save_config_change(ui, &store, |config| config.notifications.sounds = sounds);
    assert!(refused.is_none());
    assert_eq!(on_disk(), other_text);
    assert!(!ui.get_config_error().is_empty());
    assert_eq!(ui.get_studio_pending(), 1);
    // Made again over the version now on disk, it saves; the draft stays.
    crate::save_config_change(ui, &store, |config| config.notifications.sounds = sounds).unwrap();
    let saved = store.load().unwrap();
    assert_eq!(saved.notifications.sounds, sounds);
    assert_eq!(saved.typography.message_size, other.typography.message_size);
    assert_eq!(saved.geometry.corner_radius, other.geometry.corner_radius);
    assert_eq!(ui.get_studio_pending(), 1);

    // Another Morpheus writer is mid-save (it holds the writer lock) when
    // Apply runs. The window is not blocked: nothing is written, the draft
    // stays, and Apply is tried again on a timer once the writer is done.
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join("studio-race.toml.lock"))
        .unwrap();
    holder.lock().unwrap();
    let before = on_disk();
    ui.invoke_studio_apply();
    assert_eq!(on_disk(), before);
    assert_eq!(ui.get_studio_pending(), 1);
    assert!(!ui.get_studio_notice_error());
    holder.unlock().unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(ui.get_studio_pending(), 0);
    assert_eq!(store.load().unwrap().geometry.corner_radius, 8);
    // A waiting Apply is dropped when the draft changes before it runs: it
    // never saves values the person did not apply.
    crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = 9).unwrap();
    holder.lock().unwrap();
    ui.invoke_studio_apply();
    crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = 10).unwrap();
    holder.unlock().unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert_eq!(store.load().unwrap().geometry.corner_radius, 8);
    assert_eq!(ui.get_studio_pending(), 1);
    drop(holder);

    ui.invoke_studio_discard();
    ui.invoke_studio_notice_dismiss();
    ui.set_config_error("".into());
    crate::theme_studio::reset_for_test();
    ui.set_page(0);
    settle(ui).await;
}

/// The theme studio drafts look changes made in Settings and saves them only
/// on Apply. Discard writes nothing, a concurrent change of a drafted setting
/// is never overwritten without a choice, behavior settings still save at
/// once, and drafted account colors never reach another account.
async fn assert_theme_studio_draft(ui: &AppWindow, directory: &std::path::Path) {
    use morpheus_config::ConfigStore;

    let store = ConfigStore::at(directory.join("studio.toml"));
    let initial = store.load_or_create().unwrap();
    apply_config(ui, &initial, None);
    ui.set_active_account_local_id("".into());
    // The window's default variation list (which search reveals before the
    // studio registers) matches the Rust definitions it is replaced with.
    let shown_names = |ui: &AppWindow| -> Vec<String> {
        ui.get_studio_variations()
            .iter()
            .map(|variation| variation.name.to_string())
            .collect()
    };
    let before = shown_names(ui);
    crate::theme_studio::register(ui, &store);
    assert_eq!(before, shown_names(ui));
    assert_eq!(before.len(), 5);
    ui.set_page(1);
    settle(ui).await;
    let radius = |store: &ConfigStore| store.load().unwrap().geometry.corner_radius;
    let draft = |radius: u16| {
        crate::save_config_change(ui, &store, |config| config.geometry.corner_radius = radius)
    };

    // A look edit is shown and drafted, not saved.
    let shown = draft(3).unwrap();
    assert_eq!(shown.geometry.corner_radius, 3);
    assert_eq!(radius(&store), initial.geometry.corner_radius);
    assert_eq!(ui.get_studio_pending(), 1);
    // A behavior setting still saves at once and leaves the draft alone.
    let sounds = !initial.notifications.sounds;
    crate::save_config_change(ui, &store, |config| config.notifications.sounds = sounds).unwrap();
    assert_eq!(store.load().unwrap().notifications.sounds, sounds);
    assert_eq!(ui.get_studio_pending(), 1);
    // Undo and redo move through the draft only.
    ui.invoke_studio_undo();
    assert_eq!(ui.get_studio_pending(), 0);
    assert!(ui.get_studio_can_redo());
    ui.invoke_studio_redo();
    assert_eq!(ui.get_studio_pending(), 1);
    // Apply writes the draft and ends it.
    ui.invoke_studio_apply();
    assert_eq!(radius(&store), 3);
    assert_eq!(ui.get_studio_pending(), 0);

    // Discard restores the saved look and writes nothing.
    draft(5).unwrap();
    ui.invoke_studio_discard();
    assert_eq!(ui.get_studio_pending(), 0);
    assert_eq!(radius(&store), 3);

    // The file changed the drafted setting meanwhile: Apply asks first.
    draft(6).unwrap();
    let mut external = store.load().unwrap();
    external.geometry.corner_radius = 9;
    external.typography.message_size += 1;
    store.save(&external).unwrap();
    ui.invoke_studio_apply();
    assert_eq!(ui.get_studio_conflicts(), 1);
    assert_eq!(
        radius(&store),
        9,
        "a concurrent change is never overwritten silently"
    );
    // Keeping the file's value saves nothing of the conflicting setting and
    // keeps the unrelated external change.
    ui.invoke_studio_resolve(2);
    assert_eq!(ui.get_studio_pending(), 0);
    let kept = store.load().unwrap();
    assert_eq!(kept.geometry.corner_radius, 9);
    assert_eq!(
        kept.typography.message_size,
        external.typography.message_size
    );
    // Keeping mine writes this window's value and still keeps the rest.
    draft(7).unwrap();
    let mut external = store.load().unwrap();
    external.geometry.corner_radius = 8;
    external.chat.show_avatars = !external.chat.show_avatars;
    store.save(&external).unwrap();
    ui.invoke_studio_apply();
    assert_eq!(ui.get_studio_conflicts(), 1);
    ui.invoke_studio_resolve(1);
    let mine = store.load().unwrap();
    assert_eq!(mine.geometry.corner_radius, 7);
    assert_eq!(mine.chat.show_avatars, external.chat.show_avatars);

    // The new typography and per-control icon settings are drafted like any
    // look key: shown at once, saved only by Apply, gone after Discard.
    let line_height = |ui: &AppWindow| ui.global::<crate::StyleTokens>().get_line_height_factor();
    let draft_new = |ui: &AppWindow| {
        crate::save_config_change(ui, &store, |config| {
            config.typography.line_height_percent = Some(150);
            config.typography.body_weight = Some(600);
            config.colors.insert("icon_header".into(), "#123456".into());
        })
        .unwrap()
    };
    draft_new(ui);
    assert_eq!(ui.get_studio_pending(), 3);
    assert!((line_height(ui) - 1.5).abs() < 0.001);
    assert_eq!(
        ui.global::<crate::StyleTokens>().get_body_font_weight(),
        600
    );
    let header = Color::from_rgb_u8(0x12, 0x34, 0x56);
    assert_eq!(ui.invoke_token_color("icon_header".into()), header);
    let saved = store.load().unwrap();
    assert_eq!(saved.typography.line_height_percent, None);
    assert!(!saved.colors.contains_key("icon_header"));
    ui.invoke_studio_discard();
    assert!((line_height(ui) - 1.0).abs() < 0.001);
    assert_eq!(ui.global::<crate::StyleTokens>().get_body_font_weight(), 0);
    draft_new(ui);
    ui.invoke_studio_apply();
    let applied = store.load().unwrap();
    assert_eq!(applied.typography.line_height_percent, Some(150));
    assert_eq!(applied.typography.body_weight, Some(600));
    assert_eq!(
        applied.colors.get("icon_header").map(String::as_str),
        Some("#123456")
    );

    // Drafted account colors belong to the account that was active.
    ui.set_active_account_local_id("studio-first".into());
    crate::appearance::apply_theme_mode(ui);
    let saved_accent = ui.invoke_token_color("accent".into());
    assert_ne!(saved_accent, color_from_hex("#123456"));
    crate::save_config_change(ui, &store, |config| {
        config
            .account_overrides
            .entry("studio-first".into())
            .or_default()
            .colors
            .insert("accent".into(), "#123456".into());
    })
    .unwrap();
    assert_eq!(ui.get_studio_pending(), 1);
    assert_eq!(
        ui.invoke_token_color("accent".into()),
        color_from_hex("#123456")
    );
    ui.set_active_account_local_id("studio-second".into());
    crate::appearance::apply_theme_mode(ui);
    assert_eq!(ui.get_studio_pending(), 0);
    assert_eq!(ui.invoke_token_color("accent".into()), saved_accent);
    // Returning must not revive the old cached palette after its edits and
    // history were removed. Check the actual color, not just pending count.
    ui.set_active_account_local_id("studio-first".into());
    crate::appearance::apply_theme_mode(ui);
    assert_eq!(ui.invoke_token_color("accent".into()), saved_accent);
    ui.invoke_studio_undo();
    assert_eq!(ui.invoke_token_color("accent".into()), saved_accent);
    ui.invoke_studio_apply();
    assert!(store.load().unwrap().account_overrides.is_empty());

    ui.set_active_account_local_id("".into());
    ui.invoke_studio_notice_dismiss();
    crate::theme_studio::reset_for_test();
    ui.set_page(0);
    settle(ui).await;
}

/// A member row's Details and Manage actions on the real generated window:
/// at the default and narrowest details pane, at 100% and 200% text, both
/// are instantiated once, whole inside the details pane and the window, and
/// never overlap each other.
async fn assert_member_row_actions_fit(ui: &AppWindow) {
    let size = ui.window().size();
    let saved = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_details_width(),
        ui.get_show_side_panel(),
        ui.get_font_scale_percent(),
    );
    let privacy_scope = (
        ui.get_active_account_local_id(),
        ui.get_active_room_id(),
        ui.get_access_open(),
        ui.get_security_busy(),
        ui.get_sas_busy(),
        ui.get_security_recovery_key(),
        ui.get_account_switch_pending(),
        ui.get_account_profile_open(),
        ui.get_people_open(),
        ui.get_sas_stage(),
    );
    ui.set_active_account_local_id("member-layout".into());
    ui.set_active_room_id("!member-layout:example.org".into());
    ui.set_access_open(false);
    ui.set_security_busy(false);
    ui.set_sas_busy(false);
    ui.set_sas_stage(0);
    ui.set_security_recovery_key("".into());
    ui.set_account_switch_pending(false);
    ui.set_account_profile_open(false);
    ui.set_people_open(false);
    // Wide enough that the details pane is shown at its configured width;
    // 620 px is the short height the actions must survive.
    ui.window().set_size(slint::PhysicalSize::new(1280, 620));
    ui.set_page(0);
    ui.set_show_side_panel(true);
    ui.set_live_session(true);
    ui.set_members(ModelRc::new(VecModel::from(vec![MemberItem {
        user_id: "@synthetic-member:example.org".into(),
        name: "Synthetic member with a long display name".into(),
        initials: "SM".into(),
        ..Default::default()
    }])));
    let who = "Synthetic member with a long display name \u{b7} @synthetic-member:example.org";
    for (pane, scale) in [(250, 100), (200, 100), (250, 200), (200, 200), (520, 200)] {
        ui.set_details_width(pane);
        ui.set_font_scale_percent(scale);
        settle(ui).await;
        let details = control_bounds(ui, &format!("Details for room member {who}"));
        let manage = control_bounds(ui, &format!("Manage room member {who}"));
        assert_eq!(
            (details.len(), manage.len()),
            (1, 1),
            "{pane} px at {scale}%"
        );
        let (width, _) = window_size(ui);
        let pane_left = width - pane as f32 - 0.5;
        for bounds in [details[0], manage[0]] {
            assert!(
                inside_window(ui, bounds),
                "{pane} px at {scale}%: inside the window"
            );
            assert!(
                bounds.left >= pane_left,
                "{pane} px at {scale}%: inside the details pane"
            );
        }
        let (details, manage) = (details[0], manage[0]);
        assert!(
            details.right <= manage.left + 0.5 || details.bottom <= manage.top + 0.5,
            "{pane} px at {scale}%: Details and Manage do not overlap"
        );
    }
    // Use the real generated modal context, with a stand-in only for the
    // controller's Close callback: no account or homeserver read is made.
    let privacy = ui.global::<crate::MemberPrivacy>();
    let weak = ui.as_weak();
    privacy.on_close_inspector(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<crate::MemberPrivacy>()
                .set_inspector(crate::MemberInspectorView::default());
        }
    });
    for held in 0..8 {
        privacy.set_inspector(crate::MemberInspectorView {
            open: true,
            name: "Synthetic member".into(),
            user_id: "@synthetic-member:example.org".into(),
            ..Default::default()
        });
        settle(ui).await;
        assert!(privacy.get_inspector().open, "the ordinary inspector opens");
        match held {
            0 => ui.set_access_open(true),
            1 => ui.set_security_busy(true),
            2 => ui.set_sas_busy(true),
            3 => ui.set_security_recovery_key("synthetic-key-not-a-credential".into()),
            4 => ui.set_account_switch_pending(true),
            5 => ui.set_account_profile_open(true),
            6 => ui.set_people_open(true),
            _ => ui.set_sas_stage(4),
        }
        settle(ui).await;
        assert!(
            !privacy.get_inspector().open,
            "context {held} closes private details"
        );
        ui.set_access_open(false);
        ui.set_security_busy(false);
        ui.set_sas_busy(false);
        ui.set_sas_stage(0);
        ui.set_security_recovery_key("".into());
        ui.set_account_switch_pending(false);
        ui.set_account_profile_open(false);
        ui.set_people_open(false);
    }
    ui.set_members(ModelRc::new(VecModel::<MemberItem>::default()));
    ui.set_active_account_local_id(privacy_scope.0);
    ui.set_active_room_id(privacy_scope.1);
    ui.set_access_open(privacy_scope.2);
    ui.set_security_busy(privacy_scope.3);
    ui.set_sas_busy(privacy_scope.4);
    ui.set_security_recovery_key(privacy_scope.5);
    ui.set_account_switch_pending(privacy_scope.6);
    ui.set_account_profile_open(privacy_scope.7);
    ui.set_people_open(privacy_scope.8);
    ui.set_sas_stage(privacy_scope.9);
    ui.set_page(saved.0);
    ui.set_live_session(saved.1);
    ui.set_details_width(saved.2);
    ui.set_show_side_panel(saved.3);
    ui.set_font_scale_percent(saved.4);
    ui.window().set_size(size);
    settle(ui).await;
}

/// The SAS comparison values leave the real generated window the moment the
/// verification view closes (leaving Settings) or protection starts (a
/// security step, account access, a pending switch, sign-in), whatever the
/// controller does later. Synthetic values only; no account is used.
async fn assert_sas_values_leave_on_close_and_protection(ui: &AppWindow) {
    let saved = (ui.get_page(), ui.get_live_session(), ui.get_show_sign_in());
    let protections: [(&str, fn(&AppWindow, bool)); 4] = [
        ("security step", |ui, on| ui.set_security_busy(on)),
        ("account access", |ui, on| ui.set_access_open(on)),
        ("pending switch", |ui, on| ui.set_account_switch_pending(on)),
        ("sign-in", |ui, on| ui.set_show_sign_in(on)),
    ];
    let show_values = |ui: &AppWindow| {
        ui.set_sas_stage(4);
        ui.set_sas_emoji("synthetic emoji".into());
        ui.set_sas_decimals("1111 \u{b7} 2222 \u{b7} 3333".into());
        ui.set_sas_compared(true);
    };
    ui.set_live_session(true);
    ui.set_show_sign_in(false);
    ui.set_page(1);
    settle(ui).await;
    show_values(ui);
    ui.set_page(0);
    assert_eq!(ui.get_sas_emoji(), "", "leaving Settings clears the emoji");
    assert_eq!(
        ui.get_sas_decimals(),
        "",
        "leaving Settings clears the numbers"
    );
    assert!(!ui.get_sas_compared());
    ui.set_page(1);
    for (name, protect) in protections {
        show_values(ui);
        assert!(!ui.get_sas_protected(), "{name}: not protected before");
        protect(ui, true);
        assert!(ui.get_sas_protected(), "{name}: protected");
        assert_eq!(ui.get_sas_emoji(), "", "{name} clears the emoji");
        assert_eq!(ui.get_sas_decimals(), "", "{name} clears the numbers");
        assert!(!ui.get_sas_compared(), "{name} clears the comparison");
        protect(ui, false);
    }
    ui.set_sas_stage(0);
    ui.set_page(saved.0);
    ui.set_live_session(saved.1);
    ui.set_show_sign_in(saved.2);
    settle(ui).await;
}

/// Actual generated join controls and the production withdrawal handlers,
/// with held operation metadata only: no account/client or HTTP is used.
async fn assert_discovery_join_withdrawal(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    use crate::{JoinSlot, register_join_cancellation};
    use slint::private_unstable_api::re_exports::{AccessibilityAction, AccessibleStringProperty};
    use std::rc::Rc;
    let saved = (
        ui.get_page(),
        ui.get_live_session(),
        ui.get_show_sign_in(),
        ui.get_room_action_mode(),
        live.is_live.load(Ordering::Acquire),
    );
    let room: JoinSlot = Arc::default();
    let directory: JoinSlot = Arc::default();
    register_join_cancellation(ui, room.clone(), directory.clone());
    crate::space_explorer::register(ui, live.clone(), runtime.clone());
    let submissions = Rc::new(std::cell::Cell::new(0));
    let count = submissions.clone();
    let (weak, owner, live_for_click) = (ui.as_weak(), room.clone(), live.clone());
    ui.on_submit_room_action(move |mode, _| {
        assert_eq!(mode, 1);
        count.set(count.get() + 1);
        if let Some(ui) = weak.upgrade() {
            let operation = ui.get_room_action_operation().wrapping_add(1);
            *owner.lock().unwrap() = Some((operation, live_for_click.join_asked()));
            ui.set_room_action_operation(operation);
            ui.set_room_action_busy(true);
        }
    });
    live.is_live.store(true, Ordering::Release);
    ui.set_live_session(true);
    ui.set_show_sign_in(false);
    ui.set_page(0);
    ui.set_room_action_mode(1);
    for escape in [true, false] {
        ui.set_room_actions_open(true);
        settle(ui).await;
        assert!(ui.get_discovery_context_allowed());
        control_with_label(ui, "Join Matrix room").accessible_action(&AccessibilityAction::Default);
        settle(ui).await;
        let asked = room.lock().unwrap().as_ref().unwrap().1.clone();
        assert!(focused_control_has_label(ui, "Cancel the join"));
        assert_eq!(
            control_with_label(ui, "Join Matrix room")
                .accessible_string_property(AccessibleStringProperty::Enabled)
                .as_deref(),
            Some("false")
        );
        if escape {
            key(ui, Key::Escape);
        } else {
            control_with_label(ui, "Cancel the join and close room actions")
                .accessible_action(&AccessibilityAction::Default);
        }
        settle(ui).await;
        assert!(asked.withdrawn_for_test());
        assert!(!ui.get_room_actions_open() && !ui.get_room_action_busy());
        assert!(room.lock().unwrap().is_none());
    }
    assert_eq!(
        submissions.get(),
        2,
        "Close and Escape must not submit another join"
    );
    ui.set_room_actions_open(true);
    settle(ui).await;
    control_with_label(ui, "Join Matrix room").accessible_action(&AccessibilityAction::Default);
    let asked = room.lock().unwrap().as_ref().unwrap().1.clone();
    ui.set_sas_stage(1);
    settle(ui).await;
    assert!(asked.withdrawn_for_test() && !ui.get_room_actions_open());
    assert_eq!(submissions.get(), 3, "protection withdraws, never resends");
    ui.set_sas_stage(0);

    let directory_entries = Rc::new(std::cell::Cell::new(0));
    let count = directory_entries.clone();
    let (weak, owner, live_for_click) = (ui.as_weak(), directory.clone(), live.clone());
    ui.on_join_discovery_target(move |_, _, _, _, _| {
        count.set(count.get() + 1);
        if let Some(ui) = weak.upgrade() {
            let operation = ui.get_directory_join_operation().wrapping_add(1);
            *owner.lock().unwrap() = Some((operation, live_for_click.join_asked()));
            ui.set_directory_join_operation(operation);
            ui.set_directory_join_busy(true);
        }
    });
    ui.set_page(3);
    settle(ui).await;
    control_with_label(ui, "Join room by Matrix link or alias")
        .accessible_action(&AccessibilityAction::Default);
    let asked = directory.lock().unwrap().as_ref().unwrap().1.clone();
    ui.set_access_open(true);
    settle(ui).await;
    assert!(asked.withdrawn_for_test() && !ui.get_directory_join_busy());
    assert!(directory.lock().unwrap().is_none());
    assert_eq!(
        control_with_label(ui, "Join room by Matrix link or alias")
            .accessible_string_property(AccessibleStringProperty::Enabled)
            .as_deref(),
        Some("false")
    );
    assert_eq!(directory_entries.get(), 1);
    ui.set_access_open(false);
    settle(ui).await;
    control_with_label(ui, "Join room by Matrix link or alias")
        .accessible_action(&AccessibilityAction::Default);
    let asked = directory.lock().unwrap().as_ref().unwrap().1.clone();
    ui.set_page(1);
    settle(ui).await;
    assert!(asked.withdrawn_for_test() && !ui.get_directory_join_busy());
    assert_eq!(directory_entries.get(), 2, "page exit sends nothing extra");

    ui.set_page(0);
    let asked = live.join_asked();
    crate::space_explorer::hold_join_for_test(ui, asked.clone());
    settle(ui).await;
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(asked.withdrawn_for_test() && !crate::space_explorer::is_open(ui));
    let asked = live.join_asked();
    crate::space_explorer::hold_join_for_test(ui, asked.clone());
    ui.set_sas_stage(1);
    settle(ui).await;
    assert!(asked.withdrawn_for_test() && !crate::space_explorer::is_open(ui));
    assert_eq!(submissions.get(), 3);
    assert_eq!(
        directory_entries.get(),
        2,
        "explorer Close never invokes another join entry"
    );
    ui.set_sas_stage(0);
    ui.on_submit_room_action(|_, _| {});
    ui.on_join_discovery_target(|_, _, _, _, _| {});
    ui.set_room_action_mode(saved.3);
    ui.set_page(saved.0);
    ui.set_live_session(saved.1);
    ui.set_show_sign_in(saved.2);
    live.is_live.store(saved.4, Ordering::Release);
    settle(ui).await;
}

/// The roles and permissions view through its registered callbacks and the
/// window's own hooks (mode 3 of the room admin dialog, its scope handlers
/// and the live reset), with the core's reads and saves held. A read that
/// returns after the room changed and came back is not shown. Staging
/// works only on a listed row this account may change. Lowering your own
/// level is never sent without its confirmation. A save is refused at its
/// hand-off once a security step began and ended. The reset clears
/// everything at once. No account, network or vault.
async fn assert_room_permissions_editor(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    use crate::RoomPermissions;
    use crate::room_permissions::{HeldAnswer, HeldCore, RoomPermissionsUi};
    use morpheus_matrix::{
        EventClass, PermissionKey, PermissionLevel, PermissionThreshold, PermissionUser,
        RoomPermissionsSnapshot,
    };
    use std::sync::atomic::Ordering;

    const ME: &str = "@casey:example.org";
    async fn within(wait: impl std::future::Future<Output = ()>) {
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the held permissions core answers");
    }
    async fn until(ui: &AppWindow, done: impl Fn(&AppWindow) -> bool) {
        let wait = async {
            while !done(ui) {
                settle(ui).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the permissions view reaches the window");
    }
    fn state(ui: &AppWindow) -> i32 {
        ui.global::<RoomPermissions>().get_state()
    }
    fn person(id: &str, level: i64, editable: bool) -> PermissionUser {
        PermissionUser {
            user_id: id.into(),
            display_name: id.into(),
            level: PermissionLevel::Level(level),
            editable,
            own: id == ME,
        }
    }
    async fn reopen(ui: &AppWindow) {
        ui.set_room_admin_open(false);
        settle(ui).await;
        ui.set_room_admin_mode(3);
        ui.set_room_admin_open(true);
        settle(ui).await;
    }

    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    let was_session = ui.get_live_session();
    let shown = "!perms:example.org";
    ui.set_live_session(true);
    ui.set_active_account_local_id("permissions-account".into());
    ui.set_active_room_id(shown.into());
    settle(ui).await;
    let was_live = live.is_live.swap(true, Ordering::AcqRel);
    let ban = PermissionThreshold {
        key: PermissionKey::Ban,
        level: 50,
        explicit: true,
        class: EventClass::NotEvent,
        editable: true,
    };
    let snapshot = RoomPermissionsSnapshot {
        room_id: shown.into(),
        room_version: "10".into(),
        own_user: ME.into(),
        own_level: PermissionLevel::Level(100),
        can_change: true,
        change_level: 50,
        users_default: 0,
        own_explicit: true,
        exact_users: [
            (ME, 100),
            ("@peer:example.org", 100),
            ("@dana:example.org", 50),
        ]
        .into_iter()
        .map(|(id, level)| (id.to_owned(), level))
        .collect(),
        has_complete_state: true,
        thresholds: vec![ban],
        users: vec![
            person(ME, 100, true),
            person("@peer:example.org", 100, false),
            person("@dana:example.org", 50, true),
        ],
        omitted_users: 0,
        omitted_events: 0,
        read_only: None,
    };
    let base = snapshot.clone();

    // The settings form's way into mode 3 refuses while room addresses are
    // open over that form; with them closed the same action opens mode 3.
    // This runs before the editor is registered, so no read is started.
    {
        use crate::RoomAddresses;
        use slint::private_unstable_api::re_exports::AccessibilityAction;
        let opener = "Open this room's roles and permissions";
        let saved = (
            ui.get_room_admin_busy(),
            ui.get_room_settings_confirmation(),
            ui.get_room_settings_dirty(),
        );
        ui.set_room_admin_busy(false);
        ui.set_room_settings_confirmation(0);
        ui.set_room_settings_dirty(false);
        ui.set_room_admin_mode(0);
        ui.set_room_admin_open(true);
        settle(ui).await;
        ui.global::<RoomAddresses>().set_open(true);
        settle(ui).await;
        assert!(ui.global::<RoomAddresses>().get_open());
        control_with_label(ui, opener).accessible_action(&AccessibilityAction::Default);
        settle(ui).await;
        assert_eq!(
            ui.get_room_admin_mode(),
            0,
            "refused while room addresses are open"
        );
        ui.global::<RoomAddresses>().set_open(false);
        settle(ui).await;
        control_with_label(ui, opener).accessible_action(&AccessibilityAction::Default);
        settle(ui).await;
        assert_eq!(ui.get_room_admin_mode(), 3);
        ui.set_room_admin_open(false);
        ui.set_room_admin_mode(0);
        ui.set_room_admin_busy(saved.0);
        ui.set_room_settings_confirmation(saved.1);
        ui.set_room_settings_dirty(saved.2);
        settle(ui).await;
    }

    let core = Arc::new(HeldCore::new(snapshot));
    let _editor = RoomPermissionsUi::register(ui, live.clone(), runtime, core.clone());
    let global = ui.global::<RoomPermissions>();

    // Mode 3 opens the view; its read is held while the room changes and
    // comes back, unsettled: equal final strings fire no change, and the
    // navigation epoch the editor opened in marks the read stale.
    ui.set_room_admin_mode(3);
    ui.set_room_admin_open(true);
    settle(ui).await;
    within(core.wait_calls(1)).await;
    live.invalidate_navigation();
    ui.set_active_room_id("!elsewhere:example.org".into());
    live.invalidate_navigation();
    ui.set_active_room_id(shown.into());
    core.release();
    within(core.wait_answered(1)).await;
    until(ui, |ui| state(ui) == 3).await;
    assert_eq!(global.get_rows().row_count(), 0);

    // Opened again with nothing changed, the read is shown.
    reopen(ui).await;
    within(core.wait_calls(2)).await;
    core.release();
    until(ui, |ui| state(ui) == 2).await;
    assert_eq!(global.get_rows().row_count(), 3);
    // Only a listed row this account may change can be staged.
    global.invoke_stage("u:@peer:example.org".into(), "10".into());
    assert_eq!(global.get_staged(), 0);
    assert!(!global.get_error().is_empty());
    global.invoke_stage("u:@nobody:example.org".into(), "10".into());
    assert_eq!(global.get_staged(), 0);
    global.invoke_stage("u:@dana:example.org".into(), "75".into());
    global.invoke_stage(format!("u:{ME}").into(), "50".into());
    assert_eq!(global.get_staged(), 2);

    // The review asks to confirm your own demotion; without it nothing is
    // sent, even when Save is invoked directly.
    global.invoke_review();
    assert!(global.get_review_open());
    assert!(global.get_review_needs_confirmation());
    global.invoke_save();
    assert!(global.get_review_open());
    assert!(!global.get_busy());
    // Confirmed, the save is held while a security step begins and ends:
    // its admission refuses it at the hand-off.
    global.set_review_confirmed(true);
    global.invoke_save();
    assert!(global.get_busy());
    within(core.wait_calls(3)).await;
    let gate = &live.security_gate;
    drop(gate.try_account_access().expect("the gate is idle"));
    core.release_save();
    within(core.wait_answered(3)).await;
    until(ui, |ui| !ui.global::<RoomPermissions>().get_busy()).await;
    assert!(core.sent.lock().unwrap().is_empty());
    assert!(!global.get_error().is_empty());

    // A fresh view: one staged change, reviewed and saved, is sent once.
    reopen(ui).await;
    within(core.wait_calls(4)).await;
    core.release();
    until(ui, |ui| state(ui) == 2).await;
    global.invoke_stage("u:@dana:example.org".into(), "75".into());
    global.invoke_review();
    assert!(!global.get_review_needs_confirmation());
    global.invoke_save();
    within(core.wait_calls(5)).await;
    core.release_save();
    within(core.wait_answered(5)).await;
    until(ui, |ui| !ui.global::<RoomPermissions>().get_busy()).await;
    let sent = core.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].changes.len(), 1);
    assert!(!sent[0].own_demotion_confirmed);
    assert!(global.get_notice().starts_with("Saved 1 change."));
    assert_eq!(global.get_staged(), 0);

    // Escape ends the editor before the view hides: a read still held when
    // it is pressed never shows. (Close and Back call the same reset first.)
    reopen(ui).await;
    within(core.wait_calls(6)).await;
    key(ui, Key::Escape);
    assert_eq!(global.get_state(), 0, "reset before the dialog hides");
    assert_eq!(global.get_rows().row_count(), 0);
    core.release();
    within(core.wait_answered(6)).await;
    settle(ui).await;
    assert_eq!(global.get_state(), 0, "the held read is not shown");
    // Accepted but not shown by its read-back: the room stays paused until
    // a read started after the answer shows the exact change; a read of the
    // old state does not release it.
    reopen(ui).await;
    within(core.wait_calls(7)).await;
    core.release();
    until(ui, |ui| state(ui) == 2).await;
    *core.answer.lock().unwrap() = Some(HeldAnswer::AcceptedNotShown);
    global.invoke_stage("u:@dana:example.org".into(), "80".into());
    global.invoke_review();
    global.invoke_save();
    within(core.wait_calls(8)).await;
    core.release_save();
    within(core.wait_answered(8)).await;
    until(ui, |ui| !ui.global::<RoomPermissions>().get_busy()).await;
    assert!(global.get_refresh_required());
    assert!(global.get_notice().contains("no read since"));
    global.invoke_refresh();
    within(core.wait_calls(9)).await;
    core.release();
    within(core.wait_answered(9)).await;
    until(ui, |ui| state(ui) == 2).await;
    assert!(
        global.get_refresh_required(),
        "the old state does not settle it"
    );
    let mut shows_it = base.clone();
    shows_it.exact_users.insert("@dana:example.org".into(), 80);
    for row in &mut shows_it.users {
        if row.user_id == "@dana:example.org" {
            row.level = PermissionLevel::Level(80);
        }
    }
    core.set_snapshot(shows_it);
    global.invoke_refresh();
    within(core.wait_calls(10)).await;
    core.release();
    until(ui, |ui| {
        state(ui) == 2 && !ui.global::<RoomPermissions>().get_refresh_required()
    })
    .await;
    assert!(global.get_notice().contains("now shows"));
    core.set_snapshot(base.clone());

    // No answer: the room stays paused while the view stays open, after
    // Close and reopen, after a fresh read of the old state and after
    // Refresh. Nothing is sent again, whatever is invoked.
    *core.answer.lock().unwrap() = Some(HeldAnswer::Unknown);
    global.invoke_stage("u:@dana:example.org".into(), "70".into());
    global.invoke_review();
    global.invoke_save();
    within(core.wait_calls(11)).await;
    core.release_save();
    within(core.wait_answered(11)).await;
    until(ui, |ui| !ui.global::<RoomPermissions>().get_busy()).await;
    let sent = core.sent.lock().unwrap().len();
    assert!(global.get_refresh_required());
    assert!(global.get_notice().contains("got no answer"));
    global.invoke_reset();
    reopen(ui).await;
    within(core.wait_calls(12)).await;
    core.release();
    until(ui, |ui| state(ui) == 2).await;
    assert!(
        global.get_refresh_required(),
        "paused across close and reopen"
    );
    assert!(global.get_notice().contains("got no answer"));
    global.invoke_refresh();
    within(core.wait_calls(13)).await;
    core.release();
    within(core.wait_answered(13)).await;
    until(ui, |ui| state(ui) == 2).await;
    assert!(
        global.get_refresh_required(),
        "a fresh read of the old state keeps it paused"
    );
    global.invoke_stage("u:@dana:example.org".into(), "90".into());
    global.invoke_review();
    assert!(!global.get_review_open());
    global.invoke_save();
    assert!(!global.get_busy());
    assert_eq!(core.sent.lock().unwrap().len(), sent, "no second write");
    // Partial account removal uses this same production draft cleanup, but
    // it must not discard a permission save whose server outcome is unknown.
    live.forget_account_drafts("permissions-account");
    global.invoke_refresh();
    within(core.wait_calls(14)).await;
    core.release();
    within(core.wait_answered(14)).await;
    until(ui, |ui| state(ui) == 2).await;
    assert!(
        global.get_refresh_required(),
        "draft cleanup retains the unknown save"
    );
    assert!(global.get_notice().contains("got no answer"));
    global.invoke_review();
    global.invoke_save();
    assert_eq!(
        core.sent.lock().unwrap().len(),
        sent,
        "cleanup never permits a second write"
    );
    // Removing the account forgets its receipts.
    crate::room_permissions::forget_account("permissions-account");

    // The live reset hook clears every row before it returns.
    crate::room_permissions::reset_window(ui);
    assert_eq!(global.get_rows().row_count(), 0);
    assert_eq!(global.get_state(), 0);

    ui.set_room_admin_open(false);
    ui.set_room_admin_mode(0);
    live.is_live.store(was_live, Ordering::Release);
    ui.set_active_account_local_id(account);
    ui.set_active_room_id(room);
    ui.set_live_session(was_session);
    settle(ui).await;
}
