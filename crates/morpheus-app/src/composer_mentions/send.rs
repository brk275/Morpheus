//! Shared live send adapter. Queue acceptance never clears a reply/edit/thread;
//! every UI completion is bound to the selection, scope and local edit revision.
//!
//! Room and thread sends are recorded in the encrypted draft row before the
//! SDK sees them and resolved only by their exact transaction id. Replies and
//! edits have no durable draft and keep the in-session lease only. Every wait
//! on the SDK ends with the account selection, and every exit path releases
//! the lease it held, so a paused queue or a switch cannot pin a scope.

use super::{
    Scope, Target,
    ui::{ComposerUi, Submission},
};
use crate::{AppWindow, draft_recovery::SendScope, live::LiveUi};
use morpheus_matrix::{AccountSelection, QueuedSendUpdate};
use slint::ComponentHandle;
use std::sync::Arc;
use tokio::runtime::Handle;

const MAX_PENDING_SCOPES: usize = 64;

#[derive(Clone)]
pub(super) struct SendLease {
    scope: Scope,
    identity: Arc<()>,
    /// The selection the send runs under; a lease outlives it only until the
    /// next reservation, which evicts leases of departed selections.
    selection: Option<AccountSelection>,
}
#[derive(Default)]
pub(super) struct SendLedger {
    leases: Vec<SendLease>,
}
impl SendLedger {
    pub(super) fn reserve(
        &mut self,
        scope: Scope,
        selection: Option<AccountSelection>,
    ) -> Option<SendLease> {
        // Leases of a revoked selection can never complete; drop them so a
        // switch or removal cannot leave a scope blocked or the ledger full.
        self.leases.retain(|lease| {
            lease
                .selection
                .as_ref()
                .is_none_or(AccountSelection::is_current)
        });
        if self.leases.len() >= MAX_PENDING_SCOPES
            || self.leases.iter().any(|lease| lease.scope == scope)
        {
            return None;
        }
        let lease = SendLease {
            scope,
            identity: Arc::new(()),
            selection,
        };
        self.leases.push(lease.clone());
        Some(lease)
    }
    pub(super) fn release(&mut self, expected: &SendLease) -> bool {
        let Some(index) = self.leases.iter().position(|lease| {
            lease.scope == expected.scope && Arc::ptr_eq(&lease.identity, &expected.identity)
        }) else {
            return false;
        };
        self.leases.remove(index);
        true
    }
}

fn send_scope(submission: &Submission) -> SendScope {
    SendScope {
        account: submission.scope.account.clone(),
        room: submission.scope.room.clone(),
        thread: match &submission.scope.target {
            Target::Thread(root) => Some(root.clone()),
            _ => None,
        },
    }
}

impl ComposerUi {
    pub fn send(&self, window: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, threaded: bool) {
        let submission = match self.snapshot(window, live, threaded) {
            Ok(value) => value,
            Err(_) => {
                if !threaded {
                    window.set_composer_busy(false);
                }
                window.set_session_error("This composer changed or its edit could not be restored. Reopen the message before sending.".into());
                return;
            }
        };
        let trimmed = match submission.text.trimmed_for_send() {
            Ok(text) if !text.body.is_empty() => text,
            Ok(_) => {
                if !threaded {
                    window.set_composer_busy(false);
                }
                return;
            }
            Err(_) => {
                if !threaded {
                    window.set_composer_busy(false);
                }
                window.set_session_error(
                    "The message or its selected mentions are invalid; your draft is retained."
                        .into(),
                );
                return;
            }
        };
        let durable = matches!(submission.scope.target, Target::Room | Target::Thread(_));
        let scope = send_scope(&submission);
        if durable {
            let summary = live.pending_summary(&scope.account, &scope.room, scope.thread());
            if summary.blocks_sends() {
                if !threaded {
                    window.set_composer_busy(false);
                }
                window.set_transfer_notice(if summary.unconfirmed {
                    "An earlier message here may not have finished sending. Check the conversation and choose before sending again."
                } else {
                    "Saved send status here must be reviewed before sending."
                }.into());
                return;
            }
        }
        let Some(lease) = self.reserve_send(&submission.scope, &submission.selection) else {
            if !threaded {
                window.set_composer_busy(true);
            }
            window.set_transfer_notice("A previous send is still being recorded; wait a moment before retrying.".into());
            return;
        };
        if durable
            && live
                .remember_composed_draft(&scope.account, &scope.room, scope.thread(), submission.text.clone())
                .is_err()
        {
            self.release_send(&lease);
            if !threaded {
                window.set_composer_busy(false);
            }
            window
                .set_session_error("Could not retain this draft. The message was not sent.".into());
            return;
        }
        let revision = if durable {
            live.scoped_revision(&scope.account, &scope.room, scope.thread())
        } else {
            None
        };
        if threaded {
            window.set_thread_send_pending(true);
        }
        let operation = window.get_composer_operation();
        let owner = self.clone();
        let live = live.clone();
        let weak = window.as_weak();
        runtime.spawn(async move {
            let selection = submission.selection.clone();
            let fail = |message: &'static str, release: bool| {
                let shown = submission.clone();
                let current = live.clone();
                let composer = owner.clone();
                let lease = lease.clone();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if release {
                        composer.release_send(&lease);
                    }
                    if visible(&window, &shown, operation) {
                        if release && !threaded {
                            window.set_composer_busy(false);
                        }
                        crate::draft_recovery::publish_scope(&window, &current, &send_scope(&shown));
                        window.set_session_error(message.into());
                    }
                });
            };
            // Room and thread sends are saved as pending, with the room's
            // thread index, before the SDK assigns a transaction id. The core
            // then issues the single-use enqueue grant for that record.
            let handle = if durable {
                let Some(revision) = revision else {
                    fail("Could not record this message's send status. It was not sent; your text is retained.", true);
                    return;
                };
                match live
                    .prepare_pending_send(&selection, &scope, &submission.text, revision)
                    .await
                {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        let message = error.to_string();
                        let shown = submission.clone();
                        let current = live.clone();
                        let composer = owner.clone();
                        let _ = weak.upgrade_in_event_loop(move |window| {
                            composer.release_send(&lease);
                            if visible(&window, &shown, operation) {
                                if !threaded { window.set_composer_busy(false); }
                                crate::draft_recovery::publish_scope(&window, &current, &send_scope(&shown));
                                window.set_session_error(message.into());
                            }
                        });
                        return;
                    }
                }
            } else {
                None
            };
            let result = live.send_composed(
                selection.clone(), submission.scope.room.clone(), submission.target(),
                trimmed, submission.baseline.clone(), handle.as_ref().map(|handle| handle.nonce()),
            ).await;
            let mut queued = match result {
                Ok(queued) => queued,
                Err(error) => {
                    let uncertain = matches!(error, morpheus_matrix::MatrixCoreError::ComposedSendUncertain);
                    if let Some(handle) = &handle {
                        if uncertain {
                            // The SDK may hold the request without our id: the
                            // saved enqueuing record stays, marked for review.
                            live.mark_pending_unconfirmed(&scope, handle.nonce()).await;
                        } else {
                            live.abandon_pending_send(&scope, handle.nonce()).await;
                        }
                    }
                    // A durable record now guards the scope, so its lease can
                    // go; an uncertain reply or edit keeps the in-session lease
                    // until the selection changes.
                    let release = !uncertain || handle.is_some();
                    fail(
                        if uncertain {
                            "Send could not be confirmed; the request may still send. Your text is retained. Check the conversation, then choose."
                        } else {
                            "The message could not be prepared. Your text and selected mentions are retained; an edit may need to be reopened."
                        },
                        release,
                    );
                    return;
                }
            };
            let transaction = queued.transaction_id().to_owned();
            // The exact transaction id is saved before any acknowledgement is
            // acted on; an unchanged composer is cleared, the record holds the
            // submitted text.
            let hidden = match &handle {
                Some(handle) => live
                    .record_pending_queued(&scope, handle, &transaction)
                    .await
                    .unwrap_or(false),
                None => false,
            };
            let room_send = matches!(submission.scope.target, Target::Room);
            {
                let shown = submission.clone();
                let current = live.clone();
                let composer = owner.clone();
                let lease = lease.clone();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if room_send {
                        // Several room messages may wait at once; the lease
                        // covered only the recording window.
                        composer.release_send(&lease);
                    }
                    if visible(&window, &shown, operation) {
                        if hidden { composer.complete(&window, &current, &shown); }
                        if room_send { window.set_composer_busy(false); }
                        crate::draft_recovery::publish_scope(&window, &current, &send_scope(&shown));
                    }
                });
            }
            loop {
                // A paused queue may never emit again: the wait ends with the
                // selection, and the lease goes with it.
                let update = tokio::select! {
                    _ = selection.revoked() => {
                        owner.release_send(&lease);
                        return;
                    }
                    update = queued.next_update() => update,
                };
                if !selection.is_current() {
                    owner.release_send(&lease);
                    return;
                }
                let terminal = matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled | QueuedSendUpdate::Unknown);
                if let Some(handle) = &handle {
                    live.apply_pending_update(&selection, &scope, handle.nonce(), &transaction, update, &weak).await;
                    if terminal && !room_send && owner.release_send(&lease) {
                        let shown = submission.clone();
                        let current = live.clone();
                        let _ = weak.upgrade_in_event_loop(move |window| {
                            if visible(&window, &shown, operation) {
                                crate::draft_recovery::publish_scope(&window, &current, &send_scope(&shown));
                            }
                        });
                    }
                } else {
                    // Replies and edits: lease only, no durable record. An
                    // unknown outcome keeps the lease until the selection ends.
                    if matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled) {
                        owner.release_send(&lease);
                    }
                    let shown = submission.clone();
                    let current = live.clone();
                    let composer = owner.clone();
                    let _ = weak.upgrade_in_event_loop(move |window| {
                        if !visible(&window, &shown, operation) { return; }
                        if update == QueuedSendUpdate::Sent {
                            composer.complete(&window, &current, &shown);
                        }
                        if matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled) {
                            window.set_composer_busy(false);
                        }
                        window.set_transfer_notice(crate::draft_recovery::update_notice(update, None).into());
                    });
                }
                if terminal { break; }
            }
        });
    }
}

fn visible(window: &AppWindow, submission: &Submission, operation: i32) -> bool {
    submission.selection.is_current()
        && window.get_live_session()
        && window.get_active_account_local_id().as_str() == submission.scope.account
        && window.get_active_room_id().as_str() == submission.scope.room
        && match &submission.scope.target {
            Target::Thread(root) => window.get_thread_root_id().as_str() == root,
            Target::Room => window.get_composer_operation() == operation,
            Target::Reply(event) => {
                window.get_composer_operation() == operation
                    && window.get_composer_mode() == 1
                    && window.get_composer_target_id().as_str() == event
            }
            Target::Edit(event) => {
                window.get_composer_operation() == operation
                    && window.get_composer_mode() == 2
                    && window.get_composer_target_id().as_str() == event
            }
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(account: &str, target: Target) -> Scope {
        Scope {
            account: account.into(),
            room: "!room:example.org".into(),
            target,
        }
    }
    #[test]
    fn pending_scope_blocks_duplicates_but_not_other_accounts_or_threads() {
        let mut ledger = SendLedger::default();
        let lease = ledger
            .reserve(scope("a", Target::Thread("$one".into())), None)
            .unwrap();
        assert!(ledger.reserve(lease.scope.clone(), None).is_none());
        assert!(
            ledger
                .reserve(scope("b", Target::Thread("$one".into())), None)
                .is_some()
        );
        assert!(
            ledger
                .reserve(scope("a", Target::Thread("$two".into())), None)
                .is_some()
        );
    }
    #[test]
    fn old_completion_cannot_release_new_same_scope_after_return() {
        let mut ledger = SendLedger::default();
        let scope = scope("a", Target::Room);
        let old = ledger.reserve(scope.clone(), None).unwrap();
        assert!(ledger.release(&old));
        let newer = ledger.reserve(scope.clone(), None).unwrap();
        assert!(!ledger.release(&old));
        assert!(ledger.reserve(scope, None).is_none());
        assert!(ledger.release(&newer));
    }
    #[test]
    fn uncertain_scopes_are_bounded_without_evicting_the_first() {
        let mut ledger = SendLedger::default();
        let first = ledger.reserve(scope("a", Target::Room), None).unwrap();
        for i in 1..MAX_PENDING_SCOPES {
            assert!(
                ledger
                    .reserve(scope("a", Target::Reply(format!("$r{i}"))), None)
                    .is_some()
            );
        }
        assert!(ledger.reserve(scope("b", Target::Room), None).is_none());
        assert!(ledger.reserve(first.scope.clone(), None).is_none());
        assert!(ledger.release(&first));
        assert!(ledger.reserve(scope("b", Target::Room), None).is_some());
    }
}
