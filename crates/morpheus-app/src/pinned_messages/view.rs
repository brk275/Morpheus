//! Pure projection of pinned rows, summaries and fixed wording. Server text
//! never reaches an error string, and row text is bounded twice: once in the
//! core and again here for the pane.

use morpheus_matrix::{
    MAX_PINNED_DISPLAY, PinnedMessage, PinnedMessageKind, PinnedMessagesError, PinnedState,
};

use crate::PinnedMessageItem;

pub(super) const MAX_FILTER_CHARS: usize = 200;
const ROW_TEXT_CHARS: usize = 300;
const SENDER_CHARS: usize = 100;
const SENDER_ID_CHARS: usize = 120;

pub(super) fn status_text(kind: PinnedMessageKind) -> &'static str {
    match kind {
        PinnedMessageKind::Message => "",
        PinnedMessageKind::Redacted => "This message was deleted.",
        PinnedMessageKind::Undecryptable => "This device can't decrypt this message.",
        PinnedMessageKind::Unsupported => "Morpheus can't show this kind of pinned event.",
        PinnedMessageKind::Unavailable => {
            "This message isn't available right now. Refresh to try again."
        }
    }
}

pub(super) fn item(message: &PinnedMessage, can_unpin: bool) -> PinnedMessageItem {
    let sender_id = crate::directory_plain_text(&message.sender_id, SENDER_ID_CHARS);
    let sender = crate::directory_plain_text(&message.sender_name, SENDER_CHARS);
    PinnedMessageItem {
        event_id: message.event_id.as_str().into(),
        thread_id: message.thread_root.as_deref().unwrap_or("").into(),
        sender: if sender.is_empty() {
            sender_id.as_str().into()
        } else {
            sender.into()
        },
        sender_id: sender_id.into(),
        time: if message.timestamp_ms == 0 {
            "".into()
        } else {
            crate::search_time(message.timestamp_ms).into()
        },
        text: crate::directory_plain_text(&message.text, ROW_TEXT_CHARS).into(),
        status: status_text(message.kind).into(),
        attachment: message
            .attachment
            .as_deref()
            .map(|text| crate::directory_plain_text(text, 140))
            .unwrap_or_default()
            .into(),
        warning: message.authenticity_warning,
        edited: message.edited,
        can_open: !message.event_id.is_empty(),
        can_unpin,
    }
}

/// Lowercase, bounded search text. Empty matches everything.
pub(super) fn needle(filter: &str) -> String {
    filter
        .chars()
        .take(MAX_FILTER_CHARS)
        .collect::<String>()
        .trim()
        .to_lowercase()
}

pub(super) fn matches(message: &PinnedMessage, needle: &str) -> bool {
    needle.is_empty()
        || [
            message.sender_name.as_str(),
            message.sender_id.as_str(),
            message.text.as_str(),
            message.attachment.as_deref().unwrap_or(""),
            status_text(message.kind),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(needle))
}

pub(super) fn summary(state: &PinnedState) -> String {
    let total = state.event_ids.len();
    let mut text = match total {
        0 => "No pinned messages".to_owned(),
        1 => "1 pinned message".to_owned(),
        n if n > MAX_PINNED_DISPLAY => {
            format!("Showing the newest {MAX_PINNED_DISPLAY} of {n} pinned messages")
        }
        n => format!("{n} pinned messages"),
    };
    match state.unsupported_entries {
        0 => {}
        1 => text.push_str(" · 1 list entry isn't a message and can't be shown"),
        n => text.push_str(&format!(
            " · {n} list entries aren't messages and can't be shown"
        )),
    }
    text
}

pub(super) fn read_only_note(state: &PinnedState) -> &'static str {
    if !state.permitted {
        "Your permissions in this room don't allow changing pins."
    } else if !state.editable {
        "This room's pinned list has a format Morpheus won't change, so it is read-only here."
    } else {
        ""
    }
}

pub(super) fn load_error(error: PinnedMessagesError) -> &'static str {
    match error {
        PinnedMessagesError::Cancelled | PinnedMessagesError::AccountChanged => "",
        PinnedMessagesError::NotJoined => "Join this room to see its pinned messages.",
        PinnedMessagesError::RateLimited => {
            "The homeserver is limiting requests. Wait, then refresh."
        }
        PinnedMessagesError::TimedOut => "Loading pinned messages timed out. Refresh to try again.",
        PinnedMessagesError::InvalidState => {
            "The homeserver returned pinned-message state Morpheus can't read."
        }
        PinnedMessagesError::Unavailable
        | PinnedMessagesError::Unsupported
        | PinnedMessagesError::PermissionDenied
        | PinnedMessagesError::TooMany
        | PinnedMessagesError::TooLarge => "Pinned messages are unavailable. Refresh to try again.",
    }
}

pub(super) fn write_error(error: PinnedMessagesError) -> &'static str {
    match error {
        PinnedMessagesError::Cancelled | PinnedMessagesError::AccountChanged => "",
        PinnedMessagesError::NotJoined => "Join this room to change its pins.",
        PinnedMessagesError::PermissionDenied => {
            "Your permissions in this room don't allow changing pins."
        }
        PinnedMessagesError::TooMany => {
            "This room already has the most pins Morpheus will add. Unpin one first."
        }
        PinnedMessagesError::TooLarge => "The pinned list would be too large to save.",
        PinnedMessagesError::Unsupported => {
            "This room's pinned list has a format Morpheus won't change."
        }
        PinnedMessagesError::RateLimited => {
            "The homeserver is limiting requests. Wait, then try again."
        }
        PinnedMessagesError::InvalidState => {
            "The homeserver returned pinned-message state Morpheus can't read."
        }
        // The single request may still have reached the homeserver.
        PinnedMessagesError::TimedOut | PinnedMessagesError::Unavailable => {
            "The change may have reached your homeserver. Check the refreshed list before trying again."
        }
    }
}

pub(super) fn write_notice(pin: bool, changed: bool, confirmed: bool) -> &'static str {
    match (pin, changed, confirmed) {
        (true, true, true) => "Message pinned.",
        (false, true, true) => "Message unpinned.",
        (true, true, false) => "Message pinned. Refreshing the list…",
        (false, true, false) => "Message unpinned. Refreshing the list…",
        (true, false, _) => "This message is already pinned.",
        (false, false, _) => "This message is no longer pinned.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: PinnedMessageKind) -> PinnedMessage {
        PinnedMessage {
            event_id: "$e:example.org".into(),
            kind,
            sender_id: "@riley:example.org".into(),
            sender_name: "Riley\u{202e}".into(),
            timestamp_ms: 0,
            text: "Hello\u{200b} world".into(),
            attachment: None,
            thread_root: Some("$root:example.org".into()),
            edited: true,
            authenticity_warning: true,
        }
    }

    #[test]
    fn rows_are_sanitized_and_say_why_text_is_missing() {
        let row = item(&message(PinnedMessageKind::Message), true);
        assert_eq!(row.sender.as_str(), "Riley");
        assert_eq!(row.text.as_str(), "Hello  world");
        assert_eq!(row.thread_id.as_str(), "$root:example.org");
        assert_eq!(row.time.as_str(), "");
        assert!(row.status.is_empty() && row.can_unpin && row.can_open);
        for kind in [
            PinnedMessageKind::Redacted,
            PinnedMessageKind::Undecryptable,
            PinnedMessageKind::Unsupported,
            PinnedMessageKind::Unavailable,
        ] {
            assert!(!item(&message(kind), false).status.is_empty());
        }
        let mut unnamed = message(PinnedMessageKind::Redacted);
        unnamed.sender_name.clear();
        assert_eq!(item(&unnamed, false).sender.as_str(), "@riley:example.org");
    }

    #[test]
    fn filter_matches_sender_text_and_status_case_insensitively() {
        let message = message(PinnedMessageKind::Message);
        assert!(matches(&message, &needle("  RILEY ")));
        assert!(matches(&message, &needle("world")));
        assert!(matches(&message, &needle("")));
        assert!(!matches(&message, &needle("absent")));
        let deleted = super::tests::message(PinnedMessageKind::Redacted);
        assert!(matches(&deleted, &needle("deleted")));
        assert_eq!(needle(&"x".repeat(1000)).chars().count(), MAX_FILTER_CHARS);
    }

    #[test]
    fn summaries_count_shown_hidden_and_unsupported_entries() {
        let mut state = PinnedState {
            event_ids: vec!["$a:example.org".into()],
            unsupported_entries: 0,
            permitted: true,
            editable: true,
        };
        assert_eq!(summary(&state), "1 pinned message");
        assert_eq!(read_only_note(&state), "");
        state.event_ids = (0..MAX_PINNED_DISPLAY + 5)
            .map(|i| format!("$e{i}:example.org"))
            .collect();
        state.unsupported_entries = 2;
        assert!(summary(&state).starts_with("Showing the newest 50 of 55"));
        assert!(summary(&state).contains("2 list entries"));
        state.permitted = false;
        assert!(!read_only_note(&state).is_empty());
    }

    #[test]
    fn errors_are_fixed_text_and_account_changes_are_silent() {
        for error in [
            PinnedMessagesError::NotJoined,
            PinnedMessagesError::Unavailable,
            PinnedMessagesError::RateLimited,
            PinnedMessagesError::TimedOut,
            PinnedMessagesError::InvalidState,
            PinnedMessagesError::Unsupported,
            PinnedMessagesError::PermissionDenied,
            PinnedMessagesError::TooMany,
            PinnedMessagesError::TooLarge,
        ] {
            assert!(!load_error(error).is_empty());
            assert!(!write_error(error).is_empty());
        }
        assert!(load_error(PinnedMessagesError::AccountChanged).is_empty());
        assert!(write_error(PinnedMessagesError::Cancelled).is_empty());
    }
}
