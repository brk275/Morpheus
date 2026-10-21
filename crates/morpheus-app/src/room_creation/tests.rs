//! Regression source for the creation dialog's decisions: what the choices
//! evaluate to, and what each finished attempt leaves on screen. In
//! particular:
//! - an unknown outcome keeps the dialog, and nothing is sent again for
//!   that account without an explicit acknowledgement, whatever became of
//!   the dialog (closed, another mode or account, cleared);
//! - the journal behind it is bounded: a create reserves its entry before
//!   anything is sent and is refused when it cannot, nothing is evicted, and
//!   an account's forgetting outlasts its late completions;
//! - a security or account step since the click refuses a create or a link
//!   with nothing sent, and a link may be tried again;
//! - a confirmed room is never reported as unknown, whatever its placement,
//!   sync or opening came to;
//! - the dialog closes only for a room placed as planned and open;
//! - an unconfirmed placement says so, and is never called "not added";
//! - opened means the owned reveal's own snapshot was admitted (told before
//!   the follow-ups after it) or claimed by the UI when its bound passed. Its
//!   bound withdraws it, linearized with that claim, never drops it: an
//!   unclaimed reveal gives its commit back. A room that did not open keeps
//!   its ID, and Open only opens that room. The held production waits, the
//!   claim race and the click's own navigation are covered in live.rs;
//! - a created room's open is latent owned navigation from its click until
//!   its attempt finishes: a protected surface's entry ends it for good, a
//!   finished attempt is released and never withdrawn, and the Thread,
//!   Leave and creation entries refuse over a visible surface before they
//!   retire anything (the window part runs as UI suite phase 91);
//! - the room list's "+" refuses, disabled and with nothing reset, withdrawn
//!   or begun, while its own dialog or creation step is up or any
//!   incompatible visible owner is shown, and is admitted in the offline
//!   preview and on the Settings page (phase 91, through its real
//!   accessibility default action).

use std::sync::atomic::{AtomicBool, AtomicUsize};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};

use super::*;

fn context(can_link: bool) -> CreationContext {
    CreationContext {
        user_id: "@alice:example.org".into(),
        server_name: "example.org".into(),
        room_version: Some("10".into()),
        space_members_supported: true,
        parent: Some(morpheus_matrix::ParentContext {
            space_id: "!team:example.org".into(),
            name: "Team".into(),
            can_link,
        }),
    }
}

fn created(parent: Option<&str>) -> CreatedRoom {
    CreatedRoom {
        room_id: "!new:example.org".into(),
        kind: CreationKind::Room,
        name: "Design\u{202e} review".into(),
        parent: parent.map(str::to_owned),
        suggested: false,
    }
}

fn done(placement: Placement, synced: bool, opened: bool) -> Confirmed {
    let parent = match &placement {
        Placement::Home => None,
        Placement::Linked { .. } | Placement::NotLinked { .. } => Some("!team:example.org"),
    };
    Confirmed {
        created: created(parent),
        placement,
        synced,
        opened,
    }
}

fn linked() -> Placement {
    Placement::Linked {
        space_id: "!team:example.org".into(),
    }
}

fn not_linked(failure: LinkFailure) -> Placement {
    Placement::NotLinked {
        space_id: "!team:example.org".into(),
        failure,
    }
}

#[test]
fn choices_are_only_valid_once_the_context_is_read_and_a_name_is_given() {
    let mut choices = CreationChoices::new(CreationKind::Room, None);
    let unread = evaluate(&choices, None);
    assert!(!unread.valid && unread.preview.is_empty() && unread.problem.is_empty());
    let empty = evaluate(&choices, Some(&context(true)));
    assert!(!empty.valid);
    assert_eq!(empty.problem, CreationError::NameMissing.text());
    choices.name = "Design".into();
    let ready = evaluate(&choices, Some(&context(true)));
    assert!(ready.valid && ready.problem.is_empty());
    let messages = ready
        .preview
        .iter()
        .find(|(label, _)| label == "Messages")
        .expect("the encryption line");
    assert!(messages.1.contains("End-to-end encrypted"));
    // A Space starts unencrypted and says why.
    let mut space = CreationChoices::new(CreationKind::Space, None);
    space.name = "Team".into();
    let space = evaluate(&space, Some(&context(true)));
    assert!(space.valid);
    assert!(
        space
            .preview
            .iter()
            .any(|(_, value)| value.contains("holds no messages"))
    );
}

#[test]
fn a_room_that_may_not_be_placed_is_refused_before_anything_is_sent() {
    let mut choices = CreationChoices::new(CreationKind::Room, Some("!team:example.org".into()));
    choices.name = "Notes".into();
    let refused = evaluate(&choices, Some(&context(false)));
    assert!(!refused.valid);
    assert_eq!(refused.problem, CreationError::PlacementNotAllowed.text());
    // Created in Home instead, it is valid.
    choices.parent = None;
    assert!(evaluate(&choices, Some(&context(false))).valid);
}

#[test]
fn the_dialog_closes_only_for_a_room_placed_as_planned_and_open() {
    assert_eq!(
        confirmed(&done(Placement::Home, true, true), ""),
        Finish::Done
    );
    assert_eq!(confirmed(&done(linked(), true, true), "Team"), Finish::Done);
    // Created and placed, but not open: it stays, and says so.
    for (placement, synced) in [
        (Placement::Home, false),
        (Placement::Home, true),
        (linked(), false),
    ] {
        let Finish::Outcome { state, text, .. } =
            confirmed(&done(placement, synced, false), "Team")
        else {
            panic!("a room that is not open keeps the dialog");
        };
        assert_eq!(state, OUTCOME_NOT_OPENED);
        assert!(text.contains("was created"), "{text}");
        assert!(!text.contains("open in"), "{text}");
        assert_eq!(text.contains("arrived through sync"), !synced, "{text}");
        assert!(!text.contains('\u{202e}'), "the name is plain text");
    }
}

#[test]
fn a_created_room_whose_placement_failed_or_is_unconfirmed_is_its_own_outcome() {
    let Finish::Outcome {
        state,
        text,
        can_relink,
        ..
    } = confirmed(
        &done(not_linked(LinkFailure::Unavailable), true, true),
        "Team",
    )
    else {
        panic!("a created but unlisted room keeps the dialog");
    };
    assert_eq!(state, OUTCOME_NOT_LINKED);
    assert!(
        text.contains("was created") && text.contains("not added to “Team”"),
        "{text}"
    );
    assert!(text.contains("open in Home"), "{text}");
    assert!(can_relink);
    // No reply to the link: placement unconfirmed, never "not added".
    let Finish::Outcome {
        text, can_relink, ..
    } = confirmed(
        &done(not_linked(LinkFailure::OutcomeUnknown), false, false),
        "Team",
    )
    else {
        panic!("an unconfirmed placement keeps the dialog");
    };
    assert!(
        text.contains("placement in “Team” is unconfirmed"),
        "{text}"
    );
    assert!(
        !text.contains("not added") && !text.contains("open in Home"),
        "{text}"
    );
    assert!(can_relink);
    // A refusal by the Space is not offered again.
    let refused = confirmed(
        &done(not_linked(LinkFailure::NotAllowed), true, true),
        "Team",
    );
    assert!(matches!(
        refused,
        Finish::Outcome {
            can_relink: false,
            ..
        }
    ));
    assert!(retryable(LinkFailure::OutcomeUnknown));
    assert!(!retryable(LinkFailure::NotJoined));
    // Refused at the hand-off for a security step: nothing was sent, and
    // it may be added again once the step ended.
    let busy = confirmed(
        &done(not_linked(LinkFailure::SecurityBusy), true, true),
        "Team",
    );
    assert!(matches!(
        busy,
        Finish::Outcome { state: OUTCOME_NOT_LINKED, can_relink: true, ref text, .. }
            if text.contains("not added to “Team”")
    ));
    // Another try reports the same distinction.
    let again = relinked(Err(LinkFailure::OutcomeUnknown), "Notes", "Team", true);
    let Finish::Outcome { text, .. } = again else {
        panic!("a relink keeps the dialog");
    };
    assert!(
        text.contains("still unconfirmed") && !text.contains("still not in"),
        "{text}"
    );
    let refused = relinked(Err(LinkFailure::NotAllowed), "Notes", "Team", true);
    assert!(matches!(refused, Finish::Outcome { ref text, .. } if text.contains("still not in")));
    assert!(matches!(
        relinked(Ok(()), "Notes", "Team", true),
        Finish::Outcome {
            state: OUTCOME_LINKED,
            can_open: false,
            ..
        }
    ));
}

#[test]
fn a_confirmed_room_never_reads_as_an_unknown_outcome() {
    // Whatever its placement, sync or opening came to (a timeout included),
    // a room the homeserver confirmed is reported as created.
    let placements = [
        Placement::Home,
        linked(),
        not_linked(LinkFailure::OutcomeUnknown),
        not_linked(LinkFailure::AccountChanged),
    ];
    for placement in placements {
        for (synced, opened) in [(false, false), (true, false), (true, true)] {
            let finish = confirmed(&done(placement.clone(), synced, opened), "Team");
            if let Finish::Outcome { state, text, .. } = &finish {
                assert_ne!(*state, OUTCOME_UNKNOWN);
                assert!(text.contains("was created"), "{text}");
            }
            assert!(!matches!(finish, Finish::Failed(_)));
        }
    }
    // Only a create request without a reply is unknown.
    assert!(matches!(
        failed(CreationError::OutcomeUnknown),
        Finish::Outcome {
            state: OUTCOME_UNKNOWN,
            ..
        }
    ));
}

#[test]
fn an_unknown_outcome_keeps_the_choices_and_needs_an_acknowledgement() {
    let Finish::Outcome {
        state,
        text,
        can_relink,
        can_open,
    } = failed(CreationError::OutcomeUnknown)
    else {
        panic!("an unknown outcome keeps the dialog");
    };
    // OUTCOME_UNKNOWN blocks submit() until acknowledge() resets it.
    assert_eq!(state, OUTCOME_UNKNOWN);
    assert_ne!(state, OUTCOME_NONE);
    assert!(text.contains("may or may not") && text.contains("choices are kept"));
    // No room ID is known, so there is nothing to open.
    assert!(!can_relink && !can_open);
    // A definite refusal created nothing and leaves the form editable.
    assert_eq!(
        failed(CreationError::AliasTaken),
        Finish::Failed(CreationError::AliasTaken.text())
    );
    // So does a refusal at the hand-off for a security or account step.
    let busy = failed(CreationError::SecurityBusy);
    assert_eq!(busy, Finish::Failed(CreationError::SecurityBusy.text()));
    assert!(
        CreationError::SecurityBusy
            .text()
            .contains("Nothing was created")
    );

    // The record is per account and outlives the dialog: a close, another
    // mode or account, and a clear end its session and operation (clear()
    // does exactly this to them), never the record. Only acknowledge()
    // forgets it, for its own account. (The process-wide journal; these
    // account IDs are this test's own.)
    let ticket = journal().reserve("ledger-account-a").unwrap();
    assert!(journal().settle("ledger-account-a", ticket, true));
    assert_eq!(
        journal().reserve("ledger-account-a"),
        Err(Refusal::Unknown),
        "one entry per account, never a second"
    );
    next_operation();
    SESSION.with(|session| *session.borrow_mut() = None);
    assert!(unknown_for("ledger-account-a"));
    assert!(!unknown_for("ledger-account-b"));
    assert!(!journal().acknowledge("ledger-account-b"));
    assert!(unknown_for("ledger-account-a"));
    assert!(journal().acknowledge("ledger-account-a"));
    assert!(!unknown_for("ledger-account-a"));
}

#[test]
fn a_full_journal_refuses_a_create_before_anything_is_sent_and_evicts_nothing() {
    // submit() reserves before it spawns the request: a refusal here means
    // the request is never built, admitted or handed off.
    let mut journal = Journal::new();
    let accounts = (0..MAX_JOURNAL)
        .map(|index| format!("account-{index}"))
        .collect::<Vec<_>>();
    let tickets = accounts
        .iter()
        .map(|account| journal.reserve(account).expect("a free entry"))
        .collect::<Vec<_>>();
    // Half of them settle unknown: unresolved, they hold their entries.
    for (account, ticket) in accounts.iter().zip(&tickets).step_by(2) {
        assert!(journal.settle(account, *ticket, true));
    }
    assert_eq!(journal.reserve("one-more"), Err(Refusal::Full));
    assert_eq!(journal.held(), MAX_JOURNAL, "nothing was evicted for it");
    assert!(
        accounts
            .iter()
            .step_by(2)
            .all(|account| journal.unknown(account))
    );
    // A held reservation is never acknowledged away; a definite settlement
    // releases its entry, and only then may another account reserve.
    assert!(!journal.acknowledge(&accounts[1]));
    assert_eq!(journal.reserve("one-more"), Err(Refusal::Full));
    assert!(journal.settle(&accounts[1], tickets[1], false));
    assert!(journal.reserve("one-more").is_ok());
    assert_eq!(journal.reserve("another"), Err(Refusal::Full));
}

#[test]
fn an_unknown_outcome_survives_an_account_switch_and_holds_only_its_account() {
    let mut journal = Journal::new();
    let ticket = journal.reserve("a").unwrap();
    // While A's request may be on its way, A cannot create again, from
    // any session of it (an A -> B -> A switch included).
    assert_eq!(journal.reserve("a"), Err(Refusal::InFlight));
    assert!(journal.settle("a", ticket, true));
    // A switch to B: B creates and settles; A's unknown outcome stays.
    let other = journal.reserve("b").unwrap();
    assert!(journal.settle("b", other, false));
    assert!(journal.unknown("a") && !journal.unknown("b"));
    // Back on A: refused until the person acknowledges it there.
    assert_eq!(journal.reserve("a"), Err(Refusal::Unknown));
    assert!(journal.acknowledge("a"));
    assert!(journal.reserve("a").is_ok());
}

#[test]
fn a_forgotten_account_ignores_its_late_completions() {
    let mut journal = Journal::new();
    let old = journal.reserve("a").unwrap();
    // Removed or logged out while its request is on its way.
    journal.forget("a");
    assert_eq!(journal.held(), 0);
    // The request settles late: nothing is recreated and nothing is kept.
    assert!(!journal.settle("a", old, true));
    assert!(!journal.unknown("a"));
    assert_eq!(journal.held(), 0, "no tombstone either");
    // A newer reservation of the same ID is never touched by the old one.
    let newer = journal.reserve("a").unwrap();
    assert_ne!(newer, old);
    assert!(!journal.settle("a", old, true));
    assert_eq!(journal.reserve("a"), Err(Refusal::InFlight));
    assert!(!journal.unknown("a"));
    // An unknown outcome is forgotten with its account too.
    assert!(journal.settle("a", newer, true));
    journal.forget("a");
    assert!(!journal.unknown("a"));
    assert_eq!(journal.held(), 0);
}

#[test]
fn a_reservation_dropped_unsettled_keeps_its_account_unknown() {
    // A task that ended before its request settled may have sent it.
    let ticket = journal().reserve("ledger-dropped").unwrap();
    drop(Reservation {
        account: "ledger-dropped".into(),
        ticket,
        settled: false,
    });
    assert!(unknown_for("ledger-dropped"));
    // Settled explicitly, it is not settled again when dropped.
    assert!(journal().acknowledge("ledger-dropped"));
    let ticket = journal().reserve("ledger-dropped").unwrap();
    Reservation {
        account: "ledger-dropped".into(),
        ticket,
        settled: false,
    }
    .settle(false);
    assert!(!unknown_for("ledger-dropped"));
    assert!(journal().reserve("ledger-dropped").is_ok());
    forget_account("ledger-dropped");
}

#[test]
fn a_room_that_did_not_open_keeps_its_id_and_open_only_opens_it() {
    // Not admitted, navigated away from since the click, or its bound passed
    // after the commit: the confirmed room stays, with its ID, as not open,
    // and Open is offered.
    let not_open = done(linked(), true, false);
    let Finish::Outcome {
        state,
        text,
        can_relink,
        can_open,
    } = confirmed(&not_open, "Team")
    else {
        panic!("a room that did not open keeps the dialog");
    };
    assert_eq!(state, OUTCOME_NOT_OPENED);
    assert!(can_open && !can_relink);
    assert!(
        text.contains("was created") && text.contains("Open shows it"),
        "{text}"
    );
    assert!(!text.contains("open in"), "{text}");
    // Open targets that same room and placement; nothing is created again.
    let (room, placement) = reopening(&not_open).expect("a room that is not open may be opened");
    assert_eq!(room, not_open.created);
    assert_eq!(room.room_id, "!new:example.org");
    assert_eq!(placement, linked());
    // Not synced yet: Open is offered too, and waits for sync first.
    let unsynced = confirmed(&done(Placement::Home, false, false), "");
    assert!(matches!(unsynced, Finish::Outcome { can_open: true, .. }));
    // Once open, nothing is offered again and the dialog closes.
    let open = done(linked(), true, true);
    assert_eq!(reopening(&open), None);
    assert_eq!(confirmed(&open, "Team"), Finish::Done);
    // A room listed after another try keeps Open until it is open.
    let relisted = relinked(Ok(()), "Notes", "Team", false);
    assert!(matches!(
        relisted,
        Finish::Outcome {
            state: OUTCOME_LINKED,
            can_open: true,
            ..
        }
    ));
}

#[test]
fn opened_means_its_own_snapshot_was_admitted_never_matching_ids() {
    assert!(shows_created(Admission::Admitted));
    assert!(shows_created(Admission::Claimed));
    // Refused (no navigation of its own, withdrawn before any claim,
    // replaced, another account, not visible, no window): not opened.
    assert!(!shows_created(Admission::Refused));
}

#[tokio::test]
async fn the_owner_path_admits_once_without_waiting_for_its_follow_ups() {
    let runs = Arc::new(AtomicUsize::new(0));
    let followed = Arc::new(AtomicBool::new(false));
    let authority = NavigationAuthority::default();
    let (admitted, admission) = tokio::sync::oneshot::channel();
    // The UI claims and applies the snapshot (the production claim), the
    // reveal tells its milestone, then slow follow-ups run.
    let reveal = {
        let (runs, followed, authority) = (runs.clone(), followed.clone(), authority.clone());
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            assert!(authority.claim());
            let _ = admitted.send(());
            tokio::time::sleep(Duration::from_millis(300)).await;
            followed.store(true, Ordering::SeqCst);
        }
    };
    let answer = bounded_reveal(reveal, admission, &authority, Duration::from_secs(5)).await;
    assert_eq!(answer, Admission::Admitted);
    // Answered at the admission, not after the follow-ups; never withdrawn.
    assert!(!followed.load(Ordering::SeqCst));
    assert!(!authority.withdrawn());
    assert_eq!(runs.load(Ordering::SeqCst), 1, "one reveal per open");
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while !followed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert!(ended.await.is_ok(), "the follow-ups run on to their end");
}

#[tokio::test]
async fn once_the_ui_claimed_the_admission_its_bound_never_reads_it_as_refused() {
    // The UI claimed the snapshot and is applying it when the bound passes,
    // before the reveal could tell its milestone: shown, so opened.
    let authority = NavigationAuthority::default();
    let (admitted, admission) = tokio::sync::oneshot::channel::<()>();
    let reveal = {
        let authority = authority.clone();
        async move {
            assert!(authority.claim());
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(admitted);
        }
    };
    let answer = bounded_reveal(reveal, admission, &authority, Duration::from_millis(50)).await;
    assert_eq!(answer, Admission::Claimed);
    assert!(shows_created(answer));
    // Withdrawn, so a later commit under it is refused.
    assert!(authority.withdrawn());
}

#[tokio::test]
async fn a_bound_before_the_claim_withdraws_and_the_commit_is_given_back() {
    let authority = NavigationAuthority::default();
    let committed = Arc::new(AtomicBool::new(false));
    let given_back = Arc::new(AtomicBool::new(false));
    let (admitted, admission) = tokio::sync::oneshot::channel::<()>();
    // Committed, then waiting on a read that never ends by itself: the
    // withdrawal wakes it (the production wake), it gives the commit back
    // and is never admitted.
    let reveal = {
        let (authority, committed, given_back) =
            (authority.clone(), committed.clone(), given_back.clone());
        async move {
            committed.store(true, Ordering::SeqCst);
            let read = crate::live::unless_withdrawn(&authority, std::future::pending::<()>());
            assert!(read.await.is_none());
            given_back.store(true, Ordering::SeqCst);
            drop(admitted);
        }
    };
    let answer = bounded_reveal(reveal, admission, &authority, Duration::from_millis(50)).await;
    assert_eq!(answer, Admission::Refused);
    assert!(!shows_created(answer));
    assert!(committed.load(Ordering::SeqCst));
    // The UI closure that runs late can no longer claim it: never shown.
    assert!(!authority.claim());
    assert!(!authority.admitted());
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        while !given_back.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert!(
        settled.await.is_ok(),
        "the reveal was aborted before its give-back"
    );
}

#[tokio::test]
async fn an_open_that_never_navigates_reads_refused_at_once_not_at_its_bound() {
    // A reveal that ends without an admission (navigated since the click, or
    // not admitted) drops its milestone: the bound answers Refused as soon
    // as that happens, without waiting for the bound or withdrawing.
    let authority = NavigationAuthority::default();
    let (admitted, admission) = tokio::sync::oneshot::channel::<()>();
    let reveal = async move { drop(admitted) };
    let started = std::time::Instant::now();
    let answer = bounded_reveal(reveal, admission, &authority, Duration::from_secs(30)).await;
    assert_eq!(answer, Admission::Refused);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        !authority.withdrawn(),
        "nothing to withdraw: it never navigated"
    );
}

#[test]
fn a_latent_created_room_open_ends_for_good_and_a_finished_one_is_released() {
    // Held from the click: a protected entry withdraws it, synchronously,
    // and nothing is left to bring it back.
    let latent = NavigationAuthority::default();
    install_open(7, latent.clone());
    assert!(withdraw_open(), "a latent open is withdrawn");
    assert!(latent.withdrawn());
    assert!(!latent.claim(), "and it never claims a snapshot afterwards");
    assert!(!withdraw_open(), "nothing is held any more");

    // A finished attempt is released, never withdrawn: the follow-ups of
    // the room it showed stay its own. Another operation's finish leaves it.
    let finished = NavigationAuthority::default();
    install_open(8, finished.clone());
    release_open(7);
    release_open(8);
    assert!(!withdraw_open(), "a released open is not held");
    assert!(!finished.withdrawn());

    // One open at a time: a newer click withdraws an older one still held.
    let (older, newer) = (
        NavigationAuthority::default(),
        NavigationAuthority::default(),
    );
    install_open(9, older.clone());
    install_open(10, newer.clone());
    assert!(older.withdrawn() && !newer.withdrawn());
    assert!(withdraw_open());
    assert!(newer.withdrawn());
}

async fn settle(ui: &AppWindow) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    ui.as_weak()
        .upgrade_in_event_loop(move |_| {
            let _ = sender.send(());
        })
        .unwrap();
    receiver.await.unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

/// The mutual Thread / Leave / creation entries in the real window, with
/// view-only stand-ins (no RoomLeaveUi, create request or HTTP):
/// - the shared protected entry (matrix_links::clear, the link withdrawal
///   behind every protected surface's entry, MatrixLink.withdraw() in
///   Slint) ends a created room's open still to come, synchronously, for
///   good, as it ends an accepted invitation's own open;
/// - the thread browser's Show refuses over the room-actions dialog (where
///   a creation may be finishing) before it retires anything: a latent
///   Leave and that creation's open stay as they were. Leave's admission
///   is refused there the same way;
/// - an admitted Show retires a latent Leave and a created room's open
///   before it opens;
/// - the open's authority refuses, for good, an admission while a newer
///   protected surface is up (Leave, the thread browser, a link card,
///   identity details, the invitations center), but not for the
///   room-actions dialog it runs in;
/// - the room list's "+", the dialog's one entry, refuses before it resets,
///   withdraws or begins anything, and is admitted with no owner up
///   (assert_room_action_entry).
/// State is set and reset with no settle between where a change handler
/// would otherwise run, so later phases see nothing. UI suite phase 91.
/// Needs thread_browser's `register` (phase 89) to have run. These are
/// stand-ins: no server creation and no created room's member list runs
/// here (the generated-window member publication is
/// live_timeline_controller_tests' opt-in stage).
pub(crate) async fn assert_creation_protected_entries(ui: &AppWindow) {
    let (page, live_session) = (ui.get_page(), ui.get_live_session());
    ui.set_live_session(true);
    ui.set_page(0);
    settle(ui).await;
    let browser = ui.global::<crate::ThreadBrowser>();
    let leave = ui.global::<crate::RoomLeave>();
    let link = ui.global::<crate::MatrixLink>();
    assert!(!surface_blocked(ui), "nothing newer is up");

    // The shared protected entry ends a latent open, synchronously: the
    // link withdrawal itself, as MatrixLink.withdraw() runs it.
    let latent = NavigationAuthority::default().blocked_by(surface_blocked);
    install_open(1, latent.clone());
    crate::matrix_links::clear(ui);
    assert!(latent.withdrawn(), "the protected entry ended the open");
    assert!(!latent.claim(), "and it never claims a snapshot afterwards");
    assert!(!withdraw_open(), "nothing is held any more");

    // Show refuses over the room-actions dialog before retiring anything.
    let finishing = NavigationAuthority::default().blocked_by(surface_blocked);
    install_open(2, finishing.clone());
    let pending_leave = morpheus_matrix::RoomLeaveCancellation::new();
    let cancellation = pending_leave.clone();
    leave.on_reset(move || cancellation.cancel());
    ui.set_room_actions_open(true);
    assert!(
        !surface_blocked(ui),
        "the dialog a creation runs in is its own surface"
    );
    assert!(finishing.admits(ui), "so it may still be admitted there");
    browser.invoke_show();
    assert!(!browser.get_open(), "Show cannot open over room actions");
    assert!(
        !pending_leave.is_cancelled(),
        "a refused Show retires no Leave"
    );
    assert!(
        !finishing.withdrawn(),
        "nor the creation finishing in the dialog"
    );
    assert!(ui.get_room_leave_blocked(), "room actions refuse Leave too");
    ui.set_room_actions_open(false);
    assert!(withdraw_open(), "the creation's open was still held");

    // An admitted Show retires a latent Leave and a created room's open
    // before it opens.
    let latent = NavigationAuthority::default().blocked_by(surface_blocked);
    install_open(3, latent.clone());
    browser.invoke_show();
    assert!(
        pending_leave.is_cancelled(),
        "Show retired the latent Leave"
    );
    assert!(latent.withdrawn(), "and the created room's open");
    crate::thread_browser::clear(ui);
    leave.on_reset(|| {});
    settle(ui).await;

    // A newer protected surface refuses the open's admission for good;
    // closing it never revives the open.
    let creation = NavigationAuthority::default().blocked_by(surface_blocked);
    assert!(creation.admits(ui));
    leave.set_view(crate::RoomLeaveView {
        open: true,
        ..Default::default()
    });
    assert!(surface_blocked(ui), "Leave is newer");
    assert!(!creation.admits(ui), "admitted under Leave: ended");
    leave.set_view(crate::RoomLeaveView::default());
    assert!(!surface_blocked(ui));
    assert!(!creation.admits(ui), "closing Leave never revives it");
    browser.set_open(true);
    assert!(surface_blocked(ui), "so is the thread browser");
    browser.set_open(false);
    link.set_open(true);
    assert!(surface_blocked(ui), "and a link card");
    link.set_open(false);
    let identity = ui.global::<crate::IdentityWarnings>();
    identity.set_open(true);
    assert!(surface_blocked(ui), "identity details are newer too");
    identity.set_open(false);
    let invitations = ui.global::<crate::InvitationsState>();
    invitations.set_open(true);
    assert!(surface_blocked(ui), "and the invitations center");
    invitations.set_open(false);
    assert!(!surface_blocked(ui));

    assert_room_action_entry(ui);

    ui.set_page(page);
    ui.set_live_session(live_session);
    settle(ui).await;
}

/// The room list's "+" (room-action-trigger), the dialog's one entry,
/// through its real generated accessibility default action: the action a
/// screen reader invokes, which no modal scrim stops. Refused, it changes
/// nothing: the dialog's mode, input and error, the creation form and its
/// outcome, the creation's operation and latent open, a held owned open, a
/// latent Leave and the shown owner all stay, and RoomCreation.begin never
/// runs. (A creation session needs a real account selection, which this
/// suite has none of; its only writer on this path is begin's clear, which
/// would also advance the operation, so the begin spy and the unchanged
/// operation stand for it.) Admitted with no owner up, in the offline
/// preview, on the Settings page and in a live session, it ends those
/// peers, resets its own values, begins a room creation and opens the
/// dialog. The +'s pointer and keys obey the same `enabled`. Everything set
/// here is reset before the caller's next settle.
fn assert_room_action_entry(ui: &AppWindow) {
    // Found once, while no dialog is up; the + is no conditional element,
    // so the handle stays valid and nothing else is laid out for it.
    let plus = {
        let mut buttons =
            ElementHandle::find_by_accessible_label(ui, "Create or join a Matrix room")
                .filter(|element| element.accessible_role() == Some(AccessibleRole::Button));
        let plus = buttons.next().expect("the room list's + is in the window");
        assert!(buttons.next().is_none(), "the + is the one such button");
        plus
    };
    let state = ui.global::<RoomCreation>();
    let leave = ui.global::<crate::RoomLeave>();
    let shown = (
        ui.get_room_action_mode(),
        ui.get_room_action_input(),
        ui.get_room_action_error(),
    );
    // BEGIN SPY (test only): records each RoomCreation.begin kind. This
    // suite registers no creation controller (room_creation::register), so
    // the real handler here is the unset callback; an empty handler, which
    // behaves exactly as it does, is put back at the end.
    let begun = std::rc::Rc::new(RefCell::new(Vec::<i32>::new()));
    let spy = begun.clone();
    state.on_begin(move |kind| spy.borrow_mut().push(kind));
    // What a refused entry must leave as it was: the dialog's own values and
    // a creation form with an outcome on screen.
    let keep = |ui: &AppWindow| {
        ui.set_room_action_mode(2);
        ui.set_room_action_input("kept join input".into());
        ui.set_room_action_error("kept dialog error".into());
        let form = ui.global::<RoomCreation>();
        form.set_kind(1);
        form.set_name("Kept Space".into());
        form.set_outcome(OUTCOME_CONFIRMED);
        form.set_outcome_text("kept outcome".into());
    };
    let kept = |ui: &AppWindow, case: &str| {
        let form = ui.global::<RoomCreation>();
        assert_eq!(ui.get_room_action_mode(), 2, "{case}: mode kept");
        assert_eq!(
            ui.get_room_action_input().as_str(),
            "kept join input",
            "{case}"
        );
        assert_eq!(
            ui.get_room_action_error().as_str(),
            "kept dialog error",
            "{case}"
        );
        assert_eq!(form.get_kind(), 1, "{case}: form kept");
        assert_eq!(form.get_name().as_str(), "Kept Space", "{case}: name kept");
        assert_eq!(
            form.get_outcome(),
            OUTCOME_CONFIRMED,
            "{case}: outcome kept"
        );
        assert_eq!(form.get_outcome_text().as_str(), "kept outcome", "{case}");
    };
    // A latent Leave the + would retire if it ran (ThreadBrowser.withdraw()).
    let latent_leave = || {
        let pending = morpheus_matrix::RoomLeaveCancellation::new();
        let cancellation = pending.clone();
        leave.on_reset(move || cancellation.cancel());
        pending
    };

    // Its own creation, still finishing in its open dialog: the + under the
    // scrim reads disabled and its default action refuses, so that attempt
    // keeps its operation, its latent open (also the window's owned
    // automatic open, which MatrixLink.withdraw() would end) and its outcome.
    keep(ui);
    let operation = next_operation();
    let own = NavigationAuthority::default().blocked_by(surface_blocked);
    install_open(operation, own.clone());
    let pending = latent_leave();
    state.set_busy(true);
    ui.set_room_actions_open(true);
    for case in ["its own dialog", "its own step", "its own relink"] {
        match case {
            // A step still under way behind a dialog closed meanwhile.
            "its own step" => ui.set_room_actions_open(false),
            "its own relink" => {
                state.set_busy(false);
                state.set_relinking(true);
            }
            _ => {}
        }
        assert!(
            ui.get_room_creation_entry_blocked(),
            "{case} refuses the entry"
        );
        assert_eq!(
            plus.accessible_enabled(),
            Some(false),
            "{case}: the + is disabled"
        );
        plus.invoke_accessible_default_action();
        assert_eq!(
            ui.get_room_actions_open(),
            case == "its own dialog",
            "{case}: the dialog is as it was"
        );
        kept(ui, case);
        assert_eq!(
            current_operation(),
            operation,
            "{case}: its late result still applies"
        );
        assert!(!own.withdrawn(), "{case}: its latent open stays");
        assert!(!pending.is_cancelled(), "{case}: no peer is retired");
        assert!(begun.borrow().is_empty(), "{case}: begin never ran");
    }
    state.set_relinking(false);
    // Its stand-in ends as an account change ends it.
    clear(ui);

    // An incompatible visible owner: the + refuses before it resets,
    // withdraws or begins anything, and the owner stays shown.
    type Owner = (&'static str, fn(&AppWindow, bool), fn(&AppWindow) -> bool);
    let owners: [Owner; 11] = [
        (
            "Leave",
            |ui: &AppWindow, open: bool| {
                let view = crate::RoomLeaveView {
                    open,
                    ..Default::default()
                };
                ui.global::<crate::RoomLeave>().set_view(view);
            },
            |ui: &AppWindow| ui.global::<crate::RoomLeave>().get_view().open,
        ),
        (
            "the thread browser",
            |ui: &AppWindow, open: bool| ui.global::<crate::ThreadBrowser>().set_open(open),
            |ui: &AppWindow| ui.global::<crate::ThreadBrowser>().get_open(),
        ),
        (
            "identity details",
            |ui: &AppWindow, open: bool| ui.global::<crate::IdentityWarnings>().set_open(open),
            |ui: &AppWindow| ui.global::<crate::IdentityWarnings>().get_open(),
        ),
        (
            "the invitations center",
            |ui: &AppWindow, open: bool| ui.global::<crate::InvitationsState>().set_open(open),
            |ui: &AppWindow| ui.global::<crate::InvitationsState>().get_open(),
        ),
        (
            "a link card",
            |ui: &AppWindow, open: bool| ui.global::<crate::MatrixLink>().set_open(open),
            |ui: &AppWindow| ui.global::<crate::MatrixLink>().get_open(),
        ),
        (
            "an ignored-user review",
            |ui: &AppWindow, open: bool| {
                let privacy = ui.global::<crate::MemberPrivacy>();
                let mut view = privacy.get_ignored();
                view.stage = if open { 1 } else { 0 };
                privacy.set_ignored(view);
            },
            |ui: &AppWindow| ui.global::<crate::MemberPrivacy>().get_ignored().stage == 1,
        ),
        (
            "an ignored-user write",
            |ui: &AppWindow, busy: bool| {
                let privacy = ui.global::<crate::MemberPrivacy>();
                let mut view = privacy.get_ignored();
                view.stage = if busy { 2 } else { 0 };
                view.busy = busy;
                privacy.set_ignored(view);
            },
            |ui: &AppWindow| ui.global::<crate::MemberPrivacy>().get_ignored().busy,
        ),
        (
            "People",
            |ui: &AppWindow, open: bool| ui.set_people_open(open),
            |ui: &AppWindow| ui.get_people_open(),
        ),
        (
            "the quick switcher",
            |ui: &AppWindow, open: bool| ui.set_quick_switcher_open(open),
            |ui: &AppWindow| ui.get_quick_switcher_open(),
        ),
        (
            "the account menu",
            |ui: &AppWindow, open: bool| ui.set_account_menu_open(open),
            |ui: &AppWindow| ui.get_account_menu_open(),
        ),
        (
            "a security step",
            |ui: &AppWindow, busy: bool| ui.set_security_busy(busy),
            |ui: &AppWindow| ui.get_security_busy(),
        ),
    ];
    for (owner, show, shown_now) in owners {
        keep(ui);
        let operation = current_operation();
        // A held owned open the + would end if it ran (MatrixLink.withdraw()).
        let owned = NavigationAuthority::default();
        crate::matrix_links::own_automatic_open(owned.clone());
        let pending = latent_leave();
        show(ui, true);
        assert!(
            ui.get_room_creation_entry_blocked(),
            "{owner} refuses the entry"
        );
        assert_eq!(
            plus.accessible_enabled(),
            Some(false),
            "{owner}: the + is disabled"
        );
        plus.invoke_accessible_default_action();
        assert!(!ui.get_room_actions_open(), "{owner}: no dialog opens");
        assert!(shown_now(ui), "{owner} stays shown");
        kept(ui, owner);
        assert_eq!(current_operation(), operation, "{owner}: no new operation");
        assert!(!owned.withdrawn(), "{owner}: a held owned open stays");
        assert!(!pending.is_cancelled(), "{owner}: a latent Leave stays");
        assert!(begun.borrow().is_empty(), "{owner}: begin never ran");
        show(ui, false);
        crate::matrix_links::release_automatic_open(&owned);
    }

    // Admitted with no owner up, without a live session or the conversation
    // page: the peers end first, then its own values reset, one room
    // creation begins and the dialog opens.
    let preview = ui.get_preview_session();
    for (case, live, page) in [
        ("the offline preview", false, 0),
        ("the Settings page", true, 1),
        ("a live session", true, 0),
    ] {
        ui.set_live_session(live);
        ui.set_preview_session(!live);
        ui.set_page(page);
        keep(ui);
        let owned = NavigationAuthority::default();
        crate::matrix_links::own_automatic_open(owned.clone());
        let pending = latent_leave();
        assert!(
            !ui.get_room_creation_entry_blocked(),
            "{case}: nothing is up"
        );
        assert!(
            crate::member_privacy::ignored_action_admitted(ui),
            "{case}: an idle review may begin"
        );
        assert_eq!(
            plus.accessible_enabled(),
            Some(true),
            "{case}: the + is enabled"
        );
        plus.invoke_accessible_default_action();
        assert!(ui.get_room_actions_open(), "{case}: the dialog opens");
        assert!(
            !crate::member_privacy::ignored_action_admitted(ui),
            "{case}: an obscured review or send is refused"
        );
        assert_eq!(ui.get_room_action_mode(), 0, "{case}: in Create room");
        assert!(ui.get_room_action_input().is_empty(), "{case}: input reset");
        assert!(ui.get_room_action_error().is_empty(), "{case}: error reset");
        assert_eq!(
            begun.borrow().as_slice(),
            [0],
            "{case}: one room creation begun"
        );
        assert!(owned.withdrawn(), "{case}: a held owned open ended");
        assert!(pending.is_cancelled(), "{case}: and a latent Leave");
        // Its stand-in closes as Close does.
        ui.set_room_actions_open(false);
        begun.borrow_mut().clear();
    }
    ui.set_preview_session(preview);
    ui.set_page(0);
    ui.set_live_session(true);

    // Put back what this left: the form's defaults, the dialog's values, no
    // Leave reset hook and the suite's own (empty) begin handler.
    clear(ui);
    ui.set_room_action_mode(shown.0);
    ui.set_room_action_input(shown.1);
    ui.set_room_action_error(shown.2);
    leave.on_reset(|| {});
    state.on_begin(|_| {});
}

#[test]
fn the_space_name_shown_is_bounded_plain_text_however_it_arrives() {
    // Both display sinks of the Space's name (begin's shown Space and the
    // read context's) go through label(): even a raw accepted room ID, with
    // bidi and control characters and of any length, comes out clean and
    // bounded. The session keeps the exact ID for every action.
    let raw = format!("!evil\u{202e}gnp\u{7}{}:example.org", "a".repeat(600));
    let shown = label(&raw);
    let hidden = |ch: char| {
        ch.is_control() || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}')
    };
    assert!(!shown.chars().any(hidden), "{shown:?}");
    assert!(shown.chars().count() <= MAX_LABEL_CHARS);
    assert!(shown.starts_with("!evil"));
}
