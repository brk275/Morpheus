//! Native glue for durable send recovery: exact observers for requests the
//! core found at account selection, the composer status strip, and the plain
//! wording shown for queued, paused, cancelled and unconfirmed sends.
//!
//! Every observer is bound to the account selection it was created under and
//! ends when that selection is revoked, so an account switch or removal never
//! lets a late acknowledgement touch another account's drafts. Nothing here
//! sends, resends or deletes text; the user decides unconfirmed outcomes.

use std::{fmt, sync::Arc};

use morpheus_matrix::{
    AccountSelection, Generation, MatrixCoreError, QueuedMessage, QueuedSendUpdate,
};
use slint::Weak;

use crate::{
    AppWindow,
    drafts::{PendingError, PendingSummary},
    live::LiveUi,
};

/// One draft scope: an account's room, optionally one thread in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendScope {
    pub account: String,
    pub room: String,
    pub thread: Option<String>,
}

impl SendScope {
    pub fn thread(&self) -> Option<&str> {
        self.thread.as_deref()
    }
}

/// Why a send could not be recorded before enqueue. The message was not sent
/// and the composer text is unchanged in either case.
#[derive(Debug)]
pub enum PrepareError {
    Record(PendingError),
    Store(MatrixCoreError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::Store(_) => f.write_str(
                "Could not save this message's send status, so it was not sent. Your text is retained.",
            ),
        }
    }
}

/// The scope the given composer currently edits, if any.
pub(crate) fn current_scope(window: &AppWindow, threaded: bool) -> Option<SendScope> {
    let account = window.get_active_account_local_id().to_string();
    let room = window.get_active_room_id().to_string();
    if account.is_empty() || room.is_empty() {
        return None;
    }
    let thread = if threaded {
        let root = window.get_thread_root_id().to_string();
        if root.is_empty() {
            return None;
        }
        Some(root)
    } else {
        None
    };
    Some(SendScope {
        account,
        room,
        thread,
    })
}

pub(crate) fn scope_visible(window: &AppWindow, scope: &SendScope) -> bool {
    window.get_live_session()
        && window.get_active_account_local_id().as_str() == scope.account
        && window.get_active_room_id().as_str() == scope.room
        && match &scope.thread {
            Some(root) => window.get_thread_root_id().as_str() == root,
            None => true,
        }
}

pub(crate) fn publish_room(window: &AppWindow, summary: &PendingSummary) {
    window.set_composer_pending_count(i32::try_from(summary.count).unwrap_or(i32::MAX));
    window.set_composer_pending_blocked(summary.blocked);
    window.set_composer_unconfirmed(summary.unconfirmed || summary.metadata_unreadable);
    window.set_composer_retained_available(summary.retained_available);
}

pub(crate) fn publish_thread(window: &AppWindow, summary: &PendingSummary) {
    window.set_thread_send_pending(summary.count > 0);
    window.set_thread_pending_blocked(summary.blocked);
    window.set_thread_unconfirmed(summary.unconfirmed || summary.metadata_unreadable);
    window.set_thread_retained_available(summary.retained_available);
}

/// Publish the scope's status if that scope is what the window shows.
pub(crate) fn publish_scope(window: &AppWindow, live: &LiveUi, scope: &SendScope) {
    if !scope_visible(window, scope) {
        return;
    }
    let summary = live.pending_summary(&scope.account, &scope.room, scope.thread());
    if scope.thread.is_some() {
        publish_thread(window, &summary);
    } else {
        publish_room(window, &summary);
    }
}

pub(crate) fn update_notice(update: QueuedSendUpdate, restored: bool) -> &'static str {
    match update {
        QueuedSendUpdate::Sent => "Message sent",
        QueuedSendUpdate::Cancelled if restored => {
            "Sending cancelled; your text is back in the composer. A message that had already reached the server stays sent."
        }
        QueuedSendUpdate::Cancelled => {
            "Sending cancelled. A message that had already reached the server stays sent."
        }
        QueuedSendUpdate::Retained { recoverable: true } => {
            "Message queued for retry; it may still send"
        }
        QueuedSendUpdate::Retained { recoverable: false } => {
            "Sending is paused. Retry or cancel; do not send a duplicate."
        }
        QueuedSendUpdate::Unknown => {
            "Could not confirm whether this message was sent. Check the conversation, then choose."
        }
    }
}

/// Observe one request the SDK still holds for an exact saved record. The
/// task stops when the account selection is revoked and never outlives it.
pub(crate) fn spawn_observer(
    live: Arc<LiveUi>,
    ui: Weak<AppWindow>,
    selection: AccountSelection,
    scope: SendScope,
    nonce: Generation,
    transaction_id: String,
    mut queued: QueuedMessage,
) {
    tokio::spawn(async move {
        loop {
            let update = tokio::select! {
                _ = selection.revoked() => return,
                update = queued.next_update() => update,
            };
            if !selection.is_current() {
                return;
            }
            if live
                .apply_pending_update(&scope, &nonce, &transaction_id, update, &ui)
                .await
            {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_never_claim_delivery_was_prevented_or_infer_success() {
        for restored in [true, false] {
            let cancelled = update_notice(QueuedSendUpdate::Cancelled, restored);
            assert!(cancelled.contains("stays sent"));
        }
        assert!(update_notice(QueuedSendUpdate::Unknown, false).contains("Could not confirm"));
        assert!(update_notice(QueuedSendUpdate::Retained { recoverable: false }, false).contains("duplicate"));
        for notice in [
            update_notice(QueuedSendUpdate::Sent, false),
            update_notice(QueuedSendUpdate::Cancelled, true),
            update_notice(QueuedSendUpdate::Unknown, false),
        ] {
            for jargon in ["nonce", "transaction", "ledger", "generation", "txn"] {
                assert!(!notice.to_lowercase().contains(jargon), "{notice}");
            }
        }
    }

    #[test]
    fn prepare_errors_keep_the_text_and_name_no_identifiers() {
        let record = PrepareError::Record(PendingError::Mismatch).to_string();
        let store = PrepareError::Store(MatrixCoreError::NoActiveAccount).to_string();
        assert!(record.contains("Try again"));
        assert!(store.contains("not sent") && store.contains("retained"));
        assert!(!store.contains("account"), "store failures are not reflected");
    }
}
