//! Regression source for the room-address panel's decisions: which change
//! each action asks for, what its review says before Confirm (the exact
//! address and room, no lock or atomicity claimed, separate outcomes), what
//! a finished change leaves, and how addresses are shown by owner.

use super::*;

fn address(alias: &str, local: bool, listed: bool, main: bool, alternative: bool) -> RoomAddress {
    RoomAddress {
        alias: alias.into(),
        server: alias.rsplit(':').next().unwrap_or("").into(),
        local,
        listed,
        main,
        alternative,
    }
}

fn snapshot(addresses: Vec<RoomAddress>) -> RoomAddressesSnapshot {
    RoomAddressesSnapshot {
        addresses,
        truncated: false,
        local_list_known: true,
        own_server: "example.org".into(),
        can_publish: true,
        publish_editable: true,
    }
}

#[test]
fn each_action_asks_for_its_own_change_on_the_exact_address() {
    let alias = "#lobby:example.org";
    assert_eq!(
        change_for(alias, ACTION_MAKE_MAIN),
        Some(Change::Publish(PublishEdit::SetMain(alias.into())))
    );
    assert_eq!(
        change_for(alias, ACTION_PUBLISH),
        Some(Change::Publish(PublishEdit::AddAlternative(alias.into())))
    );
    assert_eq!(
        change_for(alias, ACTION_UNPUBLISH),
        Some(Change::Publish(PublishEdit::RemoveAlternative(
            alias.into()
        )))
    );
    assert_eq!(
        change_for(alias, ACTION_UNPUBLISH_MAIN),
        Some(Change::Publish(PublishEdit::ClearMain))
    );
    assert_eq!(
        change_for(alias, ACTION_REMOVE),
        Some(Change::Remove(alias.into()))
    );
    assert_eq!(change_for(alias, 9), None);
}

#[test]
fn a_review_names_the_address_and_room_and_promises_no_lock_or_atomicity() {
    let (title, lines, confirm) = review_text(
        &Change::Remove("#lobby:example.org".into()),
        "Team\u{202e}",
        None,
    );
    assert!(title.contains("#lobby:example.org"), "{title}");
    let lines = lines.join(" ");
    assert!(lines.contains("still points to “Team”"), "{lines}");
    assert!(lines.contains("not a lock"), "{lines}");
    assert!(
        lines.contains("stays listed as published until you unpublish it"),
        "{lines}"
    );
    assert!(lines.contains("messages stay"), "{lines}");
    assert_eq!(confirm, "Remove address");
    assert!(!title.contains('\u{202e}'));
    // Adding does not publish.
    let (_, lines, _) = review_text(&Change::Add("#new:example.org".into()), "Team", None);
    assert!(lines.join(" ").contains("not published yet"));
    // Publishing checks the address points here and keeps other fields.
    let edit = Change::Publish(PublishEdit::SetMain("#new:other.org".into()));
    let (title, lines, _) = review_text(&edit, "Team", Some("#old:example.org"));
    assert!(title.contains("main address"));
    let lines = lines.join(" ");
    assert!(
        lines.contains("previous main address stays published"),
        "{lines}"
    );
    assert!(lines.contains("points to this room") && lines.contains("kept as they are"));
    let clear = Change::Publish(PublishEdit::ClearMain);
    let (title, lines, _) = review_text(&clear, "Team", Some("#old:example.org"));
    assert!(title.contains("#old:example.org"), "{title}");
    assert!(lines.join(" ").contains("still leads here"));
}

#[test]
fn a_change_without_a_definite_answer_stays_unknown_until_read_again() {
    let change = Change::Add("#new:example.org".into());
    let done = outcome_text(&Ok(snapshot(Vec::new())), &change);
    assert_eq!(
        done,
        ("Added #new:example.org.".to_owned(), String::new(), false)
    );
    let (notice, error, unknown) = outcome_text(&Err(RoomAddressError::Unknown), &change);
    assert!(notice.is_empty() && unknown);
    assert!(error.contains("may or may not"), "{error}");
    // A definite refusal changed nothing and is not unknown.
    for refused in [
        RoomAddressError::AliasTaken,
        RoomAddressError::MappingChanged,
        RoomAddressError::PermissionDenied,
        RoomAddressError::NotLocal,
    ] {
        let (_, error, unknown) = outcome_text(&Err(refused), &change);
        assert!(!unknown);
        assert_eq!(error, refused.to_string());
    }
    let (_, _, unknown) = outcome_text(&Err(RoomAddressError::SavedNotRead), &change);
    assert!(unknown, "saved but not read back is read again too");
}

#[test]
fn addresses_show_who_owns_them_and_remote_ones_are_never_changed_here() {
    let rows = [
        address("#main:other.org", false, false, true, false),
        address("#alt:example.org", true, true, false, true),
        address("#quiet:example.org", true, true, false, false),
    ];
    let shown = visible(&rows, "");
    assert_eq!(shown.len(), 3);
    assert_eq!(shown[0].1, "Main address · On other.org; not changed here");
    assert_eq!(shown[1].1, "Published · On your homeserver");
    assert_eq!(shown[2].1, "Not published · On your homeserver");
    assert_eq!(visible(&rows, " ALT\u{7} ").len(), 1);
    let mut partial = snapshot(rows.to_vec());
    partial.local_list_known = false;
    partial.can_publish = false;
    let text = summary(&partial);
    assert!(
        text.contains("3 addresses") && text.contains("example.org"),
        "{text}"
    );
    assert!(text.contains("only published ones are shown"), "{text}");
    assert!(
        text.contains("cannot change the published addresses"),
        "{text}"
    );
}

#[test]
fn retained_receipts_never_settle_unknown_from_reads_or_resurrect_after_removal() {
    let mut store = Receipts::default();
    let unknown = store.reserve("alice", "!room:example.org").unwrap();
    store.finish(&unknown, &Err(RoomAddressError::Unknown));
    for _ in 0..3 {
        store.observe(&unknown);
    }
    assert_eq!(
        store.pending("alice", "!room:example.org"),
        Some(Pending::Unknown)
    );
    assert!(store.reserve("alice", "!room:example.org").is_err());
    assert!(store.pending("bob", "!room:example.org").is_none());
    store.forget("alice");
    let replacement = store.reserve("alice", "!room:example.org").unwrap();
    assert_ne!(replacement.id, unknown.id);
    store.finish(&unknown, &Ok(snapshot(Vec::new())));
    store.observe(&unknown);
    assert_eq!(
        store.pending("alice", "!room:example.org"),
        Some(Pending::Writing)
    );
    store.forget("alice");
    store.finish(&replacement, &Err(RoomAddressError::Unknown));
    assert!(store.0.is_empty());
}

#[test]
fn accepted_receipt_needs_later_fresh_read_and_saturation_never_evicts() {
    let mut store = Receipts::default();
    let accepted = store.reserve("alice", "!accepted:example.org").unwrap();
    store.observe(&accepted); // even a fresh read cannot release in-flight work
    assert_eq!(
        store.pending("alice", "!accepted:example.org"),
        Some(Pending::Writing)
    );
    let early_read = store.ticket("alice", "!accepted:example.org").unwrap();
    store.finish(&accepted, &Err(RoomAddressError::SavedNotRead));
    assert!(store.reserve("alice", "!accepted:example.org").is_err());
    store.observe(&early_read); // GET began before the successful write reply
    assert_eq!(
        store.pending("alice", "!accepted:example.org"),
        Some(Pending::Accepted)
    );
    let later_read = store.ticket("alice", "!accepted:example.org").unwrap();
    store.observe(&later_read);
    assert!(store.pending("alice", "!accepted:example.org").is_none());
    for n in 0..MAX_ACCOUNT_RECEIPTS {
        let ticket = store
            .reserve("alice", &format!("!r{n}:example.org"))
            .unwrap();
        store.finish(&ticket, &Err(RoomAddressError::Unknown));
    }
    assert!(store.reserve("alice", "!full:example.org").is_err());
    assert_eq!(store.0.len(), MAX_ACCOUNT_RECEIPTS);
    assert!(store.0.values().all(|r| r.pending == Pending::Unknown));
    let other = store.reserve("bob", "!full:example.org").unwrap();
    store.finish(&other, &Err(RoomAddressError::PermissionDenied));
    assert!(store.pending("bob", "!full:example.org").is_none());
}

#[test]
fn dropping_the_actual_completion_owner_keeps_the_existing_receipt_unknown() {
    let ticket = receipts()
        .reserve("completion-fixture", "!r:example.org")
        .unwrap();
    let id = ticket.id;
    drop(Completion(Some(ticket)));
    assert_eq!(
        receipts().pending("completion-fixture", "!r:example.org"),
        Some(Pending::Unknown)
    );
    assert_eq!(
        receipts()
            .ticket("completion-fixture", "!r:example.org")
            .unwrap()
            .id,
        id
    );
    forget_account("completion-fixture");
}
