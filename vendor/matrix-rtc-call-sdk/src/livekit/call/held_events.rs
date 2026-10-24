// Copyright Morpheus contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! One retained attachment receiver, handed to one facade subscriber.
//! No queue is added: the original broadcast capacity and lag report survive.

use std::sync::Mutex;
use tokio::sync::broadcast;

pub(super) fn take_or_subscribe<T: Clone>(
    first: &Mutex<Option<broadcast::Receiver<T>>>,
    subscribe: impl FnOnce() -> broadcast::Receiver<T>,
) -> broadcast::Receiver<T> {
    // Release the lock before the fallback, including a reentrant subscriber.
    let retained = first
        .lock()
        .expect("call first-receiver mutex poisoned")
        .take();
    retained.unwrap_or_else(subscribe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn buffered_events_reach_one_first_subscriber_and_later_subscribers_start_fresh() {
        let (source, original) = broadcast::channel(256);
        let first = Arc::new(Mutex::new(Some(original)));
        source.send(7_u8).unwrap();
        source.send(11_u8).unwrap();
        let fresh = Arc::new(AtomicUsize::new(0));
        let mut receivers = std::thread::scope(|scope| {
            let mut tasks = Vec::new();
            for _ in 0..2 {
                let source = source.clone();
                let first = first.clone();
                let fresh = fresh.clone();
                tasks.push(scope.spawn(move || {
                    take_or_subscribe(&first, || {
                        fresh.fetch_add(1, Ordering::SeqCst);
                        source.subscribe()
                    })
                }));
            }
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(fresh.load(Ordering::SeqCst), 1);
        let mut buffered = 0;
        for receiver in &mut receivers {
            match receiver.try_recv() {
                Ok(7) => {
                    assert_eq!(receiver.try_recv(), Ok(11));
                    buffered += 1;
                }
                Err(broadcast::error::TryRecvError::Empty) => {}
                other => panic!("unexpected bounded event receipt: {other:?}"),
            }
            assert_eq!(
                receiver.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            );
        }
        assert_eq!(buffered, 1);
        let mut later = take_or_subscribe(&first, || {
            assert!(
                first.try_lock().is_ok(),
                "fresh subscription must run outside the lock"
            );
            // Reentrant takeover is also fresh, never a second original.
            let nested = take_or_subscribe(&first, || source.subscribe());
            drop(nested);
            source.subscribe()
        });
        assert_eq!(later.try_recv(), Err(broadcast::error::TryRecvError::Empty));
        source.send(19).unwrap();
        for receiver in &mut receivers {
            assert_eq!(receiver.try_recv(), Ok(19));
        }
        assert_eq!(later.try_recv(), Ok(19));
    }

    #[test]
    fn retained_receiver_preserves_original_lag_instead_of_resubscribing() {
        let (source, original) = broadcast::channel(2);
        let first = Mutex::new(Some(original));
        for event in 0_u8..4 {
            source.send(event).unwrap();
        }
        let mut retained =
            take_or_subscribe(&first, || panic!("the original receiver must survive"));
        assert_eq!(
            retained.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(2))
        );
        assert_eq!(retained.try_recv(), Ok(2));
        assert_eq!(retained.try_recv(), Ok(3));
        assert_eq!(
            retained.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        let mut later = take_or_subscribe(&first, || source.subscribe());
        assert_eq!(later.try_recv(), Err(broadcast::error::TryRecvError::Empty));
        source.send(4).unwrap();
        assert_eq!(retained.try_recv(), Ok(4));
        assert_eq!(later.try_recv(), Ok(4));
    }

    #[test]
    fn separate_calls_keep_their_own_receiver_and_drop_unclaimed_history() {
        let (old_source, old_original) = broadcast::channel(256);
        let old = Mutex::new(Some(old_original));
        old_source.send((1_u8, 10_u8)).unwrap();
        let (new_source, new_original) = broadcast::channel(256);
        let new = Mutex::new(Some(new_original));
        new_source.send((2_u8, 20_u8)).unwrap();
        let mut current = take_or_subscribe(&new, || panic!("wrong new-call takeover"));
        assert_eq!(current.try_recv(), Ok((2, 20)));
        assert_eq!(
            current.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        // Dropping an unconsumed call releases its receiver; it neither moves
        // that old history into the new call nor retains another subscription.
        drop(old);
        assert!(old_source.send((1, 11)).is_err());
        new_source.send((2, 21)).unwrap();
        assert_eq!(current.try_recv(), Ok((2, 21)));
        assert_eq!(
            current.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
    }
}
