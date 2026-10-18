//! Native glue for durable send recovery: exact observers for requests the
//! core found at account selection, the per-record composer status surface,
//! and the plain wording shown for queued, paused, kept, cancelled and
//! unconfirmed sends.
//!
//! Every observer is bound to the account selection it was created under and
//! ends when that selection is revoked, so an account switch or removal never
//! lets a late acknowledgement touch another account's drafts. Every control
//! acts on the exact record (nonce and transaction id) and edit revision the
//! user saw when they clicked; nothing here sends, resends or deletes text on
//! its own, and no identifier is shown as text.

use std::{fmt, sync::Arc};

use morpheus_matrix::{
    AccountSelection, Generation, MatrixCoreError, QueuedMessage, QueuedSendUpdate, TextFormat,
};
use slint::{ModelRc, VecModel, Weak};

use crate::{
    AppWindow, PendingSendItem,
    drafts::{PendingError, PendingItem, PendingStatus, PendingSummary, Resolution},
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
            Self::Store(MatrixCoreError::RecoveryIncomplete) => f.write_str(
                "Saved send status for this room is still being checked, so the message was not sent. Try again in a moment; your text is retained.",
            ),
            Self::Store(MatrixCoreError::RecoveryBlocked) => f.write_str(
                "Saved send status here must be reviewed before sending. Your text is retained.",
            ),
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

/// Fixed wording per record state; never an identifier.
pub(crate) fn status_label(status: PendingStatus) -> &'static str {
    match status {
        PendingStatus::Enqueuing | PendingStatus::Queued => "Waiting to send",
        PendingStatus::Paused { recoverable: true } => "Sending is paused; it will be retried",
        PendingStatus::Paused { recoverable: false } => "Sending is paused",
        PendingStatus::Unconfirmed => "May not have finished sending",
        PendingStatus::Kept => "Unsent text kept",
    }
}

/// Fixed wording for a scope whose saved data blocks sends, or empty.
pub(crate) fn blocked_notice(summary: &PendingSummary) -> &'static str {
    if summary.read_only {
        "Saved data here is too large for this version to read, or holds a draft kind or attachments it does not handle. It is kept untouched; nothing typed here is saved until you discard it."
    } else if summary.quarantined {
        "Saved send status here could not be read by this version. It is kept with your drafts; sending here waits until you review it."
    } else {
        ""
    }
}

fn items_model(items: &[PendingItem]) -> ModelRc<PendingSendItem> {
    ModelRc::new(VecModel::from(
        items
            .iter()
            .map(|item| PendingSendItem {
                key: item.nonce.as_key().into(),
                status: status_label(item.status).into(),
                preview: item.preview.clone().into(),
                can_cancel: item.has_request,
                can_retry: matches!(item.status, PendingStatus::Paused { .. }),
                unconfirmed: item.status == PendingStatus::Unconfirmed,
                kept: item.status == PendingStatus::Kept,
            })
            .collect::<Vec<_>>(),
    ))
}

pub(crate) fn publish_room(window: &AppWindow, summary: &PendingSummary, items: &[PendingItem]) {
    window.set_composer_pending_count(i32::try_from(summary.count).unwrap_or(i32::MAX));
    window.set_composer_pending_blocked(summary.blocked);
    window.set_composer_unconfirmed(summary.unconfirmed);
    window.set_composer_blocked_notice(blocked_notice(summary).into());
    window.set_composer_pending_items(items_model(items));
}

pub(crate) fn publish_thread(window: &AppWindow, summary: &PendingSummary, items: &[PendingItem]) {
    window.set_thread_send_pending(summary.count > 0);
    window.set_thread_pending_blocked(summary.blocked);
    window.set_thread_unconfirmed(summary.unconfirmed);
    window.set_thread_blocked_notice(blocked_notice(summary).into());
    window.set_thread_pending_items(items_model(items));
}

/// Both strips back to nothing: used when the account detaches or the room
/// selection changes, so no departed scope's state survives.
pub(crate) fn clear_recovery_ui(window: &AppWindow) {
    let empty = PendingSummary::default();
    publish_room(window, &empty, &[]);
    publish_thread(window, &empty, &[]);
}

/// Publish the scope's status if that scope is what the window shows.
pub(crate) fn publish_scope(window: &AppWindow, live: &LiveUi, scope: &SendScope) {
    if !scope_visible(window, scope) {
        return;
    }
    let summary = live.pending_summary(&scope.account, &scope.room, scope.thread());
    let items = live.pending_items(&scope.account, &scope.room, scope.thread());
    if scope.thread.is_some() {
        publish_thread(window, &summary, &items);
    } else {
        publish_room(window, &summary, &items);
    }
}

/// Put restored record text into the visible composer, but only while the
/// scope is still shown and no edit happened since the mutation that
/// produced it (`resolution.revision`). An empty composer is still a newer
/// edit when its revision moved on; it is never overwritten.
pub(crate) fn apply_resolution(
    window: &AppWindow,
    live: &LiveUi,
    scope: &SendScope,
    resolution: &Resolution,
) -> bool {
    let Some(text) = &resolution.restored else {
        return false;
    };
    if !scope_visible(window, scope)
        || live.scoped_revision(&scope.account, &scope.room, scope.thread()) != Some(resolution.revision)
    {
        return false;
    }
    if scope.thread.is_some() {
        window.set_thread_composer_text(text.body.clone().into());
        window.set_thread_markdown(text.format == TextFormat::Markdown);
    } else {
        window.set_composer_text(text.body.clone().into());
        window.set_composer_markdown(text.format == TextFormat::Markdown);
    }
    true
}

pub(crate) fn update_notice(update: QueuedSendUpdate, resolution: Option<&Resolution>) -> &'static str {
    let restored = resolution.is_some_and(|resolution| resolution.restored.is_some());
    let kept = resolution.is_some_and(|resolution| resolution.kept);
    match update {
        QueuedSendUpdate::Sent => "Message sent",
        QueuedSendUpdate::Cancelled if restored => {
            "Sending cancelled; your text is back in the composer. A message that had already reached the server stays sent."
        }
        QueuedSendUpdate::Cancelled if kept => {
            "Sending cancelled; the unsent text is kept below the composer. A message that had already reached the server stays sent."
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
/// wait itself ends when the account selection is revoked, so a queue that
/// never emits again cannot pin the task, and nothing runs for a departed
/// selection.
/// Whether a cache mutation for `scope` is still authorized by the selection
/// the work started under: that selection must still be current and name the
/// scope's own account. Every observer and completion path checks this again
/// after each await, immediately before touching the cache, so a core call
/// that outlives the selection (a switch, a removal or A->B->A) changes
/// nothing in the departed account's cache and schedules no write. The exact
/// record, its transaction id and the account epoch stay the authority for
/// what a current selection may change.
pub(crate) fn still_owned(selection: &AccountSelection, scope: &SendScope) -> bool {
    selection.is_current() && selection.account_id() == scope.account
}

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
                .apply_pending_update(&selection, &scope, &nonce, &transaction_id, update, &ui)
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
        let restored = Resolution {
            restored: Some(morpheus_matrix::ComposeText::plain("x")),
            kept: false,
            revision: 1,
        };
        let kept = Resolution {
            restored: None,
            kept: true,
            revision: 1,
        };
        for resolution in [None, Some(&restored), Some(&kept)] {
            assert!(update_notice(QueuedSendUpdate::Cancelled, resolution).contains("stays sent"));
        }
        assert!(update_notice(QueuedSendUpdate::Cancelled, Some(&kept)).contains("kept"));
        assert!(update_notice(QueuedSendUpdate::Unknown, None).contains("Could not confirm"));
        assert!(update_notice(QueuedSendUpdate::Retained { recoverable: false }, None).contains("duplicate"));
        let labels = [
            status_label(PendingStatus::Queued),
            status_label(PendingStatus::Paused { recoverable: false }),
            status_label(PendingStatus::Unconfirmed),
            status_label(PendingStatus::Kept),
            update_notice(QueuedSendUpdate::Sent, None),
            update_notice(QueuedSendUpdate::Cancelled, Some(&restored)),
            update_notice(QueuedSendUpdate::Unknown, None),
        ];
        for notice in labels {
            for jargon in ["nonce", "transaction", "ledger", "generation", "txn", "warning"] {
                assert!(!notice.to_lowercase().contains(jargon), "{notice}");
            }
        }
        assert!(status_label(PendingStatus::Unconfirmed).contains("May not"));
    }

    #[test]
    fn prepare_errors_keep_the_text_and_name_no_identifiers() {
        let record = PrepareError::Record(PendingError::Mismatch).to_string();
        let store = PrepareError::Store(MatrixCoreError::NoActiveAccount).to_string();
        let waiting = PrepareError::Store(MatrixCoreError::RecoveryIncomplete).to_string();
        assert!(record.contains("Try again"));
        assert!(store.contains("not sent") && store.contains("retained"));
        assert!(waiting.contains("still being checked") && waiting.contains("retained"));
        assert!(!store.contains("account"), "store failures are not reflected");
    }

    #[test]
    fn blocked_notices_distinguish_read_only_from_carried_data_and_stay_quiet_otherwise() {
        let mut summary = PendingSummary::default();
        assert_eq!(blocked_notice(&summary), "");
        summary.quarantined = true;
        assert!(blocked_notice(&summary).contains("review"));
        summary.read_only = true;
        assert!(blocked_notice(&summary).contains("kept untouched"));
    }
}
