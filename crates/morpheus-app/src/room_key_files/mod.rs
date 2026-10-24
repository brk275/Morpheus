//! Explicit encrypted Matrix room-key import/export for one captured account.
//!
//! VerificationWork owns the enclosing future. DialogWorkTicket separately
//! proves the real chooser/CPU worker and supervisor joins. Cancellation only
//! revokes new handoffs and presentation; admitted SDK/native work is awaited.

mod io;

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use morpheus_matrix::{
    AccountSelection, RoomKeyFileError, RoomKeyImportCounts, VerificationCancellation,
};
use slint::ComponentHandle;
use tokio::{runtime::Handle, sync::oneshot};
use zeroize::Zeroizing;

use crate::{AppWindow, live::LiveUi, style_profiles::DialogPermit};

type Admission = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone, Copy)]
enum Action {
    Import,
    Export,
}
struct Active {
    identity: Arc<()>,
    account: String,
    selection: AccountSelection,
    cancellation: VerificationCancellation,
    presentation: Arc<AtomicBool>,
}
#[derive(Default)]
struct State {
    closed: bool,
    active: Option<Active>,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

enum Completed {
    Cancelled,
    Imported(RoomKeyImportCounts),
    Exported {
        keys: usize,
        publication: io::Publication,
    },
}
enum Failure {
    Core(RoomKeyFileError),
    Io(io::IoError),
    Worker,
}
impl Failure {
    fn owner_failed(&self) -> bool {
        matches!(self, Self::Worker)
            || matches!(self, Self::Io(error) if error.ownership_uncertain())
    }
    fn message(&self) -> String {
        match self {
            Self::Core(error) => error.to_string(),
            Self::Io(error) => error.to_string(),
            Self::Worker => "The encrypted key-file worker failed. Its effects could not be confirmed; account removal and clean shutdown remain blocked.".to_owned(),
        }
    }
}
fn eligible(ui: &AppWindow, live: &LiveUi) -> bool {
    ui.get_live_session()
        && ui.get_page() == 1
        && !ui.get_room_key_file_closed()
        && !ui.get_sas_protected()
        && !ui.get_sas_busy()
        && ui.get_sas_stage() == 0
        && !live.security_gate.blocked()
}

/// Existing navigation/protection entry points call this before changing the
/// visible context. It is unconditional because that old context is still live
/// at this boundary; deferred property observers are only a fallback.
fn withdraw(ui: &AppWindow) {
    with(|state| {
        if let Some(active) = &state.active {
            active.presentation.store(false, Ordering::Release);
            active.cancellation.cancel();
        }
    });
    ui.set_room_key_file_message("".into());
}

/// Called when persistent page/account/protection observers are processed. It clears UI
/// secrets in the component and revokes later actions, never drops a worker.
pub(crate) fn context_changed(ui: &AppWindow) {
    with(|state| {
        if let Some(active) = &state.active {
            if !ui.get_live_session()
                || ui.get_application_closing()
                || ui.get_room_key_file_closed()
                || ui.get_page() != 1
                || ui.get_sas_protected()
                || ui.get_sas_busy()
                || ui.get_sas_stage() != 0
                || ui.get_active_account_local_id().as_str() != active.account
                || !active.selection.is_current()
            {
                active.presentation.store(false, Ordering::Release);
                active.cancellation.cancel();
            }
        }
    });
    ui.set_room_key_file_message("".into());
}
/// Called before normal teardown yields, alongside the existing verification
/// owner close. That owner's real drain retains this future until both joins.
pub(crate) fn close_admission(ui: &AppWindow) {
    // Quit may already be outside the event loop: clear private inputs and
    // withdraw their view before closing flags can defer/coalesce observers.
    ui.invoke_withdraw_verification();
    ui.set_room_key_file_closed(true);
    with(|state| {
        state.closed = true;
        if let Some(active) = &state.active {
            active.presentation.store(false, Ordering::Release);
            active.cancellation.cancel();
        }
    });
}

fn cancel(ui: &AppWindow) {
    with(|state| {
        if let Some(active) = &state.active {
            active.presentation.store(false, Ordering::Release);
            active.cancellation.cancel();
        }
    });
    ui.set_room_key_file_message("New file actions were cancelled. Close an open chooser to finish; an import or publication already admitted may still complete.".into());
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    let import_live = live.clone();
    let import_handle = handle.clone();
    ui.on_room_key_file_import(move |passphrase| {
        if let Some(ui) = weak.upgrade() {
            if !passphrase_allowed(Action::Import, passphrase.as_str().len()) {
                ui.set_room_key_file_message(
                    "Enter the key-file passphrase (1–1024 UTF-8 bytes).".into(),
                );
                return;
            }
            start(
                &ui,
                import_live.clone(),
                import_handle.clone(),
                Action::Import,
                Zeroizing::new(passphrase.to_string()),
            );
        }
    });
    let weak = ui.as_weak();
    ui.on_room_key_file_export(move |passphrase, confirmation| {
        if let Some(ui) = weak.upgrade() {
            if !passphrase_allowed(Action::Export, passphrase.as_str().len()) {
                ui.set_room_key_file_message(
                    "A fresh export passphrase requires 12–1024 UTF-8 bytes.".into(),
                );
                return;
            }
            if passphrase.as_str() != confirmation.as_str() {
                ui.set_room_key_file_message("The new export passphrases do not match.".into());
                return;
            }
            start(
                &ui,
                live.clone(),
                handle.clone(),
                Action::Export,
                Zeroizing::new(passphrase.to_string()),
            );
        }
    });
    let weak = ui.as_weak();
    ui.on_room_key_file_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            cancel(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_room_key_file_withdraw(move || {
        if let Some(ui) = weak.upgrade() {
            withdraw(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_room_key_file_context_changed(move || {
        if let Some(ui) = weak.upgrade() {
            context_changed(&ui);
        }
    });
}

fn passphrase_allowed(action: Action, bytes: usize) -> bool {
    let minimum = match action {
        Action::Import => 1,
        Action::Export => 12,
    };
    (minimum..=1024).contains(&bytes)
}
fn start(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    handle: Handle,
    action: Action,
    passphrase: Zeroizing<String>,
) {
    if !eligible(ui, &live) || with(|state| state.closed || state.active.is_some()) {
        return;
    }
    if !passphrase_allowed(action, passphrase.len()) {
        ui.set_room_key_file_message("Enter a passphrase of at most 1024 UTF-8 bytes. A new export passphrase needs at least 12 bytes.".into());
        return;
    }
    let account = ui.get_active_account_local_id().to_string();
    let Some(selection) = live.manager.account_selection(&account) else {
        return;
    };
    let cancellation = VerificationCancellation::new();
    let Some(work) = live.verification_work_lease(&selection, &cancellation) else {
        ui.set_room_key_file_message("This account is stopping or its work ownership could not be confirmed. No file action was started.".into());
        return;
    };
    let Some(permit) = DialogPermit::acquire() else {
        ui.set_room_key_file_message(DialogPermit::unavailable_message().into());
        return;
    };
    let gate = live.security_gate.clone();
    let generation = gate.ringing_generation();
    let presentation_gate = gate.clone();
    let presentation = Arc::new(AtomicBool::new(true));
    let presentation_current = presentation.clone();
    let present: Admission = Arc::new(move || {
        presentation_current.load(Ordering::Acquire)
            && presentation_gate.ringing_allowed(generation)
    });
    // The original view flag is write authority too: leaving the persistent
    // window context cannot be revived by remounting the Settings panel.
    let admitted_view = presentation.clone();
    let admitted_work = work.clone();
    let admitted_selection = selection.clone();
    let admission: Admission = Arc::new(move || {
        admitted_view.load(Ordering::Acquire)
            && admitted_work.is_current()
            && admitted_selection.is_current()
            && gate.ringing_allowed(generation)
    });
    let identity = Arc::new(());
    with(|state| {
        state.active = Some(Active {
            identity: identity.clone(),
            account: account.clone(),
            selection: selection.clone(),
            cancellation,
            presentation,
        })
    });
    ui.set_room_key_file_busy(true);
    ui.set_room_key_file_message(
        "The encrypted key-file operation is waiting for your file choice.".into(),
    );
    let weak = ui.as_weak();
    let task_identity = identity.clone();
    let task_live = live.clone();
    let task_work = work.clone();
    let worker_handle = handle.clone();
    let task = async move {
        let result = run(
            task_live.clone(),
            worker_handle,
            &selection,
            action,
            passphrase,
            permit,
            admission.clone(),
        )
        .await;
        let failed_owner = match &result {
            Err(error) => error.owner_failed(),
            Ok(Completed::Exported {
                publication: io::Publication::PublishedCleanupUncertain,
                ..
            }) => true,
            _ => false,
        };
        // run() either refuses native admission or awaits the exact worker
        // AND supervisor joins.
        // A joined panic/retained native capability is Failed, not a fake live
        // task. Its actual enclosing JoinHandle remains registered until return.
        if failed_owner {
            task_work.fail_native_completion();
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish(
                &ui,
                &task_live,
                &task_identity,
                &account,
                &selection,
                present.as_ref(),
                result,
            );
        });
    };
    if !live.spawn_verification_work(&handle, &work, task) {
        with(|state| {
            if state
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.identity, &identity))
            {
                state.active = None;
            }
        });
        ui.set_room_key_file_busy(false);
        ui.set_room_key_file_message(
            "The account stopped before file work was admitted. No file action was started.".into(),
        );
    }
}

async fn run(
    live: Arc<LiveUi>,
    runtime: Handle,
    selection: &AccountSelection,
    action: Action,
    passphrase: Zeroizing<String>,
    permit: DialogPermit,
    admission: Admission,
) -> Result<Completed, Failure> {
    let scope = live
        .manager
        .room_key_file_scope_for(selection)
        .await
        .map_err(Failure::Core)?;
    if !admission() {
        return Ok(Completed::Cancelled);
    }
    let (send, receive) = oneshot::channel();
    let send = Arc::new(std::sync::Mutex::new(Some(send)));
    let failed = send.clone();
    let owner = permit.clone();
    let manager = live.manager.clone();
    let ticket = owner
        .spawn_owned(
            "morpheus-encrypted-room-keys",
            move || {
                let result = (|| {
                    if !admission() {
                        return Ok(Completed::Cancelled);
                    }
                    let dialog =
                        rfd::FileDialog::new().add_filter("Encrypted Matrix room keys", &["txt"]);
                    let path = match action {
                        Action::Import => dialog
                            .set_title("Import encrypted Matrix room keys")
                            .pick_file(),
                        Action::Export => dialog
                            .set_title("Export encrypted Matrix room keys")
                            .set_file_name("Morpheus-room-keys.txt")
                            .save_file(),
                    };
                    let Some(path) = path else {
                        return Ok(Completed::Cancelled);
                    };
                    if !admission() {
                        return Ok(Completed::Cancelled);
                    }
                    match action {
                        Action::Import => {
                            let encrypted = io::read_encrypted(&path).map_err(Failure::Io)?;
                            // No selection/cancellation future may drop this admitted
                            // canonical preparation or actual store-import operation.
                            let counts = runtime
                                .block_on(manager.import_room_key_file_for(
                                    &scope,
                                    &encrypted,
                                    &passphrase,
                                    admission.as_ref(),
                                ))
                                .map_err(Failure::Core)?;
                            Ok(Completed::Imported(counts))
                        }
                        Action::Export => {
                            let encrypted = runtime
                                .block_on(manager.encrypt_room_key_file_for(
                                    &scope,
                                    &passphrase,
                                    admission.as_ref(),
                                ))
                                .map_err(Failure::Core)?;
                            if !admission() {
                                return Ok(Completed::Cancelled);
                            }
                            let keys = encrypted.key_count();
                            let mut stage = io::ExportStage::prepare(&path, encrypted.bytes())
                                .map_err(Failure::Io)?;
                            // Stage remains borrowed by the callback: refused authority
                            // still leaves this exact stage available for checked discard.
                            match runtime.block_on(manager.commit_room_key_file_for(
                                &scope,
                                admission.as_ref(),
                                || stage.publish(),
                            )) {
                                Ok(Ok(publication)) => {
                                    Ok(Completed::Exported { keys, publication })
                                }
                                Ok(Err(error)) => {
                                    // Preparing may be cleaned here. A commit-attempted
                                    // stage must reject this call without deletion/retry.
                                    let cleanup = stage.discard();
                                    if error.ownership_uncertain() {
                                        Err(Failure::Io(error))
                                    } else {
                                        cleanup.map_err(Failure::Io)?;
                                        Err(Failure::Io(error))
                                    }
                                }
                                Err(error) => {
                                    stage.discard().map_err(Failure::Io)?;
                                    Err(Failure::Core(error))
                                }
                            }
                        }
                    }
                })();
                if let Ok(mut send) = send.lock() {
                    if let Some(send) = send.take() {
                        let _ = send.send(result);
                    }
                }
                drop(permit);
            },
            move || {
                if let Ok(mut send) = failed.lock() {
                    if let Some(send) = send.take() {
                        let _ = send.send(Err(Failure::Worker));
                    }
                }
            },
        )
        .map_err(|_| Failure::Worker)?;
    let result = receive.await.unwrap_or(Err(Failure::Worker));
    let joined = ticket.wait_joined().await;
    if joined.is_err() {
        return Err(Failure::Worker);
    }
    result
}

fn finish(
    ui: &AppWindow,
    live: &LiveUi,
    identity: &Arc<()>,
    account: &str,
    selection: &AccountSelection,
    admitted: &(dyn Fn() -> bool + Send + Sync),
    result: Result<Completed, Failure>,
) {
    let ours = with(|state| {
        if !state
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.identity, identity))
        {
            return false;
        }
        state.active = None;
        true
    });
    if !ours {
        return;
    }
    ui.set_room_key_file_busy(false);
    // A cancelled/protected/account-replaced presentation never revives. Fixed
    // native ownership failure is still recorded in the task owner, independent
    // of whether this UI context is allowed to see a result.
    if !eligible(ui, live)
        || ui.get_active_account_local_id().as_str() != account
        || !selection.is_current()
        || !admitted()
    {
        return;
    }
    let message = match result {
        Ok(Completed::Cancelled) => "No new file action was started.".to_owned(),
        Ok(Completed::Imported(counts)) => format!(
            "Imported {} room keys; the SDK accepted {} of {} file records. This does not confirm homeserver backup completion.",
            counts.imported, counts.sdk_valid, counts.file_records
        ),
        Ok(Completed::Exported {
            keys,
            publication: io::Publication::Published,
        }) => format!(
            "Published an encrypted file containing all {keys} keys in the captured account snapshot. Keep its fresh passphrase separately. Directory/media durability is not confirmed."
        ),
        Ok(Completed::Exported {
            keys,
            publication: io::Publication::PublishedCleanupUncertain,
        }) => format!(
            "The encrypted {keys}-key file was published, but native cleanup could not be confirmed. Do not repeat the export blindly; account removal and clean shutdown remain blocked."
        ),
        Err(error) => error.message(),
    };
    ui.set_room_key_file_message(message.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_export_passphrase_and_uncertain_owner_outcomes_are_distinct() {
        assert!(!passphrase_allowed(Action::Export, 11));
        assert!(passphrase_allowed(Action::Export, 12));
        assert!(passphrase_allowed(Action::Import, 1));
        assert!(!passphrase_allowed(Action::Import, 0));
        assert!(!passphrase_allowed(Action::Import, 1025));
        assert!(Failure::Io(io::IoError::CommitUncertain).owner_failed());
        assert!(Failure::Io(io::IoError::NativeUncertain).owner_failed());
        assert!(!Failure::Io(io::IoError::Exists).owner_failed());
        assert!(!Failure::Core(RoomKeyFileError::InvalidFile).owner_failed());
    }
}
