// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.
//
// Morpheus modification: independently retry failed current media-key deliveries;
// see MORPHEUS_PATCH.md.

//! The task that keeps one joined slot session alive, spawned by
//! [`SlotSession::join`](crate::SlotSession::join) on the
//! [`executor`](crate::executor) and stopped by the leave, by dropping the
//! session, or through its [`AbortHandle`](crate::executor::AbortHandle).
//!
//! Two wake-ups: the keep-alive interval (restart the delayed leave, refresh
//! the sticky membership), and the deadline of a coalesced key rotation,
//! followed through the encryption manager's watch so a member who left during
//! a key's window is locked out when the window ends. It holds the membership
//! machine and the encryption manager, never the room: a tick does not stall
//! anything else the room does.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::{Semaphore, watch};

use crate::encryption::{EncryptionManager, KeyRetry, UndeliveredKey};
use crate::executor::{self, AbortOnDrop, JoinHandleExt};
use crate::host::backend::MatrixBackend;
use crate::own_membership::{OwnMembershipMachine, OwnMembershipState, now_ms};

/// What a joined slot session's upkeep needs.
pub(crate) struct SessionUpkeep<T: MatrixBackend + 'static> {
    pub machine: Arc<OwnMembershipMachine<T>>,
    pub encryption: Option<EncryptionManager<T>>,
    pub interval: Duration,
    pub log_tag: String,
}

impl<T: MatrixBackend + 'static> SessionUpkeep<T> {
    /// Starts it, or returns `None` with nowhere to run it (natively, no
    /// current tokio runtime): the host then ticks
    /// [`SlotSession::keep_alive`](crate::SlotSession::keep_alive) itself.
    pub fn spawn(self) -> Option<AbortOnDrop<()>> {
        if !executor::can_spawn() {
            log::warn!(
                "[{}] no runtime to run the keep-alive on; the host must tick it",
                self.log_tag,
            );
            return None;
        }
        Some(executor::spawn(self.run()).abort_on_drop())
    }

    async fn run(self) {
        let Self {
            machine,
            encryption,
            interval,
            log_tag,
        } = self;
        let mut rotation_due = encryption
            .as_ref()
            .map(EncryptionManager::subscribe_rotation_due);
        // The deadline a flush last answered, so one that leaves it in place
        // (the flush failed) waits for the next tick instead of spinning.
        let mut flushed_due: Option<u64> = None;
        let mut beat = pin!(executor::sleep(interval));
        // This owner retains the controller's abort handle. The separate task
        // keeps retry scheduling moving even while a keep-alive awaits the host.
        let _key_retries = encryption.as_ref().map(|encryption| {
            executor::spawn(retry_keys(encryption.clone())).abort_on_drop()
        });

        loop {
            let due = rotation_due
                .as_ref()
                .and_then(|due| *due.borrow())
                .filter(|due| Some(*due) != flushed_due);
            let due_in = due.map(|due| Duration::from_millis(due.saturating_sub(now_ms())));

            tokio::select! {
                _ = &mut beat => {
                    tick(&machine, encryption.as_ref(), &log_tag).await;
                    if machine.state() != OwnMembershipState::Joined {
                        break;
                    }
                    flushed_due = None;
                    beat.set(executor::sleep(interval));
                }
                _ = executor::sleep(due_in.unwrap_or_default()), if due_in.is_some() => {
                    if let Some(encryption) = &encryption {
                        flush(encryption, &log_tag).await;
                    }
                    flushed_due = due;
                }
                changed = changed(&mut rotation_due) => {
                    if !changed {
                        rotation_due = None;
                    }
                }
            }
        }
        log::debug!("[{log_tag}] upkeep stopped");
    }
}

/// One keep-alive tick: the membership machine's, then a due rotation, so a
/// missed deadline wake-up makes a rotation late by one tick, never lost.
pub(crate) async fn tick<T: MatrixBackend + 'static>(
    machine: &OwnMembershipMachine<T>,
    encryption: Option<&EncryptionManager<T>>,
    log_tag: &str,
) {
    log::trace!("[{log_tag}] keep-alive");
    machine.keep_alive().await;
    if let Some(encryption) = encryption {
        flush(encryption, log_tag).await;
    }
}

async fn flush<T: MatrixBackend + 'static>(encryption: &EncryptionManager<T>, log_tag: &str) {
    if let Err(error) = encryption.flush_due_rotation().await {
        log::warn!("[{log_tag}] a deferred key rotation failed: {error:?}");
    }
}

/// Resolves on the next change; `false` once the sender is gone. Never
/// resolves without a receiver.
async fn changed(receiver: &mut Option<watch::Receiver<Option<u64>>>) -> bool {
    match receiver {
        Some(receiver) => receiver.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

// These are source-fixed retry limits, not caller/manifest-selected inputs.
const KEY_RETRY_INITIAL: Duration = Duration::from_secs(1);
const KEY_RETRY_MAX: Duration = Duration::from_secs(30);
const KEY_RETRY_ATTEMPT: Duration = Duration::from_secs(30);
const KEY_RETRY_IN_FLIGHT: usize = 8;

/// The existing upkeep owns this controller. There is one worker per current
/// debt, at most eight host retry calls in flight, and no retained key payload
/// while a worker waits. Dropping the controller aborts all its workers.
pub(crate) async fn retry_keys<T: MatrixBackend + 'static>(encryption: EncryptionManager<T>) {
    let mut updates = encryption.subscribe_key_deliveries();
    let capacity = Arc::new(Semaphore::new(KEY_RETRY_IN_FLIGHT));
    let mut workers: HashMap<String, (Arc<UndeliveredKey>, Option<AbortOnDrop<()>>)> =
        HashMap::new();
    loop {
        let owed = updates.borrow_and_update().owed.clone();
        let retired: Vec<_> = workers
            .iter()
            .filter(|(member, (token, _))| {
                !owed.get(*member)
                    .is_some_and(|current| Arc::ptr_eq(current, token))
            })
            .map(|(member, _)| member.clone())
            .collect();
        let retired: Vec<_> = retired
            .into_iter()
            .filter_map(|member| workers.remove(&member).and_then(|(_, task)| task))
            .collect();
        // Cancel the whole retired batch before awaiting any one of it. Native
        // JoinHandle completion retires the owned future before replacements;
        // the existing wasm executor reports abort without that join guarantee.
        for task in &retired {
            task.abort_handle().abort();
        }
        for task in retired {
            let _ = task.await;
        }
        for (member, token) in owed {
            if !encryption.key_retry_is_current(&token) {
                continue;
            }
            workers.entry(member).or_insert_with(|| {
                let task = executor::spawn(retry_key(
                    encryption.clone(),
                    token.clone(),
                    capacity.clone(),
                ))
                .abort_on_drop();
                (token, Some(task))
            });
        }
        tokio::select! {
            biased;
            // Poll real task completion before unrelated debt notifications.
            // Consume the join result before retiring its handle. A completed
            // token stays inert until its debt changes: do not restart a failed
            // task automatically or retain a native-owned task for that token.
            member = poll_fn(|cx| {
                for (member, (_, task)) in &mut workers {
                    if let Some(task) = task.as_mut() {
                        if Pin::new(task).poll(cx).is_ready() {
                            return Poll::Ready(member.clone());
                        }
                    }
                }
                Poll::Pending
            }) => {
                if let Some((_, task)) = workers.get_mut(&member) {
                    *task = None;
                }
            }
            changed = updates.changed() => {
                if changed.is_err() {
                    return std::future::pending().await;
                }
            }
        }
    }
}

async fn retry_key<T: MatrixBackend + 'static>(
    encryption: EncryptionManager<T>,
    owed: Arc<UndeliveredKey>,
    capacity: Arc<Semaphore>,
) {
    let mut updates = encryption.subscribe_key_deliveries();
    let mut backoff = KEY_RETRY_INITIAL;
    loop {
        if !encryption.key_retry_is_current(&owed) {
            return;
        }
        let mut wait = pin!(executor::sleep(backoff));
        loop {
            tokio::select! {
                biased;
                () = &mut wait => break,
                changed = updates.changed() => {
                    if changed.is_err() || !encryption.key_retry_is_current(&owed) {
                        return;
                    }
                }
            }
        }
        // Keep the same pending permit across unrelated watch notifications;
        // Tokio's existing fair semaphore queue then cannot be starved by them.
        let mut permit = pin!(capacity.clone().acquire_owned());
        let permit = loop {
            tokio::select! {
                biased;
                permit = &mut permit => match permit {
                    Ok(permit) => break permit,
                    Err(_) => return,
                },
                changed = updates.changed() => {
                    if changed.is_err() || !encryption.key_retry_is_current(&owed) {
                        return;
                    }
                }
            }
        };
        let outcome = {
            let mut attempt = pin!(encryption.retry_undelivered_key(&owed));
            let mut deadline = pin!(executor::sleep(KEY_RETRY_ATTEMPT));
            loop {
                tokio::select! {
                    biased;
                    () = &mut deadline => break KeyRetry::Undelivered,
                    outcome = &mut attempt => break outcome,
                    changed = updates.changed() => {
                        if changed.is_err() || !encryption.key_retry_is_current(&owed) {
                            return;
                        }
                    }
                }
            }
        };
        // The attempt's hidden pin! storage is gone before capacity is released,
        // including on timeout. A new permit never overlaps a retired future.
        drop(permit);
        if outcome != KeyRetry::Undelivered {
            return;
        }
        backoff = (backoff * 2).min(KEY_RETRY_MAX);
    }
}
