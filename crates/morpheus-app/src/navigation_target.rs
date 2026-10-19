//! The one place a conversation can be addressed from outside the room list:
//! a saved account, a joined room and optionally one event and one thread.
//!
//! Notification activation builds these from its own bounded record of shown
//! banners; message search can build them from a listed hit. Both hand the
//! target to `notification_center::activation::Activator::open`, which goes
//! through the protected account switch when the target is in another
//! account and fences every later step by that account's selection. A target
//! holds identifiers only, never message text, and is never persisted.

/// Matrix room and event IDs are at most 255 bytes.
const MAX_MATRIX_ID_BYTES: usize = 255;
/// Local account IDs are UUIDs; leave room for test fixtures.
const MAX_ACCOUNT_ID_BYTES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NavigationTarget {
    /// Opaque local account ID from the account registry.
    account_id: String,
    room_id: String,
    event_id: Option<String>,
    thread_root: Option<String>,
}

impl NavigationTarget {
    /// A target with well-formed, bounded identifiers, or `None`. Validity
    /// says nothing about whether the account, room or event still exist;
    /// the executor checks that against the account store and the room list.
    pub(crate) fn new(
        account_id: &str,
        room_id: &str,
        event_id: Option<&str>,
        thread_root: Option<&str>,
    ) -> Option<Self> {
        let valid = valid_account_id(account_id)
            && valid_matrix_id(room_id, '!')
            && event_id.is_none_or(|event_id| valid_matrix_id(event_id, '$'))
            && thread_root.is_none_or(|root| valid_matrix_id(root, '$'));
        valid.then(|| Self {
            account_id: account_id.to_owned(),
            room_id: room_id.to_owned(),
            event_id: event_id.map(str::to_owned),
            thread_root: thread_root.map(str::to_owned),
        })
    }

    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }

    pub(crate) fn room_id(&self) -> &str {
        &self.room_id
    }

    pub(crate) fn event_id(&self) -> Option<&str> {
        self.event_id.as_deref()
    }

    pub(crate) fn thread_root(&self) -> Option<&str> {
        self.thread_root.as_deref()
    }
}

fn valid_account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ACCOUNT_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_matrix_id(id: &str, sigil: char) -> bool {
    id.len() > 1
        && id.len() <= MAX_MATRIX_ID_BYTES
        && id.starts_with(sigil)
        && !id
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bounded_well_formed_identifiers_become_targets() {
        let target = NavigationTarget::new(
            "0d6c9c1e-6b0e-4a8f-9a55-1f0b6f0c2a11",
            "!room:example.org",
            Some("$event:example.org"),
            Some("$root:example.org"),
        )
        .unwrap();
        assert_eq!(target.room_id(), "!room:example.org");
        assert_eq!(target.event_id(), Some("$event:example.org"));
        assert_eq!(target.thread_root(), Some("$root:example.org"));

        for (account, room, event, root) in [
            ("", "!room:example.org", None, None),
            ("../alice", "!room:example.org", None, None),
            ("alice", "#alias:example.org", None, None),
            ("alice", "!", None, None),
            ("alice", "!room:example.org\n", None, None),
            ("alice", "!room:example.org", Some("event"), None),
            ("alice", "!room:example.org", None, Some("$root example")),
        ] {
            assert!(
                NavigationTarget::new(account, room, event, root).is_none(),
                "{account} {room} {event:?} {root:?}"
            );
        }
        let long = format!("!{}:example.org", "a".repeat(MAX_MATRIX_ID_BYTES));
        assert!(NavigationTarget::new("alice", &long, None, None).is_none());
        assert!(NavigationTarget::new(&"a".repeat(65), "!room:example.org", None, None).is_none());
    }
}
