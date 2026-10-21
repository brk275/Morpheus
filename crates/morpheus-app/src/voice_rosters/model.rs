//! Pure, bounded voice-room presence: which rooms are watched, how observed
//! call memberships become people, and what each state may say. No network,
//! UI or account access happens here.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use morpheus_config::RoomCallMode;
use morpheus_rtc::{InteropMode, ObservedParticipant};

/// Listed voice rooms watched at once (the first ones in the room list).
pub(super) const MAX_LISTED: usize = 6;
/// Rooms watched at once: the open room, an expanded roster and the listed.
pub(super) const MAX_WATCHED: usize = 8;
/// People named in one roster list; the rest are counted.
pub(super) const MAX_SHOWN: usize = 50;
/// Initials shown beside a room entry.
pub(super) const MAX_PREVIEW: usize = 3;
/// Characters of a display name or Matrix ID that are shown.
pub(super) const MAX_LABEL_CHARS: usize = 64;
/// A roster not confirmed again within this time is shown as stale.
pub(super) const STALE_AFTER: Duration = Duration::from_secs(90);

/// The passive dialect for a room's configured call mode: the same slot a
/// join would use. Legacy room-state calls have no passive roster.
pub(super) fn interop_mode(mode: RoomCallMode) -> Option<InteropMode> {
    match mode {
        RoomCallMode::Standard => Some(InteropMode::Current),
        RoomCallMode::ElementCallMatrix2 => Some(InteropMode::Sticky2025),
        RoomCallMode::ElementCallLegacy => None,
    }
}

/// Voice rooms in list order, without repeats, at most MAX_LISTED.
pub(super) fn listed<'a>(rooms: impl IntoIterator<Item = (&'a str, bool)>) -> Vec<String> {
    let mut listed: Vec<String> = Vec::new();
    for (id, voice) in rooms {
        if listed.len() == MAX_LISTED {
            break;
        }
        if voice && !id.is_empty() && !listed.iter().any(|known| known == id) {
            listed.push(id.to_owned());
        }
    }
    listed
}

/// The rooms to watch: the open voice room, then an expanded roster, then
/// the listed rooms while the room list is shown. Never more than
/// MAX_WATCHED and never the same room twice, so a room listed under several
/// Spaces shares one roster.
pub(super) fn watched(
    active: Option<&str>,
    expanded: Option<&str>,
    listed: &[String],
    list_shown: bool,
) -> Vec<String> {
    let listed = listed.iter().map(String::as_str).filter(|_| list_shown);
    let mut rooms: Vec<String> = Vec::new();
    for room in active.into_iter().chain(expanded).chain(listed) {
        if rooms.len() == MAX_WATCHED {
            break;
        }
        if !room.is_empty() && !rooms.iter().any(|known| known == room) {
            rooms.push(room.to_owned());
        }
    }
    rooms
}

/// One person present in the call slot, with every device they joined from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Person {
    pub(super) user_id: String,
    pub(super) name: String,
    pub(super) initials: String,
    pub(super) devices: usize,
}

/// Bounded, sanitized text for an untrusted name or ID.
pub(super) fn label(value: &str) -> String {
    crate::directory_plain_text(value.trim(), MAX_LABEL_CHARS)
}

/// Up to two letters for an avatar fallback.
pub(super) fn initials(name: &str) -> String {
    let words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let firsts = words.filter_map(|word| word.chars().next());
    let letters: String = firsts.take(2).collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters.to_uppercase()
    }
}

/// The people in an observed call: one entry per Matrix user, however many
/// devices joined, sorted by name, then ID. `name` reads a local display
/// name; without one the ID's localpart is shown.
pub(super) fn people(
    participants: &[ObservedParticipant],
    name: impl Fn(&str) -> Option<String>,
) -> Vec<Person> {
    let mut devices: BTreeMap<&str, usize> = BTreeMap::new();
    for participant in participants {
        *devices.entry(participant.user_id.as_str()).or_default() += 1;
    }
    let mut people: Vec<Person> = devices
        .into_iter()
        .map(|(user_id, devices)| {
            let bare = user_id.trim_start_matches('@');
            let localpart = bare.split(':').next().unwrap_or("");
            let shown = name(user_id)
                .map(|name| label(&name))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| label(localpart));
            Person {
                user_id: label(user_id),
                initials: initials(&shown),
                name: shown,
                devices,
            }
        })
        .collect();
    people.sort_by(|a, b| {
        let by_name = a.name.to_lowercase().cmp(&b.name.to_lowercase());
        by_name.then_with(|| a.user_id.cmp(&b.user_id))
    });
    people
}

/// Why presence cannot be read for a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unsupported {
    /// The room uses legacy room-state calls.
    Dialect,
    /// The room is not encrypted; the standard call path needs encryption.
    Unencrypted,
    /// The homeserver or this sync cannot carry live call memberships.
    Server,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Presence {
    Loading,
    /// Confirmed at `at`; `refresh_failed` once a later check failed.
    Live {
        people: Vec<Person>,
        at: Instant,
        refresh_failed: bool,
    },
    /// No open call slot could be confirmed. This is not a claim that
    /// nobody is present: a failed read looks the same.
    NoCall,
    Unsupported(Unsupported),
    /// Not joined, removed, or the account's client is unavailable.
    Unavailable,
}

/// What a room entry and its roster say. `state`: 1 loading, 2 live,
/// 3 stale, 4 no open call confirmed, 5 unsupported, 6 unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct View {
    pub(super) state: i32,
    pub(super) count: usize,
    pub(super) label: String,
    pub(super) preview: String,
    pub(super) description: String,
}

pub(super) fn view(presence: &Presence, now: Instant) -> View {
    let quiet = |state, label: &str, description: &str| View {
        state,
        count: 0,
        label: label.into(),
        preview: String::new(),
        description: description.into(),
    };
    match presence {
        Presence::Loading => quiet(1, "…", "Checking who is in this voice room"),
        Presence::Live {
            people,
            at,
            refresh_failed,
        } => {
            let stale = *refresh_failed || now.saturating_duration_since(*at) > STALE_AFTER;
            let count = people.len();
            let preview: Vec<&str> = people
                .iter()
                .take(MAX_PREVIEW)
                .map(|person| person.initials.as_str())
                .collect();
            let people_text = match count {
                0 => "Nobody in the call".to_owned(),
                1 => "1 person in the call".to_owned(),
                count => format!("{count} people in the call"),
            };
            View {
                state: if stale { 3 } else { 2 },
                count,
                label: if stale {
                    format!("{count}?")
                } else {
                    count.to_string()
                },
                preview: preview.join(" "),
                description: if stale {
                    format!("Last known: {people_text}; could not check again")
                } else {
                    people_text
                },
            }
        }
        Presence::NoCall => quiet(4, "–", "No open call could be confirmed in this room"),
        Presence::Unsupported(reason) => quiet(
            5,
            "–",
            match reason {
                Unsupported::Dialect => "This room uses legacy calls; who is here cannot be shown",
                Unsupported::Unencrypted => {
                    "This room is not encrypted; who is here cannot be shown"
                }
                Unsupported::Server => "This homeserver cannot show who is in calls",
            },
        ),
        Presence::Unavailable => quiet(6, "–", "Who is here cannot be checked right now"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(user: &str, device: &str) -> ObservedParticipant {
        ObservedParticipant {
            user_id: user.into(),
            device_id: device.into(),
            membership_id: format!("{user}-{device}"),
            membership_event_id: format!("$m-{device}"),
        }
    }

    #[test]
    fn rooms_watched_are_bounded_unique_and_follow_visibility() {
        let ids = ["!a", "!b", "!c", "!d", "!e", "!f", "!g", "!h"];
        let listed = listed(ids.iter().cycle().take(20).map(|id| (*id, true)));
        assert_eq!(listed.len(), MAX_LISTED);
        let watched_now = watched(Some("!open"), Some("!a"), &listed, true);
        assert_eq!(watched_now.len(), MAX_LISTED + 1);
        assert_eq!(watched_now[0], "!open");
        // A room shown in several Spaces or lists is watched once.
        assert_eq!(
            watched_now.iter().filter(|id| id.as_str() == "!a").count(),
            1
        );
        // A hidden room list releases every listed room.
        assert_eq!(watched(Some("!open"), None, &listed, false), ["!open"]);
        assert!(watched(None, None, &listed, false).is_empty());
        let many: Vec<String> = (0..20).map(|n| format!("!r{n}")).collect();
        assert_eq!(
            watched(Some("!x"), Some("!y"), &many, true).len(),
            MAX_WATCHED
        );
        // Non-voice and empty rooms never count.
        assert!(listed_of(&[("!a", false), ("", true)]).is_empty());
    }

    fn listed_of(rooms: &[(&str, bool)]) -> Vec<String> {
        listed(rooms.iter().copied())
    }

    #[test]
    fn people_are_one_per_user_with_bounded_sanitized_labels() {
        let call = [
            participant("@bob:example.org", "B1"),
            participant("@alice:example.org", "A1"),
            participant("@alice:example.org", "A2"),
        ];
        let named = people(&call, |user| {
            (user == "@alice:example.org").then(|| "Al\u{202e}ice  Smith".to_owned())
        });
        assert_eq!(named.len(), 2);
        // Direction controls never reach the screen.
        assert!(named[0].name.starts_with("Al") && !named[0].name.contains('\u{202e}'));
        assert_eq!(named[0].devices, 2);
        assert_eq!(initials("Alice Smith"), "AS");
        // No display name: the localpart, never an invented name.
        assert_eq!(named[1].name, "bob");
        let long = "x".repeat(10 * MAX_LABEL_CHARS);
        let hostile = people(&[participant("@y:example.org", "Y")], |_| {
            Some(long.clone())
        });
        assert_eq!(hostile[0].name.chars().count(), MAX_LABEL_CHARS);
        assert_eq!(initials("  "), "?");
    }

    #[test]
    fn presence_never_overstates_and_goes_stale() {
        let now = Instant::now();
        let people = people(&[participant("@a:x.org", "D")], |_| None);
        let live = Presence::Live {
            people: people.clone(),
            at: now,
            refresh_failed: false,
        };
        let fresh = view(&live, now);
        assert_eq!(
            (fresh.state, fresh.count, fresh.label.as_str()),
            (2, 1, "1")
        );
        // Expiry of the confirmation: shown as stale, never as current.
        let old = view(&live, now + STALE_AFTER + Duration::from_secs(1));
        assert_eq!((old.state, old.label.as_str()), (3, "1?"));
        assert!(old.description.starts_with("Last known"));
        let failed = Presence::Live {
            people,
            at: now,
            refresh_failed: true,
        };
        assert_eq!(view(&failed, now).state, 3);
        // A missing slot and a failed read look alike: no zero is claimed.
        let no_call = view(&Presence::NoCall, now);
        assert_eq!((no_call.state, no_call.count), (4, 0));
        assert!(!no_call.description.contains("Nobody"));
        assert_eq!(
            view(&Presence::Unsupported(Unsupported::Dialect), now).state,
            5
        );
        assert_eq!(view(&Presence::Loading, now).state, 1);
    }

    #[test]
    fn only_passive_dialects_get_a_roster() {
        assert_eq!(
            interop_mode(RoomCallMode::Standard),
            Some(InteropMode::Current)
        );
        assert_eq!(
            interop_mode(RoomCallMode::ElementCallMatrix2),
            Some(InteropMode::Sticky2025)
        );
        assert_eq!(interop_mode(RoomCallMode::ElementCallLegacy), None);
    }
}
