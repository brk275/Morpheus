//! Unconfirmed push-rule changes outlive their particular Settings view.
//! Only account/scope IDs and monotonic counters are held, never rule text.

use std::collections::BTreeMap;

use morpheus_matrix::MAX_NOTIFICATION_ROOMS;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum ChangeScope {
    Room(String),
    Default(bool),
    Keywords,
}

pub(super) struct ChangeTicket {
    account: String,
    scope: ChangeScope,
}

#[derive(Clone, Copy)]
pub(super) struct SyncMark(u64);

#[derive(Default)]
pub(super) struct Uncertainty {
    failed: u64,
    pending: BTreeMap<(String, ChangeScope), u64>,
    // Never evict an unresolved account/scope to satisfy the bound. At the
    // existing room-list limit, retain a conservative process-wide warning.
    // Likewise, counter exhaustion cannot make old work current again.
    saturated: bool,
}

impl Uncertainty {
    pub(super) fn begin(&self, account: &str, scope: ChangeScope) -> ChangeTicket {
        ChangeTicket {
            account: account.to_owned(),
            scope,
        }
    }

    /// Called on the worker as soon as the write ends, before any UI-owned
    /// callback can be retired. A success cannot erase an earlier unknown
    /// change: the SDK can return Ok based only on its cached room rules, and
    /// a keyword success does not confirm other keywords. Only a full sync
    /// after the failure settles it.
    pub(super) fn complete(&mut self, ticket: ChangeTicket, confirmed: bool) -> SyncMark {
        if !confirmed {
            let key = (ticket.account, ticket.scope);
            self.failed = match self.failed.checked_add(1) {
                Some(next) => next,
                None => {
                    self.saturated = true;
                    u64::MAX
                }
            };
            if let Some(pending) = self.pending.get_mut(&key) {
                *pending = self.failed;
            } else if self.pending.len() < MAX_NOTIFICATION_ROOMS {
                self.pending.insert(key, self.failed);
            } else {
                self.saturated = true;
            }
        }
        self.mark()
    }

    pub(super) fn mark(&self) -> SyncMark {
        SyncMark(self.failed)
    }

    pub(super) fn has(&self, account: &str, room: Option<&str>) -> bool {
        self.saturated
            || self.pending.keys().any(|(known, scope)| {
                known == account
                    && match (room, scope) {
                        (Some(room), ChangeScope::Room(known)) => known == room,
                        _ => true,
                    }
            })
    }

    /// A full push-rule event observed after `mark` settles only failures
    /// already recorded then, never a newer write that ended while waiting.
    pub(super) fn synced(&mut self, account: &str, mark: SyncMark) {
        self.pending
            .retain(|(known, _), failed| known != account || *failed > mark.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str) -> ChangeScope {
        ChangeScope::Room(id.to_owned())
    }

    #[test]
    fn a_retired_presentation_does_not_own_completion_or_multiple_rooms() {
        let mut state = Uncertainty::default();
        let first = state.begin("a", room("!one"));
        let second = state.begin("a", room("!two"));
        // The worker records both, even if their UI callbacks are discarded
        // after close, refresh or reopening a different room/account.
        let earlier = state.complete(first, false);
        let mark = state.complete(second, false);
        assert!(state.has("a", Some("!one")));
        assert!(state.has("a", Some("!two")));
        assert!(!state.has("b", None));
        assert!(state.has("a", None));
        state.synced("b", mark);
        assert!(state.has("a", Some("!one")));
        state.synced("a", earlier);
        assert!(!state.has("a", Some("!one")));
        assert!(state.has("a", Some("!two")));
        state.synced("a", mark);
        assert!(!state.has("a", None));
    }

    #[test]
    fn an_older_wait_or_success_does_not_settle_a_newer_unknown_write() {
        let mut state = Uncertainty::default();
        let old_success = state.begin("a", room("!one"));
        let first = state.begin("a", room("!one"));
        let old_wait = state.complete(first, false);
        let later = state.begin("a", room("!one"));
        let latest = state.complete(later, false);
        state.synced("a", old_wait);
        state.complete(old_success, true);
        assert!(state.has("a", Some("!one")));
        state.synced("a", latest);
        assert!(!state.has("a", None));
    }

    #[test]
    fn defaults_and_keywords_affect_their_account_and_survive_unrelated_success() {
        let mut state = Uncertainty::default();
        let defaults = state.begin("a", ChangeScope::Default(false));
        state.complete(defaults, false);
        let keywords = state.begin("b", ChangeScope::Keywords);
        state.complete(keywords, false);
        let unrelated = state.begin("a", room("!one"));
        state.complete(unrelated, true);
        // Another keyword may succeed, but cannot confirm the earlier one.
        let another_keyword = state.begin("b", ChangeScope::Keywords);
        state.complete(another_keyword, true);
        assert!(state.has("a", Some("!one")));
        assert!(state.has("b", Some("!other")));
        assert!(!state.has("c", None));
    }

    #[test]
    fn reaching_the_bound_never_forgets_an_unresolved_scope() {
        let mut state = Uncertainty::default();
        for index in 0..=MAX_NOTIFICATION_ROOMS {
            let ticket = state.begin("a", room(&format!("!{index}")));
            state.complete(ticket, false);
        }
        assert_eq!(state.pending.len(), MAX_NOTIFICATION_ROOMS);
        assert!(state.has("a", Some("!0")));
        assert!(state.has("a", Some(&format!("!{MAX_NOTIFICATION_ROOMS}"))));
        let mark = state.mark();
        state.synced("a", mark);
        assert!(
            state.has("a", None),
            "saturation keeps a conservative warning"
        );
    }
}
