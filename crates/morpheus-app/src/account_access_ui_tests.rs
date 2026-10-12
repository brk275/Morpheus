//! Real generated Slint properties, keyboard routing, and account presentation.
//! The headless backend has no native display or screen reader. These tests do
//! not authenticate or remove a core account, and cannot access a credential vault.

use super::*;
use crate::{
    AccountItem, MemberItem, MessageItem, MessageSearchItem, RoomItem, apply_config,
    voice_spaces::VoiceSpaces,
};
use morpheus_config::{BuiltInStylePreset, Config};
use slint::fontique_011::fontique;
use slint::platform::{Key, WindowEvent};
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

fn assert_passwords_empty(ui: &AppWindow) {
    assert!(ui.get_access_current_password().is_empty());
    assert!(ui.get_access_new_password().is_empty());
    assert!(ui.get_access_confirm_password().is_empty());
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

    // Seed private information in distinct user-visible surfaces, then use the
    // production detach adapter with a real Weak<AppWindow>, not Weak::default.
    ui.set_live_session(true);
    ui.set_active_account_local_id("old-account".into());
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
    live.account_detached(ui.as_weak()).await;
    settle(&ui).await;
    assert!(!ui.get_live_session());
    assert!(ui.get_show_sign_in());
    assert!(ui.get_active_account_local_id().is_empty());
    assert!(ui.get_active_account_handle().is_empty());
    assert_eq!(ui.get_active_account_index(), -1);
    assert!(ui.get_active_room_id().is_empty());
    assert!(ui.get_active_room_name().is_empty());
    assert_eq!(ui.get_accounts().row_count(), 0);
    assert_eq!(ui.get_channels().row_count(), 0);
    assert_eq!(ui.get_messages().row_count(), 0);
    assert_eq!(ui.get_thread_messages().row_count(), 0);
    assert_eq!(ui.get_members().row_count(), 0);
    assert!(ui.get_composer_text().is_empty());
    assert!(ui.get_thread_root_body().is_empty());
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
