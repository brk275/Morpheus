//! "Start a conversation": find people on the selected account's homeserver
//! and open or start a direct conversation with one of them.
//!
//! Searching asks only that homeserver's user directory (see the core
//! module), after a short typing pause or on Enter; a complete Matrix user
//! ID is offered at once without a directory result. Choosing a person only
//! looks up direct conversations this device already knows; opening one
//! goes through the shared conversation seam, which keeps every draft.
//! Starting a new conversation needs an explicit confirmation naming the
//! exact user ID and sends one creation request, encrypted from creation;
//! a failure after it left is reported as unknown and never retried here
//! (see the core note on the SDK's token-refresh replay).
//!
//! The window captures the exact account selection, a cancellation token
//! and operation generations; a switch (also A -> B -> A), lock, detach or
//! closing cancels the token and clears every row, name and notice
//! synchronously, so a late result can never show or act for another
//! account.

use std::{cell::RefCell, sync::Arc, sync::atomic::Ordering};

use morpheus_matrix::{
    AccountSelection, DirectListing, DirectWriteError, DirectoryPerson, KnownDirect,
    PeopleCancellation, PeopleError, exact_user_id, validate_people_query,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, PersonItem, live::LiveUi};

#[cfg(test)]
#[path = "people_directory_live_tests.rs"]
pub(crate) mod live_tests;

const SEARCHING: &str = "Searching your homeserver's user directory…";
const NO_MATCH: &str = "No one in your homeserver's directory matches. A complete Matrix ID such as @name:example.org can still be used.";
const LIMITED: &str = "More people match; type more to narrow the list.";
const CHECKING: &str = "Checking for an existing conversation on this device…";
const CREATING: &str = "Starting the encrypted conversation…";
const CREATED_PENDING: &str =
    "The conversation was created. It opens here once your homeserver syncs it.";
const CREATED_READY: &str = "The conversation was created; Open conversation shows it.";
const NOT_MARKED: &str = " It could not be listed as a direct conversation in your account data.";
const LISTING_UNKNOWN: &str = " Whether it is listed as a direct conversation in your account data could not be confirmed; it was not tried again.";
const LISTING_CHANGED: &str = " Your direct-message list changed on the server meanwhile, so it was not listed as a direct conversation; nothing was overwritten.";

// Known-direct states shown in the window.
const KNOWN_UNSET: i32 = 0;
const KNOWN_CHECKING: i32 = 1;
const KNOWN_NONE: i32 = 2;
const KNOWN_ONE: i32 = 3;
const KNOWN_SEVERAL: i32 = 4;

#[derive(Default)]
struct Directory {
    epoch: u64,
    operation: u64,
    account: String,
    selection: Option<AccountSelection>,
    cancellation: Option<PeopleCancellation>,
    people: Vec<DirectoryPerson>,
    chosen: Option<String>,
    known: Option<KnownDirect>,
    busy: bool,
}

thread_local! {
    // Slint callbacks run on the UI thread only.
    static DIRECTORY: RefCell<Directory> = RefCell::new(Directory::default());
}

fn with<R>(f: impl FnOnce(&mut Directory) -> R) -> R {
    DIRECTORY.with(|directory| f(&mut directory.borrow_mut()))
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) {
    let weak = ui.as_weak();
    let open_live = live.clone();
    ui.on_open_people(move |prefill| {
        if let Some(ui) = weak.upgrade() {
            open(&ui, &open_live, prefill.as_str());
        }
    });
    let weak = ui.as_weak();
    ui.on_close_people(move || {
        if let Some(ui) = weak.upgrade() {
            clear_presentation(&ui);
        }
    });
    let weak = ui.as_weak();
    let search_live = live.clone();
    let search_runtime = runtime.clone();
    ui.on_people_search(move || {
        if let Some(ui) = weak.upgrade() {
            search(&ui, &search_live, &search_runtime);
        }
    });
    let weak = ui.as_weak();
    let choose_live = live.clone();
    let choose_runtime = runtime.clone();
    ui.on_people_choose(move |index| {
        if let Some(ui) = weak.upgrade() {
            choose(&ui, &choose_live, &choose_runtime, index);
        }
    });
    let weak = ui.as_weak();
    ui.on_people_open_dm(move || {
        if let Some(ui) = weak.upgrade() {
            open_known(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_people_ask_start(move || {
        if let Some(ui) = weak.upgrade() {
            ask_start(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_people_confirm_start(move || {
        if let Some(ui) = weak.upgrade() {
            confirm_start(&ui, &live, &runtime);
        }
    });
}

/// Cancel every step and forget rows, names and notices synchronously.
/// Called on close and with the account's other private presentation.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    with(|directory| {
        if let Some(cancellation) = directory.cancellation.take() {
            cancellation.cancel();
        }
        let epoch = directory.epoch.wrapping_add(1);
        let operation = directory.operation.wrapping_add(1);
        *directory = Directory {
            epoch,
            operation,
            ..Directory::default()
        };
    });
    ui.set_people_open(false);
    ui.set_people_query("".into());
    ui.set_people_items(ModelRc::new(VecModel::<PersonItem>::default()));
    ui.set_people_index(-1);
    ui.set_people_status("".into());
    ui.set_people_error("".into());
    ui.set_people_busy(false);
    ui.set_people_chosen_id("".into());
    ui.set_people_chosen_name("".into());
    ui.set_people_known(KNOWN_UNSET);
    ui.set_people_known_count(0);
    ui.set_people_confirm(false);
}

/// No account, security, call or dialog flow is in the way.
fn unobstructed(ui: &AppWindow, live: &LiveUi) -> bool {
    ui.get_live_session()
        && live.is_live.load(Ordering::Acquire)
        && !live.security_gate.blocked()
        && !ui.get_show_sign_in()
        && !ui.get_access_open()
        && !ui.get_account_switch_pending()
        && ui.get_security_recovery_key().is_empty()
        && !ui.get_leave_call_dialog()
        && !ui.get_manage_accounts_open()
        && !ui.get_account_menu_open()
        && !ui.get_native_capture_picker_open()
        && !ui.get_member_confirm_open()
        && !ui.get_room_admin_open()
        && !ui.get_room_actions_open()
        && !ui.get_image_preview_open()
        && !ui.get_poll_dialog_open()
        && !ui.get_quick_switcher_open()
        && !ui.get_account_profile_open()
        && !crate::space_explorer::is_open(ui)
        && !ui.global::<crate::RoomNotificationState>().get_open()
        && !ui.global::<crate::RoomActivity>().get_seen_open()
}

/// What one step captured when it started.
#[derive(Clone)]
struct Scope {
    epoch: u64,
    operation: u64,
    account: String,
    selection: AccountSelection,
    cancellation: PeopleCancellation,
}

impl Scope {
    fn current(&self, ui: &AppWindow) -> bool {
        let presented = ui.get_people_open()
            && ui.get_active_account_local_id().as_str() == self.account
            && with(|directory| {
                directory.epoch == self.epoch && directory.operation == self.operation
            });
        if !presented {
            return false;
        }
        if self.cancellation.is_cancelled() || !self.selection.is_current() {
            // A security epoch can end without a visible dialog being shown.
            // Retire only this exact presentation rather than orphaning busy.
            clear_presentation(ui);
            return false;
        }
        true
    }
}

/// A new step for the open window; it supersedes any earlier one.
/// Refused while a creation runs: a new step would supersede the creation's
/// operation, so its completion could never clear the busy state.
fn step(ui: &AppWindow) -> Option<Scope> {
    with(|directory| {
        let selection = directory.selection.clone()?;
        let cancellation = directory.cancellation.clone()?;
        if !step_allowed(
            directory.busy,
            selection.is_current(),
            directory.account == ui.get_active_account_local_id().as_str(),
        ) {
            return None;
        }
        directory.operation = directory.operation.wrapping_add(1);
        Some(Scope {
            epoch: directory.epoch,
            operation: directory.operation,
            account: directory.account.clone(),
            selection,
            cancellation,
        })
    })
}

/// Whether a new step (search, choice, creation) may start.
fn step_allowed(busy: bool, selection_current: bool, same_account: bool) -> bool {
    !busy && selection_current && same_account
}

fn open(ui: &AppWindow, live: &LiveUi, prefill: &str) {
    // A creation in flight keeps its window: reopening (from a Matrix ID
    // typed in another dialog) would cancel it and drop its completion, so
    // the person would never learn whether it was created. Closing still
    // cancels it explicitly.
    if with(|directory| directory.busy) {
        return;
    }
    if !unobstructed(ui, live) {
        return;
    }
    let account = ui.get_active_account_local_id().to_string();
    if account.is_empty() {
        return;
    }
    let Some(selection) = live.manager.account_selection(&account) else {
        return;
    };
    clear_presentation(ui);
    with(|directory| {
        directory.account = account;
        directory.selection = Some(selection);
        let gate = live.security_gate.clone();
        let generation = gate.ringing_generation();
        directory.cancellation = Some(
            PeopleCancellation::new()
                .with_admission(Arc::new(move || gate.ringing_allowed(generation))),
        );
    });
    let prefill: String = prefill
        .chars()
        .take(morpheus_matrix::MAX_USER_QUERY_CHARS)
        .collect();
    ui.set_people_query(prefill.as_str().into());
    ui.set_people_open(true);
    project(ui);
}

/// Rows: the exact Matrix ID typed (if one), then directory results.
fn rows(query: &str, people: &[DirectoryPerson]) -> Vec<PersonItem> {
    let mut items = Vec::new();
    let exact = exact_user_id(query);
    if let Some(user_id) = &exact {
        let named = people.iter().find(|person| &person.user_id == user_id);
        items.push(PersonItem {
            user_id: user_id.as_str().into(),
            name: named
                .and_then(|person| person.display_name.as_deref())
                .unwrap_or(user_id.as_str())
                .into(),
            exact: true,
        });
    }
    for person in people {
        if exact.as_deref() == Some(person.user_id.as_str()) {
            continue;
        }
        items.push(PersonItem {
            user_id: person.user_id.as_str().into(),
            name: person
                .display_name
                .as_deref()
                .unwrap_or(person.user_id.as_str())
                .into(),
            exact: false,
        });
    }
    items
}

fn project(ui: &AppWindow) {
    let query = ui.get_people_query();
    let items = with(|directory| rows(query.as_str(), &directory.people));
    let index = if items.is_empty() { -1 } else { 0 };
    ui.set_people_items(ModelRc::new(VecModel::from(items)));
    ui.set_people_index(index);
}

fn search(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    // While a creation runs nothing changes: no rows, no status, no step.
    if with(|directory| directory.busy) {
        return;
    }
    let query = ui.get_people_query().to_string();
    // A complete ID is usable at once, even without a directory answer.
    with(|directory| directory.people.clear());
    project(ui);
    let query = match validate_people_query(&query) {
        Ok(query) => query,
        Err(_) => {
            ui.set_people_status("".into());
            return;
        }
    };
    let Some(scope) = step(ui) else {
        return;
    };
    ui.set_people_error("".into());
    ui.set_people_status(SEARCHING.into());
    let weak = ui.as_weak();
    let live = live.clone();
    runtime.spawn(async move {
        let page = live
            .manager
            .search_people_for(&scope.selection, &scope.cancellation, &query)
            .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            // Only the newest search for the query still typed applies.
            if !scope.current(&ui) || ui.get_people_query().trim() != query {
                return;
            }
            match page {
                Ok(page) => {
                    let limited = page.limited;
                    let empty = page.people.is_empty();
                    with(|directory| directory.people = page.people);
                    project(&ui);
                    ui.set_people_status(
                        if empty && exact_user_id(&query).is_none() {
                            NO_MATCH
                        } else if limited {
                            LIMITED
                        } else {
                            ""
                        }
                        .into(),
                    );
                }
                Err(PeopleError::Cancelled | PeopleError::AccountChanged) => {}
                Err(error) => {
                    ui.set_people_status("".into());
                    ui.set_people_error(error.to_string().into());
                }
            }
        });
    });
}

/// Choose a person: look up direct conversations this device knows.
fn choose(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, index: i32) {
    if with(|directory| directory.busy) {
        return;
    }
    let Some(item) = usize::try_from(index)
        .ok()
        .and_then(|index| ui.get_people_items().row_data(index))
    else {
        return;
    };
    let Some(scope) = step(ui) else {
        return;
    };
    let user_id = item.user_id.to_string();
    with(|directory| {
        directory.chosen = Some(user_id.clone());
        directory.known = None;
    });
    ui.set_people_index(index);
    ui.set_people_chosen_id(item.user_id.clone());
    ui.set_people_chosen_name(item.name.clone());
    ui.set_people_known(KNOWN_CHECKING);
    ui.set_people_confirm(false);
    ui.set_people_error("".into());
    ui.set_people_status(CHECKING.into());
    let weak = ui.as_weak();
    let live = live.clone();
    runtime.spawn(async move {
        let known = live
            .manager
            .known_direct_for(&scope.selection, &scope.cancellation, &user_id)
            .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !scope.current(&ui)
                || with(|directory| directory.chosen.as_deref() != Some(user_id.as_str()))
            {
                return;
            }
            ui.set_people_status("".into());
            match known {
                Ok(known) => {
                    let (state, count) = known_state(&known);
                    with(|directory| directory.known = Some(known));
                    ui.set_people_known(state);
                    ui.set_people_known_count(count);
                }
                Err(PeopleError::Cancelled | PeopleError::AccountChanged) => {}
                Err(error) => {
                    ui.set_people_known(KNOWN_UNSET);
                    ui.set_people_error(error.to_string().into());
                }
            }
        });
    });
}

/// The created room's `m.direct` listing in plain words, appended to the
/// creation status. The room exists in every case.
fn listing_note(listing: &Result<DirectListing, DirectWriteError>) -> &'static str {
    match listing {
        Ok(_) => "",
        Err(DirectWriteError::Unknown) => LISTING_UNKNOWN,
        Err(DirectWriteError::BaselineChanged) => LISTING_CHANGED,
        Err(_) => NOT_MARKED,
    }
}

fn known_state(known: &KnownDirect) -> (i32, i32) {
    match known {
        KnownDirect::NoneKnown => (KNOWN_NONE, 0),
        KnownDirect::One(_) => (KNOWN_ONE, 1),
        KnownDirect::Several { count, .. } => {
            (KNOWN_SEVERAL, i32::try_from(*count).unwrap_or(i32::MAX))
        }
    }
}

/// The room to open for a known conversation: the only one, or the first
/// of several (the window says there are several).
fn known_room(known: &KnownDirect) -> Option<&str> {
    match known {
        KnownDirect::NoneKnown => None,
        KnownDirect::One(room) => Some(room),
        KnownDirect::Several { rooms, .. } => rooms.first().map(String::as_str),
    }
}

/// Open the known conversation through the shared seam (drafts kept).
fn open_known(ui: &AppWindow) {
    let target = with(|directory| {
        let selection = directory.selection.clone()?;
        if !selection.is_current()
            || directory.account != ui.get_active_account_local_id().as_str()
            || directory.busy
        {
            return None;
        }
        let room = known_room(directory.known.as_ref()?)?.to_owned();
        Some((directory.account.clone(), room))
    });
    let Some((account, room)) = target else {
        return;
    };
    clear_presentation(ui);
    ui.set_page(0);
    ui.invoke_open_conversation(
        account.into(),
        room.into(),
        SharedString::new(),
        SharedString::new(),
    );
}

/// Ask for an explicit confirmation naming the exact user ID.
fn ask_start(ui: &AppWindow) {
    let ready = with(|directory| {
        !directory.busy && matches!(directory.known, Some(KnownDirect::NoneKnown))
    });
    if ready && !ui.get_people_chosen_id().is_empty() {
        ui.set_people_confirm(true);
    }
}

/// Create the conversation once, after the confirmation.
fn confirm_start(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    ui.set_people_confirm(false);
    if !unobstructed(ui, live) {
        return;
    }
    let chosen = ui.get_people_chosen_id().to_string();
    let ready = with(|directory| {
        !directory.busy
            && directory.chosen.as_deref() == Some(chosen.as_str())
            && matches!(directory.known, Some(KnownDirect::NoneKnown))
    });
    if !ready {
        return;
    }
    let Some(scope) = step(ui) else {
        return;
    };
    // Busy from admission: every later search, choice or step is refused
    // until this creation's own completion clears it.
    with(|directory| directory.busy = true);
    ui.set_people_busy(true);
    ui.set_people_error("".into());
    ui.set_people_status(CREATING.into());
    let weak = ui.as_weak();
    let live = live.clone();
    runtime.spawn(async move {
        // Test builds only: a test may hold the admitted creation here, at
        // the boundary before its request, to act while it is busy.
        #[cfg(test)]
        live.pauses.reach("people.before_create").await;
        let created = live
            .manager
            .create_direct_for(&scope.selection, &scope.cancellation, &chosen)
            .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            // A closed window or changed account shows nothing more; the
            // conversation, if created, appears in that account's list.
            if !scope.current(&ui) {
                return;
            }
            with(|directory| directory.busy = false);
            ui.set_people_busy(false);
            match created {
                // Open at once only when it is synced and listed; otherwise
                // stay and say exactly what is and is not confirmed.
                Ok(created) if created.synced && created.direct_listing.is_ok() => {
                    let account = SharedString::from(scope.account.as_str());
                    clear_presentation(&ui);
                    ui.set_page(0);
                    ui.invoke_open_conversation(
                        account,
                        created.room_id.as_str().into(),
                        SharedString::new(),
                        SharedString::new(),
                    );
                }
                Ok(created) => {
                    with(|directory| {
                        directory.known = Some(KnownDirect::One(created.room_id.clone()));
                    });
                    ui.set_people_known(KNOWN_ONE);
                    ui.set_people_known_count(1);
                    ui.set_people_status(
                        format!(
                            "{}{}",
                            if created.synced {
                                CREATED_READY
                            } else {
                                CREATED_PENDING
                            },
                            listing_note(&created.direct_listing)
                        )
                        .into(),
                    );
                }
                Err(error) => {
                    ui.set_people_status("".into());
                    ui.set_people_error(error.to_string().into());
                    // After an unknown outcome the person may already have a
                    // conversation: never offer an immediate second start.
                    if error == PeopleError::CreateUnknown {
                        with(|directory| directory.known = None);
                        ui.set_people_known(KNOWN_UNSET);
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(id: &str, name: Option<&str>) -> DirectoryPerson {
        DirectoryPerson {
            user_id: id.into(),
            display_name: name.map(str::to_owned),
        }
    }

    #[test]
    fn a_busy_creation_refuses_every_new_step_so_its_completion_still_applies() {
        // A step while busy would bump the operation and orphan the
        // creation's completion: refused.
        assert!(!step_allowed(true, true, true));
        assert!(step_allowed(false, true, true));
        assert!(!step_allowed(false, false, true));
        assert!(!step_allowed(false, true, false));
        // The search guard leaves rows untouched while busy.
        DIRECTORY.with(|directory| *directory.borrow_mut() = Directory::default());
        with(|directory| {
            directory.busy = true;
            directory.people = vec![person("@kept:example.org", None)];
        });
        let operation = with(|directory| directory.operation);
        assert!(with(|directory| directory.busy));
        assert_eq!(with(|directory| directory.operation), operation);
        assert_eq!(with(|directory| directory.people.len()), 1);
        DIRECTORY.with(|directory| *directory.borrow_mut() = Directory::default());
    }

    #[test]
    fn an_exact_id_is_offered_first_and_never_twice() {
        let people = [
            person("@b:example.org", Some("Synthetic B")),
            person("@a:example.org", Some("Synthetic A")),
        ];
        let items = rows("@a:example.org", &people);
        assert_eq!(items.len(), 2);
        assert!(items[0].exact);
        assert_eq!(items[0].user_id, "@a:example.org");
        assert_eq!(items[0].name, "Synthetic A");
        assert_eq!(items[1].user_id, "@b:example.org");
        // An exact ID without a directory result is still usable.
        let alone = rows("@new:example.org", &[]);
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].name, "@new:example.org");
        // A plain search shows directory rows only, names or IDs.
        let plain = rows("synthetic", &[person("@c:example.org", None)]);
        assert_eq!(plain.len(), 1);
        assert!(!plain[0].exact);
        assert_eq!(plain[0].name, "@c:example.org");
    }

    #[test]
    fn a_created_room_is_never_reported_failed_for_its_listing() {
        assert_eq!(listing_note(&Ok(DirectListing::Listed)), "");
        assert_eq!(listing_note(&Ok(DirectListing::AlreadyListed)), "");
        // Unknown after the write left: unconfirmed, not retried.
        let unknown = listing_note(&Err(DirectWriteError::Unknown));
        assert!(unknown.contains("could not be confirmed") && unknown.contains("not tried again"));
        assert!(
            listing_note(&Err(DirectWriteError::BaselineChanged))
                .contains("nothing was overwritten")
        );
        assert_eq!(listing_note(&Err(DirectWriteError::Refused)), NOT_MARKED);
        // None of them says the creation failed.
        for error in [
            DirectWriteError::Unknown,
            DirectWriteError::BaselineChanged,
            DirectWriteError::Refused,
            DirectWriteError::Unreadable,
            DirectWriteError::NotSent,
        ] {
            assert!(!listing_note(&Err(error)).contains("failed"));
        }
    }

    #[test]
    fn several_and_none_known_are_distinct_states() {
        assert_eq!(known_state(&KnownDirect::NoneKnown), (KNOWN_NONE, 0));
        assert_eq!(
            known_state(&KnownDirect::One("!a:example.org".into())),
            (KNOWN_ONE, 1)
        );
        let several = KnownDirect::Several {
            rooms: vec!["!a:example.org".into(), "!b:example.org".into()],
            count: 9,
        };
        assert_eq!(known_state(&several), (KNOWN_SEVERAL, 9));
        assert_eq!(known_room(&several), Some("!a:example.org"));
        // Nothing known: there is nothing to open, only an explicit start.
        assert_eq!(known_room(&KnownDirect::NoneKnown), None);
    }
}
