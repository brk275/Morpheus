//! Attachment transfers: the per-room composer queue and the account's
//! transfer center.
//!
//! Chosen files wait as local paths only, with no bytes read, until the user
//! sends them, in the order shown (staged files can be reordered or removed).
//! Each sent file then has its own task and small command channel. The core
//! reads it with the 25 MiB cap, and the homeserver's own upload limit when
//! it reports one, into the SDK's encrypted per-account media store and
//! persistent room send queue, so this module keeps no second copy of any
//! content. At most two files are read and prepared at once.
//!
//! No item is ever sent twice automatically. A file that failed before it was
//! queued can be retried once by the user, and starts again from the
//! beginning. A paused upload is retried in place with the SDK's unwedge,
//! never re-enqueued. An upload whose outcome is unknown can only be
//! dismissed.
//!
//! Saving a received attachment asks for the location every time, then
//! waits in the account's download queue: one download is in memory at a
//! time, each can be cancelled, and one that failed can be tried again (the
//! location is asked again). Downloads continue while the user moves between
//! the account's rooms; leaving the account stops them. The bounded local
//! write, guarded by the core against account changes, is never interrupted
//! halfway, so the reported outcome is always what happened on disk. A file
//! is saved only under a new name; existing entries are never replaced.
//! Nothing is opened afterwards.
//!
//! The transfer center lists the visible account's uploads in every room,
//! its downloads and the transfers that recently ended, by name only: no
//! path, URL or server text is shown. Composer rows are projected only for
//! the account and room on screen.

use std::{
    collections::{HashMap, VecDeque},
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
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
    sync::{Notify, Semaphore, mpsc, watch},
};

use crate::{
    AppWindow, AttachmentItem, TransferItem,
    attachments::{mime_type_for, safe_file_name},
    live::LiveUi,
};

/// Attachments that can wait in one room at once.
const MAX_ROOM_ITEMS: usize = 10;
/// Attachments remembered across every account and room.
const MAX_ITEMS: usize = 64;
/// Bound simultaneous file reads and SDK preparation across all rooms.
const MAX_PREPARING: usize = 2;
/// Downloads waiting or running across every account.
const MAX_DOWNLOADS: usize = 8;
/// Failed download records retained across accounts; active work is never evicted.
const MAX_FAILED_DOWNLOADS: usize = 24;
/// Downloads held in memory at once; each is bounded by the 25 MiB cap.
const MAX_DOWNLOADING: usize = 1;
/// Ended transfers remembered for the transfer center, across accounts.
const MAX_FINISHED: usize = 24;
/// Rows the transfer center lists at once.
const MAX_TRANSFER_ROWS: usize = 40;
/// Minimum spacing of one upload's progress updates.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// Commands are user clicks; a full channel drops a repeated click.
const COMMAND_CAPACITY: usize = 4;
const TOO_LARGE: &str = "Too large: attachments are limited to 25 MiB.";

const ACTION_REMOVE: i32 = 0;
const ACTION_CANCEL: i32 = 1;
const ACTION_RETRY: i32 = 2;
const ACTION_OPEN_ROOM: i32 = 3;
const ACTION_MOVE_UP: i32 = 4;
const ACTION_MOVE_DOWN: i32 = 5;

/// Transfer center row kinds, as the window reports them back.
const KIND_UPLOAD: i32 = 0;
const KIND_DOWNLOAD: i32 = 1;
const KIND_FINISHED: i32 = 2;

/// Transfer center groups, in display order.
const GROUP_ACTIVE: i32 = 0;
const GROUP_ATTENTION: i32 = 1;
const GROUP_FINISHED: i32 = 2;

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
    /// The room's name when the file was chosen, for the transfer center.
    room_name: String,
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

/// Reservations are created synchronously in the user's send order. File
/// reads may complete in any order, so admission must not depend on which
/// Tokio task first reaches the SDK. Dropping a middle reservation removes
/// only that reservation; it never lets a successor overtake the head.
#[derive(Default)]
struct AdmissionQueue {
    pending: Mutex<VecDeque<u64>>,
    changed: Notify,
}

struct Admission {
    queue: Arc<AdmissionQueue>,
    id: u64,
    released: AtomicBool,
}

impl AdmissionQueue {
    fn reserve(self: &Arc<Self>, id: u64) -> Admission {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(id);
        Admission {
            queue: self.clone(),
            id,
            released: AtomicBool::new(false),
        }
    }
}

impl Admission {
    async fn wait(&self) {
        loop {
            let changed = self.queue.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .queue
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .front()
                == Some(&self.id)
            {
                return;
            }
            changed.await;
        }
    }

    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let removed = {
            let mut pending = self
                .queue
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(index) = pending.iter().position(|id| *id == self.id) {
                pending.remove(index);
                true
            } else {
                false
            }
        };
        if removed {
            self.queue.changed.notify_waiters();
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.release();
    }
}

/// What one upload task owns: never the window or the item itself.
struct Job {
    id: u64,
    admission: Admission,
    selection: AccountSelection,
    account_id: String,
    room_id: String,
    room_name: String,
    path: PathBuf,
    mime_type: String,
    filename: String,
    size: Option<u64>,
}

enum Outcome {
    Sent,
    Cancelled,
    AlreadySent,
    Failed(String),
    Rejected(String),
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DownloadPhase {
    /// Chosen; waiting for the one download slot.
    Waiting,
    /// Downloading and decrypting; the SDK reports no byte progress.
    Downloading,
    /// Writing to the chosen location.
    Saving,
    Cancelling,
    /// Nothing was written; it can be tried again.
    Failed(String),
}

impl DownloadPhase {
    fn running(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }

    /// Whether a cancel can still stop it: an admitted write finishes.
    fn cancellable(&self) -> bool {
        matches!(self, Self::Waiting | Self::Downloading)
    }
}

/// One save of a received attachment. The chosen location stays here and
/// is never shown; only its final file name is.
struct Download {
    id: u64,
    account_id: String,
    room_id: String,
    room_name: String,
    event_id: String,
    /// The sanitized name suggested to the Save dialog.
    filename: String,
    phase: DownloadPhase,
    cancel: Option<watch::Sender<bool>>,
}

/// What one download task owns.
struct DownloadJob {
    id: u64,
    selection: AccountSelection,
    account_id: String,
    room_id: String,
    event_id: String,
    path: PathBuf,
    operation: i32,
}

enum SaveOutcome {
    /// Saved under this final file name.
    Saved(String),
    /// Stopped before anything was written.
    Cancelled,
    Failed(String),
}

/// A transfer that ended, kept for the transfer center only: names and what
/// happened, never content, a path or server text.
struct Finished {
    id: u64,
    account_id: String,
    room_id: String,
    room_name: String,
    filename: String,
    upload: bool,
    outcome: String,
}

#[derive(Debug, PartialEq, Eq)]
enum DownloadAdmissionError {
    Duplicate,
    Full,
}

impl DownloadAdmissionError {
    fn notice(&self) -> String {
        match self {
            Self::Duplicate => "This attachment is already being saved.".into(),
            Self::Full => {
                format!("Up to {MAX_DOWNLOADS} downloads can wait; let one finish first.")
            }
        }
    }
}

#[derive(Default)]
struct State {
    next_id: u64,
    items: Vec<Item>,
    downloads: Vec<Download>,
    /// Oldest first.
    finished: VecDeque<Finished>,
}

impl State {
    fn check_download_admission(
        &self,
        account: &str,
        event: &str,
    ) -> Result<(), DownloadAdmissionError> {
        if self.downloads.iter().any(|download| {
            download.account_id == account && download.event_id == event && download.phase.running()
        }) {
            return Err(DownloadAdmissionError::Duplicate);
        }
        if self
            .downloads
            .iter()
            .filter(|download| download.phase.running())
            .count()
            >= MAX_DOWNLOADS
        {
            return Err(DownloadAdmissionError::Full);
        }
        Ok(())
    }

    fn trim_failed_downloads(&mut self, latest: u64) {
        let failed = self
            .downloads
            .iter()
            .filter(|download| !download.phase.running())
            .count();
        for _ in MAX_FAILED_DOWNLOADS..failed {
            let Some(index) = self
                .downloads
                .iter()
                .position(|download| download.id != latest && !download.phase.running())
            else {
                break;
            };
            self.downloads.remove(index);
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn remember(&mut self, finished: Finished) {
        while self.finished.len() >= MAX_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(finished);
    }

    /// Move a staged item one place up or down among the staged items of
    /// its room, which are sent in this order.
    fn move_staged(&mut self, account_id: &str, id: u64, up: bool) -> bool {
        let Some(index) = self
            .items
            .iter()
            .position(|item| item.id == id && item.account_id == account_id)
        else {
            return false;
        };
        if self.items[index].phase != Phase::Staged {
            return false;
        }
        let room_id = self.items[index].room_id.clone();
        let staged_here = |item: &Item| {
            item.account_id == account_id && item.room_id == room_id && item.phase == Phase::Staged
        };
        let neighbour = if up {
            self.items[..index].iter().rposition(staged_here)
        } else {
            self.items[index + 1..]
                .iter()
                .position(staged_here)
                .map(|offset| index + 1 + offset)
        };
        match neighbour {
            Some(neighbour) => {
                self.items.swap(index, neighbour);
                true
            }
            None => false,
        }
    }
}

pub struct Workbench {
    live: Arc<LiveUi>,
    runtime: Handle,
    window: slint::Weak<AppWindow>,
    state: Mutex<State>,
    /// A projection is already waiting on the event loop.
    queued: AtomicBool,
    preparing: Semaphore,
    admissions: Mutex<HashMap<(String, String), Weak<AdmissionQueue>>>,
    downloading: Semaphore,
    /// Each account's homeserver upload limit, once it was read.
    limits: Mutex<HashMap<String, u64>>,
}

impl Workbench {
    /// Install the composer attachment and transfer center callbacks. The
    /// returned owner is also held by those callbacks for the window's
    /// lifetime.
    pub fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) -> Arc<Self> {
        let owner = Arc::new(Self::new(ui, live, runtime, State::default()));
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
                owner.upload_action(&ui, id, action);
            });
        }
        {
            let owner = owner.clone();
            ui.on_transfer_action(move |kind, id, action| {
                let (Some(ui), Ok(id)) = (owner.window.upgrade(), u64::try_from(id)) else {
                    return;
                };
                match kind {
                    KIND_UPLOAD => owner.upload_action(&ui, id, action),
                    KIND_DOWNLOAD => owner.download_action(&ui, id, action),
                    KIND_FINISHED => owner.finished_action(&ui, id, action),
                    _ => {}
                }
            });
        }
        {
            let owner = owner.clone();
            ui.on_clear_finished_transfers(move || {
                if let Some(ui) = owner.window.upgrade() {
                    owner.clear_finished(&ui);
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

    fn new(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle, state: State) -> Self {
        Self {
            live,
            runtime,
            window: ui.as_weak(),
            state: Mutex::new(state),
            queued: AtomicBool::new(false),
            preparing: Semaphore::new(MAX_PREPARING),
            admissions: Mutex::new(HashMap::new()),
            downloading: Semaphore::new(MAX_DOWNLOADING),
            limits: Mutex::new(HashMap::new()),
        }
    }

    // Used by the existing real generated-UI detach regression. No picker,
    // file read, client, or transfer is started: only the production refresh
    // is posted, with a private row belonging to the still-shown old IDs.
    #[cfg(test)]
    pub(crate) fn queue_private_projection_for_test(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: Handle,
    ) {
        let item = Item {
            id: 1,
            account_id: ui.get_active_account_local_id().to_string(),
            room_id: ui.get_active_room_id().to_string(),
            room_name: "synthetic room".into(),
            path: PathBuf::from("synthetic-never-opened.png"),
            filename: "synthetic-private-file.png".into(),
            mime_type: "image/png".into(),
            size: Some(1),
            phase: Phase::Staged,
            note: String::new(),
            commands: None,
            cancel: None,
            progress_at: None,
        };
        ui.set_attachment_items(ModelRc::new(VecModel::from(vec![row(&item)])));
        ui.set_attachment_ready_count(1);
        ui.set_attachment_can_add(true);
        ui.set_attachment_status("private staged file".into());
        let state = State {
            next_id: 1,
            items: vec![item],
            ..State::default()
        };
        let owner = Arc::new(Self::new(ui, live, runtime, state));
        owner.refresh();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn lock_limits(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.limits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Save an attachment of the room on screen.
    fn save(self: &Arc<Self>, event_id: &str, filename: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let account_id = ui.get_active_account_local_id().to_string();
        let room_id = ui.get_active_room_id().to_string();
        let room_name = ui.get_active_room_name().to_string();
        self.request_download(&ui, &account_id, &room_id, &room_name, event_id, filename);
    }

    /// Ask where to save one attachment, then queue its download. The
    /// account, and the room it was asked from, must still be on screen
    /// when the dialog closes.
    fn request_download(
        self: &Arc<Self>,
        ui: &AppWindow,
        account_id: &str,
        room_id: &str,
        room_name: &str,
        event_id: &str,
        filename: &str,
    ) -> bool {
        if !self.live.is_live.load(Ordering::Acquire)
            || !ui.get_live_session()
            || event_id.is_empty()
            || room_id.is_empty()
            || ui.get_active_account_local_id().as_str() != account_id
        {
            return false;
        }
        let Some(selection) = self.live.manager.account_selection(account_id) else {
            return false;
        };
        let admitted = self.lock().check_download_admission(account_id, event_id);
        if let Err(error) = admitted {
            ui.set_transfer_notice(error.notice().into());
            return false;
        }
        let visible_room = ui.get_active_room_id().to_string();
        // The sender chose this name: suggest only a sanitized final
        // component, and let the user pick the folder explicitly.
        let suggested = safe_file_name(filename);
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save attachment")
            .set_file_name(&suggested);
        if let Some(downloads) = directories::UserDirs::new()
            .and_then(|dirs| dirs.download_dir().map(|dir| dir.to_path_buf()))
        {
            dialog = dialog.set_directory(downloads);
        }
        let Some(path) = dialog.save_file() else {
            return false;
        };
        if ui.get_active_account_local_id().as_str() != account_id
            || ui.get_active_room_id().as_str() != visible_room
            || !selection.is_current()
            || path.file_name().is_none()
        {
            return false;
        }
        let operation = ui.get_download_operation().wrapping_add(1);
        ui.set_download_operation(operation);
        if room_id == visible_room {
            ui.set_downloading_event_id(event_id.into());
        }
        ui.set_transfer_notice(format!("Saving {}…", plain(&suggested, 120)).into());
        let (cancel, cancelled) = watch::channel(false);
        let id = {
            let mut state = self.lock();
            // Native modal dialogs may pump callbacks. Recheck under the
            // insertion lock rather than trusting the pre-dialog count.
            if let Err(error) = state.check_download_admission(account_id, event_id) {
                drop(state);
                ui.set_transfer_notice(error.notice().into());
                return false;
            }
            let id = state.next_id();
            state.downloads.push(Download {
                id,
                account_id: account_id.to_owned(),
                room_id: room_id.to_owned(),
                room_name: room_name.to_owned(),
                event_id: event_id.to_owned(),
                filename: suggested,
                phase: DownloadPhase::Waiting,
                cancel: Some(cancel),
            });
            id
        };
        let job = DownloadJob {
            id,
            selection,
            account_id: account_id.to_owned(),
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            path,
            operation,
        };
        let owner = self.clone();
        self.runtime.spawn(async move {
            let mut cancelled = cancelled;
            let outcome = owner.run_download(&job, &mut cancelled).await;
            owner.finish_download(&job, outcome);
        });
        self.project(ui);
        true
    }

    async fn run_download(
        self: &Arc<Self>,
        job: &DownloadJob,
        cancelled: &mut watch::Receiver<bool>,
    ) -> SaveOutcome {
        let _slot = tokio::select! {
            biased;
            () = wait_for_cancel(cancelled) => return SaveOutcome::Cancelled,
            _ = job.selection.revoked() => return SaveOutcome::Cancelled,
            permit = self.downloading.acquire() => match permit {
                Ok(permit) => permit,
                Err(_) => return SaveOutcome::Failed("Downloads are unavailable.".into()),
            },
        };
        self.set_download_phase(job.id, DownloadPhase::Downloading);
        let downloaded = tokio::select! {
            biased;
            // A cancel, or leaving the account, drops the download.
            () = wait_for_cancel(cancelled) => return SaveOutcome::Cancelled,
            _ = job.selection.revoked() => return SaveOutcome::Cancelled,
            downloaded = self.live.download_attachment_selected(&job.selection, &job.room_id, &job.event_id) => match downloaded {
                Ok(downloaded) => downloaded,
                Err(error) => return SaveOutcome::Failed(download_failure(&error).into()),
            },
        };
        if !job.selection.is_current() || *cancelled.borrow() || cancelled.has_changed().is_err() {
            return SaveOutcome::Cancelled;
        }
        self.set_download_phase(job.id, DownloadPhase::Saving);
        // The account guard is held through the local write, so an account
        // change cannot race the plaintext save. Once it starts, the bounded
        // write finishes and its real outcome is reported.
        match self
            .live
            .save_download_selected(
                &job.selection,
                job.path.clone(),
                downloaded.bytes,
                cancelled,
            )
            .await
        {
            Ok(Some(path)) => SaveOutcome::Saved(
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            Ok(None) => SaveOutcome::Cancelled,
            Err(error) => SaveOutcome::Failed(download_failure(&error).into()),
        }
    }

    fn finish_download(self: &Arc<Self>, job: &DownloadJob, outcome: SaveOutcome) {
        let notice = {
            let mut state = self.lock();
            let Some(index) = state
                .downloads
                .iter()
                .position(|download| download.id == job.id)
            else {
                return;
            };
            match outcome {
                SaveOutcome::Failed(reason) => {
                    let download = &mut state.downloads[index];
                    download.cancel = None;
                    let notice = format!("Could not save {}: {reason}", download.filename);
                    download.phase = DownloadPhase::Failed(reason);
                    state.trim_failed_downloads(job.id);
                    notice
                }
                SaveOutcome::Saved(name) => {
                    let download = state.downloads.remove(index);
                    let notice = format!("Saved {name}");
                    state.remember(Finished {
                        id: download.id,
                        account_id: download.account_id,
                        room_id: download.room_id,
                        room_name: download.room_name,
                        filename: name,
                        upload: false,
                        outcome: "Saved".into(),
                    });
                    notice
                }
                SaveOutcome::Cancelled => {
                    let download = state.downloads.remove(index);
                    state.remember(Finished {
                        id: download.id,
                        account_id: download.account_id,
                        room_id: download.room_id,
                        room_name: download.room_name,
                        filename: download.filename,
                        upload: false,
                        outcome: "Save cancelled; nothing was written".into(),
                    });
                    "Save cancelled; nothing was written.".to_owned()
                }
            }
        };
        let (account_id, room_id, event_id) = (
            job.account_id.clone(),
            job.room_id.clone(),
            job.event_id.clone(),
        );
        let selection = job.selection.clone();
        let operation = job.operation;
        let _ = self.window.upgrade_in_event_loop(move |ui| {
            // Only the account the file came from may show its name, and only
            // the latest save may replace the notice.
            if !selection.is_current()
                || !ui.get_live_session()
                || ui.get_active_account_local_id().as_str() != account_id
            {
                return;
            }
            if ui.get_downloading_event_id().as_str() == event_id
                && ui.get_active_room_id().as_str() == room_id
            {
                ui.set_downloading_event_id("".into());
            }
            if ui.get_download_operation() == operation {
                ui.set_transfer_notice(plain(&notice, 300).into());
            }
        });
        self.refresh();
    }

    fn set_download_phase(self: &Arc<Self>, id: u64, phase: DownloadPhase) {
        {
            let mut state = self.lock();
            let Some(download) = state
                .downloads
                .iter_mut()
                .find(|download| download.id == id)
            else {
                return;
            };
            // A pending cancel keeps showing until the task ends.
            if download.phase != DownloadPhase::Cancelling {
                download.phase = phase;
            }
        }
        self.refresh();
    }

    /// The composer notice's Cancel: the save of the attachment it names.
    fn cancel_save(self: &Arc<Self>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let account_id = ui.get_active_account_local_id().to_string();
        let event_id = ui.get_downloading_event_id().to_string();
        if event_id.is_empty() {
            return;
        }
        let id = self
            .lock()
            .downloads
            .iter()
            .find(|download| {
                download.account_id == account_id
                    && download.event_id == event_id
                    && download.phase.running()
            })
            .map(|download| download.id);
        if let Some(id) = id {
            self.download_action(&ui, id, ACTION_CANCEL);
        }
    }

    fn download_action(self: &Arc<Self>, ui: &AppWindow, id: u64, action: i32) {
        let account_id = ui.get_active_account_local_id().to_string();
        match action {
            ACTION_CANCEL => {
                let sent = {
                    let mut state = self.lock();
                    // An admitted write finishes; only a wait or a download
                    // in progress can still be stopped.
                    let Some(download) = state.downloads.iter_mut().find(|download| {
                        download.id == id
                            && download.account_id == account_id
                            && download.phase.cancellable()
                    }) else {
                        return;
                    };
                    let sent = download
                        .cancel
                        .as_ref()
                        .is_some_and(|cancel| cancel.send(true).is_ok());
                    if sent {
                        download.phase = DownloadPhase::Cancelling;
                    }
                    sent
                };
                if sent {
                    ui.set_transfer_notice("Cancelling save…".into());
                    self.refresh();
                }
            }
            ACTION_RETRY => {
                // Tried again from the start, with the location asked again;
                // the failed row stays until a new download is queued.
                let failed = self
                    .lock()
                    .downloads
                    .iter()
                    .find(|download| {
                        download.id == id
                            && download.account_id == account_id
                            && !download.phase.running()
                    })
                    .map(|download| {
                        (
                            download.room_id.clone(),
                            download.room_name.clone(),
                            download.event_id.clone(),
                            download.filename.clone(),
                        )
                    });
                if let Some((room_id, room_name, event_id, filename)) = failed
                    && self.request_download(
                        ui,
                        &account_id,
                        &room_id,
                        &room_name,
                        &event_id,
                        &filename,
                    )
                {
                    self.lock().downloads.retain(|download| download.id != id);
                    self.project(ui);
                }
            }
            ACTION_REMOVE => {
                self.lock().downloads.retain(|download| {
                    download.id != id
                        || download.account_id != account_id
                        || download.phase.running()
                });
                self.project(ui);
            }
            ACTION_OPEN_ROOM => {
                let room = self
                    .lock()
                    .downloads
                    .iter()
                    .find(|download| download.id == id && download.account_id == account_id)
                    .map(|download| download.room_id.clone());
                if let Some(room_id) = room {
                    open_room(ui, &account_id, &room_id);
                }
            }
            _ => {}
        }
    }

    fn finished_action(&self, ui: &AppWindow, id: u64, action: i32) {
        let account_id = ui.get_active_account_local_id().to_string();
        match action {
            ACTION_REMOVE => {
                self.lock()
                    .finished
                    .retain(|finished| finished.id != id || finished.account_id != account_id);
                self.project(ui);
            }
            ACTION_OPEN_ROOM => {
                let room = self
                    .lock()
                    .finished
                    .iter()
                    .find(|finished| finished.id == id && finished.account_id == account_id)
                    .map(|finished| finished.room_id.clone());
                if let Some(room_id) = room {
                    open_room(ui, &account_id, &room_id);
                }
            }
            _ => {}
        }
    }

    /// Forget the visible account's ended transfers. Recent failed downloads
    /// remain available for retry, subject to the bounded failure history.
    fn clear_finished(&self, ui: &AppWindow) {
        let account_id = ui.get_active_account_local_id().to_string();
        self.lock()
            .finished
            .retain(|finished| finished.account_id != account_id);
        self.project(ui);
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
        let room_name = ui.get_active_room_name().to_string();
        self.stage(&ui, &account_id, &room_id, &room_name, paths);
    }

    fn stage(
        &self,
        ui: &AppWindow,
        account_id: &str,
        room_id: &str,
        room_name: &str,
        paths: Vec<PathBuf>,
    ) {
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
                let id = state.next_id();
                state.items.push(Item {
                    id,
                    account_id: account_id.to_owned(),
                    room_id: room_id.to_owned(),
                    room_name: room_name.to_owned(),
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
        // In the order shown; the room queue sends them in this order.
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

    fn reserve_admission(&self, account: &str, room: &str, id: u64) -> Admission {
        let mut rooms = self
            .admissions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Expired rooms keep neither account IDs nor an unbounded map alive.
        rooms.retain(|_, queue| queue.strong_count() > 0);
        let key = (account.to_owned(), room.to_owned());
        let queue = rooms.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let queue = Arc::new(AdmissionQueue::default());
            rooms.insert(key, Arc::downgrade(&queue));
            queue
        });
        queue.reserve(id)
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
                admission: self.reserve_admission(&item.account_id, &item.room_id, id),
                selection,
                account_id: item.account_id.clone(),
                room_id: item.room_id.clone(),
                room_name: item.room_name.clone(),
                path: item.path.clone(),
                mime_type: item.mime_type.clone(),
                filename: item.filename.clone(),
                size: item.size,
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

    /// The account's homeserver upload limit, read once per account. `None`
    /// when it can't be read now; the SDK still enforces the server's limit.
    async fn upload_limit(&self, selection: &AccountSelection) -> Option<u64> {
        if let Some(limit) = self.lock_limits().get(selection.account_id()) {
            return Some(*limit);
        }
        let limit = self
            .live
            .manager
            .attachment_upload_limit_selected(selection)
            .await
            .ok()?;
        self.lock_limits()
            .insert(selection.account_id().to_owned(), limit);
        Some(limit)
    }

    async fn upload(
        self: &Arc<Self>,
        job: &Job,
        commands: &mut mpsc::Receiver<Command>,
        cancelled: &watch::Receiver<bool>,
    ) -> Outcome {
        let mut listening = true;
        let mut cancel = false;
        // Refuse what the homeserver would refuse before any file is read.
        let limit = tokio::select! {
            biased;
            _ = wait_for_cancel(cancelled) => return Outcome::Cancelled,
            _ = job.selection.revoked() => return Outcome::Failed("Select this account before sending its attachments.".into()),
            limit = self.upload_limit(&job.selection) => limit,
        };
        if let (Some(limit), Some(size)) = (limit, job.size)
            && size > limit
        {
            return Outcome::Rejected(server_too_large(limit));
        }
        // Wait before occupying a global preparation slot. Other rooms can
        // prepare concurrently, while this room enters the SDK in send order.
        tokio::select! {
            biased;
            _ = wait_for_cancel(cancelled) => return Outcome::Cancelled,
            _ = job.selection.revoked() => return Outcome::Failed("Select this account before sending its attachments.".into()),
            () = job.admission.wait() => {},
        }
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
        // The enqueue future has settled. Release admission before observing
        // progress, so uploads in the same room can overlap after SDK commit.
        job.admission.release();
        let mut queued = match queued {
            Ok(Some(queued)) => queued,
            Ok(None) => return Outcome::Cancelled,
            Err(MatrixCoreError::AttachmentSendUncertain) => return Outcome::Uncertain,
            Err(MatrixCoreError::MediaTooLarge { .. }) => {
                return Outcome::Rejected(TOO_LARGE.into());
            }
            Err(error) => return Outcome::Failed(upload_failure(&error).into()),
        };
        // Cancellation before SDK admission returned None above. Once SDK
        // commit began, withdraw the returned handle and report its outcome.
        if cancel || *cancelled.borrow() || cancelled.has_changed().is_err() {
            return abort(&queued).await.unwrap_or(Outcome::Uncertain);
        }
        self.set_phase(job.id, Phase::Uploading(0), "");
        let mut cancellation_delivered = false;
        loop {
            tokio::select! {
                biased;
                // The persistent SDK queue owns an admitted upload. Stop its
                // old observer on account departure, rather than retaining a
                // removed profile until a paused queue emits another event.
                _ = job.selection.revoked() => return Outcome::Uncertain,
                command = upload_command(commands, cancelled, &mut cancellation_delivered), if listening => match command {
                    Some(Command::Cancel) => {
                        self.set_phase(job.id, Phase::Cancelling, "");
                        match abort(&queued).await {
                            Ok(outcome) => return outcome,
                            Err(_) => self.set_phase(
                                job.id,
                                Phase::Paused,
                                "Could not cancel; it may still send",
                            ),
                        }
                    }
                    Some(Command::Retry) => match queued.retry_wedged().await {
                        Ok(()) => self.set_phase(job.id, Phase::Uploading(0), ""),
                        Err(_) => self.set_phase(
                            job.id,
                            Phase::Paused,
                            "Could not retry",
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
            let ended = match outcome {
                Outcome::Sent => Some(("Sent", format!("Sent {}", job.filename))),
                Outcome::Cancelled => Some(("Cancelled", format!("Cancelled {}", job.filename))),
                Outcome::AlreadySent => Some((
                    "Already sent before the cancel",
                    format!("{} was already sent", job.filename),
                )),
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
            };
            ended.map(|(outcome, notice)| {
                state.items.remove(index);
                state.remember(Finished {
                    id: job.id,
                    account_id: job.account_id.clone(),
                    room_id: job.room_id.clone(),
                    room_name: job.room_name.clone(),
                    filename: job.filename.clone(),
                    upload: true,
                    outcome: outcome.into(),
                });
                notice
            })
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
                    ui.set_transfer_notice(plain(&notice, 200).into());
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

    /// A choice on one upload, from the composer queue or the transfer
    /// center. Only the visible account's uploads can be acted on, in any of
    /// its rooms; each keeps the room it was chosen for.
    fn upload_action(self: &Arc<Self>, ui: &AppWindow, id: u64, action: i32) {
        match action {
            ACTION_REMOVE => self.remove(ui, id),
            ACTION_CANCEL => self.cancel(ui, id),
            ACTION_RETRY => self.retry(ui, id),
            ACTION_OPEN_ROOM => {
                let account_id = ui.get_active_account_local_id().to_string();
                let room = self
                    .lock()
                    .items
                    .iter()
                    .find(|item| item.id == id && item.account_id == account_id)
                    .map(|item| item.room_id.clone());
                if let Some(room_id) = room {
                    open_room(ui, &account_id, &room_id);
                }
            }
            ACTION_MOVE_UP | ACTION_MOVE_DOWN => {
                let account_id = ui.get_active_account_local_id().to_string();
                let moved = self
                    .lock()
                    .move_staged(&account_id, id, action == ACTION_MOVE_UP);
                if moved {
                    self.project(ui);
                }
            }
            _ => {}
        }
    }

    fn remove(&self, ui: &AppWindow, id: u64) {
        let account_id = ui.get_active_account_local_id().to_string();
        {
            let mut state = self.lock();
            state.items.retain(|item| {
                item.id != id
                    || account_id.is_empty()
                    || item.account_id != account_id
                    || item.phase.running()
            });
        }
        self.project(ui);
    }

    fn cancel(self: &Arc<Self>, ui: &AppWindow, id: u64) {
        let account_id = ui.get_active_account_local_id().to_string();
        let sent = {
            let state = self.lock();
            state
                .items
                .iter()
                .find(|item| {
                    item.id == id && !account_id.is_empty() && item.account_id == account_id
                })
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
        let account_id = ui.get_active_account_local_id().to_string();
        let phase = {
            let state = self.lock();
            state
                .items
                .iter()
                .find(|item| {
                    item.id == id && !account_id.is_empty() && item.account_id == account_id
                })
                .map(|item| (item.phase.clone(), item.commands.clone()))
        };
        match phase {
            // Never queued: one new send, from the beginning, cannot
            // duplicate it. It still goes to the room it was chosen for.
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

    /// Drop transfers of accounts that are no longer saved on this device.
    /// Their tasks already stopped when the account's selection ended.
    fn forget_removed_accounts(self: &Arc<Self>) {
        let owner = self.clone();
        self.runtime.spawn(async move {
            let accounts = owner.live.manager.accounts().await;
            let saved =
                |account_id: &String| accounts.iter().any(|account| &account.id == account_id);
            let removed = {
                let mut state = owner.lock();
                let before = state.items.len() + state.downloads.len() + state.finished.len();
                state.items.retain(|item| saved(&item.account_id));
                // Dropping a download's cancel sender stops it too.
                state
                    .downloads
                    .retain(|download| saved(&download.account_id));
                state
                    .finished
                    .retain(|finished| saved(&finished.account_id));
                before != state.items.len() + state.downloads.len() + state.finished.len()
            };
            owner
                .lock_limits()
                .retain(|account_id, _| saved(account_id));
            if removed {
                owner.refresh();
            }
        });
    }

    /// Queue one projection of the account and room on screen; repeated
    /// requests made while one waits on the event loop share it.
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

    /// Show the composer queue of the account and room on screen and the
    /// transfer center of that account, and nothing else.
    fn project(&self, ui: &AppWindow) {
        let account_id = ui.get_active_account_local_id();
        let room_id = ui.get_active_room_id();
        let account_live = ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && !account_id.is_empty()
            && self.live.manager.account_selection(&account_id).is_some();
        self.project_transfers(ui, account_live.then_some(account_id.as_str()));
        if !account_live || room_id.is_empty() {
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
            // Staged rows can move among themselves, never past the ends.
            let staged: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.state == 0)
                .map(|(index, _)| index)
                .collect();
            for (rank, index) in staged.iter().enumerate() {
                rows[*index].can_move_up = rank > 0;
                rows[*index].can_move_down = rank + 1 < staged.len();
            }
            let in_room = rows.len();
            (rows, ready, running, in_room)
        };
        update_model(
            &ui.get_attachment_items(),
            rows,
            |shown, row| shown.id == row.id,
            |model| ui.set_attachment_items(model),
        );
        ui.set_attachment_ready_count(ready);
        ui.set_attachment_can_add(in_room < MAX_ROOM_ITEMS);
        ui.set_attachment_status(summary(in_room, ready, running).into());
    }

    /// The visible account's transfers in every room: work in progress
    /// first, then what needs a choice, then what ended, newest first.
    fn project_transfers(&self, ui: &AppWindow, account_id: Option<&str>) {
        let Some(account_id) = account_id else {
            ui.set_transfer_items(ModelRc::new(VecModel::<TransferItem>::default()));
            ui.set_transfer_active_count(0);
            ui.set_transfer_summary("".into());
            return;
        };
        let mut rows = {
            let state = self.lock();
            let mut rows = Vec::new();
            for item in state
                .items
                .iter()
                .filter(|item| item.account_id == account_id)
            {
                rows.push(upload_transfer(item));
            }
            for download in state
                .downloads
                .iter()
                .filter(|download| download.account_id == account_id)
            {
                rows.push(download_transfer(download));
            }
            for finished in state
                .finished
                .iter()
                .rev()
                .filter(|finished| finished.account_id == account_id)
            {
                rows.push(finished_transfer(finished));
            }
            rows
        };
        // A stable sort keeps the order within each group.
        rows.sort_by_key(|row| row.group);
        let active = rows.iter().filter(|row| row.group == GROUP_ACTIVE).count();
        let attention = rows
            .iter()
            .filter(|row| row.group == GROUP_ATTENTION)
            .count();
        let finished = rows.len() - active - attention;
        rows.truncate(MAX_TRANSFER_ROWS);
        update_model(
            &ui.get_transfer_items(),
            rows,
            |shown, row| shown.kind == row.kind && shown.id == row.id,
            |model| ui.set_transfer_items(model),
        );
        ui.set_transfer_active_count(i32::try_from(active).unwrap_or(i32::MAX));
        ui.set_transfer_summary(transfer_summary(active, attention, finished).into());
    }
}

/// Update a model in place while its rows are the same, else replace it, so
/// focus stays on a row whose state changes.
fn update_model<T: Clone + PartialEq + 'static>(
    model: &ModelRc<T>,
    rows: Vec<T>,
    same: impl Fn(&T, &T) -> bool,
    replace: impl FnOnce(ModelRc<T>),
) {
    let unchanged = model.row_count() == rows.len()
        && rows
            .iter()
            .enumerate()
            .all(|(index, row)| model.row_data(index).is_some_and(|shown| same(&shown, row)));
    if unchanged {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
    } else {
        replace(ModelRc::new(VecModel::from(rows)));
    }
}

/// Open a transfer's room in the visible account, with its composer, the
/// way a room-list click does: drafts and replies stay with their rooms.
fn open_room(ui: &AppWindow, account_id: &str, room_id: &str) {
    if !ui.get_live_session() || ui.get_active_account_local_id().as_str() != account_id {
        return;
    }
    ui.invoke_open_conversation(account_id.into(), room_id.into(), "".into(), "".into());
    ui.invoke_focus_composer(false);
}

async fn wait_for_cancel(signal: &watch::Receiver<bool>) {
    let mut signal = signal.clone();
    let _ = signal.wait_for(|stopped| *stopped).await;
}

/// Cancellation is a persistent watch signal, independent of the small
/// click queue. Deliver it once with priority even when Retry fills the
/// channel. A failed abort then waits for another explicit user action,
/// rather than spinning forever on a permanently true watch value.
async fn upload_command(
    commands: &mut mpsc::Receiver<Command>,
    cancelled: &watch::Receiver<bool>,
    cancellation_delivered: &mut bool,
) -> Option<Command> {
    tokio::select! {
        biased;
        () = wait_for_cancel(cancelled), if !*cancellation_delivered => {
            *cancellation_delivered = true;
            Some(Command::Cancel)
        },
        command = commands.recv() => {
            if command == Some(Command::Cancel) {
                *cancellation_delivered = true;
            }
            command
        },
    }
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

fn server_too_large(limit: u64) -> String {
    format!(
        "Too large: this homeserver accepts files up to {}.",
        human_size(limit)
    )
}

/// Fixed wording for a file that was never queued. Server text, paths and
/// SDK details are never shown.
fn upload_failure(error: &MatrixCoreError) -> &'static str {
    match error {
        MatrixCoreError::SelectedAccountChanged | MatrixCoreError::AccountNotFound(_) => {
            "Select this account before sending its attachments."
        }
        MatrixCoreError::RoomNotFound(_) => "This room is no longer available.",
        MatrixCoreError::Io(_) => "This file can't be read; it may have moved.",
        MatrixCoreError::InvalidMedia(_) => "This file can't be sent as an attachment.",
        _ => "The file could not be prepared. Try again.",
    }
}

/// Fixed wording for a download or save that wrote nothing.
fn download_failure(error: &MatrixCoreError) -> &'static str {
    match error {
        MatrixCoreError::MediaTooLarge { .. } => TOO_LARGE,
        MatrixCoreError::SelectedAccountChanged
        | MatrixCoreError::SelectedRoomChanged
        | MatrixCoreError::AccountNotFound(_) => "The account changed; nothing was written.",
        MatrixCoreError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            "That name is already in use. Retry and choose a different name; nothing was replaced."
        }
        MatrixCoreError::Io(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            "That folder can't be written to; nothing was saved."
        }
        MatrixCoreError::Io(_) => "The file could not be written; nothing was saved.",
        MatrixCoreError::RoomNotFound(_) => "This room is no longer available.",
        MatrixCoreError::InvalidMedia(_) => "This attachment can't be downloaded.",
        _ => "The attachment could not be downloaded. Try again later.",
    }
}

fn plain(text: &str, max_chars: usize) -> String {
    crate::directory_plain_text(text, max_chars)
}

fn upload_status(item: &Item) -> String {
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
    if item.note.is_empty() {
        status
    } else {
        format!("{} · {status}", item.note)
    }
}

fn upload_detail(item: &Item) -> String {
    match item.size {
        Some(size) => format!("{} · {}", human_size(size), item.mime_type),
        None => item.mime_type.clone(),
    }
}

fn row(item: &Item) -> AttachmentItem {
    AttachmentItem {
        // Zero is never an item ID, so an unrepresentable one has no actions.
        id: i32::try_from(item.id).unwrap_or_default(),
        name: plain(&item.filename, 160).into(),
        detail: plain(&upload_detail(item), 120).into(),
        status: plain(&upload_status(item), 240).into(),
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
        // Set by the projection, which knows the room's staged order.
        can_move_up: false,
        can_move_down: false,
    }
}

fn upload_transfer(item: &Item) -> TransferItem {
    let shown = row(item);
    let group = match item.phase {
        Phase::Preparing | Phase::Uploading(_) | Phase::Waiting | Phase::Cancelling => GROUP_ACTIVE,
        _ => GROUP_ATTENTION,
    };
    TransferItem {
        kind: KIND_UPLOAD,
        id: shown.id,
        name: shown.name,
        room: plain(&item.room_name, 80).into(),
        direction: "Upload".into(),
        detail: shown.detail,
        status: shown.status,
        progress: shown.progress,
        busy: matches!(
            item.phase,
            Phase::Preparing | Phase::Waiting | Phase::Cancelling
        ),
        group,
        can_cancel: shown.can_cancel,
        can_retry: shown.can_retry,
        can_remove: shown.can_remove,
        can_open: shown.id != 0,
    }
}

fn download_transfer(download: &Download) -> TransferItem {
    let status = match &download.phase {
        DownloadPhase::Waiting => "Waiting for the previous download".to_owned(),
        DownloadPhase::Downloading => "Downloading and decrypting…".to_owned(),
        DownloadPhase::Saving => "Saving…".to_owned(),
        DownloadPhase::Cancelling => "Cancelling…".to_owned(),
        DownloadPhase::Failed(reason) => format!("Not saved: {reason}"),
    };
    let id = i32::try_from(download.id).unwrap_or_default();
    TransferItem {
        kind: KIND_DOWNLOAD,
        id,
        name: plain(&download.filename, 160).into(),
        room: plain(&download.room_name, 80).into(),
        direction: "Download".into(),
        detail: "".into(),
        status: plain(&status, 240).into(),
        // The download reports no byte progress; never invent one.
        progress: -1.0,
        busy: download.phase.running(),
        group: if download.phase.running() {
            GROUP_ACTIVE
        } else {
            GROUP_ATTENTION
        },
        can_cancel: download.phase.cancellable(),
        can_retry: !download.phase.running(),
        can_remove: !download.phase.running(),
        can_open: id != 0,
    }
}

fn finished_transfer(finished: &Finished) -> TransferItem {
    let id = i32::try_from(finished.id).unwrap_or_default();
    TransferItem {
        kind: KIND_FINISHED,
        id,
        name: plain(&finished.filename, 160).into(),
        room: plain(&finished.room_name, 80).into(),
        direction: if finished.upload {
            "Upload".into()
        } else {
            "Download".into()
        },
        detail: "".into(),
        status: plain(&finished.outcome, 240).into(),
        progress: -1.0,
        busy: false,
        group: GROUP_FINISHED,
        can_cancel: false,
        can_retry: false,
        can_remove: id != 0,
        can_open: id != 0,
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

fn transfer_summary(active: usize, attention: usize, finished: usize) -> String {
    if active + attention + finished == 0 {
        return "No transfers for this account".to_owned();
    }
    let mut parts = Vec::new();
    if active > 0 {
        parts.push(format!("{active} in progress"));
    }
    if attention > 0 {
        parts.push(format!("{attention} waiting for you"));
    }
    if finished > 0 {
        parts.push(format!("{finished} ended"));
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

    #[tokio::test]
    async fn room_admission_keeps_order_through_middle_cancellation_and_retry() {
        let room = Arc::new(AdmissionQueue::default());
        let first = room.reserve(1);
        let middle = room.reserve(2);
        let last = room.reserve(3);
        first.wait().await;
        let waiting = last.wait();
        tokio::pin!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        drop(middle);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        first.release();
        waiting.await;
        // Retry may reuse a row ID while the old, released task still owns
        // its guard. Dropping that guard must not release the new attempt.
        let retry = room.reserve(1);
        drop(first);
        let retry_waiting = retry.wait();
        tokio::pin!(retry_waiting);
        assert!(futures_util::poll!(retry_waiting.as_mut()).is_pending());
        last.release();
        retry_waiting.await;
        retry.release();
        assert!(room.pending.lock().unwrap().is_empty());
        // Admissions in separate rooms never wait on each other.
        let other = Arc::new(AdmissionQueue::default()).reserve(4);
        other.wait().await;
    }

    #[tokio::test]
    async fn upload_cancel_survives_a_full_click_queue_without_a_busy_loop() {
        let (sender, mut commands) = mpsc::channel(COMMAND_CAPACITY);
        for _ in 0..COMMAND_CAPACITY {
            sender.try_send(Command::Retry).unwrap();
        }
        assert!(sender.try_send(Command::Cancel).is_err());
        let (cancel, cancelled) = watch::channel(false);
        cancel.send(true).unwrap();
        let mut delivered = false;
        assert_eq!(
            upload_command(&mut commands, &cancelled, &mut delivered).await,
            Some(Command::Cancel)
        );
        for _ in 0..COMMAND_CAPACITY {
            assert_eq!(
                upload_command(&mut commands, &cancelled, &mut delivered).await,
                Some(Command::Retry)
            );
        }
        // A permanently true watch must not repeatedly call SDK abort.
        let next = upload_command(&mut commands, &cancelled, &mut delivered);
        tokio::pin!(next);
        assert!(futures_util::poll!(next.as_mut()).is_pending());
    }

    fn item(phase: Phase, commands: bool) -> Item {
        Item {
            id: 3,
            account_id: "a".into(),
            room_id: "r".into(),
            room_name: "Room".into(),
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

    fn staged(id: u64, account_id: &str, room_id: &str) -> Item {
        Item {
            id,
            account_id: account_id.into(),
            room_id: room_id.into(),
            ..item(Phase::Staged, false)
        }
    }

    fn download(id: u64, phase: DownloadPhase) -> Download {
        Download {
            id,
            account_id: "a".into(),
            room_id: "r".into(),
            room_name: "Room".into(),
            event_id: "$event".into(),
            filename: "report.pdf".into(),
            phase,
            cancel: None,
        }
    }

    #[test]
    fn download_admission_is_rechecked_and_failed_history_stays_bounded() {
        let mut state = State::default();
        assert_eq!(state.check_download_admission("a", "$new"), Ok(()));
        // Represents downloads admitted while a native modal picker pumps
        // callbacks; the same admission check must now refuse a stale slot.
        for id in 0..MAX_DOWNLOADS as u64 {
            let mut row = download(id, DownloadPhase::Waiting);
            row.event_id = format!("$event-{id}");
            state.downloads.push(row);
        }
        assert_eq!(
            state.check_download_admission("a", "$new"),
            Err(DownloadAdmissionError::Full)
        );
        assert_eq!(
            state.check_download_admission("a", "$event-0"),
            Err(DownloadAdmissionError::Duplicate)
        );
        for id in 100..(100 + MAX_FAILED_DOWNLOADS as u64 + 10) {
            state
                .downloads
                .push(download(id, DownloadPhase::Failed("unavailable".into())));
            state.trim_failed_downloads(id);
        }
        assert_eq!(state.downloads.len(), MAX_DOWNLOADS + MAX_FAILED_DOWNLOADS);
        assert_eq!(
            state
                .downloads
                .iter()
                .filter(|row| row.phase.running())
                .count(),
            MAX_DOWNLOADS
        );
        assert_eq!(
            state.downloads.last().unwrap().id,
            100 + MAX_FAILED_DOWNLOADS as u64 + 9
        );
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

    #[test]
    fn staged_files_reorder_only_among_their_room_and_account() {
        let mut state = State {
            items: vec![
                staged(1, "a", "r"),
                staged(2, "a", "s"),
                staged(3, "a", "r"),
                staged(4, "b", "r"),
            ],
            ..State::default()
        };
        // Down past another room's file, onto the next file of its room.
        assert!(state.move_staged("a", 1, false));
        let order: Vec<u64> = state.items.iter().map(|item| item.id).collect();
        assert_eq!(order, [3, 2, 1, 4]);
        // Never past the ends, never another account's file.
        assert!(!state.move_staged("a", 1, false));
        assert!(!state.move_staged("a", 3, true));
        assert!(!state.move_staged("b", 1, true));
        // A file that is already sending keeps its place.
        state.items[0].phase = Phase::Preparing;
        assert!(!state.move_staged("a", 1, true));
    }

    #[test]
    fn ended_transfers_are_bounded_and_keep_no_path() {
        let mut state = State::default();
        for id in 0..(MAX_FINISHED as u64 + 5) {
            state.remember(Finished {
                id,
                account_id: "a".into(),
                room_id: "r".into(),
                room_name: "Room".into(),
                filename: format!("file-{id}.txt"),
                upload: true,
                outcome: "Sent".into(),
            });
        }
        assert_eq!(state.finished.len(), MAX_FINISHED);
        assert_eq!(state.finished.front().map(|finished| finished.id), Some(5));
        let shown = finished_transfer(state.finished.back().unwrap());
        assert_eq!(shown.group, GROUP_FINISHED);
        assert!(shown.can_remove && shown.can_open && !shown.can_retry && !shown.can_cancel);
    }

    #[test]
    fn transfer_rows_never_invent_progress_or_resend_unknown_uploads() {
        // A download reports no byte progress: shown as busy, never as a
        // percentage, and retried only once it has failed.
        let running = download_transfer(&download(7, DownloadPhase::Downloading));
        assert!(running.progress < 0.0 && running.busy && running.can_cancel);
        assert!(!running.can_retry && !running.can_remove && running.group == GROUP_ACTIVE);
        let saving = download_transfer(&download(7, DownloadPhase::Saving));
        // An admitted write finishes; it can no longer be cancelled.
        assert!(!saving.can_cancel);
        let failed = download_transfer(&download(7, DownloadPhase::Failed("x".into())));
        assert!(failed.can_retry && failed.can_remove && failed.group == GROUP_ATTENTION);
        // An upload whose delivery is unknown is never offered again.
        let uncertain = upload_transfer(&item(Phase::Uncertain, false));
        assert!(!uncertain.can_retry && uncertain.group == GROUP_ATTENTION);
        let uploading = upload_transfer(&item(Phase::Uploading(40), true));
        assert!(uploading.group == GROUP_ACTIVE && !uploading.busy);
        assert_eq!(uploading.progress, 0.4);
        assert_eq!(transfer_summary(0, 0, 0), "No transfers for this account");
        assert_eq!(
            transfer_summary(1, 2, 3),
            "1 in progress · 2 waiting for you · 3 ended"
        );
    }

    #[test]
    fn failures_use_fixed_wording_without_paths_or_server_text() {
        let secret = "https://example.org/_matrix/media/v3/download?access_token=SECRET /home/user";
        let shown = [
            upload_failure(&MatrixCoreError::Sdk(secret.into())),
            upload_failure(&MatrixCoreError::Io(io::Error::other(secret))),
            download_failure(&MatrixCoreError::Sdk(secret.into())),
            download_failure(&MatrixCoreError::Io(io::Error::other(secret))),
        ];
        for text in shown {
            assert!(!text.contains("SECRET") && !text.contains("/home") && !text.contains("http"));
        }
        let exists = io::Error::new(io::ErrorKind::AlreadyExists, secret);
        assert_eq!(
            download_failure(&MatrixCoreError::Io(exists)),
            "That name is already in use. Retry and choose a different name; nothing was replaced."
        );
        assert_eq!(
            server_too_large(10_000_000),
            "Too large: this homeserver accepts files up to 10.0 MB."
        );
    }
}
