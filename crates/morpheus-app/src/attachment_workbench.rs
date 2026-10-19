//! Per-room composer attachment queue. Chosen files wait as local paths only,
//! with no bytes read, until the user sends them. Each sent file then has its
//! own task and small command channel. The core reads it with the 25 MiB cap
//! into the SDK's encrypted per-account media store and persistent room send
//! queue, so this module keeps no second copy of any content. Rows are
//! projected into Slint only for the account and room on screen.
//!
//! No item is ever sent twice automatically. A file that failed before it was
//! queued can be retried once by the user. A paused upload is retried in place
//! with the SDK's unwedge, never re-enqueued. An upload whose outcome is
//! unknown can only be dismissed.
//!
//! Saving a received attachment asks for the location every time and can be
//! cancelled while it downloads. The bounded local write, guarded by the core
//! against navigation, is never interrupted halfway, so the reported outcome
//! is always what happened on disk.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::{
    AccountSelection, AttachmentSendUpdate, MAX_ATTACHMENT_BYTES, MatrixCoreError, QueuedAttachment,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::{
    runtime::Handle,
    sync::{Semaphore, mpsc, watch},
};

use crate::{
    AppWindow, AttachmentItem,
    attachments::{mime_type_for, safe_file_name},
    live::LiveUi,
};

/// Attachments that can wait in one room at once.
const MAX_ROOM_ITEMS: usize = 10;
/// Attachments remembered across every account and room.
const MAX_ITEMS: usize = 64;
/// Bound simultaneous file reads and SDK preparation across all rooms.
const MAX_PREPARING: usize = 2;
/// Minimum spacing of one upload's progress updates.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// Commands are user clicks; a full channel drops a repeated click.
const COMMAND_CAPACITY: usize = 4;
const TOO_LARGE: &str = "Too large: attachments are limited to 25 MiB.";

const ACTION_REMOVE: i32 = 0;
const ACTION_CANCEL: i32 = 1;
const ACTION_RETRY: i32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Cancel,
    Retry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Phase {
    /// Chosen and waiting for Send; nothing has been read.
    Staged,
    /// Being read and handed to the room send queue.
    Preparing,
    /// In the room send queue, with SDK progress as a percentage.
    Uploading(u8),
    /// The room queue hit a recoverable error and retries by itself.
    Waiting,
    /// The SDK wedged this upload; it waits for Retry or Cancel.
    Paused,
    Cancelling,
    /// Never queued, so one retry cannot duplicate it.
    Failed(String),
    /// Can't be sent as chosen, such as a file over the size limit.
    Rejected(String),
    /// The room queue may still send it. Never retried automatically.
    Uncertain,
}

impl Phase {
    fn index(&self) -> i32 {
        match self {
            Self::Staged => 0,
            Self::Preparing => 1,
            Self::Uploading(_) => 2,
            Self::Waiting => 3,
            Self::Paused => 4,
            Self::Cancelling => 5,
            Self::Failed(_) => 6,
            Self::Rejected(_) => 7,
            Self::Uncertain => 8,
        }
    }

    /// Whether a task currently owns this item.
    fn running(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Uploading(_) | Self::Waiting | Self::Paused | Self::Cancelling
        )
    }
}

struct Item {
    id: u64,
    account_id: String,
    room_id: String,
    path: PathBuf,
    filename: String,
    mime_type: String,
    size: Option<u64>,
    phase: Phase,
    /// A transient explanation shown until the phase changes.
    note: String,
    commands: Option<mpsc::Sender<Command>>,
    cancel: Option<watch::Sender<bool>>,
    progress_at: Option<Instant>,
}

impl Item {
    fn in_room(&self, account_id: &str, room_id: &str) -> bool {
        !account_id.is_empty() && self.account_id == account_id && self.room_id == room_id
    }
}

/// What one upload task owns: never the window or the item itself.
struct Job {
    id: u64,
    selection: AccountSelection,
    account_id: String,
    room_id: String,
    path: PathBuf,
    mime_type: String,
    filename: String,
}

enum Outcome {
    Sent,
    Cancelled,
    AlreadySent,
    Failed(String),
    Rejected(String),
    Uncertain,
}

#[derive(Default)]
struct State {
    next_id: u64,
    items: Vec<Item>,
}

/// The save in progress. Replacing or dropping it cancels its download.
struct SaveJob {
    operation: i32,
    cancel: watch::Sender<bool>,
}

enum SaveOutcome {
    Saved(PathBuf),
    /// Stopped before anything was written.
    Cancelled,
    Failed(String),
}

pub struct Workbench {
    live: Arc<LiveUi>,
    runtime: Handle,
    window: slint::Weak<AppWindow>,
    state: Mutex<State>,
    /// A projection is already waiting on the event loop.
    queued: AtomicBool,
    save: Mutex<Option<SaveJob>>,
    preparing: Semaphore,
}

impl Workbench {
    /// Install the composer attachment callbacks. The returned owner is also
    /// held by those callbacks for the window's lifetime.
    pub fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime,
            window: ui.as_weak(),
            state: Mutex::new(State::default()),
            queued: AtomicBool::new(false),
            save: Mutex::new(None),
            preparing: Semaphore::new(MAX_PREPARING),
        });
        {
            let owner = owner.clone();
            ui.on_attach_file(move || owner.choose_files());
        }
        {
            let owner = owner.clone();
            ui.on_send_attachments(move || {
                if let Some(ui) = owner.window.upgrade() {
                    owner.send_staged(&ui);
                }
            });
        }
        {
            let owner = owner.clone();
            ui.on_attachment_action(move |id, action| {
                let (Some(ui), Ok(id)) = (owner.window.upgrade(), u64::try_from(id)) else {
                    return;
                };
                match action {
                    ACTION_REMOVE => owner.remove(&ui, id),
                    ACTION_CANCEL => owner.cancel(&ui, id),
                    ACTION_RETRY => owner.retry(&ui, id),
                    _ => {}
                }
            });
        }
        {
            let owner = owner.clone();
            ui.on_save_attachment(move |event_id, filename| owner.save(&event_id, &filename));
        }
        {
            let owner = owner.clone();
            ui.on_cancel_attachment_save(move || owner.cancel_save());
        }
        {
            let owner = owner.clone();
            ui.on_attachment_context_changed(move || {
                if let Some(ui) = owner.window.upgrade() {
                    owner.project(&ui);
                }
                owner.forget_removed_accounts();
            });
        }
        owner
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn lock_save(&self) -> MutexGuard<'_, Option<SaveJob>> {
        self.save.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn save(self: &Arc<Self>, event_id: &str, filename: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        if !self.live.is_live.load(Ordering::Acquire)
            || event_id.is_empty()
            || !ui.get_downloading_event_id().is_empty()
        {
            return;
        }
        let account_id = ui.get_active_account_local_id().to_string();
        let room_id = ui.get_active_room_id().to_string();
        let Some(selection) = self.live.manager.account_selection(&account_id) else {
            return;
        };
        // The sender chose this name: suggest only a sanitized final
        // component, and let the user pick the folder explicitly.
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save attachment")
            .set_file_name(safe_file_name(filename));
        if let Some(downloads) = directories::UserDirs::new()
            .and_then(|dirs| dirs.download_dir().map(|dir| dir.to_path_buf()))
        {
            dialog = dialog.set_directory(downloads);
        }
        let Some(path) = dialog.save_file() else {
            return;
        };
        if ui.get_active_account_local_id().as_str() != account_id
            || ui.get_active_room_id().as_str() != room_id
            || !selection.is_current()
            || path.file_name().is_none()
        {
            return;
        }
        let operation = ui.get_download_operation().wrapping_add(1);
        ui.set_download_operation(operation);
        ui.set_downloading_event_id(event_id.into());
        ui.set_transfer_notice("Downloading and decrypting attachment…".into());
        let (cancel, cancelled) = watch::channel(false);
        // Dropping an older job's sender cancels that save's download.
        *self.lock_save() = Some(SaveJob { operation, cancel });
        let owner = self.clone();
        let event_id = event_id.to_owned();
        self.runtime.spawn(async move {
            let mut cancelled = cancelled;
            let outcome = owner
                .download_and_save(&selection, &room_id, &event_id, path, &mut cancelled)
                .await;
            {
                let mut save = owner.lock_save();
                if save.as_ref().is_some_and(|job| job.operation == operation) {
                    *save = None;
                }
            }
            let _ = owner.window.upgrade_in_event_loop(move |ui| {
                if !selection.is_current()
                    || !ui.get_live_session()
                    || ui.get_active_account_local_id().as_str() != account_id
                    || ui.get_active_room_id().as_str() != room_id
                    || ui.get_download_operation() != operation
                    || ui.get_downloading_event_id().as_str() != event_id
                {
                    return;
                }
                ui.set_downloading_event_id("".into());
                match outcome {
                    SaveOutcome::Saved(path) => {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        let folder = path
                            .parent()
                            .map(|folder| folder.display().to_string())
                            .unwrap_or_default();
                        let notice = format!("Saved {name} to {folder}");
                        ui.set_transfer_notice(crate::directory_plain_text(&notice, 300).into());
                    }
                    SaveOutcome::Cancelled => {
                        ui.set_transfer_notice("Save cancelled; nothing was written.".into());
                    }
                    SaveOutcome::Failed(error) => {
                        ui.set_transfer_notice("".into());
                        ui.set_session_error(format!("Could not save attachment: {error}").into());
                    }
                }
            });
        });
    }

    async fn download_and_save(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        event_id: &str,
        path: PathBuf,
        cancelled: &mut watch::Receiver<bool>,
    ) -> SaveOutcome {
        let downloaded = tokio::select! {
            biased;
            // A cancel, or a newer save replacing this one, drops the download.
            () = async {
                let _ = cancelled.wait_for(|cancelled| *cancelled).await;
            } => return SaveOutcome::Cancelled,
            _ = selection.revoked() => return SaveOutcome::Cancelled,
            downloaded = self.live.download_attachment(selection.account_id(), room_id, event_id) => match downloaded {
                Ok(downloaded) => downloaded,
                Err(error) => return SaveOutcome::Failed(error.to_string()),
            },
        };
        if !selection.is_current() || *cancelled.borrow() || cancelled.has_changed().is_err() {
            return SaveOutcome::Cancelled;
        }
        // The final account and room guard is held through the local write,
        // so navigation cannot race the plaintext save. Once it starts, the
        // bounded write finishes and its real outcome is reported.
        match self
            .live
            .save_download_to(selection, room_id, path, downloaded.bytes, cancelled)
            .await
        {
            Ok(Some(path)) => SaveOutcome::Saved(path),
            Ok(None) => SaveOutcome::Cancelled,
            Err(error) => SaveOutcome::Failed(error.to_string()),
        }
    }

    fn cancel_save(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let operation = ui.get_download_operation();
        let sent = self
            .lock_save()
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.cancel.send(true).is_ok());
        if sent && !ui.get_downloading_event_id().is_empty() {
            ui.set_transfer_notice("Cancelling save…".into());
        }
    }

    fn choose_files(self: &Arc<Self>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        if !self.live.is_live.load(Ordering::Acquire)
            || ui.get_active_room_id().is_empty()
            || ui.get_composer_mode() != 0
            || !ui.get_attachment_can_add()
        {
            return;
        }
        let account_id = ui.get_active_account_local_id().to_string();
        let room_id = ui.get_active_room_id().to_string();
        // The system picker runs modally on the Slint UI thread, which is
        // required by macOS AppKit. File reads happen later, on Tokio.
        let Some(paths) = rfd::FileDialog::new()
            .set_title("Choose attachments")
            .pick_files()
        else {
            return;
        };
        if ui.get_active_account_local_id().as_str() != account_id
            || ui.get_active_room_id().as_str() != room_id
        {
            return;
        }
        self.stage(&ui, &account_id, &room_id, paths);
    }

    fn stage(&self, ui: &AppWindow, account_id: &str, room_id: &str, paths: Vec<PathBuf>) {
        let mut skipped = 0_usize;
        {
            let mut state = self.lock();
            let mut in_room = state
                .items
                .iter()
                .filter(|item| item.in_room(account_id, room_id))
                .count();
            for path in paths {
                if in_room >= MAX_ROOM_ITEMS || state.items.len() >= MAX_ITEMS {
                    skipped += 1;
                    continue;
                }
                // The picker returned this path; reading it waits for Send.
                let size = std::fs::metadata(&path)
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .map(|metadata| metadata.len());
                let phase = match size {
                    None => Phase::Rejected("This file can't be read.".into()),
                    Some(size) if size > MAX_ATTACHMENT_BYTES as u64 => {
                        Phase::Rejected(TOO_LARGE.into())
                    }
                    Some(_) => Phase::Staged,
                };
                let filename = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "attachment".into());
                state.next_id += 1;
                let id = state.next_id;
                state.items.push(Item {
                    id,
                    account_id: account_id.to_owned(),
                    room_id: room_id.to_owned(),
                    mime_type: mime_type_for(&path),
                    path,
                    filename,
                    size,
                    phase,
                    note: String::new(),
                    commands: None,
                    cancel: None,
                    progress_at: None,
                });
                in_room += 1;
            }
        }
        if skipped > 0 {
            ui.set_transfer_notice(
                format!(
                    "{skipped} not added: up to {MAX_ROOM_ITEMS} attachments can wait in a room."
                )
                .into(),
            );
        }
        self.project(ui);
    }

    fn send_staged(self: &Arc<Self>, ui: &AppWindow) {
        if !self.live.is_live.load(Ordering::Acquire) {
            return;
        }
        let account_id = ui.get_active_account_local_id();
        let room_id = ui.get_active_room_id();
        let staged: Vec<u64> = self
            .lock()
            .items
            .iter()
            .filter(|item| item.in_room(&account_id, &room_id) && item.phase == Phase::Staged)
            .map(|item| item.id)
            .collect();
        for id in staged {
            self.start(id);
        }
        ui.set_transfer_notice("".into());
        self.project(ui);
    }

    /// Hand one staged or not-yet-queued item to a new upload task.
    fn start(self: &Arc<Self>, id: u64) {
        let (job, commands, cancelled) = {
            let mut state = self.lock();
            let Some(item) = state.items.iter_mut().find(|item| item.id == id) else {
                return;
            };
            if !matches!(item.phase, Phase::Staged | Phase::Failed(_)) || item.commands.is_some() {
                return;
            }
            let Some(selection) = self.live.manager.account_selection(&item.account_id) else {
                item.phase =
                    Phase::Failed("Select this account before sending its attachments.".into());
                return;
            };
            let (sender, receiver) = mpsc::channel(COMMAND_CAPACITY);
            let (cancel, cancelled) = watch::channel(false);
            item.commands = Some(sender);
            item.cancel = Some(cancel);
            item.phase = Phase::Preparing;
            item.note.clear();
            let job = Job {
                id,
                selection,
                account_id: item.account_id.clone(),
                room_id: item.room_id.clone(),
                path: item.path.clone(),
                mime_type: item.mime_type.clone(),
                filename: item.filename.clone(),
            };
            (job, receiver, cancelled)
        };
        let owner = self.clone();
        self.runtime.spawn(async move {
            let mut commands = commands;
            let outcome = owner.upload(&job, &mut commands, &cancelled).await;
            owner.finish(&job, outcome);
        });
    }

    async fn upload(
        self: &Arc<Self>,
        job: &Job,
        commands: &mut mpsc::Receiver<Command>,
        cancelled: &watch::Receiver<bool>,
    ) -> Outcome {
        let mut listening = true;
        let mut cancel = false;
        let queued = {
            // Waiting tasks own only paths and small bounded command channels.
            let _preparation = tokio::select! {
                biased;
                _ = wait_for_cancel(cancelled) => return Outcome::Cancelled,
                _ = job.selection.revoked() => return Outcome::Failed("Select this account before sending its attachments.".into()),
                permit = self.preparing.acquire() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return Outcome::Failed("Attachment preparation is unavailable.".into()),
                },
            };
            // The core holds the account-switch lock while it queues and
            // checks the account it was given, so an upload is never
            // directed to another account's room. Navigating to another room
            // of the same account does not stop a send the user started.
            let send = self.live.manager.send_attachment_selected_for(
                &job.selection,
                &job.room_id,
                &job.path,
                &job.mime_type,
                cancelled,
            );
            tokio::pin!(send);
            loop {
                tokio::select! {
                    biased;
                    command = commands.recv(), if listening => match command {
                        Some(Command::Cancel) => {
                            cancel = true;
                            self.set_phase(job.id, Phase::Cancelling, "");
                        }
                        Some(Command::Retry) => {}
                        None => listening = false,
                    },
                    result = &mut send => break result,
                }
            }
        };
        let mut queued = match queued {
            Ok(Some(queued)) => queued,
            Ok(None) => return Outcome::Cancelled,
            Err(MatrixCoreError::AttachmentSendUncertain) => return Outcome::Uncertain,
            Err(MatrixCoreError::MediaTooLarge { .. }) => {
                return Outcome::Rejected(TOO_LARGE.into());
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        // Cancellation before SDK admission returned None above. Once SDK
        // commit began, withdraw the returned handle and report its outcome.
        if cancel || *cancelled.borrow() || cancelled.has_changed().is_err() {
            return abort(&queued).await.unwrap_or(Outcome::Uncertain);
        }
        self.set_phase(job.id, Phase::Uploading(0), "");
        loop {
            tokio::select! {
                biased;
                // The persistent SDK queue owns an admitted upload. Stop its
                // old observer on account departure, rather than retaining a
                // removed profile until a paused queue emits another event.
                _ = job.selection.revoked() => return Outcome::Uncertain,
                command = commands.recv(), if listening => match command {
                    Some(Command::Cancel) => {
                        self.set_phase(job.id, Phase::Cancelling, "");
                        match abort(&queued).await {
                            Ok(outcome) => return outcome,
                            Err(error) => self.set_phase(
                                job.id,
                                Phase::Paused,
                                &format!("Could not cancel: {error}"),
                            ),
                        }
                    }
                    Some(Command::Retry) => match queued.retry_wedged().await {
                        Ok(()) => self.set_phase(job.id, Phase::Uploading(0), ""),
                        Err(error) => self.set_phase(
                            job.id,
                            Phase::Paused,
                            &format!("Could not retry: {error}"),
                        ),
                    },
                    None => listening = false,
                },
                update = queued.next_update() => match update {
                    AttachmentSendUpdate::Progress { current, total } => {
                        self.progress(job.id, percent(current, total));
                    }
                    AttachmentSendUpdate::Sent { .. } => return Outcome::Sent,
                    AttachmentSendUpdate::Retained { recoverable: true } => {
                        self.set_phase(job.id, Phase::Waiting, "");
                    }
                    AttachmentSendUpdate::Retained { recoverable: false } => {
                        self.set_phase(job.id, Phase::Paused, "");
                    }
                    AttachmentSendUpdate::Cancelled => return Outcome::Cancelled,
                    AttachmentSendUpdate::Unknown => return Outcome::Uncertain,
                },
            }
        }
    }

    fn finish(self: &Arc<Self>, job: &Job, outcome: Outcome) {
        let notice = {
            let mut state = self.lock();
            let Some(index) = state.items.iter().position(|item| item.id == job.id) else {
                return;
            };
            let item = &mut state.items[index];
            item.commands = None;
            item.cancel = None;
            item.note.clear();
            match outcome {
                Outcome::Sent => {
                    state.items.remove(index);
                    Some(format!("Sent {}", job.filename))
                }
                Outcome::Cancelled => {
                    state.items.remove(index);
                    Some(format!("Cancelled {}", job.filename))
                }
                Outcome::AlreadySent => {
                    state.items.remove(index);
                    Some(format!("{} was already sent", job.filename))
                }
                Outcome::Failed(error) => {
                    item.phase = Phase::Failed(error);
                    None
                }
                Outcome::Rejected(reason) => {
                    item.phase = Phase::Rejected(reason);
                    None
                }
                Outcome::Uncertain => {
                    item.phase = Phase::Uncertain;
                    None
                }
            }
        };
        if let Some(notice) = notice {
            let (account_id, room_id) = (job.account_id.clone(), job.room_id.clone());
            let selection = job.selection.clone();
            let _ = self.window.upgrade_in_event_loop(move |ui| {
                // Only the room the file was sent to may show its name.
                if selection.is_current()
                    && ui.get_live_session()
                    && ui.get_active_account_local_id().as_str() == account_id
                    && ui.get_active_room_id().as_str() == room_id
                {
                    ui.set_transfer_notice(crate::directory_plain_text(&notice, 200).into());
                }
            });
        }
        self.refresh();
    }

    fn set_phase(self: &Arc<Self>, id: u64, phase: Phase, note: &str) {
        {
            let mut state = self.lock();
            let Some(item) = state.items.iter_mut().find(|item| item.id == id) else {
                return;
            };
            item.phase = phase;
            item.note = note.to_owned();
        }
        self.refresh();
    }

    fn progress(self: &Arc<Self>, id: u64, value: u8) {
        {
            let mut state = self.lock();
            let Some(item) = state.items.iter_mut().find(|item| item.id == id) else {
                return;
            };
            let due = item
                .progress_at
                .is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
            if item.phase == Phase::Uploading(value) || (!due && value < 100) {
                return;
            }
            item.phase = Phase::Uploading(value);
            item.progress_at = Some(Instant::now());
        }
        self.refresh();
    }

    fn remove(&self, ui: &AppWindow, id: u64) {
        let account_id = ui.get_active_account_local_id();
        let room_id = ui.get_active_room_id();
        {
            let mut state = self.lock();
            state.items.retain(|item| {
                item.id != id || !item.in_room(&account_id, &room_id) || item.phase.running()
            });
        }
        self.project(ui);
    }

    fn cancel(self: &Arc<Self>, ui: &AppWindow, id: u64) {
        let account_id = ui.get_active_account_local_id();
        let room_id = ui.get_active_room_id();
        let sent = {
            let state = self.lock();
            state
                .items
                .iter()
                .find(|item| item.id == id && item.in_room(&account_id, &room_id))
                .is_some_and(|item| {
                    let sent = item
                        .cancel
                        .as_ref()
                        .is_some_and(|cancel| cancel.send(true).is_ok());
                    if let Some(commands) = item.commands.as_ref() {
                        let _ = commands.try_send(Command::Cancel);
                    }
                    sent
                })
        };
        if sent {
            self.set_phase(id, Phase::Cancelling, "");
        }
    }

    fn retry(self: &Arc<Self>, ui: &AppWindow, id: u64) {
        let account_id = ui.get_active_account_local_id();
        let room_id = ui.get_active_room_id();
        let phase = {
            let state = self.lock();
            state
                .items
                .iter()
                .find(|item| item.id == id && item.in_room(&account_id, &room_id))
                .map(|item| (item.phase.clone(), item.commands.clone()))
        };
        match phase {
            // Never queued: one new send cannot duplicate it.
            Some((Phase::Failed(_), None)) if self.live.is_live.load(Ordering::Acquire) => {
                self.start(id);
                self.project(ui);
            }
            // Retried in place by the SDK, never re-enqueued.
            Some((Phase::Paused, Some(commands))) => {
                if commands.try_send(Command::Retry).is_ok() {
                    self.set_phase(id, Phase::Paused, "Retrying…");
                }
            }
            _ => {}
        }
    }

    /// Drop items of accounts that are no longer saved on this device.
    fn forget_removed_accounts(self: &Arc<Self>) {
        let owner = self.clone();
        self.runtime.spawn(async move {
            let accounts = owner.live.manager.accounts().await;
            let removed = {
                let mut state = owner.lock();
                let before = state.items.len();
                state
                    .items
                    .retain(|item| accounts.iter().any(|account| account.id == item.account_id));
                before != state.items.len()
            };
            if removed {
                owner.refresh();
            }
        });
    }

    /// Queue one projection of the room on screen; repeated requests made
    /// while one waits on the event loop share it.
    fn refresh(self: &Arc<Self>) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let owner = self.clone();
        if self
            .window
            .upgrade_in_event_loop(move |ui| {
                owner.queued.store(false, Ordering::Release);
                owner.project(&ui);
            })
            .is_err()
        {
            self.queued.store(false, Ordering::Release);
        }
    }

    /// Show the queue of the account and room on screen, and nothing else.
    fn project(&self, ui: &AppWindow) {
        let account_id = ui.get_active_account_local_id();
        let room_id = ui.get_active_room_id();
        if !ui.get_live_session()
            || !self.live.is_live.load(Ordering::Acquire)
            || account_id.is_empty()
            || room_id.is_empty()
            || self.live.manager.account_selection(&account_id).is_none()
        {
            // A queued refresh must not restore private rows after detach or
            // revocation while the old presentation strings still exist.
            ui.set_attachment_items(ModelRc::new(VecModel::<AttachmentItem>::default()));
            ui.set_attachment_ready_count(0);
            ui.set_attachment_can_add(false);
            ui.set_attachment_status("".into());
            return;
        }
        let (rows, ready, running, in_room) = {
            let state = self.lock();
            let mut rows = Vec::new();
            let (mut ready, mut running) = (0, 0);
            for item in state
                .items
                .iter()
                .filter(|item| item.in_room(&account_id, &room_id))
            {
                ready += i32::from(item.phase == Phase::Staged);
                running += i32::from(item.phase.running());
                rows.push(row(item));
            }
            let in_room = rows.len();
            (rows, ready, running, in_room)
        };
        let model = ui.get_attachment_items();
        let same = model.row_count() == rows.len()
            && rows.iter().enumerate().all(|(index, row)| {
                model
                    .row_data(index)
                    .is_some_and(|shown| shown.id == row.id)
            });
        if same {
            for (index, row) in rows.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&row) {
                    model.set_row_data(index, row);
                }
            }
        } else {
            ui.set_attachment_items(ModelRc::new(VecModel::from(rows)));
        }
        ui.set_attachment_ready_count(ready);
        ui.set_attachment_can_add(in_room < MAX_ROOM_ITEMS);
        ui.set_attachment_status(summary(in_room, ready, running).into());
    }
}

async fn wait_for_cancel(signal: &watch::Receiver<bool>) {
    let mut signal = signal.clone();
    let _ = signal.wait_for(|stopped| *stopped).await;
}

/// Withdraw a queued upload. `Ok` says what happened; `Err` leaves it queued.
async fn abort(queued: &QueuedAttachment) -> Result<Outcome, MatrixCoreError> {
    match queued.abort().await {
        Ok(true) => Ok(Outcome::Cancelled),
        Ok(false) => Ok(Outcome::AlreadySent),
        Err(error) => Err(error),
    }
}

fn percent(current: usize, total: usize) -> u8 {
    if total == 0 {
        return 0;
    }
    let value = current.min(total) as u128 * 100 / total as u128;
    u8::try_from(value).unwrap_or(100)
}

fn row(item: &Item) -> AttachmentItem {
    let status = match &item.phase {
        Phase::Staged => "Ready to send".to_owned(),
        Phase::Preparing => "Preparing…".to_owned(),
        Phase::Uploading(value) => format!("Uploading · {value}%"),
        Phase::Waiting => "Waiting for the connection; the room retries it".to_owned(),
        Phase::Paused => "Upload paused; retry or cancel".to_owned(),
        Phase::Cancelling => "Cancelling…".to_owned(),
        Phase::Failed(error) => format!("Not sent: {error}"),
        Phase::Rejected(reason) => reason.clone(),
        Phase::Uncertain => {
            "Delivery not confirmed; it may still send. Check the room before sending it again."
                .to_owned()
        }
    };
    let status = if item.note.is_empty() {
        status
    } else {
        format!("{} · {status}", item.note)
    };
    let detail = match item.size {
        Some(size) => format!("{} · {}", human_size(size), item.mime_type),
        None => item.mime_type.clone(),
    };
    AttachmentItem {
        // Zero is never an item ID, so an unrepresentable one has no actions.
        id: i32::try_from(item.id).unwrap_or_default(),
        name: crate::directory_plain_text(&item.filename, 160).into(),
        detail: crate::directory_plain_text(&detail, 120).into(),
        status: crate::directory_plain_text(&status, 240).into(),
        progress: match item.phase {
            Phase::Uploading(value) => f32::from(value) / 100.0,
            _ => -1.0,
        },
        state: item.phase.index(),
        can_remove: matches!(
            item.phase,
            Phase::Staged | Phase::Failed(_) | Phase::Rejected(_) | Phase::Uncertain
        ),
        can_cancel: matches!(
            item.phase,
            Phase::Preparing | Phase::Uploading(_) | Phase::Waiting | Phase::Paused
        ),
        can_retry: match item.phase {
            Phase::Failed(_) => item.commands.is_none(),
            Phase::Paused => item.commands.is_some(),
            _ => false,
        },
    }
}

fn summary(total: usize, ready: i32, running: i32) -> String {
    if total == 0 {
        return String::new();
    }
    let noun = if total == 1 {
        "attachment"
    } else {
        "attachments"
    };
    let mut parts = vec![format!("{total} {noun}")];
    if ready > 0 {
        parts.push(format!("{ready} ready"));
    }
    if running > 0 {
        parts.push(format!("{running} sending"));
    }
    if ready == 0 && running == 0 {
        parts.push("needs attention".to_owned());
    }
    parts.join(" · ")
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(phase: Phase, commands: bool) -> Item {
        Item {
            id: 3,
            account_id: "a".into(),
            room_id: "r".into(),
            path: PathBuf::from("photo.png"),
            filename: "photo.png".into(),
            mime_type: "image/png".into(),
            size: Some(2_400_000),
            phase,
            note: String::new(),
            commands: commands.then(|| mpsc::channel(COMMAND_CAPACITY).0),
            cancel: None,
            progress_at: None,
        }
    }

    #[test]
    fn only_unqueued_failures_and_paused_uploads_offer_retry() {
        let failed = row(&item(Phase::Failed("offline".into()), false));
        assert!(failed.can_retry && failed.can_remove && !failed.can_cancel);
        let paused = row(&item(Phase::Paused, true));
        assert!(paused.can_retry && paused.can_cancel && !paused.can_remove);
        // Unknown delivery may still send: dismiss only, never resend.
        let uncertain = row(&item(Phase::Uncertain, false));
        assert!(!uncertain.can_retry && !uncertain.can_cancel && uncertain.can_remove);
        let rejected = row(&item(Phase::Rejected(TOO_LARGE.into()), false));
        assert!(!rejected.can_retry && rejected.can_remove);
        let uploading = row(&item(Phase::Uploading(42), true));
        assert_eq!(uploading.progress, 0.42);
        assert!(uploading.can_cancel && !uploading.can_remove && !uploading.can_retry);
        assert_eq!(
            row(&item(Phase::Staged, false)).detail,
            "2.4 MB · image/png"
        );
    }

    #[test]
    fn rows_belong_to_exactly_one_account_and_room() {
        let staged = item(Phase::Staged, false);
        assert!(staged.in_room("a", "r"));
        assert!(!staged.in_room("b", "r"));
        assert!(!staged.in_room("a", "s"));
        // A detached window has no account and shows nothing.
        assert!(!staged.in_room("", "r"));
    }

    #[test]
    fn progress_is_a_bounded_percentage() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(5, 10), 50);
        assert_eq!(percent(15, 10), 100);
        assert_eq!(percent(usize::MAX, usize::MAX), 100);
        assert_eq!(summary(1, 1, 0), "1 attachment · 1 ready");
        assert_eq!(summary(2, 0, 0), "2 attachments · needs attention");
        assert_eq!(summary(3, 1, 2), "3 attachments · 1 ready · 2 sending");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(25_000_000), "25.0 MB");
    }
}
