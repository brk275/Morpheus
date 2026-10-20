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
        let phase = std::rc::Rc::new(std::cell::Cell::new(0_u8));
        let started = std::time::Instant::now();
        // One process-global event loop now covers fourteen appearance and
        // keyboard scenarios plus account dialogs. Keep a finite whole-suite
        // deadline without making each scenario share the old 20-second cap.
        let outcome = tokio::time::timeout(
            Duration::from_secs(60),
            exercise_account_ui(runtime_handle.clone(), phase.clone()),
        )
        .await;
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
    let label = "Microphone for the next call";
    assert!(focused_control_has_label(ui, label));

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

    ui.set_audio_devices_busy(false);
    ui.set_audio_devices_enabled(false);
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

async fn exercise_account_ui(runtime: Handle, phase: std::rc::Rc<std::cell::Cell<u8>>) {
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
    phase.set(15);
    let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
        directory.path().join("config.toml"),
    )));
    let live = LiveUi::new(manager.clone(), runtime.clone(), ui.as_weak(), voices);
    assert_pane_navigation_epoch(&ui, &live, directory.path()).await;
    phase.set(73);
    assert_moderation_stops_with_its_context(&ui, &live, &runtime).await;
    phase.set(74);
    assert_room_media_scope_and_privacy(&ui, &live, &runtime).await;
    #[cfg(feature = "native-calls")]
    crate::voice_rosters::assert_unowned_presentation_is_cleared(&ui, live.clone(), &runtime);
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
    click_control_once(ui, "Unmute microphone preview");
    if was_muted {
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
    assert_eq!(ui.get_active_account_initials(), "PA");
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
    use crate::room_media_browser::{HeldLoader, RoomMediaBrowser};
    use morpheus_matrix::{RoomMediaItem, RoomMediaKind, RoomMediaPage};
    use slint::private_unstable_api::re_exports::AccessibilityAction;
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
