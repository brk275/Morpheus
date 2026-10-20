//! Pure, bounded room-activity decisions and labels. No network, UI or
//! account access happens here.

use std::time::{Duration, Instant};

use morpheus_matrix::{ActivityMember, SeenBy};

/// Names shown in the typing footer; the rest are counted.
pub(super) const MAX_TYPING_NAMES: usize = 3;
/// Characters of one untrusted name or ID shown.
pub(super) const MAX_LABEL_CHARS: usize = 48;
/// A typing footer with no update for this long is cleared.
pub(super) const TYPING_EXPIRY: Duration = Duration::from_secs(30);
/// Without an edit for this long, this account stops typing.
pub(super) const TYPING_IDLE: Duration = Duration::from_secs(5);
/// A start is renewed this often while typing goes on; the SDK lets each one
/// last 4 seconds and sends at most one per 3.
pub(super) const TYPING_RESEND: Duration = Duration::from_secs(3);
/// The typing worker re-checks at least this often with nothing to do.
pub(super) const TYPING_POLL: Duration = Duration::from_secs(30);
/// An automatic read receipt waits this long on the same viewed event.
pub(super) const READ_SETTLE: Duration = Duration::from_millis(1500);
/// At most one automatic read receipt per scope this often.
pub(super) const READ_INTERVAL: Duration = Duration::from_secs(3);

/// What this account publishes; all off unless chosen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Preferences {
    pub(super) send_typing: bool,
    pub(super) public_receipts: bool,
    pub(super) read_tracking: bool,
}

impl Preferences {
    pub(super) fn from_config(config: &morpheus_config::Config) -> Self {
        Self {
            send_typing: config.activity.send_typing,
            public_receipts: config.activity.public_read_receipts,
            read_tracking: config.activity.read_tracking,
        }
    }
}

/// A bounded, sanitized label: the display name, else the ID's localpart.
pub(super) fn display(member: &ActivityMember) -> String {
    let name = member
        .name
        .as_deref()
        .map(|name| crate::directory_plain_text(name, MAX_LABEL_CHARS))
        .filter(|name| !name.is_empty());
    name.unwrap_or_else(|| {
        let bare = member.user_id.trim_start_matches('@');
        let localpart = bare.split(':').next().unwrap_or("");
        let localpart = crate::directory_plain_text(localpart, MAX_LABEL_CHARS);
        if localpart.is_empty() {
            "Someone".into()
        } else {
            localpart
        }
    })
}

/// "Alice is typing…", "Alice and Bob are typing…", "Alice, Bob and Carol
/// are typing…" or "Alice, Bob and 4 others are typing…".
pub(super) fn typing_text(members: &[ActivityMember], omitted: usize) -> String {
    let total = members.len() + omitted;
    let names: Vec<String> = members.iter().take(MAX_TYPING_NAMES).map(display).collect();
    match (names.as_slice(), total) {
        ([], _) => String::new(),
        ([one], 1) => format!("{one} is typing…"),
        ([one, two], 2) => format!("{one} and {two} are typing…"),
        ([one, two, three], 3) => format!("{one}, {two} and {three} are typing…"),
        ([one, two, ..], total) => format!("{one}, {two} and {} others are typing…", total - 2),
        ([one], total) => format!("{one} and {} others are typing…", total - 1),
    }
}

/// Seen-by rows (name, ID, detail) and a summary that claims only what the
/// receipts say.
pub(super) fn seen_rows(seen: &SeenBy) -> (Vec<(String, String, String)>, String) {
    let rows = seen
        .readers
        .iter()
        .map(|reader| {
            let id = crate::directory_plain_text(&reader.member.user_id, MAX_LABEL_CHARS * 2);
            let detail = if reader.on_this_event {
                "Read up to here"
            } else {
                "Read further"
            };
            let detail = match reader.at_ms {
                Some(at) if reader.on_this_event => {
                    format!("{detail} · {}", crate::message_search::search_time(at))
                }
                _ => detail.to_owned(),
            };
            (display(&reader.member), id, detail)
        })
        .collect();
    let count = seen.readers.len() + seen.omitted;
    let summary = match count {
        0 => "Nobody has a public read receipt here yet.".to_owned(),
        1 => "1 person has read this far.".to_owned(),
        count => format!("{count} people have read this far."),
    };
    let summary = if seen.omitted > 0 {
        format!("{summary} {} are not listed.", seen.omitted)
    } else {
        summary
    };
    (rows, summary)
}

/// The composer as the typing worker sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Composer {
    pub(super) focused: bool,
    pub(super) has_text: bool,
    pub(super) last_edit: Option<Instant>,
}

impl Composer {
    fn typing(&self, now: Instant) -> bool {
        self.focused
            && self.has_text
            && self
                .last_edit
                .is_some_and(|edit| now.saturating_duration_since(edit) < TYPING_IDLE)
    }
}

/// Whether a typing start not yet sent is still wanted: some composer is
/// focused and holds text. Blur, an emptied or sent composer, or focus moving
/// to an empty composer drop it before it goes out.
pub(super) fn start_wanted(composers: &[Composer]) -> bool {
    composers
        .iter()
        .any(|composer| composer.focused && composer.has_text)
}

/// A typing start that may still be active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Started {
    /// The room and account selection are still the ones it was sent for.
    pub(super) same_target: bool,
    pub(super) at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TypingStep {
    /// Tell the room, or renew a start that is about to end.
    Start,
    /// End a start that may be active, for the account and room it was sent
    /// for.
    Stop,
    Nothing,
}

/// One decision of the typing worker. `enabled` already includes the setting,
/// a live unlocked session and the conversation page being shown.
pub(super) fn typing_step(
    enabled: bool,
    composers: &[Composer],
    started: Option<Started>,
    now: Instant,
) -> TypingStep {
    let typing = enabled && composers.iter().any(|composer| composer.typing(now));
    match started {
        Some(started) if !(typing && started.same_target) => TypingStep::Stop,
        Some(started) if now.saturating_duration_since(started.at) >= TYPING_RESEND => {
            TypingStep::Start
        }
        Some(_) => TypingStep::Nothing,
        None if typing => TypingStep::Start,
        None => TypingStep::Nothing,
    }
}

/// How long the worker may sleep before deciding again: until a start is
/// due for renewal or the newest edit goes idle. Composer, focus, setting and
/// scope changes wake it sooner.
pub(super) fn typing_wait(
    composers: &[Composer],
    started: Option<Instant>,
    now: Instant,
) -> Duration {
    let idle = composers
        .iter()
        .filter(|composer| composer.focused && composer.has_text)
        .filter_map(|composer| composer.last_edit)
        .map(|edit| edit + TYPING_IDLE);
    let renew = started.map(|at| at + TYPING_RESEND);
    idle.chain(renew)
        .filter(|due| *due > now)
        .min()
        .map_or(TYPING_POLL, |due| due.saturating_duration_since(now))
}

/// What may move this account's read receipt automatically. Every fact must
/// hold at the moment of sending, not only when the view was reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ReadGate {
    pub(super) tracking_enabled: bool,
    /// A live session whose account access is not locked.
    pub(super) live_session: bool,
    /// The window is shown and not minimized.
    pub(super) window_shown: bool,
    /// A composer of this conversation has keyboard focus, which Slint ends
    /// when the window stops being the active window.
    pub(super) focused: bool,
    /// A live timeline (not a view focused on an older event) is scrolled to
    /// its bottom and its last row is the event.
    pub(super) at_bottom: bool,
    /// The account selection, room and thread are the ones reported.
    pub(super) same_scope: bool,
}

pub(super) fn may_mark_read(gate: &ReadGate) -> bool {
    gate.tracking_enabled
        && gate.live_session
        && gate.window_shown
        && gate.focused
        && gate.at_bottom
        && gate.same_scope
}

/// When an automatic receipt for `event`, shown since `since`, may go out:
/// never again for the last event sent, otherwise once it stayed in view for
/// READ_SETTLE and READ_INTERVAL passed since the last receipt.
pub(super) fn read_wait(
    since: Instant,
    last: Option<(&str, Instant)>,
    event: &str,
    now: Instant,
) -> Option<Duration> {
    if event.is_empty() || last.is_some_and(|(sent, _)| sent == event) {
        return None;
    }
    let settled = since + READ_SETTLE;
    let due = last.map_or(settled, |(_, at)| settled.max(at + READ_INTERVAL));
    Some(due.saturating_duration_since(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::Reader;

    fn member(id: &str, name: Option<&str>) -> ActivityMember {
        ActivityMember {
            user_id: id.into(),
            name: name.map(str::to_owned),
        }
    }

    fn typing_now(now: Instant) -> Composer {
        Composer {
            focused: true,
            has_text: true,
            last_edit: Some(now),
        }
    }

    fn started(at: Instant) -> Option<Started> {
        Some(Started {
            same_target: true,
            at,
        })
    }

    #[test]
    fn typing_is_throttled_and_stops_on_idle_blur_empty_disable_and_switch() {
        let now = Instant::now();
        let typing = [typing_now(now), Composer::default()];
        assert_eq!(typing_step(true, &typing, None, now), TypingStep::Start);
        // Off by default: nothing is ever published.
        assert_eq!(typing_step(false, &typing, None, now), TypingStep::Nothing);
        // Keystrokes within the renewal interval send nothing more.
        let soon = now + TYPING_RESEND / 2;
        assert_eq!(typing_step(true, &[typing_now(soon)], started(now), soon), TypingStep::Nothing);
        let renew = now + TYPING_RESEND;
        assert_eq!(typing_step(true, &[typing_now(renew)], started(now), renew), TypingStep::Start);
        // Disabling while a start is active stops it at once.
        assert_eq!(typing_step(false, &typing, started(now), now), TypingStep::Stop);
        // Idle, an empty composer (sent or cleared) and blur all stop.
        let idle = now + TYPING_IDLE;
        assert_eq!(typing_step(true, &typing, started(now), idle), TypingStep::Stop);
        let empty = [Composer {
            has_text: false,
            ..typing_now(now)
        }];
        assert_eq!(typing_step(true, &empty, started(now), now), TypingStep::Stop);
        let blurred = [Composer {
            focused: false,
            ..typing_now(now)
        }];
        assert_eq!(typing_step(true, &blurred, started(now), now), TypingStep::Stop);
        // A room, thread or account switch stops the old start first.
        let moved = Some(Started {
            same_target: false,
            at: now,
        });
        assert_eq!(typing_step(true, &typing, moved, now), TypingStep::Stop);
        // Nothing to stop when nothing was started.
        assert_eq!(typing_step(true, &empty, None, now), TypingStep::Nothing);
    }

    #[test]
    fn an_unsent_typing_start_is_dropped_on_blur_empty_send_or_focus_move() {
        let now = Instant::now();
        let typing = typing_now(now);
        assert!(start_wanted(&[typing, Composer::default()]));
        // Blur, including the window becoming inactive.
        let blurred = Composer {
            focused: false,
            ..typing
        };
        assert!(!start_wanted(&[blurred, Composer::default()]));
        // Emptied or sent.
        let empty = Composer {
            has_text: false,
            ..typing
        };
        assert!(!start_wanted(&[empty, Composer::default()]));
        // Focus moved from the room composer to an empty thread composer.
        let thread = Composer {
            focused: true,
            ..Composer::default()
        };
        assert!(!start_wanted(&[blurred, thread]));
        // Still typing in the thread composer.
        assert!(start_wanted(&[blurred, typing]));
    }

    #[test]
    fn the_typing_worker_sleeps_until_renewal_or_idle_and_never_spins() {
        let now = Instant::now();
        let typing = [typing_now(now)];
        assert_eq!(typing_wait(&typing, Some(now), now), TYPING_RESEND);
        assert_eq!(typing_wait(&typing, None, now), TYPING_IDLE);
        // Deadlines already passed never make a zero wait.
        let later = now + 2 * TYPING_IDLE;
        assert_eq!(typing_wait(&typing, Some(now), later), TYPING_POLL);
        assert_eq!(typing_wait(&[Composer::default()], None, now), TYPING_POLL);
    }

    #[test]
    fn a_hidden_or_unfocused_window_never_advances_the_read_receipt() {
        let open = ReadGate {
            tracking_enabled: true,
            live_session: true,
            window_shown: true,
            focused: true,
            at_bottom: true,
            same_scope: true,
        };
        assert!(may_mark_read(&open));
        for gate in [
            ReadGate {
                window_shown: false,
                ..open
            },
            ReadGate {
                focused: false,
                ..open
            },
            // Scrolled up through older history, or a view focused on an
            // older event.
            ReadGate {
                at_bottom: false,
                ..open
            },
            ReadGate {
                tracking_enabled: false,
                ..open
            },
            ReadGate {
                same_scope: false,
                ..open
            },
            ReadGate {
                live_session: false,
                ..open
            },
        ] {
            assert!(!may_mark_read(&gate));
        }
        assert!(!may_mark_read(&ReadGate::default()));
    }

    #[test]
    fn automatic_receipts_settle_and_are_throttled() {
        let now = Instant::now();
        assert_eq!(read_wait(now, None, "$a", now), Some(READ_SETTLE));
        assert_eq!(read_wait(now, None, "$a", now + READ_SETTLE), Some(Duration::ZERO));
        assert_eq!(read_wait(now, None, "", now), None);
        // The same event is never sent twice.
        assert_eq!(read_wait(now, Some(("$a", now)), "$a", now + READ_INTERVAL), None);
        // A newer event waits for the interval since the last receipt.
        let shown = now + READ_SETTLE;
        assert_eq!(
            read_wait(now, Some(("$a", now)), "$b", shown),
            Some(READ_INTERVAL - READ_SETTLE)
        );
        let due = now + READ_INTERVAL;
        assert_eq!(read_wait(now, Some(("$a", now)), "$b", due), Some(Duration::ZERO));
    }

    #[test]
    fn typing_and_receipt_labels_are_bounded_and_untrusted() {
        let hostile = "\u{202e}x".repeat(500);
        let members = [
            member("@alice:example.org", Some("Alice")),
            member("@bob:example.org", None),
            member("@evil:example.org", Some(&hostile)),
        ];
        assert_eq!(typing_text(&members[..1], 0), "Alice is typing…");
        assert_eq!(typing_text(&members[..2], 0), "Alice and bob are typing…");
        let many = typing_text(&members, 40);
        assert!(many.starts_with("Alice, bob and 41 others"));
        let three = typing_text(&members, 0);
        assert!(!three.contains('\u{202e}'));
        assert!(three.chars().count() < 3 * MAX_LABEL_CHARS + 40);
        assert_eq!(typing_text(&[], 3), "");
        assert_eq!(display(&member("@:x", None)), "Someone");
        let seen = SeenBy {
            readers: vec![
                Reader {
                    member: member("@a:x.org", Some("A")),
                    at_ms: None,
                    on_this_event: true,
                },
                Reader {
                    member: member("@b:x.org", None),
                    at_ms: Some(1),
                    on_this_event: false,
                },
            ],
            omitted: 3,
            examined: 4,
        };
        let (rows, summary) = seen_rows(&seen);
        assert_eq!(rows[0].2, "Read up to here");
        assert_eq!(rows[1].2, "Read further");
        assert_eq!(summary, "5 people have read this far. 3 are not listed.");
        let (_, nobody) = seen_rows(&SeenBy::default());
        assert!(nobody.starts_with("Nobody has a public read receipt"));
    }
}
