//! Regression source for managing a Space's rooms: what a review says
//! before Confirm, what a finished write leaves on screen, the search, and
//! rows whose link is not known.

use super::*;

fn save_mark(marks: &mut Marks, account: &str, space: &str, room: &str, state: Option<bool>) {
    let receipt = marks.reserve(account, space, room).unwrap();
    match state {
        Some(linked) => marks.finish(&receipt, linked, &Ok(true)),
        None => marks.finish(&receipt, false, &Err(SpaceManageError::OutcomeUnknown)),
    }
}

fn pending_rooms(marks: &mut Marks, account: &str, space: &str) -> Vec<String> {
    marks
        .pending(account, space)
        .into_iter()
        .map(|read| read.key.2)
        .collect()
}

fn candidate(id: &str, name: &str, linked: Option<bool>, is_space: bool) -> SpaceRoomCandidate {
    SpaceRoomCandidate {
        room_id: id.into(),
        name: name.into(),
        is_space,
        linked,
        public: false,
    }
}

#[test]
fn a_review_says_only_this_space_changes_and_explains_a_public_listing() {
    // A removal never leaves, deletes or touches another Space.
    let removal = review_lines_for(false, &[]);
    assert!(
        removal.0.starts_with("Remove “Not es” from “Team”"),
        "{}",
        removal.0
    );
    let lines = removal.1.join(" ");
    assert!(lines.contains("You stay in “Not es”") && lines.contains("nothing is deleted"));
    assert!(lines.contains("any other Space that lists it keeps it"));
    assert_eq!(removal.2, "Remove from Space");
    // Adding a private room to a public Space says what becomes visible,
    // and what does not.
    let addition = review_lines_for(true, &[LinkWarning::PublicSpaceListsPrivateRoom]);
    let lines = addition.1.join(" ");
    assert!(lines.contains("“Team” is public"));
    assert!(lines.contains("does not let them read or join"));
    assert!(lines.contains("Only this Space changes"));
    let unsure = review_lines_for(true, &[LinkWarning::MayContainThisSpace]);
    assert!(unsure.1.join(" ").contains("could not check far enough"));
    // Names are plain text.
    assert!(!removal.0.contains('\u{202e}'));
}

fn review_lines_for(add: bool, warnings: &[LinkWarning]) -> (String, Vec<String>, &'static str) {
    review_text(add, warnings, "Not\u{202e}es", "Team")
}

#[test]
fn a_write_without_a_reply_reads_as_possibly_made_and_reconciles() {
    let (notice, error, then) = written(&Ok(true), true, "Notes", "Team");
    assert_eq!(notice, "Added “Notes” to “Team”.");
    assert!(error.is_empty() && then == Then::Read);
    let (notice, _, _) = written(&Ok(false), false, "Notes", "Team");
    assert!(notice.starts_with("Removed “Notes” from “Team”."));
    assert!(notice.contains("read back"));
    // Unknown: said as such, and as being read, never as read already; the
    // one link is then read fresh (Then::Reconcile), not the synced view.
    let (notice, error, then) = written(
        &Err(SpaceManageError::OutcomeUnknown),
        true,
        "Notes",
        "Team",
    );
    assert!(notice.contains("may or may not") && error.is_empty());
    assert!(notice.contains("is reading this link again"), "{notice}");
    assert!(!notice.contains("was read") && !notice.contains("read again just now"));
    assert_eq!(then, Then::Reconcile);
    // A definite refusal, a changed baseline or changed access changed
    // nothing.
    for refusal in [
        SpaceManageError::NotAllowed,
        SpaceManageError::BaselineChanged,
        SpaceManageError::ConditionsChanged,
    ] {
        let (notice, error, then) = written(&Err(refusal), true, "Notes", "Team");
        assert!(notice.is_empty() && then == Then::Stay);
        assert_eq!(error, refusal.text());
    }
}

#[test]
fn a_fresh_read_after_no_reply_says_what_it_found_or_that_it_failed() {
    assert_eq!(
        reconciled(&Ok(true), "Notes", "Team"),
        "Read again just now: “Team” lists “Notes”."
    );
    assert_eq!(
        reconciled(&Ok(false), "Notes", "Team"),
        "Read again just now: “Team” does not list “Notes”."
    );
    let failed = reconciled(&Err(SpaceManageError::Unavailable), "Notes", "Team");
    assert!(failed.contains("could not be read again") && failed.contains("may or may not"));
    assert!(!failed.contains("just now"));
}

#[test]
fn an_unknown_link_is_never_shown_or_counted_as_absent() {
    let rooms = [
        candidate("!a:example.org", "Design", Some(true), false),
        candidate("!b:example.org", "Ops", None, false),
    ];
    let all = visible(&rooms, "");
    assert_eq!(all[1].2, "Not known yet whether this Space lists it");
    assert_eq!(all[1].3, None);
    // Its row asks for a review that decides from a fresh read.
    assert_eq!(intent(REVIEW_CHECK), Some(LinkIntent::Check));
    assert_eq!(intent(REVIEW_ADD), Some(LinkIntent::Add));
    assert_eq!(intent(REVIEW_REMOVE), Some(LinkIntent::Remove));
    assert_eq!(intent(7), None);
    let mut view = SpaceRoomsView {
        space_id: "!s:example.org".into(),
        space_name: "Team".into(),
        space_public: false,
        can_manage: true,
        candidates: rooms.to_vec(),
        truncated: false,
        links_complete: false,
        other_children: 0,
    };
    let text = summary(&view);
    assert!(text.contains("1 in this Space"), "{text}");
    assert!(text.contains("could not be read"), "{text}");
    // A write without a reply leaves its row not known, whatever the synced
    // view read afterwards says.
    view.links_complete = true;
    let mut marks = Marks::default();
    save_mark(&mut marks, "alice", &view.space_id, "!a:example.org", None);
    overlay(&mut view, &marks, "alice");
    assert_eq!(view.candidates[0].linked, None);
    assert!(summary(&view).contains("0 in this Space"));
    // Only a fresh read replaces it.
    assert_eq!(
        marks.state("alice", &view.space_id, "!a:example.org"),
        Some(None)
    );
    // A review's fresh read settles a row; other failures do not.
    assert_eq!(
        reviewed_link(&Err(SpaceManageError::AlreadyLinked)),
        Some(true)
    );
    assert_eq!(
        reviewed_link(&Err(SpaceManageError::NotLinked)),
        Some(false)
    );
    assert_eq!(reviewed_link(&Err(SpaceManageError::Unavailable)), None);
}

#[test]
fn what_morpheus_learned_outlives_the_panel_and_is_not_retired_by_synced_cache() {
    let synced = |linked: Option<bool>| SpaceRoomsView {
        space_id: "!s:example.org".into(),
        space_name: "Team".into(),
        space_public: false,
        can_manage: true,
        candidates: vec![candidate("!a:example.org", "Design", linked, false)],
        truncated: false,
        links_complete: true,
        other_children: 0,
    };
    let mut marks = Marks::default();
    // An accepted Add, then the panel is closed (the session goes, the
    // marks stay) and reopened while synced state still predates the write:
    // the row shows the accepted state, never "Not in this Space".
    save_mark(
        &mut marks,
        "alice",
        "!s:example.org",
        "!a:example.org",
        Some(true),
    );
    let mut view = synced(Some(false));
    overlay(&mut view, &marks, "alice");
    assert_eq!(view.candidates[0].linked, Some(true));
    assert_eq!(visible(&view.candidates, "")[0].2, "In this Space");
    // Opening reads it fresh again. Mere cached agreement never retires an
    // authoritative receipt: the following cache may still predate it.
    assert_eq!(
        pending_rooms(&mut marks, "alice", "!s:example.org"),
        ["!a:example.org"]
    );
    let mut view = synced(Some(true));
    overlay(&mut view, &marks, "alice");
    assert_eq!(view.candidates[0].linked, Some(true));
    assert_eq!(
        pending_rooms(&mut marks, "alice", "!s:example.org"),
        ["!a:example.org"]
    );
    // A write without a reply stays unknown across reopens, whatever synced
    // state shows, until a fresh read replaces it.
    save_mark(
        &mut marks,
        "alice",
        "!s:example.org",
        "!a:example.org",
        None,
    );
    for linked in [Some(true), Some(false), None] {
        let mut view = synced(linked);
        overlay(&mut view, &marks, "alice");
        assert_eq!(view.candidates[0].linked, None);
    }
    save_mark(
        &mut marks,
        "alice",
        "!s:example.org",
        "!a:example.org",
        Some(false),
    );
    let mut view = synced(Some(true));
    overlay(&mut view, &marks, "alice");
    assert_eq!(view.candidates[0].linked, Some(false));
    // Marks belong to one account and Space.
    let mut view = synced(Some(true));
    overlay(&mut view, &marks, "bob");
    assert_eq!(view.candidates[0].linked, Some(true));
    // Fresh reads are bounded and take unknown links first.
    let mut marks = Marks::default();
    for index in 0..40 {
        let linked = (index % 2 == 0).then_some(true);
        save_mark(
            &mut marks,
            "alice",
            "!s:example.org",
            &format!("!r{index:02}:example.org"),
            linked,
        );
    }
    let pending = pending_rooms(&mut marks, "alice", "!s:example.org");
    assert_eq!(pending.len(), MAX_FRESH_READS);
    assert_eq!(pending[0], "!r01:example.org");
    assert_eq!(marks.0.len(), 40);
}

#[test]
fn an_accounts_marks_end_with_the_account_never_with_a_switch() {
    let mut marks = Marks::default();
    save_mark(
        &mut marks,
        "alice",
        "!s:example.org",
        "!a:example.org",
        None,
    );
    save_mark(
        &mut marks,
        "alice",
        "!t:example.org",
        "!b:example.org",
        Some(true),
    );
    save_mark(
        &mut marks,
        "bob",
        "!s:example.org",
        "!a:example.org",
        Some(false),
    );
    // Keyed by account: switching to Bob and back finds Alice's marks.
    assert_eq!(
        pending_rooms(&mut marks, "alice", "!s:example.org"),
        ["!a:example.org"]
    );
    // Alice logged out or removed: only her marks go.
    marks.forget_account("alice");
    assert!(pending_rooms(&mut marks, "alice", "!s:example.org").is_empty());
    assert!(pending_rooms(&mut marks, "alice", "!t:example.org").is_empty());
    assert_eq!(
        pending_rooms(&mut marks, "bob", "!s:example.org"),
        ["!a:example.org"]
    );
    // The process-wide marks the account paths drop, at once and from any
    // thread (removal and logout run on Tokio; account_detached forgets all).
    let (alice, bob) = ("forget-test-alice", "forget-test-bob");
    save_mark(
        &mut super::marks(),
        alice,
        "!s:example.org",
        "!a:example.org",
        None,
    );
    save_mark(
        &mut super::marks(),
        bob,
        "!s:example.org",
        "!a:example.org",
        None,
    );
    std::thread::spawn(move || forget_account(alice))
        .join()
        .unwrap();
    assert!(pending_rooms(&mut super::marks(), alice, "!s:example.org").is_empty());
    assert_eq!(
        pending_rooms(&mut super::marks(), bob, "!s:example.org"),
        ["!a:example.org"]
    );
    forget_all();
    assert!(pending_rooms(&mut super::marks(), bob, "!s:example.org").is_empty());
}

#[test]
fn receipt_saturation_refuses_new_mutations_without_eviction_or_empty_scopes() {
    let mut marks = Marks::default();
    for account in 0..4 {
        for space in 0..2 {
            for room in 0..MAX_SPACE_MARKS {
                save_mark(
                    &mut marks,
                    &format!("account-{account}"),
                    &format!("!s{space}:example.org"),
                    &format!("!r{room}:example.org"),
                    None,
                );
            }
        }
    }
    assert_eq!(marks.0.len(), MAX_MARKS);
    assert!(
        marks
            .reserve("new-account", "!new:example.org", "!new:example.org")
            .is_none()
    );
    assert!(
        marks
            .reserve("account-0", "!new:example.org", "!new:example.org")
            .is_none()
    );
    assert!(
        marks
            .reserve("account-0", "!s0:example.org", "!new:example.org")
            .is_none()
    );
    for index in 0..100 {
        assert!(
            marks
                .pending("unseen", &format!("!s{index}:example.org"))
                .is_empty()
        );
    }
    assert_eq!(marks.0.len(), MAX_MARKS, "reads allocate no empty scopes");
    assert!(marks.0.values().all(|mark| mark.linked.is_none()));
    // Existing keys remain usable at capacity and known receipts are not
    // discarded merely to make room for a new one.
    save_mark(
        &mut marks,
        "account-0",
        "!s0:example.org",
        "!r0:example.org",
        Some(true),
    );
    assert!(
        marks
            .reserve("new-account", "!new:example.org", "!new:example.org")
            .is_none()
    );
    assert_eq!(
        marks.state("account-0", "!s0:example.org", "!r0:example.org"),
        Some(Some(true))
    );
    assert!(
        marks
            .reserve(
                "account-0",
                "!s0:example.org",
                &"x".repeat(MAX_SCOPE_ID_BYTES + 1)
            )
            .is_none()
    );
}

#[test]
fn stale_reads_and_forgotten_write_owners_cannot_replace_newer_receipts() {
    let mut marks = Marks::default();
    let (account, space, room) = ("alice", "!s:example.org", "!r:example.org");
    save_mark(&mut marks, account, space, room, None);
    let old_read = marks.pending(account, space).pop().unwrap();
    let writing = marks.reserve(account, space, room).unwrap();
    assert!(
        marks.reserve(account, space, room).is_none(),
        "one write per key"
    );
    marks.fresh(&old_read, false);
    assert_eq!(marks.state(account, space, room), Some(None));
    marks.finish(&writing, true, &Ok(true));
    marks.fresh(&old_read, false);
    assert_eq!(marks.state(account, space, room), Some(Some(true)));
    let older = marks.pending(account, space).pop().unwrap();
    let latest = marks.pending(account, space).pop().unwrap();
    marks.fresh(&latest, false);
    marks.fresh(&older, true);
    assert_eq!(marks.state(account, space, room), Some(Some(false)));

    let retired = marks.reserve(account, space, room).unwrap();
    marks.forget_account(account);
    marks.finish(&retired, true, &Ok(true));
    assert!(
        marks.0.is_empty(),
        "late work cannot reinsert a removed account"
    );
    // Even after relogin/reset, process-unique receipt IDs cannot be reused.
    let mut marks = Marks::default();
    let replacement = marks.reserve(account, space, room).unwrap();
    assert_ne!(replacement.read.revision, retired.read.revision);
    marks.finish(&retired, true, &Ok(true));
    assert!(marks.0[&replacement.read.key].writing);
    assert_eq!(marks.state(account, space, room), Some(None));
}

#[test]
fn closing_during_the_production_write_wait_keeps_unknown_and_known_outcomes() {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            for result in [Err(SpaceManageError::OutcomeUnknown), Ok(true)] {
                let marks = Arc::new(Mutex::new(Marks::default()));
                save_mark(
                    &mut marks.lock().unwrap(),
                    "alice",
                    "!s:example.org",
                    "!r:example.org",
                    Some(false),
                );
                let earlier_read = marks
                    .lock()
                    .unwrap()
                    .pending("alice", "!s:example.org")
                    .pop()
                    .unwrap();
                let receipt = marks
                    .lock()
                    .unwrap()
                    .reserve("alice", "!s:example.org", "!r:example.org")
                    .unwrap();
                let operation = next_operation();
                let (send, receive) = tokio::sync::oneshot::channel();
                let work = settle_write(async { receive.await.unwrap() }, |result| {
                    marks.lock().unwrap().finish(&receipt, true, result);
                });
                let close = async {
                    // The same session-clear step used by actual panel close.
                    clear_session();
                    assert_ne!(current_operation(), operation);
                    {
                        let mut store = marks.lock().unwrap();
                        assert!(store.pending("alice", "!s:example.org").is_empty());
                        assert!(store.begin_read(&receipt.read.key).is_none());
                        // A GET admitted before the write cannot retire its
                        // ownership while that server reply is still held.
                        store.fresh(&earlier_read, false);
                        assert!(store.0[&receipt.read.key].writing);
                        assert_eq!(
                            store.state("alice", "!s:example.org", "!r:example.org"),
                            Some(None)
                        );
                        assert!(
                            store
                                .reserve("alice", "!s:example.org", "!r:example.org")
                                .is_none()
                        );
                        let mut reopened = SpaceRoomsView {
                            space_id: "!s:example.org".into(),
                            space_name: "Team".into(),
                            space_public: false,
                            can_manage: true,
                            candidates: vec![candidate(
                                "!r:example.org",
                                "Room",
                                Some(false),
                                false,
                            )],
                            truncated: false,
                            links_complete: true,
                            other_children: 0,
                        };
                        overlay(&mut reopened, &store, "alice");
                        assert_eq!(reopened.candidates[0].linked, None);
                    }
                    send.send(result).unwrap();
                };
                let (actual, ()) = tokio::join!(work, close);
                assert_eq!(actual, result);
                assert_ne!(
                    current_operation(),
                    operation,
                    "the old UI completion is retired"
                );
                let expected = if result.is_ok() { Some(true) } else { None };
                assert_eq!(
                    marks
                        .lock()
                        .unwrap()
                        .state("alice", "!s:example.org", "!r:example.org"),
                    Some(expected)
                );
            }
        });
}

#[test]
fn the_search_shows_bounded_rows_with_their_link_state() {
    let rooms = [
        candidate("!a:example.org", "Design", Some(true), false),
        candidate("!b:example.org", "Ops", Some(false), true),
    ];
    let all = visible(&rooms, "");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].2, "In this Space");
    assert_eq!(all[1].2, "Not in this Space · A Space");
    let found = visible(&rooms, "  DES\u{7}");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "!a:example.org");
    assert_eq!(visible(&rooms, "!b:").len(), 1);
}

#[test]
fn form_clear_and_replacement_cancel_only_the_owned_space_write() {
    let first_operation = next_operation();
    let first = own_write(first_operation);
    assert!(!first.is_cancelled());
    let replacement_operation = next_operation();
    assert!(first.is_cancelled());
    let replacement = own_write(replacement_operation);
    finish_owner(first_operation);
    assert!(
        !replacement.is_cancelled(),
        "an old completion cannot revoke the new form"
    );
    // This exact step is called by native Close/Escape and context hooks.
    clear_session();
    assert!(replacement.is_cancelled());
    // A cancellation before dispatch restores prior knowledge; an
    // after-start Unknown keeps the already reserved uncertainty.
    for (result, expected) in [
        (Err(SpaceManageError::Cancelled), Some(false)),
        (Err(SpaceManageError::OutcomeUnknown), None),
    ] {
        let mut store = Marks::default();
        save_mark(
            &mut store,
            "a",
            "!s:example.org",
            "!r:example.org",
            Some(false),
        );
        let receipt = store
            .reserve("a", "!s:example.org", "!r:example.org")
            .unwrap();
        store.finish(&receipt, true, &result);
        assert_eq!(
            store.state("a", "!s:example.org", "!r:example.org"),
            Some(expected)
        );
    }
}
