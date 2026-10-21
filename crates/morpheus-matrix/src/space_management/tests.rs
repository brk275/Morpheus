//! Regression source for managing a Space's rooms: link content that keeps
//! every field it does not change byte for byte, unreadable links that stay
//! unknown, bounded route servers, fixed failure categories, strict join
//! rules, the cycle check's pages, and the selection race and admission
//! around the single write.

use matrix_sdk::ruma::{
    api::{
        client::uiaa::UiaaResponse,
        error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
    },
    owned_user_id, room_id,
};
use serde_json::json;

use super::*;

fn api_error(status: u16, errcode: &str) -> HttpError {
    let body: StandardErrorBody = serde_json::from_value(json!({
        "errcode": errcode, "error": "private reflected response"
    }))
    .unwrap();
    HttpError::Api(Box::new(FromHttpResponseError::Server(
        UiaaResponse::MatrixError(Error::new(
            reqwest::StatusCode::from_u16(status).unwrap(),
            ErrorBody::Standard(body),
        )),
    )))
}

fn value(content: &str) -> serde_json::Value {
    serde_json::from_str(content).unwrap()
}

#[test]
fn adding_keeps_every_field_and_sets_only_the_route() {
    let via = vec![ServerName::parse("example.org").unwrap()];
    // No event yet: a plain link.
    let plain = added_content(None, &via).unwrap();
    assert_eq!(value(&plain), json!({"via": ["example.org"]}));
    // A removed link with fields another client stored keeps them.
    let removed = r#"{"order": "m", "suggested": true, "org.example.note": {"k": 1}}"#;
    let added = added_content(Some(removed), &via).unwrap();
    let expected = json!({
        "via": ["example.org"], "order": "m", "suggested": true, "org.example.note": {"k": 1}
    });
    assert_eq!(value(&added), expected);
    assert_eq!(link_state(&added), Ok(true));
    // Content that is not an object, or names a field twice, is not edited.
    assert_eq!(
        added_content(Some(r#"["via"]"#), &via),
        Err(SpaceManageError::InvalidState)
    );
    let twice = r#"{"order": "a", "order": "b"}"#;
    assert_eq!(
        added_content(Some(twice), &via),
        Err(SpaceManageError::InvalidState)
    );
    assert_eq!(added_content(None, &[]), Err(SpaceManageError::InvalidRoom));
}

#[test]
fn unknown_fields_survive_byte_for_byte_including_big_and_decimal_numbers() {
    let via = vec![ServerName::parse("example.org").unwrap()];
    // Beyond u64, a decimal with a trailing zero, an exponent, and a deep
    // value: a serde_json::Value round trip would round or rewrite them.
    let stored = r#"{"org.example.big": 184467440737095516160, "org.example.rate": 1.10,
        "org.example.exp": 1E400, "org.example.deep": {"n": [0.30000000000000004, -0.0]}}"#;
    let added = added_content(Some(stored), &via).unwrap();
    for kept in [
        "184467440737095516160",
        "1.10",
        "1E400",
        "0.30000000000000004",
        "-0.0",
    ] {
        assert!(added.contains(kept), "{kept} was rewritten: {added}");
    }
    let removed = removed_content(Some(&added)).unwrap();
    for kept in [
        "184467440737095516160",
        "1.10",
        "1E400",
        "0.30000000000000004",
        "-0.0",
    ] {
        assert!(removed.contains(kept), "{kept} was rewritten: {removed}");
    }
    assert!(!removed.contains("via"));
    // A read-back compares numbers as written: reordered fields and other
    // whitespace match, a rounded number does not.
    let reordered = format!(r#"{{"via":["example.org"],{}"#, &stored[1..]);
    assert!(same_content(&reordered, &added));
    let rounded = added.replace("1.10", "1.1");
    assert!(!same_content(&rounded, &added));
    let big = added.replace("184467440737095516160", "184467440737095516161");
    assert!(!same_content(&big, &added));
    assert!(!same_content("[]", &added));
}

#[test]
fn removing_drops_only_the_route_of_this_spaces_link() {
    let linked = r#"{"via": ["example.org"], "order": "b", "org.example.note": 2}"#;
    let removed = removed_content(Some(linked)).unwrap();
    assert_eq!(
        value(&removed),
        json!({"order": "b", "org.example.note": 2})
    );
    assert_eq!(link_state(&removed), Ok(false));
    // Nothing to remove, or content Morpheus cannot edit safely.
    assert_eq!(removed_content(None), Err(SpaceManageError::InvalidState));
    assert_eq!(
        removed_content(Some(r#""x""#)),
        Err(SpaceManageError::InvalidState)
    );
    // An empty or missing route is no link; unreadable content is not
    // "absent".
    assert_eq!(link_state(r#"{"via": []}"#), Ok(false));
    assert_eq!(link_state("{}"), Ok(false));
    assert_eq!(link_state("[]"), Err(SpaceManageError::InvalidState));
    let twice = r#"{"via": ["a.org"], "via": []}"#;
    assert_eq!(link_state(twice), Err(SpaceManageError::InvalidState));
}

#[test]
fn a_link_event_that_cannot_be_read_is_unknown_never_absent() {
    let raw = |content: &str| Some(RawValue::from_string(content.to_owned()).unwrap());
    let mut links = Links {
        complete: true,
        ..Links::default()
    };
    links.take(
        Some("!listed:example.org".into()),
        raw(r#"{"via": ["example.org"]}"#),
    );
    links.take(Some("!removed:example.org".into()), raw("{}"));
    // Malformed content (not an object, a field twice) or none at all.
    links.take(Some("!list:example.org".into()), raw("[1]"));
    links.take(
        Some("!twice:example.org".into()),
        raw(r#"{"via": [], "via": ["a"]}"#),
    );
    links.take(Some("!empty:example.org".into()), None);
    assert_eq!(links.state("!listed:example.org"), Some(true));
    assert_eq!(links.state("!removed:example.org"), Some(false));
    assert_eq!(links.state("!other:example.org"), Some(false));
    for unknown in [
        "!list:example.org",
        "!twice:example.org",
        "!empty:example.org",
    ] {
        assert_eq!(links.state(unknown), None, "{unknown}");
    }
    assert!(links.complete);
    // A state key that is missing or not a room ID may name any room: no
    // unlisted row is known to be absent from then on.
    links.take(None, raw("{}"));
    assert!(!links.complete);
    assert_eq!(links.state("!other:example.org"), None);
    assert_eq!(links.state("!listed:example.org"), Some(true));
    let mut links = Links {
        complete: true,
        ..Links::default()
    };
    links.take(
        Some("not a room".into()),
        raw(r#"{"via": ["example.org"]}"#),
    );
    assert_eq!(links.state("!other:example.org"), None);
}

#[test]
fn a_present_join_rule_that_cannot_be_read_refuses_rather_than_hiding_the_note() {
    assert_eq!(public_rule(Some(&json!({"join_rule": "public"}))), Ok(true));
    assert_eq!(
        public_rule(Some(&json!({"join_rule": "invite"}))),
        Ok(false)
    );
    assert_eq!(
        public_rule(Some(&json!({"join_rule": "knock_restricted"}))),
        Ok(false)
    );
    // No event: the default join rule (invite).
    assert_eq!(public_rule(None), Ok(false));
    // Present but malformed: never "not public".
    for malformed in [
        json!({}),
        json!({"join_rule": 1}),
        json!({"join_rule": null}),
        json!({"join_rule": "org.example.unknown"}),
        json!({"join_rule": "restricted", "allow": 1}),
    ] {
        assert_eq!(
            public_rule(Some(&malformed)),
            Err(SpaceManageError::AccessUnreadable)
        );
    }
    assert!(!SpaceManageError::AccessUnreadable.may_have_changed());
    assert!(
        SpaceManageError::AccessUnreadable
            .text()
            .contains("Nothing was changed")
    );
}

#[test]
fn malformed_routes_are_unknown_and_cached_bytes_are_bounded_before_fields_are_read() {
    for via in [
        "null",
        "{}",
        "42",
        r#""example.org""#,
        "[null]",
        "[42]",
        r#"["not a server name"]"#,
    ] {
        let content = format!(r#"{{"via":{via}}}"#);
        assert_eq!(link_state(&content), Err(SpaceManageError::InvalidState));
        let mut links = Links {
            complete: true,
            ..Links::default()
        };
        links.take(
            Some("!room:example.org".into()),
            Some(RawValue::from_string(content).unwrap()),
        );
        assert_eq!(links.state("!room:example.org"), None);
    }
    let raw = format!(
        r#"{{"state_key":"!room:example.org","content":{{"via":["example.org"]}},"padding":"{}"}}"#,
        "x".repeat(MAX_CACHED_EVENT_BYTES)
    );
    let event = RawAnySyncOrStrippedState::Sync(Raw::from_json_string(raw).unwrap());
    let mut links = Links {
        complete: true,
        ..Links::default()
    };
    take_cached_event(&mut links, &event);
    assert!(!links.complete);
    assert!(links.linked.is_empty());
    assert_eq!(links.state("!room:example.org"), None);
    let too_large = format!(r#"{{"padding":"{}"}}"#, "x".repeat(MAX_LINK_CONTENT_BYTES));
    assert_eq!(link_state(&too_large), Err(SpaceManageError::InvalidState));
}

#[test]
fn a_route_addition_that_would_exceed_the_output_bound_is_refused() {
    let prefix = r#"{"padding":""#;
    let suffix = r#""}"#;
    let stored = format!(
        "{prefix}{}{suffix}",
        "x".repeat(MAX_LINK_CONTENT_BYTES - 256 - prefix.len() - suffix.len())
    );
    assert!(stored.len() < MAX_LINK_CONTENT_BYTES);
    let via = ['a', 'b', 'c']
        .into_iter()
        .map(|letter| {
            let label = letter.to_string().repeat(60);
            ServerName::parse(vec![label; 4].join(".")).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        added_content(Some(&stored), &via),
        Err(SpaceManageError::InvalidState)
    );
    // At the normal bound, unknown lexical values are still untouched.
    assert!(
        removed_content(Some(&stored))
            .unwrap()
            .contains(&"x".repeat(64))
    );
}

#[test]
fn the_write_starts_only_with_the_space_and_an_added_room_joined_at_admission() {
    use matrix_sdk::RoomState::{Invited, Joined, Left};
    assert!(admits_write(Joined, true, Some(Joined)));
    assert!(admits_write(Joined, false, None));
    // Left meanwhile, during the fresh reads before the write.
    assert!(!admits_write(Joined, true, Some(Left)));
    assert!(!admits_write(Joined, true, Some(Invited)));
    assert!(!admits_write(Joined, true, None));
    assert!(!admits_write(Left, false, None));
    assert!(!admits_write(Left, true, Some(Joined)));
}

#[test]
fn route_servers_are_bounded_and_validated() {
    let own = owned_user_id!("@alice:example.org");
    let names = |room: &RoomId| {
        via(&own, room)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(room_id!("!r:other.org")),
        ["example.org", "other.org"]
    );
    assert_eq!(names(room_id!("!r:example.org")), ["example.org"]);
    // A room ID without a server (room version 12) adds nothing more.
    if let Ok(opaque) = RoomId::parse("!31hneApxJ_1o-63DmFrpeqnkFfWppnzWso1JvH3ogLM") {
        assert_eq!(names(&opaque), ["example.org"]);
    }
    assert!(via(&own, room_id!("!r:other.org")).len() <= MAX_LINK_VIA);
}

#[test]
fn only_a_definite_refusal_says_nothing_changed() {
    for (status, errcode, expected) in [
        (403, "M_FORBIDDEN", SpaceManageError::NotAllowed),
        (429, "M_LIMIT_EXCEEDED", SpaceManageError::RateLimited),
        (400, "M_BAD_JSON", SpaceManageError::Rejected),
    ] {
        let failure = write_failure(&api_error(status, errcode));
        assert_eq!(failure, expected, "{errcode}");
        assert!(!failure.may_have_changed());
        assert!(!failure.text().contains("private"));
    }
    // With the replay refused, an expired token is unconfirmed; so is a
    // server error or an unreadable reply.
    for error in [
        api_error(401, "M_UNKNOWN_TOKEN"),
        api_error(500, "M_UNKNOWN"),
        HttpError::ResponseTooLarge {
            limit: MAX_REPLY_BYTES,
        },
    ] {
        let failure = write_failure(&error);
        assert_eq!(failure, SpaceManageError::OutcomeUnknown);
        assert!(failure.may_have_changed());
        assert!(failure.text().contains("may or may not"));
    }
}

#[test]
fn the_single_write_is_dropped_when_the_selection_is_revoked() {
    use crate::account_selection::Selections;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let selections = Selections::default();
        selections.replace(Some("a"));
        let selection = selections.for_account("a").unwrap();
        let sends = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(tokio::sync::Notify::new());
        let write = {
            let (sends, held) = (sends.clone(), held.clone());
            async move {
                sends.fetch_add(1, Ordering::SeqCst);
                held.notified().await;
                sends.fetch_add(1, Ordering::SeqCst);
                Ok::<_, HttpError>(())
            }
        };
        let racing = tokio::spawn(async move { unless_revoked(&selection, write).await });
        while sends.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        selections.replace(Some("b"));
        assert!(
            racing.await.unwrap().is_none(),
            "reported as unknown, not as unsent"
        );
        held.notify_waiters();
        tokio::task::yield_now().await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    });
    // The write is capped and never retried by the SDK.
    let config = format!("{:?}", write_config());
    assert!(config.contains("retry_limit: 0"), "{config}");
    assert!(
        config.contains(&format!("max_response_bytes: {MAX_REPLY_BYTES}")),
        "{config}"
    );
}

#[test]
fn a_revocation_ready_with_the_write_starts_no_write() {
    use crate::account_selection::Selections;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let selections = Selections::default();
        selections.replace(Some("a"));
        let selection = selections.for_account("a").unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        // Everything before the write is done, so it would start on the
        // first poll (apply_space_link_for's `started` marks that step)...
        let write = {
            let (writes, started) = (writes.clone(), started.clone());
            async move {
                started.store(true, Ordering::SeqCst);
                std::future::ready(()).await;
                writes.fetch_add(1, Ordering::SeqCst);
                Ok::<_, HttpError>(())
            }
        };
        // ...but the account is left before that poll: both are ready.
        selections.replace(Some("b"));
        assert!(unless_revoked(&selection, write).await.is_none());
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "a write started after revocation"
        );
        // Nothing was sent, so it reads as the account change, not unknown.
        assert!(!started.load(Ordering::SeqCst));
        assert_eq!(dropped(false), SpaceManageError::AccountChanged);
        assert_eq!(dropped(true), SpaceManageError::OutcomeUnknown);
    });
}

fn level_entry(
    id: &str,
    is_space: bool,
    links: &[&str],
    truncated: bool,
) -> crate::SpaceLevelEntry {
    crate::SpaceLevelEntry {
        node: crate::SpaceNodeInfo {
            room_id: id.into(),
            name: None,
            topic: None,
            avatar_mxc: None,
            is_space,
            children: links
                .iter()
                .map(|link| crate::SpaceChildInfo {
                    room_id: (*link).into(),
                    via_servers: vec!["example.org".into()],
                    suggested: false,
                })
                .collect(),
            canonical_alias: None,
            joined_members: 1,
            join_rule: crate::SpaceJoinRule::Public,
            unsupported_type: false,
        },
        membership: None,
        links_truncated: truncated,
    }
}

fn level_page(
    space: Option<crate::SpaceLevelEntry>,
    entries: Vec<crate::SpaceLevelEntry>,
) -> SpaceLevelPage {
    SpaceLevelPage {
        space_id: "!sub:example.org".into(),
        space,
        entries,
        linked_memberships: Vec::new(),
        next_batch: None,
        truncated: false,
    }
}

#[test]
fn a_loop_check_never_reads_an_unread_link_as_absent() {
    let from = "!sub:example.org";
    let target = "!this:example.org";
    // Listed directly, or by a child: a loop.
    let page = level_page(Some(level_entry(from, true, &[target], false)), Vec::new());
    assert_eq!(scan_page(&page, from, target), (true, false));
    // Not listed, but the Space's own links were cut at the bound: the
    // 501st link may be the target, so it is not "absent".
    let cut = level_entry(from, true, &["!a:example.org"], true);
    let page = level_page(Some(cut), Vec::new());
    assert_eq!(scan_page(&page, from, target), (false, true));
    // A nested Space with children of its own is a level deeper: unread.
    let nested = level_entry("!n:example.org", true, &["!x:example.org"], false);
    let parent = level_entry(from, true, &["!n:example.org"], false);
    let page = level_page(Some(parent), vec![nested]);
    assert_eq!(scan_page(&page, from, target), (false, true));
    // Plain rooms only, every link read: absent.
    let room = level_entry("!r:example.org", false, &[], false);
    let parent = level_entry(from, true, &["!r:example.org"], false);
    let page = level_page(Some(parent), vec![room]);
    assert_eq!(scan_page(&page, from, target), (false, false));
}

#[test]
fn a_listed_target_is_a_loop_and_an_earlier_unread_page_is_never_absent() {
    let from = "!sub:example.org";
    let target = "!this:example.org";
    let room = || level_entry("!r:example.org", false, &[], false);
    // The first page has no entry for the Space itself, but a child lists
    // the target: still a loop, not "could not check".
    let lists = level_entry("!c:example.org", true, &[target], false);
    let page = level_page(None, vec![lists]);
    let mut scan = ReachScan::default();
    assert_eq!(scan.page(true, &page, from, target), Some(Reach::Yes));
    // Without that entry and without the target: unknown.
    let mut scan = ReachScan::default();
    let page = level_page(None, vec![room()]);
    assert_eq!(scan.page(true, &page, from, target), Some(Reach::Unknown));
    // A truncated first page, then a clean last page: still unknown.
    let parent = || level_entry(from, true, &["!r:example.org"], false);
    let mut first = level_page(Some(parent()), vec![room()]);
    first.truncated = true;
    first.next_batch = Some("next".into());
    let mut scan = ReachScan::default();
    assert_eq!(scan.page(true, &first, from, target), None);
    let last = level_page(None, vec![room()]);
    assert_eq!(scan.page(false, &last, from, target), Some(Reach::Unknown));
    // Every page read in full and clean: no loop.
    let mut first = level_page(Some(parent()), vec![room()]);
    first.next_batch = Some("next".into());
    let mut scan = ReachScan::default();
    assert_eq!(scan.page(true, &first, from, target), None);
    assert_eq!(scan.page(false, &last, from, target), Some(Reach::No));
    // A later page that lists the target is a loop whatever came before.
    let mut first = level_page(None, vec![room()]);
    first.next_batch = Some("next".into());
    let mut scan = ReachScan::default();
    assert_eq!(scan.page(true, &first, from, target), None);
    let lists = level_entry("!c:example.org", true, &[target], false);
    let last = level_page(None, vec![lists]);
    assert_eq!(scan.page(false, &last, from, target), Some(Reach::Yes));
}

// No loaded SDK client, credentials or network: the actual selected apply
// must still withdraw while its real manager lock is held.
#[test]
fn a_closed_space_form_withdraws_the_actual_manager_wait() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory =
            std::env::temp_dir().join(format!("morpheus-space-scope-{}", uuid::Uuid::new_v4()));
        let manager = AccountManager::open(&directory).unwrap();
        let account = crate::AccountInfo {
            id: "space-scope-fixture".into(),
            user_id: "@casey:example.org".into(),
            device_id: "SPACE-SCOPE".into(),
            homeserver: "http://127.0.0.1:9".into(),
        };
        manager.publish_active_account(Some(account.clone()));
        {
            let selection = manager.account_selection(&account.id).unwrap();
            let change = SpaceLinkChange {
                space_id: "!s:example.org".into(),
                room_id: "!r:example.org".into(),
                add: false,
                warnings: Vec::new(),
                baseline: None,
                content: "{}".into(),
                conditions: None,
            };
            let held = manager.state.lock().await;
            let cancellation = RoomSettingsCancellation::new();
            let work =
                manager.apply_space_link_guarded(&selection, &change, &cancellation, &|| true);
            tokio::pin!(work);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut work)
                    .await
                    .is_err()
            );
            cancellation.cancel();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), &mut work)
                    .await
                    .unwrap(),
                Err(SpaceManageError::Cancelled)
            );
            assert!(
                manager.state.try_lock().is_err(),
                "the test never released the real preflight lock"
            );
            // The same production finite wait bounds a manager that never unlocks.
            let cancellation = RoomSettingsCancellation::new();
            let bounded = guarded(
                &manager,
                &selection,
                &cancellation,
                &|| true,
                Duration::from_millis(10),
                manager.managed_space(&selection, &change.space_id),
            )
            .await;
            assert!(matches!(bounded, Err(SpaceManageError::Unavailable)));
            let cancellation = RoomSettingsCancellation::new();
            let work =
                manager.apply_space_link_guarded(&selection, &change, &cancellation, &|| true);
            tokio::pin!(work);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut work)
                    .await
                    .is_err()
            );
            manager.publish_active_account(None);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), &mut work)
                    .await
                    .unwrap(),
                Err(SpaceManageError::AccountChanged)
            );
            drop(held);
        }
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    });
}

#[test]
fn the_production_dispatch_rechecks_held_preflight_authority_and_keeps_unknown_after_start() {
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory =
            std::env::temp_dir().join(format!("morpheus-space-dispatch-{}", uuid::Uuid::new_v4()));
        let manager = AccountManager::open(&directory).unwrap();
        let account = crate::AccountInfo {
            id: "space-dispatch-fixture".into(),
            user_id: "@casey:example.org".into(),
            device_id: "SPACE-DISPATCH".into(),
            homeserver: "http://127.0.0.1:9".into(),
        };
        manager.publish_active_account(Some(account.clone()));
        {
            let selection = manager.account_selection(&account.id).unwrap();
            let generation = AtomicU64::new(4);
            let admission = || generation.load(Ordering::Acquire) == 4;
            let cancellation = RoomSettingsCancellation::new();
            let (release, held) = tokio::sync::oneshot::channel();
            let work = guarded(
                &manager,
                &selection,
                &cancellation,
                &admission,
                READ_BOUND,
                async {
                    held.await.unwrap();
                    Ok(())
                },
            );
            tokio::pin!(work);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut work)
                    .await
                    .is_err()
            );
            // The protected step began and ended, or navigation went A→B→A:
            // equal visible strings do not restore the captured generation.
            generation.store(5, Ordering::Release);
            release.send(()).unwrap();
            work.await.unwrap();
            let polls = AtomicUsize::new(0);
            let result = dispatch(&manager, &selection, &cancellation, &admission, async {
                polls.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
            .await;
            assert_eq!(result, Err(SpaceManageError::SecurityBusy));
            assert_eq!(polls.load(Ordering::Acquire), 0);
            // A ready cancellation wins over a ready send's first poll.
            let cancellation = RoomSettingsCancellation::new();
            cancellation.cancel();
            assert_eq!(
                dispatch(&manager, &selection, &cancellation, &|| true, async {
                    polls.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                })
                .await,
                Err(SpaceManageError::Cancelled)
            );
            assert_eq!(polls.load(Ordering::Acquire), 0);
            // Once the actual dispatch wrapper started, closing is Unknown.
            let cancellation = RoomSettingsCancellation::new();
            let (_release, held) = tokio::sync::oneshot::channel::<()>();
            let send = dispatch(&manager, &selection, &cancellation, &|| true, async {
                polls.fetch_add(1, Ordering::AcqRel);
                held.await.unwrap();
                Ok(())
            });
            tokio::pin!(send);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut send)
                    .await
                    .is_err()
            );
            cancellation.cancel();
            assert_eq!(send.await, Err(SpaceManageError::OutcomeUnknown));
            assert_eq!(polls.load(Ordering::Acquire), 1, "no send is retried");
        }
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    });
}
