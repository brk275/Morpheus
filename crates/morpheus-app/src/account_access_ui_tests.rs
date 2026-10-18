//! Real generated Slint properties, keyboard routing, and account presentation.
//! The headless backend has no native display or screen reader. These tests do
//! not authenticate or remove a core account, and cannot access a credential vault.

use super::*;
use crate::{
    AccountItem, MemberItem, MessageItem, MessageSearchItem, RoomItem, apply_config,
    color_from_hex, voice_spaces::VoiceSpaces,
};
use morpheus_config::{BuiltInStylePreset, Config, NavigationMode};
use slint::fontique_011::fontique;
use slint::platform::{Key, WindowEvent};
use slint::{Color, StyledText};
use std::time::Duration;

fn key(ui: &AppWindow, text: impl Into<slint::SharedString>) {
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
fn focused_control_has_label(ui: &AppWindow, expected: &str) -> bool {
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

fn tab_to_control(ui: &AppWindow, label: &str) {
    use slint::private_unstable_api::re_exports::{FocusScope, TextInput, WindowInner};

    // Walk the real current tab order without activating intervening controls.
    // A missing FocusScope must fail instead of silently invoking the callback.
    let window = WindowInner::from_pub(ui.window());
    for _ in 0..160 {
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
        key(ui, Key::Space);
        assert!(store.load().unwrap().appearance.reduce_motion);
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
    let invalid = "[appearance\nreduce_motion = true\n";
    std::fs::write(config_path, invalid).unwrap();
    key(ui, Key::Space);
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), invalid);
    assert!(!ui.global::<crate::StyleTokens>().get_reduced_motion());
    assert!(!ui.get_config_error().is_empty());
    store.save(&defaults).unwrap();
    apply_config(ui, &store.load().unwrap(), None);
    ui.set_config_error("".into());
    ui.on_set_reduce_motion(|_| {});
    ui.set_page(0);
    settle(ui).await;
}

async fn assert_lazy_timeline_view(ui: &AppWindow) {
    use crate::timeline_model::{projection_count, tests::fixture, update};
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::ops::ControlFlow;

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
    let mut rows: Vec<_> = (0..10_000).map(fixture).collect();
    let original = rows[0].body.clone();
    let model = update(ModelRc::default(), rows.clone());
    assert_eq!(projection_count(&model), 0);
    ui.set_messages(model.clone());
    settle(ui).await;
    let initial = projection_count(&model);
    assert!(
        initial > 0 && initial < 1_000,
        "the real ListView must project a bounded visible subset: {initial}"
    );
    assert!(has_label(&original));

    rows[0].body = "Updated visible timeline fixture".into();
    rows[0].rich_markdown = Some(rows[0].body.clone());
    rows[0].rich_accessible = Some(rows[0].body.clone());
    let changed = update(ui.get_messages(), rows.clone());
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

    rows.push(fixture(10_000));
    let appended = update(ui.get_messages(), rows);
    assert_eq!(appended, model);
    ui.set_messages(appended);
    settle(ui).await;
    assert_eq!(ui.get_messages().row_count(), 10_001);
    assert!(
        projection_count(&model) - after_edit < 1_000,
        "appending must not format the entire history"
    );
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
/// visible button, so Slint can reveal it when reached with the keyboard.
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
            assert_eq!(focused.geometry().size, controls[0].geometry().size);
            assert!(focused.is_visible() && controls[0].is_visible());
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
        tokio::time::timeout(Duration::from_secs(20), exercise_account_ui(runtime_handle))
            .await
            .expect("account UI scenario must finish without network or vault access");
        slint::quit_event_loop().unwrap();
    })
    .unwrap();
    slint::run_event_loop_until_quit().unwrap();
}

async fn exercise_account_ui(runtime: Handle) {
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
    assert_sign_in_keyboard(&ui).await;
    settle(&ui).await;
    assert_navigation_projection_and_focus(&ui).await;
    assert_accessible_action_focus_and_first_click(&ui).await;
    assert_native_motion_and_preference(&ui, &directory.path().join("appearance.toml")).await;
    assert_lazy_timeline_view(&ui).await;
    assert_palette_and_density_keyboard(&ui).await;
    assert_scale_keyboard_and_focus_bounds(&ui).await;
    assert_message_role_precedence_and_thread_metadata(&ui).await;
    let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
        directory.path().join("config.toml"),
    )));
    let live = LiveUi::new(manager.clone(), runtime.clone(), ui.as_weak(), voices);
    // This suite tests the actual dialog/controller boundary, not credentials.
    // A denied account-change guard also prevents any mistaken key sequence
    // from starting policy writes, account activation, or local deletion.
    let access = AccountAccessUi::register(&ui, live.clone(), runtime, || false);
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
    ui.set_security_recovery_input("synthetic recovery input".into());
    ui.set_security_recovery_key("synthetic recovery output".into());
    assert_recovery_strip_projection(&ui).await;
    live.account_detached(ui.as_weak()).await;
    settle(&ui).await;
    assert!(!ui.get_live_session());
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
    assert!(manager.active_account().await.is_none());

    assert_removed_account_menu_projection(&ui);
    ui.hide().unwrap();
    drop(ui); // Drops the registered controller callbacks.
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
    use slint::private_unstable_api::re_exports::{AccessibleStringProperty, ItemRc, WindowInner};
    use std::ops::ControlFlow;

    let window = WindowInner::from_pub(ui.window());
    let root = ItemRc::new_root(window.component());
    let mut bounds = Vec::new();
    let _ = root.visit_descendants(|item| {
        if item
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

/// Bounds of the control that currently has keyboard focus, if any.
fn focused_control_bounds(ui: &AppWindow) -> Option<Bounds> {
    use slint::private_unstable_api::re_exports::WindowInner;

    let window = WindowInner::from_pub(ui.window());
    let focused = window.focus_item.borrow().upgrade()?;
    Some(item_bounds(&focused))
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
const DISCARD_UNREADABLE_LABEL: &str =
    "Discard saved send status this version cannot read; your typed text stays";

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
                "{label} is clipped: {left}..{right} of {window_width}"
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
    tab_to_record_control(ui, CANCEL_LABEL);
    let mut room_strip_controls = 0;
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
    // Walk from the thread tab into the thread strip: every focused control
    // is fully inside the window on both axes, including stacked ones.
    tab_to_control(ui, "Show thread");
    let mut seen_strip_controls = 0;
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
    draft_recovery::publish_thread(ui, &PendingSummary::default(), &[]);
    draft_recovery::publish_room(ui, &PendingSummary::default(), &[]);
    settle(ui).await;
}
