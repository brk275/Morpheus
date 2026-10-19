//! Scroll-aware unread state for the conversation on screen.
//!
//! Visibility comes from the timeline's own row delegates: whenever Rust
//! bumps `timeline-probe` from a timer, every live row of the virtualized
//! ListView reports its index, event ID and real geometry as last painted.
//! Nothing here estimates which rows are visible from average row heights.
//! A row counts as seen only after it stayed readable for `SEEN_DWELL` while
//! the window was the active, unobstructed, live conversation, and a read
//! receipt moves only through messages that were all on screen at some
//! point (or that an explicit catch-up, Jump to latest or sending, covered).
//! The first-unread divider, the unread counter, Jump to latest and scroll
//! anchoring use the same reports.
//!
//! This file holds the pure rules; `ui.rs` holds the UI-thread controller
//! and the bounded receipt worker.

pub(crate) mod ui;

pub(crate) use ui::{
    FeedScope, after_publish, after_thread_publish, caught_up, foreground, register, reset,
};

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use morpheus_matrix::MessageInfo;

/// A row must stay readable this long, in the foreground, to count as seen.
pub(crate) const SEEN_DWELL: Duration = Duration::from_millis(900);
/// How much of a row must be on screen: all of a short row, else this many
/// logical pixels.
const MIN_READABLE_PX: f32 = 40.0;
const EDGE_EPSILON: f32 = 0.5;
/// The last row's bottom within this distance of the viewport's bottom
/// counts as showing the latest message.
const LATEST_SLACK_PX: f32 = 2.0;
/// Corrections smaller than this are not worth another layout pass.
const SETTLED_PX: f32 = 0.75;
const MAX_COUNT_LABEL: usize = 99;

pub(crate) const DIVIDER_TEXT: &str = "New messages";
pub(crate) const EARLIER_TEXT: &str = "Unread messages start before the loaded history";

/// One live row's real geometry as its delegate reported it, in logical
/// pixels relative to the top of the visible timeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub(crate) top: f32,
    pub(crate) height: f32,
    pub(crate) viewport: f32,
}

impl Geometry {
    fn valid(self) -> bool {
        self.top.is_finite()
            && self.height.is_finite()
            && self.viewport.is_finite()
            && self.height > 0.0
            && self.viewport > 0.0
    }
    pub(crate) fn bottom(self) -> f32 {
        self.top + self.height
    }
    /// Enough of the row is inside the viewport to be read.
    pub(crate) fn readable(self) -> bool {
        if !self.valid() {
            return false;
        }
        let shown = self.bottom().min(self.viewport) - self.top.max(0.0);
        shown + EDGE_EPSILON >= self.height.min(MIN_READABLE_PX)
    }
    /// Some part of the row is inside the viewport.
    pub(crate) fn on_screen(self) -> bool {
        self.valid() && self.bottom() > EDGE_EPSILON && self.top < self.viewport - EDGE_EPSILON
    }
    /// The row's bottom edge is inside the viewport.
    pub(crate) fn ends_on_screen(self) -> bool {
        self.valid() && self.bottom() > 0.0 && self.bottom() <= self.viewport + LATEST_SLACK_PX
    }
}

/// One report from the latest probe, checked against the Rust model.
#[derive(Clone, Debug)]
pub(crate) struct Report {
    pub(crate) index: usize,
    pub(crate) row: Arc<MessageInfo>,
    pub(crate) geometry: Geometry,
}

/// The event a read receipt may carry for this row: a remote event from
/// someone else. Own messages, local echoes, previews and stress rows never
/// carry one.
pub(crate) fn receipt_target(row: &MessageInfo) -> Option<&str> {
    let event_id = row.event_id.as_deref()?;
    let remote = !row.is_local_echo && event_id.starts_with('$');
    (remote && !row.is_own).then_some(event_id)
}

/// Rows continuously readable since some instant under one open gate.
#[derive(Debug, Default)]
pub(crate) struct Dwell {
    since: HashMap<String, Instant>,
}

impl Dwell {
    /// Record the rows readable now and return those readable for at least
    /// `SEEN_DWELL`. A row that left the screen starts over.
    pub(crate) fn observe(&mut self, now: Instant, readable: &[&str]) -> Vec<String> {
        self.since.retain(|key, _| readable.contains(&key.as_str()));
        let mut seen = Vec::new();
        for key in readable {
            let since = *self.since.entry((*key).to_owned()).or_insert(now);
            if now.saturating_duration_since(since) >= SEEN_DWELL {
                seen.push((*key).to_owned());
            }
        }
        seen
    }
    pub(crate) fn clear(&mut self) {
        self.since.clear();
    }
}

/// Index of the row with `key` in `rows`.
pub(crate) fn index_of(rows: &[Arc<MessageInfo>], key: &str) -> Option<usize> {
    rows.iter().position(|row| row.key == key)
}

/// Index of the row carrying `event_id` in `rows`.
pub(crate) fn index_of_event(rows: &[Arc<MessageInfo>], event_id: &str) -> Option<usize> {
    rows.iter()
        .position(|row| row.event_id.as_deref() == Some(event_id))
}

/// The newest row a receipt may reach. Receipts are positional: one on a row
/// reads everything before it too. So walking on from `read_through`, the
/// last row known to be read, every message from someone else must have
/// been on screen (`glimpsed`), or be at or before `caught_up`, the latest
/// row when the reader explicitly caught up; the walk stops before the first
/// one that was not, and at `seen`, the newest row seen for the full dwell.
pub(crate) fn receipt_frontier(
    rows: &[Arc<MessageInfo>],
    read_through: Option<usize>,
    caught_up: Option<usize>,
    seen: usize,
    glimpsed: &HashSet<String>,
) -> Option<usize> {
    let last = seen.min(rows.len().checked_sub(1)?);
    let start = read_through.map_or(0, |index| index + 1);
    let mut frontier = read_through.filter(|&index| index < rows.len());
    for (index, row) in rows.iter().enumerate().take(last + 1).skip(start) {
        let covered = glimpsed.contains(&row.key) || caught_up.is_some_and(|up| index <= up);
        if receipt_target(row).is_some() && !covered {
            break;
        }
        frontier = Some(index);
    }
    frontier
}

/// The newest receipt target at or before `frontier`.
pub(crate) fn receipt_candidate(rows: &[Arc<MessageInfo>], frontier: usize) -> Option<usize> {
    let last = frontier.min(rows.len().checked_sub(1)?);
    (0..=last)
        .rev()
        .find(|&index| receipt_target(&rows[index]).is_some())
}

/// No message from someone else follows `index`.
pub(crate) fn covers_latest(rows: &[Arc<MessageInfo>], index: usize) -> bool {
    rows.iter()
        .skip(index + 1)
        .all(|row| receipt_target(row).is_none())
}

/// Unread messages below what is on screen: messages from someone else after
/// both the last readable row and the newest seen row, that are either at or
/// after the first unread row or arrived after the view opened.
pub(crate) fn unread_below(
    rows: &[Arc<MessageInfo>],
    below: Option<usize>,
    unread_from: Option<usize>,
    fresh_from: Option<usize>,
) -> usize {
    let start = below.map_or(0, |index| index + 1);
    let unread = |index: usize| {
        unread_from.is_some_and(|from| index >= from)
            || fresh_from.is_some_and(|from| index >= from)
    };
    rows.iter()
        .enumerate()
        .skip(start)
        .filter(|(index, row)| unread(*index) && receipt_target(row).is_some())
        .count()
}

pub(crate) fn counter_text(count: usize) -> String {
    match count {
        0 => "Jump to latest".to_owned(),
        1 => "1 new message".to_owned(),
        count if count > MAX_COUNT_LABEL => format!("{MAX_COUNT_LABEL}+ new messages"),
        count => format!("{count} new messages"),
    }
}

pub(crate) fn jump_label(count: usize) -> String {
    match count {
        0 => "Jump to the latest message".to_owned(),
        1 => "Jump to the latest message; 1 unread message below".to_owned(),
        count => format!("Jump to the latest message; {count} unread messages below"),
    }
}

/// Where the timeline should settle.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ScrollTarget {
    /// The row with this key, its top this far below the viewport's top.
    Row { key: String, top: f32 },
    /// The last row, its bottom at the viewport's bottom.
    End,
}

/// The next move towards a target, from the latest reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Correction {
    /// Already in place.
    Done,
    /// Move the content up by this many pixels (down when negative).
    Shift(f32),
    /// No live row to measure from.
    Unknown,
}

/// The pixel correction that brings `target` (at `target_index` of
/// `row_count` rows) into place. A live target row is placed exactly from
/// its own geometry; otherwise the list moves towards it by the real average
/// height of the live rows. A list already at its end or start is done.
pub(crate) fn correction(
    target: &ScrollTarget,
    target_index: usize,
    row_count: usize,
    reports: &[Report],
) -> Correction {
    if let Some(report) = reports.iter().find(|report| report.index == target_index) {
        let delta = match target {
            ScrollTarget::Row { top, .. } => report.geometry.top - top,
            // Only move down: a short list already shows its end.
            ScrollTarget::End => (report.geometry.bottom() - report.geometry.viewport).max(0.0),
        };
        let at_end = reports.iter().any(|report| {
            report.index + 1 == row_count && report.geometry.ends_on_screen()
        });
        let at_start = reports
            .iter()
            .any(|report| report.index == 0 && report.geometry.top >= -EDGE_EPSILON);
        let stuck = (delta > 0.0 && at_end) || (delta < 0.0 && at_start);
        return if delta.abs() < SETTLED_PX || stuck {
            Correction::Done
        } else {
            Correction::Shift(delta)
        };
    }
    let (Some(low), Some(high)) = (
        reports.iter().min_by_key(|report| report.index),
        reports.iter().max_by_key(|report| report.index),
    ) else {
        return Correction::Unknown;
    };
    let Some(average) = average_height(reports) else {
        return Correction::Unknown;
    };
    if target_index > high.index {
        let rows = (target_index - high.index) as f32;
        let past = (high.geometry.bottom() - high.geometry.viewport).max(0.0);
        Correction::Shift(rows * average + past)
    } else {
        let rows = low.index.saturating_sub(target_index) as f32;
        Correction::Shift(-(rows * average) + low.geometry.top.min(0.0))
    }
}

/// The real average height of the live rows.
pub(crate) fn average_height(reports: &[Report]) -> Option<f32> {
    let total: f32 = reports.iter().map(|report| report.geometry.height).sum();
    let average = total / reports.len() as f32;
    (average.is_finite() && average > 0.0).then_some(average)
}

/// The topmost on-screen row and its top: what a reader keeps looking at
/// while rows are added or removed above it.
pub(crate) fn anchor(reports: &[Report]) -> Option<(String, f32)> {
    reports
        .iter()
        .filter(|report| report.geometry.on_screen())
        .min_by_key(|report| report.index)
        .map(|report| (report.row.key.clone(), report.geometry.top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::MessageSendState;

    fn row(
        key: &str,
        event_id: Option<&str>,
        is_own: bool,
        is_local_echo: bool,
    ) -> Arc<MessageInfo> {
        Arc::new(MessageInfo {
            key: key.to_owned(),
            event_id: event_id.map(str::to_owned),
            sender: "@casey:example.org".to_owned(),
            body: "hello".to_owned(),
            rich_markdown: None,
            rich_accessible: None,
            attachment_name: None,
            attachment_is_image: false,
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own,
            is_local_echo,
            send_state: MessageSendState::Sent,
        })
    }
    fn report(index: usize, key: &str, top: f32, height: f32) -> Report {
        let geometry = Geometry {
            top,
            height,
            viewport: 400.0,
        };
        Report {
            index,
            row: row(key, None, false, false),
            geometry,
        }
    }

    #[test]
    fn only_rows_really_inside_the_viewport_are_readable() {
        let at = |top: f32, height: f32| Geometry {
            top,
            height,
            viewport: 400.0,
        };
        assert!(at(0.0, 60.0).readable());
        assert!(!at(380.0, 30.0).readable());
        assert!(at(-20.0, 60.0).readable());
        assert!(!at(-30.0, 60.0).readable());
        assert!(at(350.0, 600.0).readable());
        assert!(!at(400.0, 60.0).readable());
        assert!(!at(-60.0, 60.0).readable());
        assert!(!at(f32::NAN, 60.0).readable());
        let hidden = Geometry {
            top: 0.0,
            height: 60.0,
            viewport: 0.0,
        };
        assert!(!hidden.readable());
        assert!(at(340.0, 60.0).ends_on_screen());
        assert!(!at(345.0, 60.0).ends_on_screen());
    }

    #[test]
    fn a_row_is_seen_only_after_staying_readable_for_the_dwell() {
        let mut dwell = Dwell::default();
        let start = Instant::now();
        assert!(dwell.observe(start, &["a", "b"]).is_empty());
        let later = start + SEEN_DWELL;
        assert_eq!(dwell.observe(later, &["a"]), ["a"]);
        // "b" left the screen and starts over.
        let again = later + Duration::from_millis(100);
        assert_eq!(dwell.observe(again, &["a", "b"]), ["a"]);
        dwell.clear();
        assert!(dwell.observe(again + SEEN_DWELL, &["a"]).is_empty());
    }

    #[test]
    fn receipts_target_only_remote_events_from_others_before_what_was_seen() {
        let rows = [
            row("a", Some("$a"), false, false),
            row("b", Some("$b"), true, false),
            row("c", None, false, true),
            row("d", Some("$d"), false, true),
            row("e", Some("preview:1"), false, false),
            row("f", Some("$f"), false, false),
        ];
        assert_eq!(receipt_candidate(&rows, 4), Some(0));
        assert_eq!(receipt_candidate(&rows, 5), Some(5));
        assert_eq!(receipt_candidate(&rows, 99), Some(5));
        assert_eq!(receipt_candidate(&[], 0), None);
        assert!(covers_latest(&rows, 5));
        assert!(!covers_latest(&rows, 0));
    }

    #[test]
    fn receipts_never_skip_messages_that_were_not_on_screen() {
        let rows = [
            row("a", Some("$a"), false, false),
            row("b", Some("$b"), false, false),
            row("c", Some("$c"), true, false),
            row("d", Some("$d"), false, false),
            row("e", Some("$e"), false, false),
        ];
        let glimpsed: HashSet<String> = ["b", "d", "e"].map(str::to_owned).into();
        // "b" was read before; "c" is the reader's own; "d" and "e" were seen.
        assert_eq!(receipt_frontier(&rows, Some(1), None, 4, &glimpsed), Some(4));
        // Opened at the bottom: "a" was never on screen, so nothing moves.
        assert_eq!(receipt_frontier(&rows, None, None, 4, &glimpsed), None);
        // Jump to latest covers what it skipped.
        assert_eq!(receipt_frontier(&rows, None, Some(4), 4, &glimpsed), Some(4));
        // Never past the newest row seen for the full dwell.
        assert_eq!(receipt_frontier(&rows, Some(1), None, 3, &glimpsed), Some(3));
        assert_eq!(receipt_frontier(&[], None, None, 0, &glimpsed), None);
    }

    #[test]
    fn the_counter_counts_unread_messages_below_the_screen_only() {
        let rows = [
            row("a", Some("$a"), false, false),
            row("b", Some("$b"), false, false),
            row("c", Some("$c"), true, false),
            row("d", Some("$d"), false, false),
            row("e", Some("$e"), false, false),
        ];
        assert_eq!(unread_below(&rows, Some(1), Some(1), None), 2);
        assert_eq!(unread_below(&rows, Some(1), None, Some(4)), 1);
        assert_eq!(unread_below(&rows, Some(4), Some(0), None), 0);
        assert_eq!(unread_below(&rows, None, Some(0), None), 4);
        assert_eq!(unread_below(&rows, Some(0), None, None), 0);
        assert_eq!(counter_text(1), "1 new message");
        assert_eq!(counter_text(250), "99+ new messages");
    }

    #[test]
    fn corrections_place_live_targets_exactly_and_approach_others() {
        let reports = [
            report(10, "k10", -20.0, 100.0),
            report(11, "k11", 80.0, 100.0),
        ];
        let target = ScrollTarget::Row {
            key: "k11".to_owned(),
            top: 40.0,
        };
        assert_eq!(correction(&target, 11, 20, &reports), Correction::Shift(40.0));
        // The same move is impossible when row 11 is the last row: done.
        assert_eq!(correction(&target, 11, 12, &reports), Correction::Done);
        let placed = ScrollTarget::Row {
            key: "k10".to_owned(),
            top: -20.0,
        };
        assert_eq!(correction(&placed, 10, 20, &reports), Correction::Done);
        let end = ScrollTarget::End;
        assert_eq!(correction(&end, 11, 12, &reports), Correction::Done);
        assert_eq!(correction(&end, 15, 16, &reports), Correction::Shift(400.0));
        assert_eq!(correction(&target, 7, 20, &reports), Correction::Shift(-320.0));
        assert_eq!(correction(&end, 3, 4, &[]), Correction::Unknown);
        assert_eq!(anchor(&reports), Some(("k10".to_owned(), -20.0)));
    }
}
