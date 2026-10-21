use super::*;

const ME: &str = "@casey:example.org";
const DANA: &str = "@dana:example.org";
const PEER: &str = "@peer:example.org";

fn person(id: &str, name: &str, level: i64, editable: bool) -> PermissionUser {
    PermissionUser {
        user_id: id.to_owned(),
        display_name: name.to_owned(),
        level: PermissionLevel::Level(level),
        editable,
        own: id == ME,
    }
}

fn setting(key: PermissionKey, level: i64, explicit: bool) -> PermissionThreshold {
    let class = match &key {
        PermissionKey::Event(kind) => morpheus_matrix::event_class(kind),
        _ => EventClass::NotEvent,
    };
    PermissionThreshold {
        key,
        level,
        explicit,
        class,
        editable: level <= 100,
    }
}

fn event(kind: &str) -> PermissionKey {
    PermissionKey::Event(kind.to_owned())
}

fn snapshot() -> RoomPermissionsSnapshot {
    RoomPermissionsSnapshot {
        room_id: "!perms:example.org".to_owned(),
        room_version: "10".to_owned(),
        own_user: ME.to_owned(),
        own_level: PermissionLevel::Level(100),
        can_change: true,
        change_level: 50,
        users_default: 0,
        own_explicit: true,
        exact_users: [(ME, 100), (PEER, 100), (DANA, 50)]
            .into_iter()
            .map(|(id, level)| (id.to_owned(), level))
            .collect(),
        has_complete_state: true,
        thresholds: vec![
            setting(PermissionKey::UsersDefault, 0, true),
            setting(PermissionKey::EventsDefault, 0, false),
            setting(PermissionKey::StateDefault, 50, true),
            setting(PermissionKey::Ban, 50, true),
            setting(PermissionKey::NotifyRoom, 50, false),
            setting(event("m.room.name"), 50, true),
            setting(event("org.example.thing"), 120, true),
        ],
        users: vec![
            person(ME, "Casey", 100, true),
            person(PEER, "Pat", 100, false),
            person(DANA, "Dana", 50, true),
        ],
        omitted_users: 0,
        omitted_events: 0,
        read_only: None,
    }
}

fn change(target: PermissionTarget, level: Option<i64>) -> PermissionChange {
    PermissionChange { target, level }
}

fn user(id: &str) -> PermissionTarget {
    PermissionTarget::User(id.to_owned())
}

fn key(key: PermissionKey) -> PermissionTarget {
    PermissionTarget::Threshold(key)
}

#[test]
fn a_typed_level_is_a_whole_number_in_range_or_the_default_where_allowed() {
    assert_eq!(parse_level(" 50 ", false), Ok(Some(50)));
    assert_eq!(parse_level("-5", false), Ok(Some(-5)));
    assert_eq!(parse_level("", true), Ok(None));
    let not_removable = PermissionProblem::NotRemovable.text();
    assert_eq!(parse_level("", false), Err(not_removable));
    assert_eq!(parse_level("fifty", false), Err(NOT_A_NUMBER));
    assert_eq!(parse_level("1.5", false), Err(NOT_A_NUMBER));
    let huge = parse_level("9007199254740992", false);
    assert_eq!(huge, Err(PermissionProblem::OutOfRange.text()));
}

#[test]
fn rows_round_trip_to_their_targets() {
    let targets = [
        user(DANA),
        key(PermissionKey::Ban),
        key(PermissionKey::NotifyRoom),
        key(event("m.room.name")),
    ];
    for target in targets {
        let id = row_id(&target);
        assert_eq!(super::target(&id), Some(target));
    }
    assert_eq!(super::target("x:anything"), None);
    assert_eq!(super::target("k:unknown"), None);
}

#[test]
fn staging_replaces_a_rows_change_and_drops_a_change_to_what_is_already_there() {
    let snapshot = snapshot();
    let mut staged = Vec::new();
    let dana = row_id(&user(DANA));
    for level in [60, 70] {
        let staging = change(user(DANA), Some(level));
        stage_change(&mut staged, &snapshot, &dana, staging).unwrap();
    }
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].change.level, Some(70));
    // Back to Dana's current level: no change at all.
    let back = change(user(DANA), Some(50));
    stage_change(&mut staged, &snapshot, &dana, back).unwrap();
    assert!(staged.is_empty());
    // The bound on one save.
    for index in 0..MAX_PERMISSION_CHANGES {
        let id = format!("@u{index}:example.org");
        let row = row_id(&user(&id));
        let staging = change(user(&id), Some(1));
        stage_change(&mut staged, &snapshot, &row, staging).unwrap();
    }
    let more = change(user("@more:example.org"), Some(1));
    let refused = stage_change(&mut staged, &snapshot, "u:@more:example.org", more);
    assert_eq!(refused, Err(PermissionProblem::TooManyChanges));
}

#[test]
fn people_rows_say_who_you_are_why_a_row_is_locked_and_offer_a_typed_id() {
    let snapshot = snapshot();
    let staged = vec![Staged {
        row: row_id(&user(DANA)),
        change: change(user(DANA), Some(75)),
    }];
    let shown = rows(&snapshot, &staged, "", 0);
    let titles: Vec<_> = shown.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, ["Casey (you)", "Pat", "Dana"]);
    assert_eq!(shown[0].level.as_str(), "Administrator · 100");
    assert_eq!(shown[2].level.as_str(), "Moderator · 50");
    assert_eq!(shown[2].change.as_str(), "→ 75");
    assert!(!shown[1].editable);
    let below = PermissionProblem::NotBelowYou.text();
    assert_eq!(shown[1].reason.as_str(), below);
    // Search covers the loaded names and IDs only.
    assert_eq!(rows(&snapshot, &staged, "dan", 0).len(), 1);
    // A full ID that is not listed becomes a row to give them a level.
    let typed = rows(&snapshot, &staged, "@new:example.org", 0);
    assert_eq!(typed.len(), 1);
    assert_eq!(typed[0].id.as_str(), "u:@new:example.org");
    assert!(typed[0].editable && !typed[0].removable);
    assert_eq!(typed[0].level.as_str(), "Member · 0");
    assert!(rows(&snapshot, &staged, "@new", 0).is_empty());
}

#[test]
fn permission_rows_label_defaults_and_custom_levels_honestly() {
    let snapshot = snapshot();
    let shown = rows(&snapshot, &[], "", 1);
    let find = |title: &str| {
        let found = shown.iter().find(|row| row.title.as_str() == title);
        found.unwrap().clone()
    };
    let ban = find("Ban people");
    assert_eq!(ban.level.as_str(), "Moderators and above · 50");
    assert!(!ban.removable, "a top-level permission is not removed");
    let messages = find("Send messages");
    assert_eq!(messages.level.as_str(), "Everyone · 0 (default)");
    assert_eq!(find("Default level of members").level.as_str(), "Level 0");
    let custom = find("Send org.example.thing events");
    assert_eq!(custom.level.as_str(), "Level 120 and above (custom)");
    assert!(!custom.editable);
    let needs_more = PermissionProblem::NeedsMoreThanYou.text();
    assert_eq!(custom.reason.as_str(), needs_more);
    // Event entries and @room can go back to their default.
    assert!(find("Change the room name").removable);
    assert!(find("Notify the whole room (@room)").removable);
    assert_eq!(rows(&snapshot, &[], "room name", 1).len(), 1);
}

#[test]
fn a_read_only_or_view_only_room_says_so_in_its_summary() {
    let mut snapshot = snapshot();
    let text = summary(&snapshot);
    let expected = concat!(
        "Your level: Administrator · 100. ",
        "Changing roles and permissions needs level 50.",
    );
    assert_eq!(text, expected);
    snapshot.can_change = false;
    assert!(summary(&snapshot).contains(VIEW_ONLY));
    let unsupported = PermissionsReadOnly::Unsupported;
    snapshot.read_only = Some(unsupported);
    snapshot.omitted_users = 3;
    snapshot.omitted_events = 2;
    let text = summary(&snapshot);
    let reason = PermissionProblem::ReadOnly(unsupported).text();
    assert!(text.contains(reason));
    let people = "3 more people with their own level are not listed.";
    assert!(text.contains(people));
    let events = "2 more event permissions are not listed.";
    assert!(text.contains(events));
}

#[test]
fn the_review_shows_before_and_after_and_warns_about_your_own_level() {
    let snapshot = snapshot();
    let edit = PermissionsEdit {
        changes: vec![
            change(user(ME), Some(50)),
            change(user(DANA), Some(75)),
            change(key(PermissionKey::UsersDefault), Some(10)),
            change(key(event("m.room.name")), None),
        ],
        own_demotion_confirmed: true,
    };
    let review = review_permissions(&snapshot, &edit).unwrap();
    let lines = review_lines(&snapshot, &review);
    let own = "Casey (@casey:example.org): Administrator · 100 → Moderator · 50";
    assert_eq!(lines[0], own);
    let dana = "Dana (@dana:example.org): Moderator · 50 → Custom · 75";
    assert_eq!(lines[1], dana);
    assert_eq!(lines[2], "Default level of members: Level 0 → Level 10");
    let name = concat!(
        "Change the room name: Moderators and above · 50 → ",
        "the default, Moderators and above · 50",
    );
    assert_eq!(lines[3], name);
    let warning = review_warning(&snapshot, &review);
    assert!(warning.contains("lowering your own level from 100 to 50"));
    assert!(warning.contains("only someone with a higher level can"));
    let everyone = "Everyone without a level of their own changes from 0 to 10.";
    assert!(warning.contains(everyone));
    // Pat at 100 can still change permissions.
    assert!(!warning.contains("no one listed here"));
}

#[test]
fn a_failed_save_says_what_may_have_happened_and_whether_to_refresh() {
    use RoomSettingsError as Error;
    let settings = PermissionsError::Settings;
    let (text, refresh, applied) = save_failure(settings(Error::Unknown));
    assert!(text.contains("may or may not have changed"));
    assert!(text.contains("not sent again"));
    assert!(refresh && !applied);
    let (text, refresh, applied) = save_failure(settings(Error::SavedNotRead));
    assert!(text.starts_with("Saved"));
    // An applied change does not stay staged.
    assert!(refresh && applied);
    let (text, refresh, _) = save_failure(settings(Error::BaselineChanged));
    assert!(text.contains("Nothing was sent") && refresh);
    let (text, refresh, _) = save_failure(settings(Error::SecurityBusy));
    assert!(text.contains("Nothing was sent") && !refresh);
    let problem = PermissionsError::Problem(PermissionProblem::NotBelowYou);
    let (text, refresh, applied) = save_failure(problem);
    assert_eq!(text, PermissionProblem::NotBelowYou.text());
    assert!(!refresh && !applied);
    let unread = error_text(settings(Error::InvalidState));
    assert!(unread.contains("Nothing was sent"));
}

#[test]
fn levels_read_as_roles_only_where_they_are_the_usual_ones() {
    let level = PermissionLevel::Level;
    let creator = user_level_text(PermissionLevel::Creator, 0);
    assert_eq!(creator, "Creator · unlimited");
    assert_eq!(user_level_text(level(100), 0), "Administrator · 100");
    assert_eq!(user_level_text(level(0), 0), "Member · 0");
    assert_eq!(user_level_text(level(42), 0), "Custom · 42");
    assert_eq!(threshold_text(0, 0), "Everyone · 0");
    assert_eq!(threshold_text(100, 0), "Administrators · 100");
    assert_eq!(threshold_text(42, 0), "Level 42 and above (custom)");
}

/// A typed or staged user is judged on the full stored levels: one past
/// the listed rows with a level of their own shows it, can be changed only
/// below your level, and a removal of an omitted entry is a real change.
#[test]
fn unlisted_users_use_the_exact_stored_levels() {
    let mut snapshot = snapshot();
    let hidden = "@hidden:example.org";
    snapshot.exact_users.insert(hidden.to_owned(), 40);
    snapshot
        .exact_users
        .insert("@boss:example.org".to_owned(), 100);
    snapshot.omitted_users = 2;
    let shown = rows(&snapshot, &[], hidden, 0);
    let row = shown
        .iter()
        .find(|row| row.id.as_str() == format!("u:{hidden}"))
        .unwrap();
    assert!(row.title.contains("Change") && row.detail.contains("level of their own"));
    assert!(row.level.contains("40") && row.editable && row.removable);
    let boss = rows(&snapshot, &[], "@boss:example.org", 0);
    let boss = boss
        .iter()
        .find(|row| row.id == "u:@boss:example.org")
        .unwrap();
    assert!(!boss.editable, "not below your level");
    let nobody = rows(&snapshot, &[], "@nobody:example.org", 0);
    let nobody = nobody
        .iter()
        .find(|row| row.id == "u:@nobody:example.org")
        .unwrap();
    assert!(nobody.detail.contains("default level applies") && !nobody.removable);
    // Removing the omitted entry is a change; removing a user without one is not.
    let mut staged = Vec::new();
    let removal = change(user(hidden), None);
    stage_change(&mut staged, &snapshot, &format!("u:{hidden}"), removal).unwrap();
    assert_eq!(staged.len(), 1);
    let none = change(user("@nobody:example.org"), None);
    stage_change(&mut staged, &snapshot, "u:@nobody:example.org", none).unwrap();
    assert_eq!(staged.len(), 1);
}

/// A custom event type's default depends on how it is sent: the staged
/// default and the review say so instead of naming one level.
#[test]
fn a_custom_event_default_is_conditional_in_rows_and_review() {
    let snapshot = snapshot();
    let custom = key(event("org.example.thing"));
    let staged = vec![Staged {
        row: "e:org.example.thing".to_owned(),
        change: change(custom.clone(), None),
    }];
    let shown = rows(&snapshot, &staged, "org.example", 1);
    let row = shown
        .iter()
        .find(|row| row.id == "e:org.example.thing")
        .unwrap();
    assert!(row.change.contains("as a message") && row.change.contains("as state"));
    let reviewed = ReviewedChange {
        target: custom,
        before: PermissionLevel::Level(120),
        before_explicit: true,
        after: 0,
        after_explicit: false,
        after_if_state: Some(50),
    };
    let (_, after) = change_texts(&reviewed, 0);
    assert!(after.contains("as a message") && after.contains("as state"));
}

/// One receipt per account and exact room, reserved before a save: a
/// second save of the room is refused while it is open, and the bound is
/// enforced where receipts are made (no 33rd).
#[test]
fn receipts_are_reserved_per_room_and_bounded_where_they_are_made() {
    let mut table = Receipts::default();
    let changes = vec![change(user(DANA), Some(75))];
    let first = table.reserve("a", "!r:x", changes.clone()).unwrap();
    assert_eq!(
        table.reserve("a", "!r:x", changes.clone()),
        Err(Refusal::Busy(ReceiptState::Pending))
    );
    assert!(
        table.reserve("b", "!r:x", changes.clone()).is_ok(),
        "per account"
    );
    for index in 2..MAX_RECEIPTS {
        table
            .reserve("a", &format!("!r{index}:x"), changes.clone())
            .unwrap();
    }
    assert_eq!(
        table.reserve("a", "!full:x", changes.clone()),
        Err(Refusal::Full)
    );
    // Unsent and refused saves release their room at once.
    table.complete(first, SaveOutcome::Unsent);
    assert_eq!(table.blocking("a", "!r:x"), None);
    let again = table.reserve("a", "!r:x", changes.clone()).unwrap();
    table.complete(again, SaveOutcome::Refused);
    assert_eq!(table.blocking("a", "!r:x"), None);
}

/// No answer: the room stays paused through any number of reads (also ones
/// showing the change, or the old state), view changes and the bound; only
/// removing the account forgets it, and a late completion then never makes
/// it again.
#[test]
fn an_unknown_save_stays_paused_and_is_never_evicted() {
    let mut table = Receipts::default();
    let staged = change(user(DANA), Some(75));
    let id = table.reserve("a", "!r:x", vec![staged.clone()]).unwrap();
    table.complete(id, SaveOutcome::Unknown);
    let mut shows_it = snapshot();
    shows_it.exact_users.insert(DANA.to_owned(), 75);
    for snapshot in [snapshot(), shows_it] {
        let started = table.read_started();
        assert!(!table.observe_read("a", "!r:x", started, &snapshot));
        assert_eq!(table.blocking("a", "!r:x"), Some(ReceiptState::Unknown));
    }
    // Pressure from other rooms never evicts it.
    for index in 1..MAX_RECEIPTS {
        table
            .reserve("a", &format!("!r{index}:x"), vec![staged.clone()])
            .unwrap();
    }
    assert_eq!(
        table.reserve("a", "!more:x", vec![staged.clone()]),
        Err(Refusal::Full)
    );
    assert_eq!(table.blocking("a", "!r:x"), Some(ReceiptState::Unknown));
    // A pending save whose task never answers stays pending (paused).
    assert_eq!(table.blocking("a", "!r1:x"), Some(ReceiptState::Pending));
    // Removing the account forgets it; a late completion does not return it.
    table.forget_account("a");
    assert_eq!(table.blocking("a", "!r:x"), None);
    table.complete(id, SaveOutcome::Unknown);
    assert_eq!(table.blocking("a", "!r:x"), None);
}

/// Accepted but not yet shown: settled only by a read started after the
/// answer that shows the exact effect; an earlier read, or one showing the
/// old state, keeps the room paused.
#[test]
fn an_accepted_save_settles_only_on_a_later_read_showing_it() {
    let mut table = Receipts::default();
    let staged = change(user(DANA), Some(75));
    let early = table.read_started();
    let id = table.reserve("a", "!r:x", vec![staged.clone()]).unwrap();
    table.complete(id, SaveOutcome::Accepted);
    let mut shows_it = snapshot();
    shows_it.exact_users.insert(DANA.to_owned(), 75);
    assert!(
        !table.observe_read("a", "!r:x", early, &shows_it),
        "started before the answer"
    );
    let late = table.read_started();
    assert!(
        !table.observe_read("a", "!r:x", late, &snapshot()),
        "shows the old state"
    );
    assert!(matches!(
        table.blocking("a", "!r:x"),
        Some(ReceiptState::Accepted { .. })
    ));
    let later = table.read_started();
    assert!(table.observe_read("a", "!r:x", later, &shows_it));
    assert_eq!(table.blocking("a", "!r:x"), None);
    // Accepted and shown by its own read-back: settled at once.
    let id = table.reserve("a", "!r:x", vec![staged]).unwrap();
    table.complete(id, SaveOutcome::AcceptedShown);
    assert_eq!(table.blocking("a", "!r:x"), None);
}

/// Only an answer proves anything: the outcome of each result.
#[test]
fn save_outcomes_keep_unsent_refused_accepted_and_unknown_apart() {
    let settings = |error| Err::<Saved<()>, _>(PermissionsError::Settings(error));
    assert_eq!(
        outcome_of(&settings(RoomSettingsError::Unknown)),
        SaveOutcome::Unknown
    );
    assert_eq!(
        outcome_of(&settings(RoomSettingsError::SavedNotRead)),
        SaveOutcome::Accepted
    );
    assert_eq!(
        outcome_of(&settings(RoomSettingsError::Refused)),
        SaveOutcome::Refused
    );
    assert_eq!(
        outcome_of(&settings(RoomSettingsError::BaselineChanged)),
        SaveOutcome::Unsent
    );
    let problem = Err::<Saved<()>, _>(PermissionsError::Problem(PermissionProblem::NoChanges));
    assert_eq!(outcome_of(&problem), SaveOutcome::Unsent);
    let shown = Ok(Saved {
        view: (),
        shown: vec![(user(DANA), true)],
    });
    assert_eq!(outcome_of(&shown), SaveOutcome::AcceptedShown);
    let hidden = Ok(Saved {
        view: (),
        shown: vec![(user(DANA), false)],
    });
    assert_eq!(outcome_of(&hidden), SaveOutcome::Accepted);
}

#[test]
fn only_complete_permission_facts_can_settle_accepted_removals() {
    let changes = vec![
        change(user(DANA), None),
        change(key(event("org.example.thing")), None),
        change(key(PermissionKey::NotifyRoom), None),
    ];
    let mut complete = snapshot();
    complete.exact_users.remove(DANA);
    complete
        .thresholds
        .retain(|row| row.key != event("org.example.thing"));
    // Display omissions do not hide exact user facts.
    complete.users.clear();
    complete.omitted_users = morpheus_matrix::MAX_PERMISSION_USERS;
    assert!(
        changes
            .iter()
            .all(|change| snapshot_shows(&complete, change))
    );
    let mut table = Receipts::default();
    let id = table.reserve("a", "!r:x", changes).unwrap();
    table.complete(id, SaveOutcome::Accepted);

    let mut no_proof = complete.clone();
    no_proof.has_complete_state = false;
    let mut unknown = no_proof.clone();
    unknown.room_version = "unknown".into();
    unknown.exact_users.clear();
    unknown.thresholds.clear();
    unknown.omitted_events = 0;
    unknown.read_only = Some(PermissionsReadOnly::UnknownVersion);
    let mut missing = no_proof.clone();
    missing.read_only = Some(PermissionsReadOnly::NoPowerLevels);
    let mut unsupported = complete.clone();
    unsupported.read_only = Some(PermissionsReadOnly::Unsupported);
    let mut omitted = complete.clone();
    omitted.omitted_events = 1;
    for incomplete in [no_proof, unknown, missing, unsupported, omitted] {
        let started = table.read_started();
        assert!(!table.observe_read("a", "!r:x", started, &incomplete));
        assert!(matches!(
            table.blocking("a", "!r:x"),
            Some(ReceiptState::Accepted { .. })
        ));
    }
    let started = table.read_started();
    assert!(table.observe_read("a", "!r:x", started, &complete));
    assert_eq!(table.blocking("a", "!r:x"), None);
}

#[test]
fn completed_account_cleanup_cannot_let_an_old_save_complete_a_new_one() {
    let mut table = Receipts::default();
    let changes = vec![change(user(DANA), Some(75))];
    let old = table.reserve("a", "!r:x", changes.clone()).unwrap();
    let other = table.reserve("b", "!r:x", changes.clone()).unwrap();
    table.complete(old, SaveOutcome::Unknown);
    table.forget_account("a");
    assert_eq!(table.blocking("a", "!r:x"), None);
    assert_eq!(table.blocking("b", "!r:x"), Some(ReceiptState::Pending));
    let new = table.reserve("a", "!r:x", changes).unwrap();
    assert_ne!(new, old);
    // A completed removal or successful logout may forget the old account;
    // no late old producer result can resurrect it or settle a new save.
    for outcome in [
        SaveOutcome::Unknown,
        SaveOutcome::Accepted,
        SaveOutcome::AcceptedShown,
    ] {
        table.complete(old, outcome);
        assert_eq!(table.blocking("a", "!r:x"), Some(ReceiptState::Pending));
    }
    table.complete(new, SaveOutcome::Refused);
    table.complete(other, SaveOutcome::Unsent);
    assert!(table.entries.is_empty());
}
