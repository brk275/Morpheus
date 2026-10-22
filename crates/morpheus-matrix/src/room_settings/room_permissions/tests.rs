use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize},
    },
};

use matrix_sdk::ruma::{room_id, user_id};
use serde_json::{Value, json};

use super::*;
use crate::history::tests::{fixture, runtime};

const ROOM: &str = "!perms:example.org";
const ME: &str = "@casey:example.org";
const DANA: &str = "@dana:example.org";
const PEER: &str = "@peer:example.org";

/// Stored content with unknown fields, odd numbers, an order and spacing of
/// its own: everything a save must keep.
const STORED: &str = concat!(
    r#"{"org.example.big":123456789012345678901234567890,"ban":50,"#,
    r#""users":{"@casey:example.org":100,"@dana:example.org":50,"@peer:example.org":100},"#,
    r#""events":{ "m.room.name" : 50 , "org.example.thing":20 },"org.example.rate":1.10,"#,
    r#""state_default":50,"notifications":{"room":50,"org.example.key":10}}"#,
);

fn event(kind: &str, key: &str, sender: &str, content: &str) -> String {
    let head = json!({
        "type": kind,
        "state_key": key,
        "sender": sender,
        "event_id": format!("$state-{kind}"),
        "room_id": ROOM,
        "origin_server_ts": 1,
    })
    .to_string();
    format!("{},\"content\":{content}}}", &head[..head.len() - 1])
}

fn envelope(kind: StateEventType, key: &str, content: &str) -> StateEnvelope {
    let text = event(&kind.to_string(), key, ME, content);
    parse_state(&text, room_id!("!perms:example.org"), &kind, key).unwrap()
}

fn assembled(version: &str, stored: Option<&str>) -> Fresh {
    let create = envelope(
        StateEventType::RoomCreate,
        "",
        &json!({ "creator": ME, "room_version": version }).to_string(),
    );
    let power = stored.map(|text| envelope(StateEventType::RoomPowerLevels, "", text));
    let own = user_id!("@casey:example.org").to_owned();
    let room = room_id!("!perms:example.org");
    let raw = stored.map(str::to_owned);
    assemble_permissions(room, &own, &create, power.as_ref(), raw).unwrap()
}

fn user(target: &str, level: Option<i64>) -> PermissionChange {
    PermissionChange {
        target: PermissionTarget::User(target.to_owned()),
        level,
    }
}

fn threshold(key: PermissionKey, level: Option<i64>) -> PermissionChange {
    PermissionChange {
        target: PermissionTarget::Threshold(key),
        level,
    }
}

fn edit(changes: Vec<PermissionChange>) -> PermissionsEdit {
    PermissionsEdit {
        changes,
        own_demotion_confirmed: false,
    }
}

fn refusal(
    snapshot: &RoomPermissionsSnapshot,
    change: PermissionChange,
) -> Option<PermissionProblem> {
    review_permissions(snapshot, &edit(vec![change])).err()
}

fn row<'a>(snapshot: &'a RoomPermissionsSnapshot, key: &PermissionKey) -> &'a PermissionThreshold {
    snapshot
        .thresholds
        .iter()
        .find(|row| row.key == *key)
        .unwrap()
}

#[test]
fn an_edit_rewrites_only_the_changed_entries_and_keeps_every_other_byte() {
    let changes = [
        user(DANA, Some(75)),
        user("@new:example.org", Some(10)),
        threshold(PermissionKey::Ban, Some(60)),
        threshold(PermissionKey::Kick, Some(40)),
        threshold(PermissionKey::Event("org.example.thing".into()), None),
        threshold(PermissionKey::NotifyRoom, Some(80)),
    ];
    let edited = edited_power_levels(STORED, &changes).unwrap();
    let expected = concat!(
        r#"{"org.example.big":123456789012345678901234567890,"ban":60,"#,
        r#""users":{"@casey:example.org":100,"@dana:example.org":75,"#,
        r#""@peer:example.org":100,"@new:example.org":10},"#,
        r#""events":{"m.room.name":50},"org.example.rate":1.10,"#,
        r#""state_default":50,"notifications":{"room":80,"org.example.key":10},"kick":40}"#,
    );
    assert_eq!(edited.get(), expected);
    // A map only removed from, or never touched, keeps its own text.
    let removed = edited_power_levels(STORED, &[user(DANA, None)]).unwrap();
    let users = r#""users":{"@casey:example.org":100,"@peer:example.org":100}"#;
    assert!(removed.get().contains(users));
    let events = r#""events":{ "m.room.name" : 50 , "org.example.thing":20 }"#;
    assert!(removed.get().contains(events));
    // A map that does not exist yet is appended when an entry is added.
    let bare = r#"{"users":{"@casey:example.org":100}}"#;
    let added = edited_power_levels(bare, &[threshold(PermissionKey::NotifyRoom, Some(0))]);
    let expected = r#"{"users":{"@casey:example.org":100},"notifications":{"room":0}}"#;
    assert_eq!(added.unwrap().get(), expected);
    // Removing from a map that does not exist adds nothing.
    let kind = PermissionKey::Event("m.room.topic".into());
    let unchanged = edited_power_levels(bare, &[threshold(kind, None)]).unwrap();
    assert_eq!(unchanged.get(), bare);
}

#[test]
fn content_an_edit_would_lose_is_refused_never_rewritten() {
    let unsupported = PermissionProblem::ReadOnly(PermissionsReadOnly::Unsupported);
    let change = [user(DANA, Some(10))];
    for odd in [
        "[]",
        r#"{"ban":50,"ban":60}"#,
        r#"{"users":[]}"#,
        r#"{"users":{"@dana:example.org":1,"@dana:example.org":2}}"#,
    ] {
        let refused = edited_power_levels(odd, &change).err();
        assert_eq!(refused, Some(unsupported), "{odd}");
    }
    let top = [threshold(PermissionKey::Ban, None)];
    let refused = edited_power_levels(STORED, &top).err();
    assert_eq!(refused, Some(PermissionProblem::NotRemovable));
    let padding = "x".repeat(MAX_STATE_BYTES);
    let big = format!(r#"{{"org.example.pad":"{padding}"}}"#);
    let too_large = edited_power_levels(&big, &change).err();
    assert_eq!(too_large, Some(PermissionProblem::TooLarge));
}

#[test]
fn snapshot_and_readback_proof_require_complete_unambiguous_stored_levels() {
    let removed = edited_power_levels(
        STORED,
        &[
            user(DANA, None),
            threshold(PermissionKey::Event("org.example.thing".into()), None),
            threshold(PermissionKey::NotifyRoom, None),
        ],
    )
    .unwrap();
    let complete = assembled("10", Some(removed.get()));
    assert!(complete.view.snapshot.has_complete_state);
    let changes = [
        user(DANA, None),
        threshold(PermissionKey::Event("org.example.thing".into()), None),
        threshold(PermissionKey::NotifyRoom, None),
    ];
    let readback = ReadBack {
        levels: complete.levels,
        raw: complete.raw,
    };
    assert!(changes.iter().all(|change| shows(&readback, change)));

    let ambiguous = concat!(
        r#"{"users":{"@casey:example.org":100},"#,
        r#""org.example.duplicate":1,"org.example.duplicate":2}"#,
    );
    for fresh in [
        assembled("org.example.future", Some(removed.get())),
        assembled("10", None),
        assembled("10", Some(ambiguous)),
    ] {
        assert!(!fresh.view.snapshot.has_complete_state);
        let readback = ReadBack {
            levels: fresh.levels,
            raw: fresh.raw,
        };
        assert!(changes.iter().all(|change| !shows(&readback, change)));
    }
}

#[test]
fn a_read_lists_levels_and_what_this_account_may_change() {
    let fresh = assembled("10", Some(STORED));
    let snapshot = &fresh.view.snapshot;
    assert_eq!(snapshot.read_only, None);
    assert_eq!(snapshot.own_level, PermissionLevel::Level(100));
    assert!(snapshot.can_change);
    assert_eq!(snapshot.change_level, 50);
    let ban = row(snapshot, &PermissionKey::Ban);
    assert!(ban.explicit && ban.editable && ban.level == 50);
    // Not set: the default applies and is said to.
    let kick = row(snapshot, &PermissionKey::Kick);
    assert!(!kick.explicit);
    assert_eq!(kick.level, 50);
    let name = row(snapshot, &PermissionKey::Event("m.room.name".into()));
    assert!(name.explicit && name.class == EventClass::State);
    let topic = row(snapshot, &PermissionKey::Event("m.room.topic".into()));
    assert!(!topic.explicit && topic.level == 50);
    let custom = row(snapshot, &PermissionKey::Event("org.example.thing".into()));
    assert_eq!(custom.level, 20);
    assert!(row(snapshot, &PermissionKey::NotifyRoom).explicit);
    let ids: Vec<_> = snapshot.users.iter().map(|u| u.user_id.as_str()).collect();
    assert_eq!(ids, [ME, PEER, DANA], "highest first, then by ID");
    let editable: Vec<_> = snapshot.users.iter().map(|u| u.editable).collect();
    // Yourself and members below you; never a peer at your level.
    assert_eq!(editable, [true, false, true]);
    let baseline = PermissionsBaseline(Some(STORED.to_owned()));
    assert_eq!(fresh.view.baseline, baseline);
}

#[test]
fn creators_versions_and_odd_content_are_shown_read_only_where_they_must_be() {
    // Room version 12: the creator has unlimited power, never listed as a
    // number and never editable.
    let v12 = assembled("12", Some(r#"{"users":{"@dana:example.org":50}}"#));
    let snapshot = &v12.view.snapshot;
    assert_eq!(snapshot.own_level, PermissionLevel::Creator);
    assert_eq!(snapshot.users[0].level, PermissionLevel::Creator);
    assert!(!snapshot.users[0].editable);
    let unknown = assembled("org.example.future", Some(STORED));
    let reason = Some(PermissionsReadOnly::UnknownVersion);
    assert_eq!(unknown.view.snapshot.read_only, reason);
    let missing = assembled("10", None);
    let reason = Some(PermissionsReadOnly::NoPowerLevels);
    assert_eq!(missing.view.snapshot.read_only, reason);
    assert!(!missing.view.snapshot.can_change);
    // A name given twice parses (the last wins) but cannot be kept exactly.
    let twice = r#"{"users":{"@casey:example.org":100,"@casey:example.org":100}}"#;
    let odd = assembled("10", Some(twice));
    let reason = Some(PermissionsReadOnly::Unsupported);
    assert_eq!(odd.view.snapshot.read_only, reason);
    let problem = review_permissions(&odd.view.snapshot, &edit(vec![user(DANA, Some(1))]));
    let unsupported = PermissionProblem::ReadOnly(PermissionsReadOnly::Unsupported);
    assert_eq!(problem.err(), Some(unsupported));
}

#[test]
fn a_long_list_is_bounded_and_says_how_much_was_left_out() {
    let mut users = serde_json::Map::new();
    users.insert(ME.into(), json!(100));
    for index in 0..MAX_PERMISSION_USERS + 20 {
        users.insert(format!("@user{index}:example.org"), json!(1));
    }
    let mut events = serde_json::Map::new();
    for index in 0..MAX_PERMISSION_EVENTS + 5 {
        events.insert(format!("org.example.event{index}"), json!(5));
    }
    let stored = json!({ "users": users, "events": events }).to_string();
    let fresh = assembled("10", Some(&stored));
    let snapshot = &fresh.view.snapshot;
    assert_eq!(snapshot.users.len(), MAX_PERMISSION_USERS);
    assert_eq!(snapshot.omitted_users, 21);
    assert_eq!(snapshot.omitted_events, 5);
}

#[test]
fn the_matrix_rules_refuse_what_this_account_may_not_change() {
    use PermissionProblem as P;
    let fresh = assembled("10", Some(STORED));
    let snapshot = &fresh.view.snapshot;
    let refused = |change: PermissionChange| refusal(snapshot, change);
    // Above your own level.
    assert_eq!(refused(user(DANA, Some(101))), Some(P::AboveYourLevel));
    // A member at your level.
    assert_eq!(refused(user(PEER, Some(10))), Some(P::NotBelowYou));
    // Shape: bounds, IDs, types, ranges, duplicates, top-level removal.
    assert_eq!(refused(user("dana", Some(1))), Some(P::InvalidUser));
    let kind = PermissionKey::Event("bad type".into());
    assert_eq!(refused(threshold(kind, Some(1))), Some(P::InvalidEventType));
    let huge = Some(9_007_199_254_740_992);
    assert_eq!(refused(user(DANA, huge)), Some(P::OutOfRange));
    let top = threshold(PermissionKey::Invite, None);
    assert_eq!(refused(top), Some(P::NotRemovable));
    let empty = review_permissions(snapshot, &edit(Vec::new())).err();
    assert_eq!(empty, Some(PermissionProblem::NoChanges));
    let twice = edit(vec![user(DANA, Some(1)), user(DANA, Some(2))]);
    let duplicate = review_permissions(snapshot, &twice).err();
    assert_eq!(duplicate, Some(PermissionProblem::Duplicate));
    let many = (0..=MAX_PERMISSION_CHANGES)
        .map(|index| user(&format!("@u{index}:example.org"), Some(1)))
        .collect();
    let too_many = review_permissions(snapshot, &edit(many)).err();
    assert_eq!(too_many, Some(PermissionProblem::TooManyChanges));
    // A moderator at 50 cannot touch what needs 50 or more of others, nor
    // change permissions at all when they need more than 50.
    let moderated = STORED.replace(r#""@casey:example.org":100"#, r#""@casey:example.org":50"#);
    let fresh = assembled("10", Some(&moderated));
    let snapshot = &fresh.view.snapshot;
    let refused = |change: PermissionChange| refusal(snapshot, change);
    assert_eq!(refused(user(DANA, Some(10))), Some(P::NotBelowYou));
    // A permission at your level may change.
    let notify = threshold(PermissionKey::NotifyRoom, Some(50));
    assert!(refused(notify).is_none());
    let higher = r#""m.room.power_levels":100,"m.room.name""#;
    let locked = moderated.replace(r#""m.room.name""#, higher);
    let fresh = assembled("10", Some(&locked));
    let snapshot = &fresh.view.snapshot;
    assert!(!snapshot.can_change);
    let change = threshold(PermissionKey::Ban, Some(40));
    let refused = review_permissions(snapshot, &edit(vec![change])).err();
    assert_eq!(refused, Some(PermissionProblem::CannotChange));
    // A creator of a version 12 room is never changed.
    let v12 = assembled("12", Some(r#"{"users":{"@dana:example.org":50}}"#));
    let change = user(ME, Some(50));
    let refused = review_permissions(&v12.view.snapshot, &edit(vec![change])).err();
    assert_eq!(refused, Some(PermissionProblem::CreatorFixed));
}

#[test]
fn lowering_your_own_level_needs_confirmation_and_says_who_is_left() {
    let fresh = assembled("10", Some(STORED));
    let snapshot = &fresh.view.snapshot;
    let mut lowered = edit(vec![user(ME, Some(50))]);
    let refused = review_permissions(snapshot, &lowered).err();
    assert_eq!(refused, Some(PermissionProblem::OwnDemotionUnconfirmed));
    lowered.own_demotion_confirmed = true;
    let review = review_permissions(snapshot, &lowered).unwrap();
    let own = (PermissionLevel::Level(100), 50);
    assert_eq!(review.own_demotion, Some(own));
    // The peer at 100 can still change permissions (they need 50).
    assert!(!review.no_other_manager);
    // Alone at the top, lowered below what changing permissions needs.
    let alone = STORED
        .replace(r#""@peer:example.org":100"#, r#""@peer:example.org":10"#)
        .replace(r#""@dana:example.org":50"#, r#""@dana:example.org":10"#);
    let fresh = assembled("10", Some(&alone));
    let mut lowered = edit(vec![user(ME, Some(0))]);
    lowered.own_demotion_confirmed = true;
    let review = review_permissions(&fresh.view.snapshot, &lowered).unwrap();
    assert!(review.no_other_manager);
    // Raising a member and changing a permission is not a demotion.
    let plain = edit(vec![
        user(DANA, Some(60)),
        threshold(PermissionKey::Ban, Some(70)),
    ]);
    let review = review_permissions(snapshot, &plain).unwrap();
    assert_eq!(review.own_demotion, None);
    let before: Vec<_> = review.changes.iter().map(|change| change.before).collect();
    let fifty = PermissionLevel::Level(50);
    assert_eq!(before, [fifty, fifty]);
}

fn read_back(fresh: Fresh) -> ReadBack {
    ReadBack {
        levels: fresh.levels,
        raw: fresh.raw,
    }
}

#[test]
fn a_read_back_shows_each_change_or_says_it_does_not() {
    let back = read_back(assembled("10", Some(STORED)));
    assert!(shows(&back, &user(DANA, Some(50))));
    assert!(!shows(&back, &user(DANA, Some(60))));
    assert!(shows(&back, &user("@gone:example.org", None)));
    assert!(!shows(&back, &user(DANA, None)));
    assert!(shows(&back, &threshold(PermissionKey::Ban, Some(50))));
    // A top-level permission is set, never removed; one the room does not
    // set itself does not show as set even when its default matches.
    assert!(!shows(&back, &threshold(PermissionKey::Kick, None)));
    assert!(!shows(&back, &threshold(PermissionKey::Kick, Some(50))));
    let name = threshold(PermissionKey::Event("m.room.name".into()), None);
    assert!(!shows(&back, &name));
    assert!(shows(
        &back,
        &threshold(PermissionKey::NotifyRoom, Some(50))
    ));
    assert!(!shows(&back, &threshold(PermissionKey::NotifyRoom, None)));
}

/// The read-back looks at the full stored levels, not the display-capped
/// list: a removed user who still has a level past the listed 500 is not
/// shown as removed, and one changed there is shown when exact.
#[test]
fn a_read_back_checks_targets_beyond_the_listed_rows() {
    let mut users = serde_json::Map::new();
    users.insert(ME.into(), json!(100));
    for index in 0..MAX_PERMISSION_USERS + 20 {
        users.insert(format!("@user{index:04}:example.org"), json!(1));
    }
    let stored = json!({ "users": users }).to_string();
    let fresh = assembled("10", Some(&stored));
    let hidden = format!("@user{:04}:example.org", MAX_PERMISSION_USERS + 10);
    assert!(
        fresh
            .view
            .snapshot
            .users
            .iter()
            .all(|row| row.user_id != hidden)
    );
    assert_eq!(fresh.view.snapshot.exact_users.get(&hidden), Some(&1));
    let back = read_back(fresh);
    assert!(
        !shows(&back, &user(&hidden, None)),
        "still stored: not removed"
    );
    assert!(shows(&back, &user(&hidden, Some(1))));
    // Without the full levels nothing is claimed.
    let empty = ReadBack {
        levels: None,
        raw: None,
    };
    assert!(!shows(&empty, &user("@gone:example.org", None)));
}

/// Your demotion is judged on your whole effective level after the edit:
/// an inherited default lowered, or your own entry removed under a new
/// default, needs the same confirmation as lowering your own entry.
#[test]
fn an_inherited_or_removed_own_level_is_reviewed_with_the_new_defaults() {
    // You have no entry of your own and inherit 50 from users_default;
    // changing permissions needs 50.
    let inherited = r#"{"users_default":50,"users":{"@peer:example.org":100},"state_default":50}"#;
    let fresh = assembled("10", Some(inherited));
    let snapshot = &fresh.view.snapshot;
    assert!(!snapshot.own_explicit);
    let lowered = edit(vec![threshold(PermissionKey::UsersDefault, Some(0))]);
    let refused = review_permissions(snapshot, &lowered).err();
    assert_eq!(refused, Some(PermissionProblem::OwnDemotionUnconfirmed));
    let mut confirmed = lowered.clone();
    confirmed.own_demotion_confirmed = true;
    let review = review_permissions(snapshot, &confirmed).unwrap();
    assert_eq!(review.own_demotion, Some((PermissionLevel::Level(50), 0)));
    // Your own entry removed while the default is staged lower: the new
    // default applies to you, not the old one.
    let explicit = STORED.replace(r#""ban":50,"#, r#""ban":50,"users_default":60,"#);
    let fresh = assembled("10", Some(&explicit));
    let snapshot = &fresh.view.snapshot;
    assert!(snapshot.own_explicit);
    let removed = edit(vec![
        threshold(PermissionKey::UsersDefault, Some(10)),
        user(ME, None),
    ]);
    let refused = review_permissions(snapshot, &removed).err();
    assert_eq!(refused, Some(PermissionProblem::OwnDemotionUnconfirmed));
    let mut removed = removed;
    removed.own_demotion_confirmed = true;
    let review = review_permissions(snapshot, &removed).unwrap();
    assert_eq!(review.own_demotion, Some((PermissionLevel::Level(100), 10)));
    let own = review
        .changes
        .iter()
        .find(|change| change.target == PermissionTarget::User(ME.into()))
        .unwrap();
    assert_eq!((own.after, own.after_explicit), (10, false));
}

/// A removed event entry falls back to the default of its class, with the
/// edit's own staged defaults: state events to state_default (also known
/// ones beyond the common list), message events to events_default, and a
/// custom type to a conditional default, never a confident single value.
#[test]
fn removed_event_entries_fall_back_by_class_with_the_staged_defaults() {
    assert_eq!(event_class("m.space.child"), EventClass::State);
    assert_eq!(event_class("m.space.parent"), EventClass::State);
    assert_eq!(event_class("m.room.name"), EventClass::State);
    assert_eq!(event_class("m.reaction"), EventClass::Message);
    assert_eq!(event_class("m.room.message"), EventClass::Message);
    assert_eq!(event_class("org.example.thing"), EventClass::Unknown);
    let stored = STORED.replace(
        r#""org.example.thing":20 }"#,
        r#""org.example.thing":20, "m.space.child":40, "m.room.message":30 }"#,
    );
    let fresh = assembled("10", Some(&stored));
    let snapshot = &fresh.view.snapshot;
    let child = row(snapshot, &PermissionKey::Event("m.space.child".into()));
    assert_eq!(child.class, EventClass::State);
    let changes = edit(vec![
        threshold(PermissionKey::StateDefault, Some(70)),
        threshold(PermissionKey::EventsDefault, Some(5)),
        threshold(PermissionKey::Event("m.space.child".into()), None),
        threshold(PermissionKey::Event("m.room.message".into()), None),
        threshold(PermissionKey::Event("org.example.thing".into()), None),
    ]);
    let review = review_permissions(snapshot, &changes).unwrap();
    let after = |kind: &str| {
        let target = PermissionTarget::Threshold(PermissionKey::Event(kind.into()));
        let change = review.changes.iter().find(|c| c.target == target).unwrap();
        (change.after, change.after_if_state)
    };
    assert_eq!(
        after("m.space.child"),
        (70, None),
        "the staged state_default"
    );
    assert_eq!(
        after("m.room.message"),
        (5, None),
        "the staged events_default"
    );
    assert_eq!(after("org.example.thing"), (5, Some(70)), "conditional");
}

// A loopback homeserver: one joined room of one account with a restored
// synthetic session. No real account, network, vault or media.
#[derive(Clone, Default)]
struct Served {
    /// Each state event's content as stored text, served byte for byte.
    state: Arc<StdMutex<BTreeMap<(String, String), String>>>,
    syncs: Arc<StdMutex<VecDeque<Value>>>,
    /// Each state write's event type and body, as sent.
    writes: Arc<StdMutex<Vec<(String, String)>>>,
    write_reply: Arc<StdMutex<Option<(u16, Value)>>>,
    power_reads: Arc<AtomicUsize>,
    /// The power-level read with this number waits until it is reset.
    hold_power_read: Arc<AtomicUsize>,
    /// A state write taken while held is never answered or applied.
    hold_writes: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

struct PermissionsServer {
    url: String,
    served: Served,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PermissionsServer {
    fn start(power_levels: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Served::default();
        {
            let mut state = served.state.lock().unwrap();
            let create = json!({ "creator": ME, "room_version": "10" }).to_string();
            state.insert(("m.room.create".into(), String::new()), create);
            let member = json!({ "membership": "join" }).to_string();
            state.insert(("m.room.member".into(), ME.into()), member);
            let power = ("m.room.power_levels".into(), String::new());
            state.insert(power, power_levels.to_owned());
        }
        let serving = served.clone();
        let worker = std::thread::spawn(move || {
            while !serving.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let serving = serving.clone();
                        std::thread::spawn(move || answer(stream, &serving));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            served,
            worker: Some(worker),
        }
    }

    fn writes(&self) -> Vec<(String, String)> {
        self.served.writes.lock().unwrap().clone()
    }

    fn store_power_levels(&self, text: &str) {
        let key = ("m.room.power_levels".to_owned(), String::new());
        let mut state = self.served.state.lock().unwrap();
        state.insert(key, text.to_owned());
    }

    async fn wait_for_writes(&self, count: usize) {
        while self.writes().len() < count {
            time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_for_power_reads(&self, count: usize) {
        while self.served.power_reads.load(Ordering::SeqCst) < count {
            time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for PermissionsServer {
    fn drop(&mut self) {
        self.served.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn joined_room() -> Value {
    let events: Vec<Value> = [
        (
            "m.room.create",
            "",
            json!({ "creator": ME, "room_version": "10" }),
        ),
        ("m.room.member", ME, json!({ "membership": "join" })),
    ]
    .into_iter()
    .map(|(kind, key, content)| {
        let text = event(kind, key, ME, &content.to_string());
        serde_json::from_str(&text).unwrap()
    })
    .collect();
    let mut join = serde_json::Map::new();
    let room = json!({
        "state": { "events": events },
        "timeline": { "events": [], "limited": false },
    });
    join.insert(ROOM.to_owned(), room);
    json!({ "next_batch": "s1", "rooms": { "join": join } })
}

fn answer(mut stream: TcpStream, served: &Served) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (line, body) = read_request(&mut stream);
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let not_found = json!({ "errcode": "M_NOT_FOUND", "error": "x" }).to_string();
    let (status, reply) = if path.starts_with("/_matrix/client/versions") {
        (200, json!({ "versions": ["v1.15"] }).to_string())
    } else if path.contains("/sync") {
        let next = served.syncs.lock().unwrap().pop_front();
        let idle = json!({ "next_batch": "idle" });
        (200, next.unwrap_or(idle).to_string())
    } else if let Some((kind, key)) = state_target(&path) {
        if method == "PUT" {
            let write = (kind.clone(), body.clone());
            served.writes.lock().unwrap().push(write);
            if served.hold_writes.load(Ordering::SeqCst) {
                while !served.stopped.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return;
            }
            let reply = served.write_reply.lock().unwrap().clone();
            match reply {
                Some((status, reply)) => (status, reply.to_string()),
                None => {
                    served.state.lock().unwrap().insert((kind, key), body);
                    (200, json!({ "event_id": "$written" }).to_string())
                }
            }
        } else {
            if kind == "m.room.power_levels" {
                let read = served.power_reads.fetch_add(1, Ordering::SeqCst) + 1;
                while served.hold_power_read.load(Ordering::SeqCst) == read {
                    if served.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let entry = (kind.clone(), key.clone());
            let content = served.state.lock().unwrap().get(&entry).cloned();
            match content {
                Some(content) => (200, event(&kind, &key, ME, &content)),
                None => (404, not_found),
            }
        }
    } else {
        (
            404,
            json!({ "errcode": "M_UNRECOGNIZED", "error": "x" }).to_string(),
        )
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The request line and body of one HTTP/1.1 request.
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(size) = stream.read(&mut chunk) {
        if size == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..size]);
        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]).to_string();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                let length = name.eq_ignore_ascii_case("content-length");
                length.then(|| value.trim().parse::<usize>().ok()).flatten()
            })
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            let line = headers.lines().next().unwrap_or("").to_owned();
            let body = String::from_utf8_lossy(&request[end + 4..end + 4 + length]);
            return (line, body.to_string());
        }
    }
    (String::new(), String::new())
}

/// The decoded event type and state key of a state request path.
fn state_target(path: &str) -> Option<(String, String)> {
    let path = path.split('?').next()?;
    let (_, rest) = path.split_once("/state/")?;
    let (kind, key) = rest.split_once('/').unwrap_or((rest, ""));
    Some((decode(kind)?, decode(key)?))
}

fn decode(segment: &str) -> Option<String> {
    let mut decoded = Vec::new();
    let mut rest = segment.as_bytes();
    while let Some((&first, tail)) = rest.split_first() {
        if first == b'%' && tail.len() >= 2 {
            let hex = std::str::from_utf8(&tail[..2]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            decoded.push(first);
            rest = tail;
        }
    }
    String::from_utf8(decoded).ok()
}

/// A manager whose selected account is signed in with a synthetic session
/// against `server` and synced once into the joined room.
async fn signed_in(
    server: &PermissionsServer,
) -> (std::path::PathBuf, Arc<AccountManager>, crate::AccountInfo) {
    let account = crate::AccountInfo {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: ME.into(),
        device_id: "PERMISSIONS-FIXTURE".into(),
        homeserver: server.url.clone(),
    };
    let (directory, manager, account) = fixture(Some(account)).await;
    let client = {
        let state = manager.state.lock().await;
        state.loaded[&account.id].client.clone()
    };
    let session = matrix_sdk::authentication::matrix::MatrixSession {
        meta: matrix_sdk::SessionMeta {
            user_id: user_id!("@casey:example.org").to_owned(),
            device_id: "PERMISSIONS-FIXTURE".into(),
        },
        tokens: matrix_sdk::SessionTokens {
            access_token: "synthetic access token".into(),
            refresh_token: None,
        },
    };
    let restored = client
        .matrix_auth()
        .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
        .await;
    restored.unwrap();
    server.served.syncs.lock().unwrap().push_back(joined_room());
    let settings = matrix_sdk::config::SyncSettings::default();
    client.sync_once(settings).await.unwrap();
    (directory, manager, account)
}

/// A settings session for the room, admitted while `gate` is true.
async fn session(
    manager: &AccountManager,
    account: &crate::AccountInfo,
    gate: &Arc<AtomicBool>,
    form: RoomSettingsCancellation,
) -> RoomSettingsSession {
    let selection = manager.account_selection(&account.id).unwrap();
    let opened = manager.open_room_settings_for(&selection, ROOM, form).await;
    let gate = gate.clone();
    opened
        .unwrap()
        .with_admission(Arc::new(move || gate.load(Ordering::SeqCst)))
}

#[test]
fn a_save_is_one_lossless_write_and_is_read_back() {
    runtime().block_on(async {
        let server = PermissionsServer::start(STORED);
        let (directory, manager, account) = signed_in(&server).await;
        let gate = Arc::new(AtomicBool::new(true));
        let session = session(&manager, &account, &gate, RoomSettingsCancellation::new()).await;
        let view = session.load_permissions().await.unwrap();
        let changes = edit(vec![
            user(DANA, Some(75)),
            threshold(PermissionKey::Ban, Some(60)),
        ]);
        let saved = session.save_permissions(&view, &changes).await.unwrap();
        let writes = server.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, "m.room.power_levels");
        let expected = STORED
            .replace(r#""ban":50"#, r#""ban":60"#)
            .replace(r#""@dana:example.org":50"#, r#""@dana:example.org":75"#);
        assert_eq!(writes[0].1, expected, "only the two entries changed");
        assert!(saved.shown.iter().all(|(_, shown)| *shown));
        assert_eq!(saved.view.baseline, PermissionsBaseline(Some(expected)));
        drop(session);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn a_changed_baseline_or_fresh_authority_or_closed_gate_sends_nothing() {
    runtime().block_on(async {
        let server = PermissionsServer::start(STORED);
        let (directory, manager, account) = signed_in(&server).await;
        let gate = Arc::new(AtomicBool::new(true));
        let session = session(&manager, &account, &gate, RoomSettingsCancellation::new()).await;
        let view = session.load_permissions().await.unwrap();
        // Another client respells one number: equal as a value, yet not the
        // reviewed text, so nothing is sent.
        server.store_power_levels(&STORED.replace("1.10", "1.1"));
        let changes = edit(vec![user(DANA, Some(60))]);
        let changed = session.save_permissions(&view, &changes).await.err();
        let baseline = PermissionsError::Settings(RoomSettingsError::BaselineChanged);
        assert_eq!(changed, Some(baseline));
        // Reviewed against an old read: Dana now stands at your level in a
        // fresh read whose text was reviewed, so the save refuses it.
        let raised = STORED.replace(r#""@dana:example.org":50"#, r#""@dana:example.org":100"#);
        server.store_power_levels(&raised);
        let mut stale = view.clone();
        stale.baseline = PermissionsBaseline(Some(raised.clone()));
        let refused = session.save_permissions(&stale, &changes).await.err();
        let below = PermissionsError::Problem(PermissionProblem::NotBelowYou);
        assert_eq!(refused, Some(below));
        // The security gate closes while the fresh read before the write
        // waits: refused at the hand-off.
        server.store_power_levels(STORED);
        let reads = server.served.power_reads.load(Ordering::SeqCst);
        let held = &server.served.hold_power_read;
        held.store(reads + 1, Ordering::SeqCst);
        let close = async {
            server.wait_for_power_reads(reads + 1).await;
            gate.store(false, Ordering::SeqCst);
            held.store(0, Ordering::SeqCst);
        };
        let (result, ()) = tokio::join!(session.save_permissions(&view, &changes), close);
        let busy = PermissionsError::Settings(RoomSettingsError::SecurityBusy);
        assert_eq!(result.err(), Some(busy));
        assert!(server.writes().is_empty());
        drop(session);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn a_write_handed_over_then_stopped_or_refused_is_reported_as_it_is_and_never_resent() {
    use RoomSettingsError as Error;
    let unknown = PermissionsError::Settings(Error::Unknown);
    runtime().block_on(async {
        let server = PermissionsServer::start(STORED);
        let (directory, manager, account) = signed_in(&server).await;
        let gate = Arc::new(AtomicBool::new(true));
        let changes = edit(vec![user(DANA, Some(60))]);
        // The form closes while the write is with the homeserver.
        server.served.hold_writes.store(true, Ordering::SeqCst);
        let form = RoomSettingsCancellation::new();
        let session = session(&manager, &account, &gate, form.clone()).await;
        let view = session.load_permissions().await.unwrap();
        let close = async {
            server.wait_for_writes(1).await;
            form.cancel();
        };
        let (result, ()) = tokio::join!(session.save_permissions(&view, &changes), close);
        assert_eq!(result.err(), Some(unknown));
        // The account changes while the next one is with the homeserver.
        let session = self::session(&manager, &account, &gate, RoomSettingsCancellation::new());
        let session = session.await;
        let view = session.load_permissions().await.unwrap();
        let switch = async {
            server.wait_for_writes(2).await;
            manager.publish_active_account(None);
        };
        let (result, ()) = tokio::join!(session.save_permissions(&view, &changes), switch);
        assert_eq!(result.err(), Some(unknown));
        manager.publish_active_account(Some(account.clone()));
        assert_eq!(server.writes().len(), 2, "neither was sent again");
        // Answers: a 4xx changed nothing; anything else may have.
        server.served.hold_writes.store(false, Ordering::SeqCst);
        let error = |code: &str| json!({ "errcode": code, "error": "homeserver text" });
        let session = self::session(&manager, &account, &gate, RoomSettingsCancellation::new());
        let session = session.await;
        let answers = [
            (403, error("M_FORBIDDEN"), Error::PermissionDenied),
            (500, error("M_UNKNOWN"), Error::Unknown),
        ];
        for (sent, (status, body, expected)) in answers.into_iter().enumerate() {
            let view = session.load_permissions().await.unwrap();
            *server.served.write_reply.lock().unwrap() = Some((status, body));
            let result = session.save_permissions(&view, &changes).await;
            assert_eq!(result.err(), Some(PermissionsError::Settings(expected)));
            assert_eq!(server.writes().len(), 3 + sent, "status {status}");
        }
        drop(session);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}
