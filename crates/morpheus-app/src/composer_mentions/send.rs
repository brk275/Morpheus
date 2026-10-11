//! Shared live send adapter. Queue acceptance never clears a reply/edit/thread;
//! every UI completion is bound to the selection, scope and local edit revision.

use super::{
    Scope, Target,
    ui::{ComposerUi, Submission},
};
use crate::{AppWindow, live::LiveUi};
use morpheus_matrix::QueuedSendUpdate;
use slint::ComponentHandle;
use std::sync::Arc;
use tokio::runtime::Handle;

const MAX_PENDING_SCOPES: usize = 64;

#[derive(Clone)]
pub(super) struct SendLease {
    scope: Scope,
    identity: Arc<()>,
}
#[derive(Default)]
pub(super) struct SendLedger {
    leases: Vec<SendLease>,
}
impl SendLedger {
    pub(super) fn reserve(&mut self, scope: Scope) -> Option<SendLease> {
        if self.leases.len() >= MAX_PENDING_SCOPES
            || self.leases.iter().any(|lease| lease.scope == scope)
        {
            return None;
        }
        let lease = SendLease {
            scope,
            identity: Arc::new(()),
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
        let Some(lease) = self.reserve_send(&submission.scope) else {
            if !threaded {
                window.set_composer_busy(true);
            }
            window.set_transfer_notice("A previous send is still queued or unconfirmed; it may still send. Check the conversation after restarting before retrying.".into());
            return;
        };
        let account = &submission.scope.account;
        let room = &submission.scope.room;
        let thread = match &submission.scope.target {
            Target::Thread(root) => Some(root.as_str()),
            _ => None,
        };
        if let Some(root) = thread {
            if !live.begin_thread_send(account, room, root) {
                self.release_send(&lease);
                window
                    .set_transfer_notice("Thread reply is still queued; it may still send".into());
                return;
            }
        }
        let durable = matches!(submission.scope.target, Target::Room | Target::Thread(_));
        if durable
            && live
                .remember_composed_draft(account, room, thread, submission.text.clone())
                .is_err()
        {
            if self.release_send(&lease) {
                if let Some(root) = thread {
                    live.finish_thread_send(account, room, root);
                }
            }
            if !threaded {
                window.set_composer_busy(false);
            }
            window
                .set_session_error("Could not retain this draft. The message was not sent.".into());
            return;
        }
        let revision = if let Some(root) = thread {
            live.thread_draft_revision(account, room, root)
        } else if durable {
            live.draft_revision(account, room)
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
            let result = live.send_composed(
                submission.selection.clone(), submission.scope.room.clone(), submission.target(),
                trimmed, submission.baseline.clone(),
            ).await;
            let mut queued = match result {
                Ok(queued) => queued,
                Err(error) => {
                    let uncertain = matches!(error, morpheus_matrix::MatrixCoreError::ComposedSendUncertain);
                    if !uncertain && owner.release_send(&lease) {
                        if let Target::Thread(root) = &submission.scope.target {
                            live.finish_thread_send(&submission.scope.account, &submission.scope.room, root);
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |window| {
                        if visible(&window, &submission, operation) {
                            if !uncertain {
                                if let Target::Thread(root) = &submission.scope.target {
                                    window.set_thread_send_pending(live.thread_send_pending(&submission.scope.account, &submission.scope.room, root));
                                } else { window.set_composer_busy(false); }
                            }
                            window.set_session_error(if uncertain {
                                "Send could not be confirmed; the request may still send. Your text is retained. Restart, then check the conversation before retrying."
                            } else {
                                "The message could not be prepared. Your text and selected mentions are retained; an edit may need to be reopened."
                            }.into());
                        }
                    });
                    return;
                }
            };
            let room_send = matches!(submission.scope.target, Target::Room);
            let tracked = room_send && revision.is_some_and(|revision| live.mark_composed_queued(
                &submission.scope.account, &submission.scope.room, &submission.text, revision,
                queued.transaction_id(),
            ));
            if room_send {
                owner.release_send(&lease);
                let shown = submission.clone(); let current = live.clone(); let composer = owner.clone();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if visible(&window, &shown, operation) {
                        if tracked { composer.complete(&window, &current, &shown); }
                        window.set_composer_busy(false);
                    }
                });
            }
            let transaction = queued.transaction_id().to_owned();
            loop {
                let update = queued.next_update().await;
                let mut restored = false;
                let mut cleared_revision = None;
                if room_send && tracked && matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled) {
                    restored = live.resolve_queued_draft(&submission.scope.account, &submission.scope.room,
                        &transaction, update == QueuedSendUpdate::Sent);
                }
                if let Target::Thread(root) = &submission.scope.target {
                    if update == QueuedSendUpdate::Sent {
                        cleared_revision = revision.and_then(|revision| live.acknowledge_composed_thread(
                            &submission.scope.account, &submission.scope.room, root, &submission.text, revision,
                        ));
                    }
                    if matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled) && owner.release_send(&lease) {
                        live.finish_thread_send(&submission.scope.account, &submission.scope.room, root);
                    }
                }
                if !threaded && !room_send && matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled) {
                    owner.release_send(&lease);
                }
                let terminal = matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled | QueuedSendUpdate::Unknown);
                let shown = submission.clone(); let current = live.clone(); let composer = owner.clone();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if !visible(&window, &shown, operation) { return; }
                    if let Target::Thread(root) = &shown.scope.target {
                        window.set_thread_send_pending(current.thread_send_pending(&shown.scope.account, &shown.scope.room, root));
                    }
                    if update == QueuedSendUpdate::Sent && !room_send {
                        let may_clear = match &shown.scope.target {
                            Target::Thread(root) => cleared_revision.is_some()
                                && current.thread_draft_revision(&shown.scope.account, &shown.scope.room, root) == cleared_revision,
                            _ => true,
                        };
                        if may_clear { composer.complete(&window, &current, &shown); }
                    }
                    if update == QueuedSendUpdate::Cancelled && room_send && restored
                        && window.get_composer_mode() == 0 && window.get_composer_text().is_empty() {
                        composer.restore_normal(&window, &current);
                    }
                    if !threaded && !room_send && matches!(update, QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled) {
                        window.set_composer_busy(false);
                    }
                    let notice = match update {
                        QueuedSendUpdate::Sent => "Message sent",
                        QueuedSendUpdate::Cancelled => "Queued message cancelled; draft retained",
                        QueuedSendUpdate::Retained { recoverable: true } => "Message queued for retry; it may still send",
                        QueuedSendUpdate::Retained { recoverable: false } => "Message send paused; the request remains queued. Do not send a duplicate",
                        QueuedSendUpdate::Unknown => "Could not confirm send; the request may still send. Restart, then check the conversation before sending again",
                    };
                    window.set_transfer_notice(notice.into());
                });
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
            .reserve(scope("a", Target::Thread("$one".into())))
            .unwrap();
        assert!(ledger.reserve(lease.scope.clone()).is_none());
        assert!(
            ledger
                .reserve(scope("b", Target::Thread("$one".into())))
                .is_some()
        );
        assert!(
            ledger
                .reserve(scope("a", Target::Thread("$two".into())))
                .is_some()
        );
    }
    #[test]
    fn old_completion_cannot_release_new_same_scope_after_return() {
        let mut ledger = SendLedger::default();
        let scope = scope("a", Target::Room);
        let old = ledger.reserve(scope.clone()).unwrap();
        assert!(ledger.release(&old));
        let newer = ledger.reserve(scope.clone()).unwrap();
        assert!(!ledger.release(&old));
        assert!(ledger.reserve(scope).is_none());
        assert!(ledger.release(&newer));
    }
    #[test]
    fn uncertain_scopes_are_bounded_without_evicting_the_first() {
        let mut ledger = SendLedger::default();
        let first = ledger.reserve(scope("a", Target::Room)).unwrap();
        for i in 1..MAX_PENDING_SCOPES {
            assert!(
                ledger
                    .reserve(scope("a", Target::Reply(format!("$r{i}"))))
                    .is_some()
            );
        }
        assert!(ledger.reserve(scope("b", Target::Room)).is_none());
        assert!(ledger.reserve(first.scope.clone()).is_none());
        assert!(ledger.release(&first));
        assert!(ledger.reserve(scope("b", Target::Room)).is_some());
    }
}
