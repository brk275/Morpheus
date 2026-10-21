use super::*;

fn fence(generation: u64, id: u64) -> Fence {
    Fence {
        generation,
        id,
        room: "!voice:example.org".into(),
    }
}

#[test]
fn only_the_current_scope_selection_and_watcher_may_store_a_roster() {
    assert!(owns(Some(4), true, Some(9), &fence(4, 9)));
    // A -> B -> A: the account ID is the same again, but the selection that
    // started this watcher is revoked.
    assert!(!owns(Some(4), false, Some(9), &fence(4, 9)));
    // An account change, lock, sign-out or detach reset the generation.
    assert!(!owns(Some(5), true, Some(9), &fence(4, 9)));
    assert!(!owns(None, true, Some(9), &fence(4, 9)));
    // The room was released and watched again: a newer watcher owns it.
    assert!(!owns(Some(4), true, Some(10), &fence(4, 9)));
    // The room is no longer watched at all (left, removed or hidden).
    assert!(!owns(Some(4), true, None, &fence(4, 9)));
}

#[test]
fn unsupported_rooms_stop_while_a_missing_slot_claims_nothing() {
    for error in [
        ObserverError::NotEncrypted,
        ObserverError::UnsupportedServer,
        ObserverError::UnsupportedDialect,
    ] {
        assert!(unsupported(error));
        assert!(matches!(opening_failed(error), Presence::Unsupported(_)));
    }
    assert!(!unsupported(ObserverError::SlotUnavailable));
    assert_eq!(
        opening_failed(ObserverError::SlotUnavailable),
        Presence::NoCall
    );
    assert_eq!(
        opening_failed(ObserverError::NotJoined),
        Presence::Unavailable
    );
    assert_eq!(
        opening_failed(ObserverError::TimedOut),
        Presence::Unavailable
    );
}

#[tokio::test]
async fn releasing_a_watcher_stops_and_aborts_its_work() {
    let stop = Arc::new(Stop::default());
    let task = tokio::spawn(std::future::pending::<()>());
    let watcher = Watcher {
        id: 1,
        stop: stop.clone(),
        task,
        presence: Presence::Loading,
        started: Instant::now(),
        mode: morpheus_config::RoomCallMode::Standard,
        sticky: true,
    };
    let mut waiting = Box::pin(stop.wait());
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(watcher);
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap();
    assert!(stop.stopped.load(Ordering::Acquire));
}

#[tokio::test]
async fn finished_watchers_recover_and_changed_modes_restart_without_a_loop() {
    let mut task = tokio::spawn(async {});
    (&mut task).await.unwrap();
    let started = Instant::now();
    let mut watcher = Watcher {
        id: 1,
        stop: Arc::new(Stop::default()),
        task,
        presence: Presence::Unavailable,
        started,
        mode: morpheus_config::RoomCallMode::Standard,
        sticky: false,
    };
    let same = morpheus_config::RoomCallMode::Standard;
    assert!(!watcher.restart_due(same, false, started + REFRESH - Duration::from_millis(1)));
    assert!(watcher.restart_due(same, false, started + REFRESH));
    // Sticky sync becoming available and a deliberate dialect change are
    // acted on immediately, even when the room ID has not changed.
    assert!(watcher.restart_due(same, true, started));
    assert!(watcher.restart_due(
        morpheus_config::RoomCallMode::ElementCallMatrix2,
        false,
        started
    ));
    watcher.presence = Presence::Unsupported(Unsupported::Server);
    assert!(!watcher.restart_due(same, false, started + Duration::from_secs(600)));
    watcher.sticky = true;
    assert!(!watcher.restart_due(same, true, started + Duration::from_secs(299)));
    assert!(watcher.restart_due(same, true, started + Duration::from_secs(300)));
    watcher.presence = Presence::Unsupported(Unsupported::Dialect);
    assert!(!watcher.restart_due(same, true, started + Duration::from_secs(600)));
}
