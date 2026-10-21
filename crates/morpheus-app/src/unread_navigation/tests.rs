use super::*;

/// One unread-mark receipt per account and exact room, reserved before the
/// write: a second change of the room waits, and the bound is enforced
/// where receipts are made.
#[test]
fn flag_receipts_are_per_room_and_bounded_where_they_are_made() {
    let mut table = FlagReceipts::default();
    let first = table.reserve("a", "!r:x").unwrap();
    assert_eq!(
        table.reserve("a", "!r:x"),
        Err(Refusal::Busy(FlagReceipt::Pending))
    );
    assert!(table.reserve("b", "!r:x").is_ok(), "per account");
    for index in 2..MAX_FLAG_RECEIPTS {
        table.reserve("a", &format!("!r{index}:x")).unwrap();
    }
    assert_eq!(table.reserve("a", "!full:x"), Err(Refusal::Full));
    // Answered (accepted, already set, refused, never sent): released.
    table.complete(first, false);
    assert_eq!(table.blocking("a", "!r:x"), None);
}

/// No answer: kept as Unknown, never evicted by pressure, and only removing
/// the account forgets it; a late completion never brings it back.
#[test]
fn an_unanswered_flag_write_stays_until_the_account_is_removed() {
    let mut table = FlagReceipts::default();
    let id = table.reserve("a", "!r:x").unwrap();
    table.complete(id, true);
    assert_eq!(table.blocking("a", "!r:x"), Some(FlagReceipt::Unknown));
    assert_eq!(
        table.reserve("a", "!r:x"),
        Err(Refusal::Busy(FlagReceipt::Unknown))
    );
    for index in 1..MAX_FLAG_RECEIPTS {
        table.reserve("a", &format!("!r{index}:x")).unwrap();
    }
    assert_eq!(table.reserve("b", "!new:x"), Err(Refusal::Full));
    assert_eq!(table.blocking("a", "!r:x"), Some(FlagReceipt::Unknown));
    table.forget_account("a");
    assert_eq!(table.blocking("a", "!r:x"), None);
    table.complete(id, true);
    assert_eq!(table.blocking("a", "!r:x"), None);
}

/// Only a missing answer is kept; every other result releases the room.
#[test]
fn only_a_missing_answer_keeps_a_flag_receipt() {
    assert!(unanswered(&Err(MarkedUnreadError::Unknown)));
    for error in [
        MarkedUnreadError::NotSent,
        MarkedUnreadError::SecurityBusy,
        MarkedUnreadError::AccountChanged,
        MarkedUnreadError::Unreadable,
        MarkedUnreadError::Refused,
    ] {
        assert!(!unanswered(&Err(error)));
    }
    assert!(!unanswered(&Ok(MarkedUnreadOutcome::Accepted)));
    assert!(!unanswered(&Ok(MarkedUnreadOutcome::AlreadySet)));
    assert!(refusal_text(Refusal::Busy(FlagReceipt::Unknown)).contains("Nothing more is sent"));
}

/// R1: unread-mark receipts are forgotten only by a complete account removal
/// and a successful logout, beside room permissions' receipts. The draft
/// cleanup (which a partial removal also runs), an ordinary switch, a lock or
/// a close never forgets them, so an Unknown is kept there.
#[test]
fn flag_receipts_are_forgotten_only_by_complete_removal_or_logout() {
    let live = include_str!("../live.rs");
    let drafts = live
        .split("pub fn forget_account_drafts")
        .nth(1)
        .and_then(|rest| rest.split("\n    }").next())
        .unwrap();
    assert!(
        !drafts.contains("unread_navigation"),
        "not the draft cleanup"
    );
    assert!(!live.contains("unread_navigation::forget_account"));
    let main = include_str!("../main.rs");
    let access = include_str!("../account_access.rs");
    // Exactly one call in each, after its success point, next to room
    // permissions' cleanup, with no other arm or function in between.
    let removal = |source: &str, call: &str, after: &str| -> String {
        assert_eq!(
            source.matches("unread_navigation::forget_account(").count(),
            1
        );
        let (before, _) = source.split_once(call).unwrap();
        let (_, arm) = before.rsplit_once(after).unwrap();
        assert!(!arm.contains("=>") && !arm.contains("fn "), "{after}");
        assert!(arm.contains("room_permissions::forget_account("), "{after}");
        arm.to_owned()
    };
    removal(
        main,
        "unread_navigation::forget_account(&account_id)",
        "Ok(result) => {",
    );
    let complete = removal(
        access,
        "crate::unread_navigation::forget_account(&id)",
        "if result.complete {",
    );
    assert!(!complete.contains('}'), "inside `if result.complete`");
}

/// A held jump dies for good on a modal, Settings, the call page or an
/// account step: the window withdraws it when `unread-jump-allowed` stops
/// holding, and every later step checks that and the click's own security
/// generation, never a newly taken one.
#[test]
fn a_held_jump_is_withdrawn_and_never_readopts_an_epoch() {
    let window = include_str!("../../ui/app-window.slint");
    let (_, handler) = window
        .split_once("changed unread-jump-allowed => {")
        .unwrap();
    let handler = handler.split('}').next().unwrap();
    assert!(handler.contains("UnreadNavigation.withdraw-jump()"));
    // The complete modal inventory: the Matrix link's list, the link card,
    // the session and a focused view.
    let (_, allowed) = window
        .split_once("out property <bool> unread-jump-allowed:")
        .unwrap();
    let allowed = allowed.split(';').next().unwrap();
    for term in [
        "root.live-session",
        "!root.matrix-link-blocked",
        "!MatrixLink.open",
        "!root.conversation-focused",
    ] {
        assert!(allowed.contains(term), "{term}");
    }
    // The current registry: matrix-link-blocked is the shared Leave list
    // (room-leave-blocked) plus the open Leave dialog itself.
    let (_, links_blocked) = window
        .split_once("out property <bool> matrix-link-blocked:")
        .unwrap();
    let links_blocked = links_blocked.split(';').next().unwrap();
    assert!(links_blocked.contains("root.room-leave-blocked"));
    assert!(links_blocked.contains("RoomLeave.view.open"));
    let (_, blocked) = window
        .split_once("out property <bool> room-leave-blocked:")
        .unwrap();
    let blocked = blocked.split(';').next().unwrap();
    for surface in [
        "ThreadBrowser.open",
        "RoomActivity.seen-open",
        "RoomNotificationState.open",
        "root.poll-dialog-open",
        "root.account-profile-open",
        "SpaceExplorer.open",
        "root.sas-stage != 0",
        "root.page != 0",
        "root.account-switch-pending",
        "root.security-busy",
    ] {
        assert!(blocked.contains(surface), "{surface}");
    }
    // Synchronously, before each surface opens: every window entry that
    // withdraws a link withdraws the jump too, and so does the Rust choke
    // point every Rust opener calls first.
    let links = window.matches("MatrixLink.withdraw();").count();
    let paired = window
        .matches("MatrixLink.withdraw(); UnreadNavigation.withdraw-jump();")
        .count();
    assert!(links > 20);
    assert_eq!(links, paired, "every link withdrawal also ends a held jump");
    let matrix_links = include_str!("../matrix_links.rs");
    let (_, clear) = matrix_links
        .split_once("pub(crate) fn clear(ui: &AppWindow) {")
        .unwrap();
    let first = clear.lines().take(5).collect::<Vec<_>>().join("\n");
    assert!(first.contains("crate::unread_navigation::withdraw_jump(ui);"));
    // The actual Thread browser and Leave entries: a visible owner refuses
    // first, before any peer is cleared, then the held jump ends before the
    // entry's own flag changes.
    let threads = include_str!("../thread_browser.rs");
    let (_, show) = threads
        .split_once("fn show(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {")
        .unwrap();
    let refuse = show.find("!ui.get_thread_browser_entry_allowed()").unwrap();
    let peers = show
        .find("crate::room_leave::clear_presentation(ui);")
        .unwrap();
    let jump = show
        .find("crate::unread_navigation::withdraw_jump(ui);")
        .unwrap();
    let flag = show.find("state.set_open(true);").unwrap();
    assert!(refuse < peers && peers < jump && jump < flag);
    let (_, entry) = window
        .split_once("out property <bool> thread-browser-entry-allowed:")
        .unwrap();
    let entry = entry.split(';').next().unwrap();
    assert!(entry.contains("!RoomLeave.view.open") && !entry.contains("MatrixLink.open"));
    let leave = include_str!("../room_leave.rs");
    let (_, start) = leave
        .split_once("fn start(self: &Arc<Self>, ui: &AppWindow) {")
        .unwrap();
    let refuse = start.find("ui.get_room_leave_blocked()").unwrap();
    let jump = start
        .find("crate::unread_navigation::withdraw_jump(ui);")
        .unwrap();
    let peers = start.find("crate::thread_browser::clear(ui);").unwrap();
    let flag = start.find("open: true,").unwrap();
    assert!(refuse < jump && jump < peers && peers < flag);
    let source = include_str!("../unread_navigation.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let (_, shown) = production.split_once("fn still_shown(").unwrap();
    let shown = shown.split("\n    }").next().unwrap();
    assert!(shown.contains("ui.get_unread_jump_allowed()"));
    assert!(shown.contains("ringing_allowed(security)"));
    assert!(!shown.contains("ringing_generation()"), "no re-capture");
    assert!(
        !shown.contains("current_navigation_epoch()"),
        "no re-capture"
    );
    let (_, withdraw) = production
        .split_once("fn withdraw_jump(&self, ui: &AppWindow) {")
        .unwrap();
    let withdraw = withdraw.split("\n    }").next().unwrap();
    assert!(withdraw.contains("state.op = state.op.wrapping_add(1)"));
    assert!(withdraw.contains("cancellation.cancel()"));
}

/// An accepted Mark as unread is remembered until a clear, so an explicit
/// Mark as read before sync still clears it although the local flag says
/// false; an Unknown keeps pausing the room; removal forgets both.
#[test]
fn an_accepted_mark_governs_the_next_clear_before_sync() {
    let mut table = FlagReceipts::default();
    assert!(!clear_needed(false, table.maybe_accepted("a", "!r:x")));
    let id = table.reserve("a", "!r:x").unwrap();
    table.complete(id, false);
    table.note_accepted("a", "!r:x", true);
    assert!(
        clear_needed(false, table.maybe_accepted("a", "!r:x")),
        "cached false is not trusted"
    );
    assert!(!table.maybe_accepted("b", "!r:x"), "per account");
    table.note_accepted("a", "!r:x", false);
    assert!(!table.maybe_accepted("a", "!r:x"));
    // Unknown still pauses the room whatever the accepted record says.
    table.note_accepted("a", "!r:x", true);
    let id = table.reserve("a", "!r:x").unwrap();
    table.complete(id, true);
    assert_eq!(
        table.reserve("a", "!r:x"),
        Err(Refusal::Busy(FlagReceipt::Unknown))
    );
    table.forget_account("a");
    assert!(!table.maybe_accepted("a", "!r:x"));
    // Never evicted: past the bound every clear checks the server instead.
    for index in 0..=MAX_ACCEPTED_MARKS {
        table.note_accepted("c", &format!("!r{index}:x"), true);
    }
    assert!(table.maybe_accepted("d", "!other:x"));
}

/// The actual watch-admission boundary distinguishes a stale observation
/// from a departed context. A real gate step for another saved account can
/// end with this same room still shown: its old observation is withdrawn
/// once, and a new local read is admitted under the now-idle gate. Neither
/// operation may change a withdrawn jump or a mark's original authority.
#[test]
fn a_marker_watch_is_readmitted_after_a_same_context_gate_step() {
    let gate = Arc::new(crate::security::SecurityGate::default());
    let mut state = State {
        op: 17,
        marks: 29,
        ..State::default()
    };
    let old = ActivityCancellation::new();
    let reading = state.begin_marker_watch(old.clone()).unwrap();
    let security = gate.ringing_generation();
    assert_eq!(
        state.marker_publication(reading, true, gate.ringing_allowed(security)),
        MarkerPublication::Current
    );

    let step = gate.try_account_access().unwrap();
    assert_eq!(
        state.marker_publication(reading, true, gate.ringing_allowed(security)),
        MarkerPublication::Readmit
    );
    assert!(old.is_cancelled());
    assert!(state.marker_readmission_current(reading, true));
    // An already queued old update cannot publish or start another timer.
    assert_eq!(
        state.marker_publication(reading, true, true),
        MarkerPublication::Retired
    );
    assert!(gate.blocked());
    drop(step);
    assert!(!gate.blocked());
    assert!(
        !gate.ringing_allowed(security),
        "the old capture never becomes current again"
    );

    let fresh = ActivityCancellation::new();
    let fresh_reading = state.begin_marker_watch(fresh.clone()).unwrap();
    let fresh_security = gate.ringing_generation();
    assert_ne!(reading, fresh_reading);
    assert!(!state.marker_readmission_current(reading, true));
    assert_eq!(
        state.marker_publication(fresh_reading, true, gate.ringing_allowed(fresh_security)),
        MarkerPublication::Current
    );
    assert!(!fresh.is_cancelled());
    assert_eq!(
        (state.op, state.marks),
        (17, 29),
        "no jump or mark is re-admitted"
    );
    // A navigation-only mismatch has the same fresh-observation path.
    assert_eq!(
        state.marker_publication(fresh_reading, true, false),
        MarkerPublication::Readmit
    );
    assert!(fresh.is_cancelled());
}

/// A queued local re-admission belongs only to its retired watch and exact
/// context. A replacement, a reset, a departed account/room or shutdown ends
/// it; a stale publication cannot cancel the replacement's watch.
#[test]
fn marker_readmission_ends_with_its_watch_scope() {
    let mut state = State::default();
    let first = ActivityCancellation::new();
    let reading = state.begin_marker_watch(first.clone()).unwrap();
    assert_eq!(
        state.marker_publication(reading, true, false),
        MarkerPublication::Readmit
    );
    assert!(state.marker_readmission_current(reading, true));
    assert!(!state.marker_readmission_current(reading, false));

    let replacement = ActivityCancellation::new();
    let current = state.begin_marker_watch(replacement.clone()).unwrap();
    assert!(!state.marker_readmission_current(reading, true));
    assert_eq!(
        state.marker_publication(reading, false, false),
        MarkerPublication::Retired
    );
    assert!(!replacement.is_cancelled());
    assert_eq!(
        state.marker_publication(current, false, true),
        MarkerPublication::Retired
    );
    assert!(replacement.is_cancelled());
    assert!(!state.marker_readmission_current(current, false));

    state.reading = state.reading.wrapping_add(1); // reset's scope retirement
    assert!(!state.marker_readmission_current(current, true));
    state.stopping = true;
    assert!(!state.marker_readmission_current(state.reading, true));
    assert!(
        state
            .begin_marker_watch(ActivityCancellation::new())
            .is_none()
    );
}

/// The flag clear keeps the Mark as read click's own fence (selection,
/// navigation, security generation) through every wait and into the core's
/// hand-off check; it never uses `selection.is_current` alone. The click
/// takes the fence before anything is spawned.
#[test]
fn the_flag_clear_keeps_the_click_time_fence() {
    let source = include_str!("../unread_navigation.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let (_, clear) = production
        .split_once("pub(crate) async fn clear_mark_for_read(")
        .unwrap();
    let clear = clear.split("\n}\n").next().unwrap();
    assert!(clear.contains("admitted: &(dyn Fn() -> bool + Send + Sync)"));
    assert!(
        clear.matches("!admitted()").count() >= 2,
        "before and after the read"
    );
    assert!(clear.contains("set_marked_unread_selected(selection, room, false, admitted, &guard)"));
    assert!(!clear.contains("|| selection.is_current()"));
    // Mark as unread: the original generation is captured before the
    // blocked check, never after it.
    let (_, mark) = production
        .split_once("fn mark_unread(self: &Arc<Self>")
        .unwrap();
    let captured = mark.find("gate.ringing_generation()").unwrap();
    let blocked = mark.find("security_gate.blocked()").unwrap();
    assert!(captured < blocked, "captured first");
    assert_eq!(
        mark.split("\n    }\n")
            .next()
            .unwrap()
            .matches("ringing_generation()")
            .count(),
        1
    );
    let activity = include_str!("../room_activity/mod.rs");
    let (_, mark) = activity
        .split_once("fn mark_read(self: &Arc<Self>")
        .unwrap();
    let (before_spawn, _) = mark.split_once("self.runtime.spawn").unwrap();
    assert!(before_spawn.contains("gate.ringing_generation()"));
    let captured = before_spawn.find("gate.ringing_generation()").unwrap();
    let blocked = before_spawn.find("security_gate.blocked()").unwrap();
    assert!(
        captured < blocked,
        "Mark as read captures its generation first"
    );
    assert!(before_spawn.contains("current_navigation_epoch()"));
    assert!(before_spawn.contains("gate.ringing_allowed(security)"));
    assert!(before_spawn.contains("navigation_is_current(navigation)"));
    // A jump also captures both values before its first admission check,
    // with exactly one capture of each through the operation.
    let (_, jump) = production.split_once("fn jump(self: &Arc<Self>").unwrap();
    let jump = jump.split("\n    }\n").next().unwrap();
    let first_check = jump.find("if self.lock().stopping").unwrap();
    for capture in ["ringing_generation()", "current_navigation_epoch()"] {
        assert!(
            jump.find(capture).unwrap() < first_check,
            "{capture} captured first"
        );
        assert_eq!(
            jump.matches(capture).count(),
            1,
            "no later capture of {capture}"
        );
    }
}

/// A room switch leaves a running Mark as unread to finish (its answer is
/// kept, never turned into Unknown); the window's shutdown withdraws one not
/// yet handed over, before main waits on any other cleanup.
#[test]
fn a_mark_survives_a_room_switch_and_shutdown_retires_it_first() {
    let source = include_str!("../unread_navigation.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let (_, reset) = production
        .split_once("fn reset(&self, ui: &AppWindow) {")
        .unwrap();
    let reset = reset.split("\n    }\n").next().unwrap();
    assert!(!reset.contains("marking.take()"));
    assert!(reset.contains("watch.cancel()"));
    let (_, shutdown) = production
        .split_once("pub(crate) fn shutdown(&self, ui: &AppWindow) {")
        .unwrap();
    let shutdown = shutdown.split("\n    }\n").next().unwrap();
    assert!(shutdown.contains("state.stopping = true"));
    assert!(shutdown.contains("cancellation.cancel()"));
    let main = include_str!("../main.rs");
    let (_, after_run) = main.split_once("let ui_result = ui.run();").unwrap();
    let stop = after_run
        .find("unread_navigation_ui.shutdown(&ui);")
        .unwrap();
    let first_wait = after_run.find(".await").unwrap();
    assert!(stop < first_wait, "retired before any wait");
}

/// U3 and U6: the divider is exactly the row the core placed (the first shown
/// later message that somebody else sent), never "the row after the marker's
/// own row", and there is none when the place is not exact. The window places
/// it again on every list change and on a return from a focused view, and a
/// jump goes to that same row.
#[test]
fn the_divider_is_the_placed_row_or_none() {
    let first = Some("$first:example.org".to_owned());
    let placed = UnreadPosition::Placed { first };
    assert_eq!(divider_row(Some(placed)), "$first:example.org");
    for unplaced in [
        Some(UnreadPosition::Placed { first: None }),
        Some(UnreadPosition::ThreadNotLoaded),
        Some(UnreadPosition::Unplaceable),
        Some(UnreadPosition::NotLoaded),
        Some(UnreadPosition::NoMarker),
        None,
    ] {
        assert_eq!(divider_row(unplaced), "");
    }
    let window = include_str!("../../ui/app-window.slint");
    let (_, divider) = window.split_once("<bool> divider-above:").unwrap();
    let divider = divider.split(';').next().unwrap();
    let exact = "message.event-id == UnreadNavigation.first-unread-event-id";
    assert!(divider.contains(exact));
    assert!(divider.contains("!root.conversation-focused"));
    assert!(!divider.contains("index - 1"), "the placed row only");
    let (_, scope) = window.split_once("<string> unread-scope:").unwrap();
    let scope = scope.split(';').next().unwrap();
    for term in [
        "root.messages.length",
        "root.messages[root.messages.length - 1].event-id",
        "root.conversation-focused",
    ] {
        assert!(scope.contains(term), "{term}");
    }
    let source = include_str!("../unread_navigation.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let (_, scroll) = production.split_once("fn scroll_after(").unwrap();
    let scroll = scroll.split("\n    }\n").next().unwrap();
    assert!(scroll.contains("index_of_event(&messages, &target.first)"));
    assert!(!scroll.contains("index + 1"), "the placed row itself");
    // The watch places the marker it read before publishing it.
    let (_, refresh) = production.split_once("fn refresh(self").unwrap();
    let refresh = refresh.split("\n    }\n").next().unwrap();
    let call = ".unread_position_selected(&selection, &room, marker)";
    let published = "global.set_first_unread_event_id(divider.into());";
    let placed = refresh.find(call).unwrap();
    assert!(placed < refresh.find("upgrade_in_event_loop").unwrap());
    assert!(refresh.contains(published));
}
