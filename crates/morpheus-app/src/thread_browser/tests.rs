//! Regression source for the thread browser: paging on a validated cursor
//! within its bounds, failures that keep the loaded roots and never claim a
//! complete list, Retry of the failed page only, results for a departed
//! request, room or selection, search within the loaded roots, the reply
//! line, and the panel at the minimum window height with 200% text. The
//! model tests need no account; the A-B-A test uses the local homeserver
//! fixture (MORPHEUS_MATRIX_TEST_ACCOUNTS) and is skipped without it; the
//! panel assertions run inside the real-window suite in
//! account_access_ui_tests.rs, which owns the process-global event loop.

use std::{sync::Arc, time::Duration};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use morpheus_matrix::{AccountManager, SyncPolicy, THREAD_PAGE_LIMIT, ThreadLatestReply};
use slint::platform::{Key, WindowEvent};

use super::*;

const ROOM: &str = "!room:example.org";

fn root(index: usize) -> ThreadRootSummary {
    ThreadRootSummary {
        root_id: format!("$root{index}:example.org"),
        kind: ThreadRootKind::Message,
        sender_id: format!("@sender{index}:example.org"),
        sender_name: format!("Sender {index}"),
        timestamp_ms: 0,
        excerpt: format!("Root {index}"),
        attachment: None,
        edited: false,
        authenticity_warning: false,
        latest: None,
        replies: None,
        participated: false,
    }
}

fn page(range: std::ops::Range<usize>, next: Option<&str>) -> ThreadRootPage {
    ThreadRootPage {
        room_id: ROOM.into(),
        roots: range.map(root).collect(),
        next_batch: next.map(str::to_owned),
        truncated: false,
        skipped: 0,
    }
}

fn opened() -> (Browser, Request) {
    let mut browser = Browser::new(ROOM);
    let first = browser
        .next_request(2)
        .expect("the first page is read on open");
    assert_eq!(first.from, None);
    assert!(browser.loading());
    assert!(browser.next_request(3).is_none(), "one read at a time");
    (browser, first)
}

fn ids(browser: &Browser) -> Vec<String> {
    browser.visible().map(|root| root.root_id.clone()).collect()
}

#[test]
fn pages_follow_the_cursor_and_keep_each_root_once() {
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Ok(page(0..25, Some("t1")))));
    assert!(browser.can_load_more());
    assert!(!browser.complete);
    assert_eq!(browser.summary(), "25 threads loaded · more on the server");
    let next = browser.next_request(4).unwrap();
    assert_eq!(next.from.as_deref(), Some("t1"));
    // The next page repeats the last root of the first one.
    assert!(browser.apply(&next, Ok(page(24..30, None))));
    assert_eq!(browser.roots.len(), 30);
    assert!(browser.complete);
    assert!(!browser.can_load_more());
    assert!(browser.next_request(5).is_none());
    assert_eq!(browser.summary(), "30 threads, all listed");
}

#[test]
fn paging_stops_at_its_bounds_and_says_so() {
    let (mut browser, mut request) = opened();
    let mut start = 0;
    let mut operation = 10;
    loop {
        let end = start + THREAD_PAGE_LIMIT as usize;
        let token = format!("t{end}");
        assert!(browser.apply(&request, Ok(page(start..end, Some(&token)))));
        start = end;
        match browser.next_request(operation) {
            Some(next) => request = next,
            None => break,
        }
        operation += 1;
        assert!(operation < 100, "paging must end on its own");
    }
    assert!(browser.pages <= MAX_PAGES);
    assert!(browser.roots.len() <= MAX_ROOTS);
    assert!(browser.capped);
    assert!(!browser.complete);
    assert!(!browser.can_load_more());
    let summary = browser.summary();
    assert!(summary.contains("more than can be listed here"));
    assert!(!summary.contains("all listed"));
}

#[test]
fn a_repeated_or_unusable_cursor_ends_paging_without_claiming_completeness() {
    for unusable in [false, true] {
        let (mut browser, first) = opened();
        assert!(browser.apply(&first, Ok(page(0..3, Some("t1")))));
        let next = browser.next_request(5).unwrap();
        let mut second = page(3..4, Some("t1"));
        if unusable {
            // The core drops an unusable token and reports the page truncated.
            second.next_batch = None;
            second.truncated = true;
        }
        assert!(browser.apply(&next, Ok(second)));
        assert!(!browser.complete);
        assert!(browser.truncated);
        assert!(!browser.can_load_more());
        assert!(browser.next_request(6).is_none());
        let summary = browser.summary();
        assert!(summary.contains("the server's list was cut short"));
        assert!(!summary.contains("all listed"));
    }
}

#[test]
fn skipped_roots_are_counted_and_the_list_is_not_called_complete() {
    let (mut browser, first) = opened();
    let mut result = page(0..2, None);
    result.skipped = 3;
    assert!(browser.apply(&first, Ok(result)));
    assert!(browser.complete, "the server ended the list");
    assert_eq!(browser.summary(), "2 threads loaded · 3 could not be shown");
}

#[test]
fn an_empty_room_is_said_only_once_the_server_ended_the_list() {
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Ok(page(0..0, None))));
    assert_eq!(browser.summary(), "This room has no threads");
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Ok(page(0..0, Some("t1")))));
    assert_eq!(browser.summary(), "0 threads loaded · more on the server");
}

#[test]
fn a_failed_page_keeps_loaded_roots_and_never_reports_complete() {
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Ok(page(0..5, Some("t1")))));
    let next = browser.next_request(6).unwrap();
    assert!(browser.apply(&next, Err(ThreadListError::Unavailable)));
    assert_eq!(browser.roots.len(), 5);
    assert_eq!(
        browser.summary(),
        "5 threads loaded before the error · the list is incomplete"
    );
    assert!(
        browser
            .error_text()
            .ends_with("The threads shown were loaded earlier and are not refreshed.")
    );
    assert!(
        !browser.can_load_more(),
        "a failed page is retried, never skipped"
    );
    assert!(browser.next_request(7).is_none());
    assert!(browser.can_retry());
    let retried = browser.retry(8).unwrap();
    assert_eq!(
        retried.from.as_deref(),
        Some("t1"),
        "Retry asks for the same page"
    );
    assert!(browser.retry(9).is_none(), "one read at a time");
    assert!(browser.apply(&retried, Ok(page(5..6, None))));
    assert_eq!(browser.error(), None);
    assert_eq!(browser.summary(), "6 threads, all listed");
}

#[test]
fn only_transient_failures_offer_retry_and_none_claims_a_list() {
    use ThreadListError::*;
    for (error, retry) in [
        (Unsupported, false),
        (Forbidden, false),
        (NotJoined, false),
        (InvalidToken, false),
        (InvalidRoom, false),
        (TooLarge, false),
        (RateLimited, true),
        (TimedOut, true),
        (Unavailable, true),
    ] {
        let (mut browser, first) = opened();
        assert!(browser.apply(&first, Err(error)));
        assert!(browser.roots.is_empty());
        assert_eq!(browser.summary(), "");
        assert_eq!(browser.error_text(), error.text());
        assert!(!browser.can_load_more());
        // Only a homeserver without `/threads` leads to this device's roots.
        assert_eq!(browser.wants_cache(), error == Unsupported);
        assert_eq!(browser.can_retry(), retry);
        assert_eq!(browser.retry(4).is_some(), retry);
    }
}

#[test]
fn without_threads_on_the_server_this_devices_roots_are_shown_as_such() {
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Err(ThreadListError::Unsupported)));
    let local = browser
        .cache_request(5)
        .expect("one read of this device's roots");
    assert!(local.cached);
    assert_eq!(local.from, None);
    assert!(
        browser.cache_request(6).is_none(),
        "one read at a time, and once"
    );
    assert!(browser.apply(&local, Ok(page(0..3, None))));
    assert!(browser.cached);
    assert_eq!(browser.roots.len(), 3);
    // Nothing more is read, and nothing says the list is complete.
    assert!(!browser.complete);
    assert!(!browser.can_load_more());
    assert!(!browser.can_retry());
    assert!(browser.next_request(7).is_none());
    assert!(browser.cache_request(8).is_none());
    assert_eq!(
        browser.summary(),
        "3 threads found in messages loaded on this device"
    );
    assert_eq!(browser.error_text(), CACHED_NOTE);
    assert_eq!(browser.open_target("$root1:example.org"), Some(ROOM));
    browser.set_query("root 2");
    assert_eq!(
        browser.summary(),
        "1 of 3 threads found on this device match"
    );

    // A failed local read leaves the homeserver's answer, and no roots.
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Err(ThreadListError::Unsupported)));
    let local = browser.cache_request(5).unwrap();
    assert!(browser.apply(&local, Err(ThreadListError::Unavailable)));
    assert!(!browser.cached);
    assert!(browser.roots.is_empty());
    assert_eq!(browser.error_text(), ThreadListError::Unsupported.text());
    assert!(!browser.wants_cache());
    assert!(!browser.can_retry());
}

#[test]
fn a_server_continuation_is_never_offered_once_only_this_devices_roots_show() {
    // TB08 exactly: a first server page with zero usable roots and next=A,
    // then Unsupported, then a successful read of this device's roots.
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Ok(page(0..0, Some("A")))));
    assert_eq!(browser.pages, 1);
    assert!(browser.can_load_more());
    let more = browser.next_request(5).unwrap();
    assert_eq!(more.from.as_deref(), Some("A"));
    assert!(browser.apply(&more, Err(ThreadListError::Unsupported)));
    let local = browser
        .cache_request(6)
        .expect("one read of this device's roots");
    assert!(browser.apply(&local, Ok(page(0..2, None))));
    assert!(browser.cached);
    assert_eq!(browser.roots.len(), 2);
    // Cached-only: no Load more, no page request, no retry, nothing "more
    // on the server".
    assert!(!browser.can_load_more(), "Load more is not offered");
    assert!(browser.next_request(7).is_none());
    assert!(browser.retry(8).is_none());
    assert!(browser.cache_request(9).is_none());
    assert_eq!(
        browser.summary(),
        "2 threads found in messages loaded on this device"
    );
}

#[test]
fn a_device_scan_that_ended_early_says_so_and_claims_nothing_more() {
    // The core's scan stopped at a bound (here: nothing found among the
    // newest events it may look at). The status says where it looked, and
    // never that more threads exist.
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Err(ThreadListError::Unsupported)));
    let local = browser.cache_request(5).unwrap();
    let mut scanned = page(0..0, None);
    scanned.truncated = true;
    assert!(browser.apply(&local, Ok(scanned)));
    let summary = browser.summary();
    assert_eq!(
        summary,
        "No threads found in the newest messages loaded on this device · \
         older loaded messages were not checked"
    );
    assert!(!summary.contains("more than can be listed here"));
}

#[test]
fn leaving_the_room_drops_every_root_read_before() {
    // The core checks the membership again after a page is read and
    // decrypted; the browser then drops what it showed, not just adds an
    // error under it.
    for error in [ThreadListError::NotJoined, ThreadListError::Forbidden] {
        let (mut browser, first) = opened();
        assert!(browser.apply(&first, Ok(page(0..5, Some("t1")))));
        let next = browser.next_request(6).unwrap();
        assert!(browser.apply(&next, Err(error)));
        assert!(browser.roots.is_empty(), "{error:?} drops what was read");
        assert_eq!(browser.visible().count(), 0);
        assert_eq!(browser.summary(), "");
        assert_eq!(browser.error_text(), error.text());
        assert!(!browser.can_load_more() && !browser.can_retry());
        assert_eq!(browser.open_target("$root1:example.org"), None);
    }
    // Also when the roots came from this device.
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Err(ThreadListError::Unsupported)));
    let local = browser.cache_request(5).unwrap();
    assert!(browser.apply(&local, Err(ThreadListError::NotJoined)));
    assert!(browser.roots.is_empty() && !browser.cached);
    assert_eq!(browser.error_text(), ThreadListError::NotJoined.text());
}

#[test]
fn results_for_another_request_or_room_are_dropped() {
    let (mut browser, first) = opened();
    let stale = Request {
        operation: first.operation - 1,
        ..first.clone()
    };
    assert!(!browser.apply(&stale, Ok(page(0..3, None))));
    let other_room = Request {
        room_id: "!other:example.org".into(),
        ..first.clone()
    };
    assert!(!browser.apply(&other_room, Ok(page(0..3, None))));
    assert!(browser.loading(), "still waiting for its own request");
    // A page naming another room is refused even for the awaited request.
    let mut elsewhere = page(0..3, None);
    elsewhere.room_id = "!other:example.org".into();
    assert!(browser.apply(&first, Ok(elsewhere)));
    assert!(browser.roots.is_empty());
    assert_eq!(browser.error(), Some(ThreadListError::Unavailable));
    // Once taken, the same request is not applied twice.
    assert!(!browser.apply(&first, Ok(page(0..3, None))));
}

#[test]
fn search_matches_only_loaded_roots_and_bounds_the_query() {
    let (mut browser, first) = opened();
    let mut result = page(0..12, Some("t1"));
    result.roots[3].kind = ThreadRootKind::Undecryptable;
    result.roots[3].excerpt.clear();
    assert!(browser.apply(&first, Ok(result)));
    browser.set_query("  ROOT 1\u{7}");
    assert_eq!(
        ids(&browser),
        [
            "$root1:example.org",
            "$root10:example.org",
            "$root11:example.org"
        ]
    );
    assert!(
        browser
            .summary()
            .starts_with("3 of 12 loaded threads match")
    );
    // The fixed status is searchable; no body is invented for it.
    browser.set_query("decrypt");
    assert_eq!(ids(&browser), ["$root3:example.org"]);
    assert_eq!(needle(&"x".repeat(1000)).chars().count(), MAX_QUERY_CHARS);
    browser.set_query("");
    assert_eq!(browser.visible().count(), 12);
}

#[test]
fn only_a_listed_root_can_be_opened() {
    let (mut browser, first) = opened();
    assert!(browser.apply(&first, Ok(page(0..2, None))));
    assert_eq!(browser.open_target("$root1:example.org"), Some(ROOM));
    assert_eq!(browser.open_target("$unlisted:example.org"), None);
    assert_eq!(browser.open_target("root1"), None);
    assert_eq!(browser.open_target(""), None);
}

#[test]
fn the_reply_line_states_only_what_the_server_summary_says() {
    let mut item = root(1);
    assert_eq!(latest_text(&item), "");
    item.replies = Some(1);
    assert_eq!(latest_text(&item), "1 reply");
    item.latest = Some(ThreadLatestReply {
        timestamp_ms: 0,
        sender_id: "@bo:example.org".into(),
        sender_name: "Bo\u{202e}".into(),
    });
    item.replies = Some(12);
    item.participated = true;
    assert_eq!(
        latest_text(&item),
        "Latest reply by Bo · 12 replies · You took part"
    );
    let row = row_item(&item);
    assert!(!row.latest.to_lowercase().contains("unread"));
    assert_eq!(row.status.as_str(), "");
}

#[test]
fn an_undecryptable_or_deleted_root_shows_its_status_and_no_body() {
    for kind in [
        ThreadRootKind::Undecryptable,
        ThreadRootKind::Redacted,
        ThreadRootKind::Unsupported,
    ] {
        let mut item = root(2);
        item.kind = kind;
        item.excerpt.clear();
        let row = row_item(&item);
        assert_eq!(row.excerpt.as_str(), "");
        assert_eq!(row.status.as_str(), status_text(kind));
        assert!(!row.status.is_empty());
    }
}

struct FixtureAccount {
    homeserver_url: String,
    username: String,
    password: String,
}

fn fixture() -> Option<(FixtureAccount, FixtureAccount)> {
    let path = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS")?;
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("Cannot read local fixture"))
            .expect("Invalid local fixture");
    let account = |label: &str| {
        let entry = fixture["accounts"]
            .as_array()
            .expect("fixture accounts")
            .iter()
            .find(|entry| entry["label"].as_str() == Some(label))
            .expect("fixture account");
        FixtureAccount {
            homeserver_url: entry["homeserver_url"].as_str().unwrap().to_owned(),
            username: entry["username"].as_str().unwrap().to_owned(),
            password: entry["password"].as_str().unwrap().to_owned(),
        }
    };
    Some((account("a_primary"), account("b_primary")))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_departed_selection_reads_no_threads_after_a_b_a() {
    let Some((first, second)) = fixture() else {
        eprintln!("Skipped thread browser A-B-A regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(
        AccountManager::open_with_sync_policy(directory.path().join("profile"), SyncPolicy::Auto)
            .unwrap(),
    );
    let a = manager
        .add_password_account(&first.homeserver_url, &first.username, &first.password)
        .await
        .expect("first login failed");
    let b = manager
        .add_password_account(&second.homeserver_url, &second.username, &second.password)
        .await
        .expect("second login failed");
    manager.switch_to(&a.id).await.expect("switch to A");
    let departed = manager.account_selection(&a.id).unwrap();
    manager.switch_to(&b.id).await.expect("switch to B");
    manager.switch_to(&a.id).await.expect("switch back to A");
    assert!(!departed.is_current());
    // The same account is active again, but a browser opened under the
    // departed selection reads nothing, first page or later.
    for from in [None, Some("t1")] {
        let read = manager.thread_roots_for(&departed, ROOM, from).await;
        assert_eq!(read, Err(ThreadListError::AccountChanged));
    }
    manager.stop().await;
    drop(manager);
    directory.close().unwrap();
}

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
    let (sender, receiver) = tokio::sync::oneshot::channel();
    ui.as_weak()
        .upgrade_in_event_loop(move |_| {
            let _ = sender.send(());
        })
        .unwrap();
    receiver.await.unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

/// The real panel at the window's minimum size (640 x 620) with 200% text,
/// in its worst cases: a long wrapped error with an incomplete status, and
/// a search that matches nothing. The status words scroll in their own
/// region, so one whole row (when there are rows) and the fixed footer
/// (Retry, Load more, Close) stay on screen. The arrows reveal the selected
/// row by its real geometry; Tab stays inside the browser; Load more by
/// keyboard keeps focus in it; Escape clears the search, then closes, drops
/// every row and asks the header action to take focus back. Anything that
/// takes the window afterwards (Manage accounts, the account menu, a
/// pending switch, a security key, another page) hides the panel and drops
/// its rows. A surface's own entry withdraws the browser before that surface
/// opens, and nothing comes back when it closes; the quick switcher never
/// opens over it. A Matrix link and the browser hold each other: a link does
/// nothing over an open browser, a link card that opens and closes again
/// over a held page leaves nothing behind, and the browser's own entry ends a
/// joined link's message still to come. Every current surface holds it, in
/// Rust and Slint alike. UI suite phase 89. Needs `register` here and
/// matrix_links' (phase 83) to have run.
pub(crate) async fn assert_thread_browser_panel(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
) {
    let state = ui.global::<ThreadBrowser>();
    let page_before = ui.get_page();
    let live_before = ui.get_live_session();
    ui.set_live_session(true);
    ui.set_page(0);
    ui.set_show_sign_in(false);
    ui.set_font_scale_percent(200);
    ui.window().set_size(slint::PhysicalSize::new(640, 620));
    // Change handlers run first: a context change would close the browser.
    settle(ui).await;
    // The longest status and error Rust publishes for a failed page.
    let error = format!(
        "{} The threads shown were loaded earlier and are not refreshed.",
        ThreadListError::Unavailable.text()
    );
    let incomplete = "60 threads loaded before the error · the list is incomplete · \
        3 could not be shown · the server's list was cut short";
    // Show 60 rows and these words, as Rust publishes them.
    let open = |error: &str, retry: bool, more: bool| {
        let rows = (0..60)
            .map(|index| row_item(&root(index)))
            .collect::<Vec<_>>();
        ROWS.with(|model| model.set_vec(rows));
        state.set_room_name("Design review".into());
        state.set_summary(incomplete.into());
        state.set_error(error.into());
        state.set_can_retry(retry);
        state.set_can_load_more(more);
        state.set_loading(false);
        state.set_generation(state.get_generation().wrapping_add(1));
        state.set_open(true);
    };
    let on_screen = |label: &str| {
        let element = ElementHandle::find_by_accessible_label(ui, label)
            .next()
            .unwrap_or_else(|| panic!("{label} is laid out"));
        let (top, size) = (element.absolute_position(), element.size());
        assert!(size.height > 0.0 && top.y >= -0.5, "{label} is on screen");
        assert!(
            top.y + size.height <= 620.5,
            "{label} is above the window's bottom"
        );
        assert!(
            top.x + size.width <= 640.5,
            "{label} is inside the window's width"
        );
    };
    open(&error, true, false);
    settle(ui).await;

    let list = || {
        ElementHandle::find_by_accessible_label(ui, "Threads in Design review")
            .find(|element| element.accessible_role() == Some(AccessibleRole::List))
            .expect("the thread list is laid out")
    };
    let row = |index: usize| {
        ElementHandle::find_by_accessible_label(
            ui,
            &format!("Thread started by Sender {index}, Root {index}"),
        )
        .next()
    };
    let inside = |inner: &ElementHandle, outer: &ElementHandle| {
        let (top, size) = (inner.absolute_position(), inner.size());
        let (frame, bounds) = (outer.absolute_position(), outer.size());
        size.height > 0.0
            && top.y >= frame.y - 0.5
            && top.y + size.height <= frame.y + bounds.height + 0.5
    };
    // A long wrapped error and an incomplete status: one whole row and the
    // footer stay on screen.
    let first = row(0).expect("the first row is instantiated");
    assert!(
        inside(&first, &list()),
        "one whole row fits the list at 640 x 620 with 200% text"
    );
    on_screen("Read this page of threads again");
    on_screen("Close the thread browser");

    // The search field leads: Down moves the selection to the last row and
    // the list scrolls it into view.
    for _ in 0..59 {
        key(ui, Key::DownArrow);
    }
    settle(ui).await;
    let last = row(59).expect("the selected last row is instantiated");
    assert!(
        inside(&last, &list()),
        "the selected row is revealed inside the list"
    );

    // A search that matches nothing, with the error still shown: the note
    // is laid out in the scrolling region and the footer stays on screen.
    ROWS.with(|model| model.set_vec(Vec::new()));
    state.set_query("nothing here".into());
    settle(ui).await;
    assert!(
        ElementHandle::find_by_accessible_label(
            ui,
            "No loaded thread matches. Load more or change the search."
        )
        .next()
        .is_some()
    );
    on_screen("Read this page of threads again");
    on_screen("Close the thread browser");
    state.set_query("".into());

    // Back to a page that offers more.
    open("", false, true);
    settle(ui).await;
    on_screen("Load more threads of Design review");

    // Tab stays inside: Shift+Tab from the field reaches Close, Tab returns.
    key(ui, Key::Backtab);
    type_text(ui, "z");
    assert_eq!(
        state.get_query().as_str(),
        "",
        "Close has focus, not the field"
    );
    key(ui, Key::Tab);
    type_text(ui, "q");
    assert_eq!(state.get_query().as_str(), "q");

    // Load more by keyboard (Shift+Tab twice: Close, then Load more before
    // it in the footer): the button may go while the page loads, so it
    // hands focus back to the field first and typing still searches.
    key(ui, Key::Backtab);
    key(ui, Key::Backtab);
    key(ui, Key::Return);
    settle(ui).await;
    type_text(ui, "x");
    assert_eq!(
        state.get_query().as_str(),
        "qx",
        "focus is back in the field"
    );

    // Escape clears the search, then closes.
    key(ui, Key::Escape);
    settle(ui).await;
    assert_eq!(state.get_query().as_str(), "");
    assert!(state.get_open());
    let returned = state.get_return_focus();
    key(ui, Key::Escape);
    settle(ui).await;
    assert!(!state.get_open());
    assert_eq!(state.get_return_focus(), returned.wrapping_add(1));
    assert_eq!(
        ROWS.with(|model| model.row_count()),
        0,
        "closing drops every row"
    );

    // Anything that takes the window after the browser opened hides it and
    // drops its rows at once (thread-browser-allowed, then context-changed,
    // then clear), whatever Rust was doing.
    let holds: [(&str, fn(&AppWindow, bool)); 5] = [
        ("Manage accounts", |ui, on| ui.set_manage_accounts_open(on)),
        ("the account menu", |ui, on| ui.set_account_menu_open(on)),
        ("a pending account switch", |ui, on| {
            ui.set_account_switch_pending(on)
        }),
        ("a security key", |ui, on| {
            ui.set_security_recovery_key(if on {
                "presentation-only".into()
            } else {
                "".into()
            })
        }),
        ("the Settings page", |ui, on| {
            ui.set_page(if on { 1 } else { 0 })
        }),
    ];
    for (name, hold) in holds {
        open("", false, true);
        settle(ui).await;
        assert!(state.get_open());
        hold(ui, true);
        settle(ui).await;
        // The guard every publication, page and choice goes through.
        assert!(held(ui), "{name} holds the browser");
        assert!(!state.get_open(), "{name} closes the browser");
        assert_eq!(
            ROWS.with(|model| model.row_count()),
            0,
            "{name} drops every row"
        );
        assert!(
            ElementHandle::find_by_accessible_label(ui, "Close the thread browser")
                .next()
                .is_none(),
            "{name} leaves no panel drawn"
        );
        hold(ui, false);
        settle(ui).await;
        assert!(!held(ui), "{name} released");
        assert!(!state.get_open(), "nothing reopens after {name}");
    }

    // A surface's own entry withdraws the browser synchronously, before
    // that surface opens and before any change handler runs, so closing it
    // again brings nothing back: the account menu's real entry. The quick
    // switcher's real entry (shortcuts::open_picker, registered by phase 14)
    // does not open over the browser at all.
    open("", false, true);
    settle(ui).await;
    ui.invoke_open_quick_switcher();
    assert!(
        !ui.get_quick_switcher_open(),
        "the switcher never opens over the browser"
    );
    assert!(state.get_open(), "and the browser keeps its rows");
    // The real protected-entry withdrawal cancels a latent Leave owner
    // before that surface opens. This token is a view-only stand-in; no
    // RoomLeaveUi, manager admission or HTTP request is used in this phase.
    let pending_leave = morpheus_matrix::RoomLeaveCancellation::new();
    let cancellation = pending_leave.clone();
    ui.global::<crate::RoomLeave>()
        .on_reset(move || cancellation.cancel());
    ui.invoke_open_account_menu();
    assert!(
        pending_leave.is_cancelled(),
        "entry cancels Leave synchronously"
    );
    assert!(held(ui), "the account menu opened through its real entry");
    assert!(!state.get_open(), "its entry withdrew the browser first");
    assert_eq!(
        ROWS.with(|model| model.row_count()),
        0,
        "and dropped every row"
    );
    ui.set_account_menu_open(false);
    settle(ui).await;
    assert!(!held(ui), "the account menu closed");
    assert!(
        !state.get_open(),
        "nothing comes back after the account menu"
    );
    ui.global::<crate::RoomLeave>().on_reset(|| {});

    // A Matrix link and the browser hold each other. A link does nothing
    // over an open browser holding a page (the link's real entry,
    // matrix_links::open), and the browser keeps that page. A card that
    // opens anyway (a route that skipped its entry) hides the panel at once
    // and clears the held page; when the card closes again nothing comes
    // back. The browser's own entry (its Show action) ends a joined link's
    // message still to come.
    open("", false, true);
    state.set_loading(true);
    settle(ui).await;
    let link = "https://matrix.to/#/@alice:example.org";
    assert!(crate::matrix_links::open(ui, live, runtime, link));
    assert!(
        ui.get_matrix_link_blocked(),
        "an open browser holds every link"
    );
    assert!(
        !crate::matrix_links::is_open(ui),
        "the link did nothing over the browser"
    );
    assert!(state.get_open(), "and the browser keeps its held page");
    ui.global::<crate::MatrixLink>().set_open(true);
    assert!(held(ui), "a link card holds the browser");
    settle(ui).await;
    assert!(!state.get_open(), "the card cleared the held page");
    assert_eq!(
        ROWS.with(|model| model.row_count()),
        0,
        "and dropped every row"
    );
    crate::matrix_links::clear(ui);
    settle(ui).await;
    assert!(
        !crate::matrix_links::is_open(ui) && !held(ui),
        "the card closed"
    );
    assert!(
        !state.get_open(),
        "nothing comes back after the link closes"
    );
    let follow_up = crate::matrix_links::tests::install_follow_up();
    assert!(follow_up.admits(ui));
    state.invoke_show();
    assert!(
        follow_up.withdrawn(),
        "the browser's entry ended the link's follow-up"
    );
    clear(ui);
    settle(ui).await;

    // Mutual entry refusal: the shared Leave admission includes the browser,
    // and the browser's actual Show refuses without closing an open Leave.
    open("", false, true);
    settle(ui).await;
    assert!(
        ui.get_room_leave_blocked(),
        "the browser holds Leave admission"
    );
    assert!(
        ui.get_matrix_link_blocked(),
        "the same browser still holds links"
    );
    clear(ui);
    let leave = ui.global::<crate::RoomLeave>();
    leave.set_view(crate::RoomLeaveView {
        open: true,
        ..Default::default()
    });
    assert!(held(ui), "Leave holds browser admission synchronously");
    state.invoke_show();
    assert!(leave.get_view().open, "refused Show never withdraws Leave");
    assert!(!state.get_open(), "Show cannot open over Leave");
    assert_eq!(ROWS.with(|model| model.row_count()), 0);
    leave.set_view(crate::RoomLeaveView::default());
    settle(ui).await;
    assert!(
        !held(ui) && !state.get_open(),
        "closing Leave never reopens the browser"
    );

    // Rust's guard and the Slint gate are the same set, state by state. Each
    // state is set and reset with no settle between, so no change handler
    // or overlay runs and later phases see nothing.
    assert!(!held(ui) && ui.get_thread_browser_allowed());
    let every: [(&str, fn(&AppWindow, bool)); 31] = [
        ("the Settings page", |ui, on| {
            ui.set_page(if on { 1 } else { 0 })
        }),
        ("the call stage", |ui, on| {
            ui.set_page(if on { 2 } else { 0 })
        }),
        ("sign-in", |ui, on| ui.set_show_sign_in(on)),
        ("the access dialog", |ui, on| ui.set_access_open(on)),
        ("a pending account switch", |ui, on| {
            ui.set_account_switch_pending(on)
        }),
        ("a security key", |ui, on| {
            ui.set_security_recovery_key(if on {
                "presentation-only".into()
            } else {
                "".into()
            })
        }),
        ("the account menu", |ui, on| ui.set_account_menu_open(on)),
        ("Manage accounts", |ui, on| ui.set_manage_accounts_open(on)),
        ("room actions", |ui, on| ui.set_room_actions_open(on)),
        ("room admin", |ui, on| ui.set_room_admin_open(on)),
        ("member confirm", |ui, on| ui.set_member_confirm_open(on)),
        ("the capture picker", |ui, on| {
            ui.set_native_capture_picker_open(on)
        }),
        ("the leave-call dialog", |ui, on| {
            ui.set_leave_call_dialog(on)
        }),
        ("the quick switcher", |ui, on| {
            ui.set_quick_switcher_open(on)
        }),
        ("the Space explorer", |ui, on| {
            ui.global::<crate::SpaceExplorer>().set_open(on)
        }),
        ("the image preview", |ui, on| ui.set_image_preview_open(on)),
        ("a shortcut recording", |ui, on| {
            ui.set_shortcut_recording(if on { "recording".into() } else { "".into() })
        }),
        ("the poll dialog", |ui, on| ui.set_poll_dialog_open(on)),
        ("the session ending", |ui, on| ui.set_live_session(!on)),
        ("security work", |ui, on| ui.set_security_busy(on)),
        ("a verification step", |ui, on| ui.set_sas_busy(on)),
        ("a verification", |ui, on| {
            ui.set_sas_stage(if on { 1 } else { 0 })
        }),
        ("the profile", |ui, on| ui.set_account_profile_open(on)),
        ("People", |ui, on| ui.set_people_open(on)),
        ("member details", |ui, on| {
            let privacy = ui.global::<crate::MemberPrivacy>();
            let mut view = privacy.get_inspector();
            view.open = on;
            privacy.set_inspector(view);
        }),
        ("Space rooms", |ui, on| {
            ui.global::<crate::SpaceRooms>().set_open(on)
        }),
        ("room addresses", |ui, on| {
            ui.global::<crate::RoomAddresses>().set_open(on)
        }),
        ("room notifications", |ui, on| {
            ui.global::<crate::RoomNotificationState>().set_open(on)
        }),
        ("Seen by", |ui, on| {
            ui.global::<crate::RoomActivity>().set_seen_open(on)
        }),
        ("a Matrix link card", |ui, on| {
            ui.global::<crate::MatrixLink>().set_open(on)
        }),
        ("Leave", |ui, on| {
            ui.global::<crate::RoomLeave>()
                .set_view(crate::RoomLeaveView {
                    open: on,
                    ..Default::default()
                });
        }),
    ];
    for (name, set) in every {
        set(ui, true);
        let (rust, slint) = (held(ui), !ui.get_thread_browser_allowed());
        set(ui, false);
        assert!(rust, "{name} holds the browser in Rust");
        assert!(slint, "{name} hides the panel in Slint");
    }
    assert!(!held(ui) && ui.get_thread_browser_allowed());

    ui.set_font_scale_percent(100);
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_page(page_before);
    ui.set_live_session(live_before);
    settle(ui).await;
}
