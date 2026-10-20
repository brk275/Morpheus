//! Regression source for the Space explorer: pagination, cycles and bounds,
//! a room in several Spaces, and results for a departed level or selection.
//! The model tests need no account; the A-B-A test uses the local homeserver
//! fixture (MORPHEUS_MATRIX_TEST_ACCOUNTS) and is skipped without it.

use std::sync::Arc;

use morpheus_matrix::{
    AccountManager, MatrixLinkTarget, RoomMembership, SpaceChildInfo, SpaceJoinRule,
    SpaceLevelEntry, SpaceLevelError, SpaceLevelPage, SpaceNodeInfo, SyncPolicy,
};
use slint::Weak;
use tokio::runtime::Handle;

use super::*;
use crate::voice_spaces::VoiceSpaces;

const ROOT: &str = "!root:example.org";

fn node(id: &str, is_space: bool, links: &[&str]) -> SpaceNodeInfo {
    SpaceNodeInfo {
        room_id: id.into(),
        name: Some(format!("Name of {id}")),
        topic: None,
        avatar_mxc: None,
        is_space,
        children: links
            .iter()
            .map(|link| SpaceChildInfo {
                room_id: (*link).into(),
                via_servers: vec!["example.org".into()],
                suggested: false,
            })
            .collect(),
        canonical_alias: None,
        joined_members: 3,
        join_rule: SpaceJoinRule::Public,
        unsupported_type: false,
    }
}

fn entry(node: SpaceNodeInfo) -> SpaceLevelEntry {
    SpaceLevelEntry {
        node,
        membership: None,
        links_truncated: false,
    }
}

fn page(
    space_id: &str,
    space: Option<SpaceNodeInfo>,
    entries: Vec<SpaceNodeInfo>,
    next_batch: Option<&str>,
) -> SpaceLevelPage {
    SpaceLevelPage {
        space_id: space_id.into(),
        space: space.map(entry),
        entries: entries.into_iter().map(entry).collect(),
        linked_memberships: Vec::new(),
        next_batch: next_batch.map(str::to_owned),
    }
}

fn ids(explorer: &Explorer) -> Vec<String> {
    explorer.visible().map(|row| row.room_id.clone()).collect()
}

fn opened() -> (Explorer, Request) {
    let mut explorer = Explorer::new(1, ROOT, "Root", Vec::new());
    let request = explorer.next_request().expect("the first page is asked for");
    assert_eq!(request.from, None);
    assert!(explorer.next_request().is_none(), "one page at a time");
    (explorer, request)
}

#[test]
fn pages_follow_the_token_and_keep_each_room_once() {
    let links = ["!a:example.org", "!b:example.org", "!c:example.org"];
    let (mut explorer, first) = opened();
    let result = page(
        ROOT,
        Some(node(ROOT, true, &links)),
        vec![node(links[0], false, &[]), node(links[1], false, &[])],
        Some("t1"),
    );
    assert!(explorer.apply(&first, Ok(result)));
    assert_eq!(ids(&explorer), ["!a:example.org", "!b:example.org"]);
    assert!(explorer.can_load_more());
    let second = explorer.next_request().expect("Load more");
    assert_eq!(second.from.as_deref(), Some("t1"));
    // The server repeats a room on the next page.
    let result = page(
        ROOT,
        None,
        vec![node(links[0], false, &[]), node(links[2], false, &[])],
        None,
    );
    assert!(explorer.apply(&second, Ok(result)));
    assert_eq!(ids(&explorer), links);
    assert!(!explorer.can_load_more());
    assert!(explorer.next_request().is_none());
}

#[test]
fn links_the_server_does_not_describe_are_listed_and_extras_are_not() {
    let links = ["!a:example.org", "!hidden:example.org", "!mine:example.org"];
    let (mut explorer, first) = opened();
    let mut result = page(
        ROOT,
        Some(node(ROOT, true, &links)),
        // A room the Space does not link is not one of its children.
        vec![node(links[0], false, &[]), node("!extra:example.org", false, &[])],
        None,
    );
    result.linked_memberships = vec![(links[2].into(), RoomMembership::Joined)];
    assert!(explorer.apply(&first, Ok(result)));
    assert_eq!(ids(&explorer), links);
    let rows = explorer.visible().collect::<Vec<_>>();
    assert_eq!(rows[1].status(), Status::Unavailable);
    assert!(explorer.join_target(links[1]).is_none());
    // A joined room the server could not describe can still be opened.
    assert_eq!(rows[2].status(), Status::Joined);
    assert!(explorer.open_target(links[2]).is_some());
}

#[test]
fn a_cycle_goes_back_instead_of_reading_again() {
    let child = "!child:example.org";
    let (mut explorer, first) = opened();
    let result = page(
        ROOT,
        Some(node(ROOT, true, &[child])),
        vec![node(child, true, &[ROOT])],
        None,
    );
    assert!(explorer.apply(&first, Ok(result)));
    assert!(explorer.browse(child, 2));
    let in_child = explorer.next_request().expect("the child level is read");
    assert_eq!(in_child.space_id, child);
    // The child links the root back.
    let result = page(
        child,
        Some(node(child, true, &[ROOT])),
        vec![node(ROOT, true, &[child])],
        None,
    );
    assert!(explorer.apply(&in_child, Ok(result)));
    assert!(explorer.in_trail(ROOT));
    assert!(explorer.browse(ROOT, 3));
    assert_eq!(explorer.trail.len(), 1, "went back to the root");
    // The root keeps its rows: nothing is read again.
    assert!(explorer.next_request().is_none());
    assert_eq!(ids(&explorer), [child]);
}

#[test]
fn depth_pages_and_rows_are_bounded() {
    let mut explorer = Explorer::new(1, ROOT, "Root", Vec::new());
    let mut space = ROOT.to_owned();
    for depth in 0..MAX_TRAIL {
        let request = explorer.next_request().expect("a new level is read");
        let child = format!("!level{depth}:example.org");
        let result = page(
            &space,
            Some(node(&space, true, &[&child])),
            vec![node(&child, true, &[])],
            None,
        );
        assert!(explorer.apply(&request, Ok(result)));
        let browsed = explorer.browse(&child, depth as u64 + 2);
        assert_eq!(browsed, depth + 1 < MAX_TRAIL, "depth {depth}");
        space = child;
    }
    assert_eq!(explorer.trail.len(), MAX_TRAIL);
    assert!(!explorer.notice.is_empty());

    // Pages: a level stops offering more after MAX_LEVEL_PAGES.
    let mut explorer = Explorer::new(1, ROOT, "Root", Vec::new());
    for index in 0..MAX_LEVEL_PAGES {
        let request = explorer.next_request().expect("another page");
        let room = format!("!r{index}:example.org");
        let token = format!("t{index}");
        let result = page(ROOT, None, vec![node(&room, false, &[])], Some(&token));
        assert!(explorer.apply(&request, Ok(result)));
    }
    assert!(!explorer.can_load_more());
    assert!(explorer.next_request().is_none());

    // Rows: one level lists at most MAX_LEVEL_ROWS.
    let mut explorer = Explorer::new(1, ROOT, "Root", Vec::new());
    let request = explorer.next_request().unwrap();
    let rooms = (0..MAX_LEVEL_ROWS + 25)
        .map(|index| node(&format!("!many{index}:example.org"), false, &[]))
        .collect();
    assert!(explorer.apply(&request, Ok(page(ROOT, None, rooms, Some("more")))));
    assert_eq!(explorer.top().rows.len(), MAX_LEVEL_ROWS);
    assert!(explorer.top().capped);
    assert!(!explorer.can_load_more());
}

#[test]
fn a_room_in_several_spaces_is_joined_everywhere_it_is_listed() {
    let shared = "!shared:example.org";
    let child = "!child:example.org";
    let (mut explorer, first) = opened();
    let result = page(
        ROOT,
        Some(node(ROOT, true, &[shared, child])),
        vec![node(shared, false, &[]), node(child, true, &[shared])],
        None,
    );
    assert!(explorer.apply(&first, Ok(result)));
    assert!(explorer.browse(child, 2));
    let in_child = explorer.next_request().unwrap();
    let result = page(
        child,
        Some(node(child, true, &[shared])),
        vec![node(shared, false, &[])],
        None,
    );
    assert!(explorer.apply(&in_child, Ok(result)));
    let target = explorer.join_target(shared).expect("a public room is joinable");
    assert_eq!(target.via, ["example.org"]);
    assert!(explorer.begin_join(shared, 7));
    assert!(!explorer.begin_join(child, 8), "one join at a time");
    assert!(!explorer.finish_join(6, true, ""), "another join's result");
    assert!(explorer.finish_join(7, true, ""));
    for level in &explorer.trail {
        let row = level.rows.iter().find(|row| row.room_id == shared).unwrap();
        assert_eq!(row.status(), Status::Joined);
    }
}

#[test]
fn results_for_a_departed_level_or_explorer_are_dropped() {
    let child = "!child:example.org";
    let (mut explorer, first) = opened();
    let result = page(
        ROOT,
        Some(node(ROOT, true, &[child])),
        vec![node(child, true, &[])],
        Some("t"),
    );
    assert!(explorer.apply(&first, Ok(result)));
    let more = explorer.next_request().unwrap();
    // Browsing retires the root's Load more.
    assert!(explorer.browse(child, 2));
    let late = page(ROOT, None, vec![node("!late:example.org", false, &[])], None);
    assert!(!explorer.apply(&more, Ok(late)));
    // Going back while the child is read retires the child's first page,
    // and the root keeps the rows and token it had.
    let in_child = explorer.next_request().unwrap();
    assert!(explorer.back(3));
    assert!(!explorer.apply(&in_child, Ok(page(child, None, Vec::new(), None))));
    assert_eq!(ids(&explorer), [child]);
    assert!(explorer.can_load_more());
    // A reopened explorer never takes a request of the one before it.
    let mut reopened = Explorer::new(9, ROOT, "Root", Vec::new());
    let current = reopened.next_request().unwrap();
    assert!(!reopened.apply(&first, Ok(page(ROOT, None, Vec::new(), None))));
    assert!(reopened.apply(&current, Err(SpaceLevelError::Forbidden)));
    assert_eq!(reopened.top().error, Some(SpaceLevelError::Forbidden));
    // The Space can still be joined deliberately, then read.
    assert!(reopened.join_target(ROOT).is_some());
    assert!(reopened.retry().is_some());
}

#[test]
fn statuses_decide_what_may_be_joined_or_opened() {
    let row = |rule, membership, unsupported| Row {
        room_id: "!r:example.org".into(),
        name: "Room".into(),
        alias: String::new(),
        topic: String::new(),
        members: 1,
        is_space: false,
        unsupported,
        join_rule: rule,
        membership,
        suggested: false,
        via: Vec::new(),
        links: None,
        missing: false,
    };
    use SpaceJoinRule::*;
    assert_eq!(row(Public, None, false).status(), Status::Public);
    assert_eq!(row(Restricted, None, false).status(), Status::Restricted);
    assert_eq!(row(KnockRestricted, None, false).status(), Status::Restricted);
    assert_eq!(row(Knock, None, false).status(), Status::Knock);
    assert_eq!(row(Invite, None, false).status(), Status::InviteOnly);
    assert_eq!(row(Private, None, false).status(), Status::InviteOnly);
    assert_eq!(row(Other, None, false).status(), Status::UnknownRule);
    assert_eq!(row(Public, None, true).status(), Status::Unsupported);
    assert_eq!(
        row(Invite, Some(RoomMembership::Invited), false).status(),
        Status::Invited
    );
    assert_eq!(
        row(Public, Some(RoomMembership::Banned), false).status(),
        Status::Banned
    );
    for status in [Status::Invited, Status::Public, Status::Restricted] {
        assert!(status.joinable());
    }
    for status in [
        Status::Joined,
        Status::Knock,
        Status::InviteOnly,
        Status::Banned,
        Status::Unsupported,
        Status::Unavailable,
        Status::UnknownRule,
    ] {
        assert!(!status.joinable());
    }
    // A restricted join failure names the rule, never the server's text.
    let target = JoinTarget {
        room_id: "!r:example.org".into(),
        name: "Room".into(),
        via: Vec::new(),
        is_space: false,
        restricted: true,
    };
    let failure = target.failure(&MatrixCoreError::Sdk("M_FORBIDDEN: secret".into()));
    assert!(failure.contains("certain Spaces") && !failure.contains("secret"));
    // A target without route servers is routed through its own server.
    let MatrixLinkTarget::Room { via_servers, .. } = target.link() else {
        panic!("a room target");
    };
    assert_eq!(via_servers, ["example.org"]);
}

#[test]
fn search_filters_the_loaded_level_and_navigation_clears_it() {
    let links = ["!design:example.org", "!ops:example.org"];
    let (mut explorer, first) = opened();
    let mut design = node(links[0], true, &[]);
    design.name = Some("Design Reviews".into());
    let result = page(
        ROOT,
        Some(node(ROOT, true, &links)),
        vec![design, node(links[1], false, &[])],
        None,
    );
    assert!(explorer.apply(&first, Ok(result)));
    explorer.set_query("  REVIEW\u{7}");
    assert_eq!(ids(&explorer), [links[0]]);
    assert!(explorer.browse(links[0], 2));
    assert!(explorer.back(3));
    assert_eq!(ids(&explorer), links);
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
async fn a_departed_selection_neither_reads_nor_joins_after_a_b_a() {
    let Some((alice, carol)) = fixture() else {
        eprintln!("Skipped explorer A-B-A regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(
        AccountManager::open_with_sync_policy(directory.path().join("profile"), SyncPolicy::Auto)
            .unwrap(),
    );
    let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
        directory.path().join("config.toml"),
    )));
    let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
    let a = manager
        .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
        .await
        .expect("first login failed");
    let b = manager
        .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
        .await
        .expect("second login failed");
    manager.switch_to(&a.id).await.expect("switch to A");
    let departed = manager.account_selection(&a.id).unwrap();
    manager.switch_to(&b.id).await.expect("switch to B");
    manager.switch_to(&a.id).await.expect("switch back to A");
    assert!(!departed.is_current());
    assert!(manager.account_selection(&a.id).unwrap().is_current());
    // The same account is active again, but the explorer opened under the
    // departed selection reads nothing...
    let read = manager.space_level_for(&departed, "!space:example.org", None).await;
    assert_eq!(read, Err(SpaceLevelError::AccountChanged));
    // ...and joins nothing.
    let target = MatrixLinkTarget::Room {
        room_or_alias_id: "!space:example.org".into(),
        event_id: None,
        via_servers: vec!["example.org".into()],
    };
    let joined = live
        .join_explorer_target(&departed, &target, Some(true), "Space", Weak::default())
        .await;
    assert!(matches!(joined, Err(MatrixCoreError::SelectedAccountChanged)));
    manager.stop().await;
    drop(live);
    drop(manager);
    directory.close().unwrap();
}
