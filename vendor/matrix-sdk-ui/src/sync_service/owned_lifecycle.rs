// Morpheus local Matrix SDK patch. SPDX-License-Identifier: Apache-2.0
// Same-version upstream provenance is recorded in this package’s MORPHEUS_PATCH.md.

use futures_util::{Stream, stream};
use tokio::sync::oneshot;

#[cfg(target_family = "wasm")]
use matrix_sdk_common::executor::JoinHandle;
#[cfg(not(target_family = "wasm"))]
use tokio::task::JoinHandle;

/// Acknowledges AFTER one actual stream poll, preserving that poll's item,
/// error, EOF or Pending unchanged. For the two pinned SDK sync wrappers only,
/// that poll constructs SlidingSync's stop receiver before their first await.
/// This is not a generic registration or completion receipt.
pub(super) fn after_first_poll<S: Stream>(
    stream: S,
    ready: oneshot::Sender<()>,
) -> impl Stream<Item = S::Item> {
    let mut stream = Box::pin(stream);
    let mut ready = Some(ready);
    stream::poll_fn(move |cx| {
        let result = stream.as_mut().poll_next(cx);
        if let Some(ready) = ready.take() {
            let _ = ready.send(());
        }
        result
    })
}

/// No child may start until its encryption permit exists. Stop remains
/// selectable while a prior/external owner retains that permit.
pub(super) async fn permit_or_stop<P: std::future::Future, S: std::future::Future>(
    permit: P,
    stop: S,
) -> futures_util::future::Either<P::Output, S::Output> {
    tokio::select! {
        biased;
        report = stop => futures_util::future::Either::Right(report),
        permit = permit => futures_util::future::Either::Left(permit),
    }
}

/// Request both original children to stop before any expiration or join.
/// Each actual handle is consumed only by its retained supervisor future.
pub(super) async fn stop_children(
    room_task: JoinHandle<()>,
    encryption_task: JoinHandle<()>,
    stop_room: impl FnOnce(),
    stop_encryption: impl FnOnce(),
    expire: impl std::future::Future<Output = ()>,
) -> bool {
    let mut room_task = SupervisorTask::new(room_task);
    let mut encryption_task = SupervisorTask::new(encryption_task);
    stop_room();
    stop_encryption();
    expire.await;
    let room_joined = room_task.join().await;
    let encryption_joined = encryption_task.join().await;
    room_joined && encryption_joined
}

/// The SAME supervisor task remains awaitable after a cancelled shutdown.
/// A failed join is sticky; retirement never follows an is_finished check.
pub(super) struct SupervisorTask {
    task: Option<JoinHandle<()>>,
    failed: bool,
}

impl SupervisorTask {
    pub(super) fn new(task: JoinHandle<()>) -> Self {
        Self {
            task: Some(task),
            failed: false,
        }
    }

    pub(super) async fn join(&mut self) -> bool {
        if let Some(task) = self.task.as_mut() {
            let result = task.await;
            // No suspension between Ready and removing its consumed handle.
            self.task.take();
            if let Err(error) = result {
                self.failed = true;
                // Cache failure before touching a foreign panic payload. This
                // one task can retain at most its one payload; never re-await it.
                #[cfg(not(target_family = "wasm"))]
                if let Ok(payload) = error.try_into_panic() {
                    std::mem::forget(payload);
                }
                #[cfg(target_family = "wasm")]
                drop(error);
            }
        }
        !self.failed
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use futures_util::{FutureExt, StreamExt};
    use std::{
        future::pending,
        task::{Context, Poll, Waker},
    };
    use tokio::sync::broadcast;

    #[test]
    fn stop_authority_exists_before_first_poll_receipt_and_no_first_item_is_lost() {
        let (stop, unused) = broadcast::channel::<()>(1);
        drop(unused);
        assert!(stop.send(()).is_err());
        let original = stop.clone();
        let lazy = stream::once(async move {
            let mut receiver = original.subscribe();
            receiver.recv().await.map(|_| 7_u8)
        });
        let (started, mut ready) = oneshot::channel();
        let mut guarded = Box::pin(after_first_poll(lazy, started));
        assert!(matches!(
            ready.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(guarded.as_mut().poll_next(&mut cx).is_pending());
        assert_eq!(ready.try_recv(), Ok(()));
        assert!(matches!(stop.send(()), Ok(1)));
        assert_eq!(
            guarded.as_mut().poll_next(&mut cx),
            Poll::Ready(Some(Ok(7)))
        );
        assert_eq!(guarded.as_mut().poll_next(&mut cx), Poll::Ready(None));
        for expected in [Some(Ok(9_u8)), Some(Err(())), None] {
            let (started, mut ready) = oneshot::channel();
            let mut guarded = Box::pin(after_first_poll(stream::iter(expected), started));
            assert_eq!(guarded.as_mut().poll_next(&mut cx), Poll::Ready(expected));
            assert_eq!(ready.try_recv(), Ok(()));
        }
    }

    #[test]
    fn early_poll_panic_closes_readiness_and_the_actual_failed_task_is_joined() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started, ready) = oneshot::channel();
            let task = tokio::spawn(async move {
                let stream = stream::poll_fn(|_| -> Poll<Option<()>> {
                    panic!("fixed synthetic stream failure")
                });
                let mut stream = Box::pin(after_first_poll(stream, started));
                stream.next().await;
            });
            let mut owner = SupervisorTask::new(task);
            let readiness_closed = ready.await.is_err();
            let joined = owner.join().await;
            let handle_retired = owner.task.is_none();
            let still_failed = !owner.join().await;
            assert!(readiness_closed && !joined && handle_retired && still_failed);
        });
    }

    #[test]
    fn cancelled_supervisor_join_keeps_the_same_original_handle_until_actual_join() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (release, held) = oneshot::channel();
            let task = tokio::spawn(async move {
                let _ = held.await;
            });
            let original = task.id();
            let mut owner = SupervisorTask::new(task);
            let mut first = Box::pin(owner.join());
            let pending = first.as_mut().now_or_never().is_none();
            drop(first);
            let retained = owner.task.as_ref().map(JoinHandle::id);
            let released = release.send(()).is_ok();
            let joined = owner.join().await;
            assert_eq!(retained, Some(original));
            assert!(pending && released && joined);
            assert!(owner.task.is_none());
            assert!(owner.join().await);
        });
    }

    #[test]
    fn waiting_for_permit_observes_stop_before_either_child_can_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let permit = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            let held = permit.clone().lock_owned().await;
            let (stop, stopped) = oneshot::channel();
            let mut startup = Box::pin(permit_or_stop(permit.clone().lock_owned(), stopped));
            let pending = startup.as_mut().now_or_never().is_none();
            let sent = stop.send(()).is_ok();
            let stopped = matches!(startup.await, futures_util::future::Either::Right(Ok(())));
            drop(held);
            let released = permit.try_lock().is_ok();
            assert!(pending && sent && stopped && released);
        });
    }

    #[test]
    fn both_original_children_are_stopped_before_expiration_and_join() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (room_stop, room_stopped) = oneshot::channel();
            let (encryption_stop, encryption_stopped) = oneshot::channel();
            let (room_done, room_observed) = oneshot::channel();
            let (encryption_done, encryption_observed) = oneshot::channel();
            let room_task = tokio::spawn(async move {
                let _ = room_stopped.await;
                let _ = room_done.send(());
            });
            let encryption_task = tokio::spawn(async move {
                let _ = encryption_stopped.await;
                let _ = encryption_done.send(());
            });
            let mut both_observed = false;
            let joined = stop_children(
                room_task,
                encryption_task,
                || {
                    let _ = room_stop.send(());
                },
                || {
                    let _ = encryption_stop.send(());
                },
                async {
                    // Actual expiration phase cannot start before BOTH stop
                    // authorities have been submitted by production helper.
                    let (room, encryption) =
                        futures_util::future::join(room_observed, encryption_observed).await;
                    both_observed = room.is_ok() && encryption.is_ok();
                },
            )
            .await;
            assert!(joined && both_observed);
        });
    }

    #[test]
    fn unpolled_stream_has_no_readiness_receipt() {
        let (started, mut ready) = oneshot::channel();
        let stream = after_first_poll(stream::once(pending::<()>()), started);
        drop(stream);
        assert!(matches!(
            ready.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }
}
