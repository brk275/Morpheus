//! Bounded local-clock presentation for the timeline and thread reply model.
//!
//! Keep fixed strings in the UI model instead of passing millisecond epochs
//! through Slint's numeric representation. Config reload can then switch the
//! existing rows immediately, without fetching messages or rewriting history.

use chrono::{Local, NaiveTime, TimeZone, Timelike};
use morpheus_matrix::MessageSendState;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ClockLabels {
    pub twelve_hour: String,
    pub twenty_four_hour: String,
    pub twenty_four_hour_seconds: String,
}

impl ClockLabels {
    pub(super) fn for_message(timestamp_ms: u64, state: MessageSendState) -> Self {
        match state {
            MessageSendState::Pending => Self {
                twelve_hour: "Sending…".into(),
                ..Self::default()
            },
            MessageSendState::Failed => Self {
                twelve_hour: "Failed to send".into(),
                ..Self::default()
            },
            MessageSendState::Sent => i64::try_from(timestamp_ms)
                .ok()
                .and_then(|timestamp| Local.timestamp_millis_opt(timestamp).single())
                .map(|date| Self::from_time(date.time()))
                .unwrap_or_default(),
        }
    }

    /// Offline fixtures already carry a 12-hour label. Preserve that label at
    /// the caller and supply alternatives only when it is an actual clock.
    pub(super) fn from_preview(label: &str) -> Self {
        NaiveTime::parse_from_str(label, "%-I:%M %p")
            .map(Self::from_time)
            .unwrap_or_default()
    }

    fn from_time(time: NaiveTime) -> Self {
        // Every requested timeline row needs these labels, even when its rich
        // text is cached. Format the fixed numeric fields directly instead of
        // parsing three Chrono format strings on every projection. The Local
        // conversion remains fresh in for_message; no clock or account cache.
        let hour = time.hour();
        let minute = time.minute();
        let (afternoon, hour12) = time.hour12();
        let period = if afternoon { "PM" } else { "AM" };
        // Chrono represents a leap second as second 59 plus >= 1e9 nanos;
        // its %S formatter displays 60, which the direct projection preserves.
        let second = time.second() + time.nanosecond() / 1_000_000_000;
        Self {
            twelve_hour: format!("{hour12}:{minute:02} {period}"),
            twenty_four_hour: format!("{hour:02}:{minute:02}"),
            twenty_four_hour_seconds: format!("{hour:02}:{minute:02}:{second:02}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_formats_cover_midnight_noon_seconds_and_single_digit_hours() {
        for (hour, minute, second, twelve, twenty_four, seconds) in [
            (0, 0, 0, "12:00 AM", "00:00", "00:00:00"),
            (9, 4, 5, "9:04 AM", "09:04", "09:04:05"),
            (12, 0, 0, "12:00 PM", "12:00", "12:00:00"),
            (15, 4, 5, "3:04 PM", "15:04", "15:04:05"),
            (23, 59, 59, "11:59 PM", "23:59", "23:59:59"),
        ] {
            let labels =
                ClockLabels::from_time(NaiveTime::from_hms_opt(hour, minute, second).unwrap());
            assert_eq!(labels.twelve_hour, twelve);
            assert_eq!(labels.twenty_four_hour, twenty_four);
            assert_eq!(labels.twenty_four_hour_seconds, seconds);
        }
    }

    #[test]
    fn direct_clock_fields_match_chrono_formats_at_boundaries_and_leap_seconds() {
        for (hour, minute, second, nanos) in [
            (0, 0, 0, 0),
            (0, 59, 59, 999_999_999),
            (1, 2, 3, 4),
            (9, 4, 5, 500_000_000),
            (11, 59, 59, 0),
            (12, 0, 0, 0),
            (12, 59, 59, 999_999_999),
            (13, 0, 0, 0),
            (23, 59, 59, 999_999_999),
            (11, 59, 59, 1_000_000_000),
            (23, 59, 59, 1_999_999_999),
        ] {
            let time = NaiveTime::from_hms_nano_opt(hour, minute, second, nanos).unwrap();
            let expected = ClockLabels {
                twelve_hour: time.format("%-I:%M %p").to_string(),
                twenty_four_hour: time.format("%H:%M").to_string(),
                twenty_four_hour_seconds: time.format("%H:%M:%S").to_string(),
            };
            assert_eq!(ClockLabels::from_time(time), expected);
        }
    }

    #[test]
    fn delivery_status_has_no_clock_alternates_and_invalid_epochs_stay_empty() {
        for (state, expected) in [
            (MessageSendState::Pending, "Sending…"),
            (MessageSendState::Failed, "Failed to send"),
        ] {
            for epoch in [0, 1_700_000_000_000, u64::MAX] {
                let labels = ClockLabels::for_message(epoch, state);
                assert_eq!(labels.twelve_hour, expected);
                assert!(labels.twenty_four_hour.is_empty());
                assert!(labels.twenty_four_hour_seconds.is_empty());
            }
        }
        for epoch in [i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            assert_eq!(
                ClockLabels::for_message(epoch, MessageSendState::Sent),
                ClockLabels::default()
            );
        }
    }

    #[test]
    fn live_timestamp_uses_local_timezone_without_losing_seconds() {
        let date = Local
            .with_ymd_and_hms(2026, 1, 15, 15, 4, 5)
            .single()
            .unwrap();
        let labels = ClockLabels::for_message(
            u64::try_from(date.timestamp_millis()).unwrap(),
            MessageSendState::Sent,
        );
        assert_eq!(labels.twelve_hour, "3:04 PM");
        assert_eq!(labels.twenty_four_hour, "15:04");
        assert_eq!(labels.twenty_four_hour_seconds, "15:04:05");
    }

    #[test]
    fn preview_alternates_only_parse_clock_labels() {
        let labels = ClockLabels::from_preview("3:04 PM");
        assert_eq!(labels.twenty_four_hour, "15:04");
        assert_eq!(labels.twenty_four_hour_seconds, "15:04:00");
        for label in ["", "Sending…", "Failed to send", "Yesterday", "25:04 PM"] {
            assert_eq!(ClockLabels::from_preview(label), ClockLabels::default());
        }
    }
}
