//! One waiting UI callback per timeline feed, carrying its latest full snapshot.
//!
//! Each room/thread subscription owns a separate slot. Scope checks still run
//! in the UI callback; replacing a snapshot never authorizes its publication.

use std::sync::Mutex;

pub(crate) struct Latest<T> {
    state: Mutex<State<T>>,
}

struct State<T> {
    pending: Option<T>,
    scheduled: bool,
}

impl<T> Latest<T> {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                pending: None,
                scheduled: false,
            }),
        }
    }

    /// Replace pending data. True means the caller must schedule a callback.
    /// A scheduling failure must stop the subscription and drop this slot.
    pub(crate) fn submit(&self, value: T) -> bool {
        let (schedule, previous) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let previous = state.pending.replace(value);
            let schedule = !state.scheduled;
            state.scheduled = true;
            (schedule, previous)
        };
        // Release superseded payloads outside the lock, including their drops.
        drop(previous);
        schedule
    }

    /// Called once by the queued UI callback, before checking its scope.
    /// Taking data also allows a later update to schedule the next callback.
    pub(crate) fn take(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.scheduled = false;
        state.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };

    #[test]
    fn a_stalled_ui_retains_only_the_latest_snapshot_from_a_burst() {
        let delivery = Latest::new();
        let mut old = Vec::new();
        let mut scheduled = 0;
        for version in 0..10_000 {
            let snapshot = Arc::new(version);
            old.push(Arc::downgrade(&snapshot));
            scheduled += usize::from(delivery.submit(snapshot));
        }
        assert_eq!(scheduled, 1);
        assert!(
            old[..9_999]
                .iter()
                .all(|snapshot| snapshot.upgrade().is_none())
        );
        let latest = delivery.take().unwrap();
        assert_eq!(*latest, 9_999);
        drop(latest);
        assert!(old.iter().all(|snapshot| snapshot.upgrade().is_none()));
        assert!(delivery.submit(Arc::new(10_000)));
        assert_eq!(*delivery.take().unwrap(), 10_000);
    }

    #[test]
    fn an_update_during_ui_processing_schedules_the_next_delivery() {
        let delivery = Arc::new(Latest::new());
        let (ready, before_take) = mpsc::sync_channel(1);
        let (after_take, resume) = mpsc::sync_channel(1);
        let producer = {
            let delivery = delivery.clone();
            std::thread::spawn(move || {
                ready
                    .send((delivery.submit(1), delivery.submit(2)))
                    .unwrap();
                resume.recv_timeout(Duration::from_secs(30)).unwrap();
                (delivery.submit(3), delivery.submit(4))
            })
        };
        assert_eq!(
            before_take.recv_timeout(Duration::from_secs(30)).unwrap(),
            (true, false)
        );
        let processing = delivery.take().unwrap();
        assert_eq!(processing, 2);
        after_take.send(()).unwrap();
        assert_eq!(producer.join().unwrap(), (true, false));
        assert_eq!(processing, 2);
        assert_eq!(delivery.take(), Some(4));
        assert!(delivery.take().is_none());
    }

    #[test]
    fn subscriptions_have_independent_delivery_and_release_on_drop() {
        let room = Latest::new();
        let thread = Latest::new();
        let room_value = Arc::new("room");
        let weak = Arc::downgrade(&room_value);
        assert!(room.submit(room_value));
        assert!(thread.submit(Arc::new("thread")));
        assert_eq!(*thread.take().unwrap(), "thread");
        assert!(weak.upgrade().is_some());
        drop(room);
        assert!(weak.upgrade().is_none());
    }
}
