//! Native sticker picker and the one tracked sticker send.
//!
//! The Matrix core owns each catalog, its media addresses and every
//! authorization check. This controller keeps the opaque catalog, publishes
//! sanitized labels with opaque keys to Slint, and binds every asynchronous
//! result to its picker session before publication. A picker belongs to one
//! account selection, room and optional thread: the existing selection
//! revocation (account switch, protected switch, removal or detach) and any
//! change of the visible destination close it, so returning later never
//! revives a choice, preview or consent.
//!
//! Sticker images are public pack media. In an encrypted conversation they
//! are fetched or sent only after an explicit consent for the exact catalog;
//! the labels stay usable without it. A rebuilt catalog needs consent again.

use crate::{
    AppWindow, StickerItem,
    image_previews::{self, PreviewError},
    live::LiveUi,
};
use morpheus_matrix::{
    AccountSelection, MatrixCoreError, PublicMediaConsent, QueuedMessage, QueuedSendUpdate,
    StickerCatalog, StickerEntry, StickerPack,
};
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::{
    runtime::Handle,
    sync::{Notify, broadcast, mpsc},
};
use zeroize::Zeroizing;

const CATALOG_TIMEOUT: Duration = Duration::from_secs(20);
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(45);
const PREVIEW_DELAY: Duration = Duration::from_millis(200);
const WORKER_WAIT: Duration = Duration::from_secs(5);
const WATCH_INTERVAL: Duration = Duration::from_secs(3);
const WATCH_MINIMUM: Duration = Duration::from_secs(1);
const SENT_STATUS: Duration = Duration::from_secs(5);
const MAX_QUERY_CHARS: usize = 100;
const MAX_ROWS: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Destination {
    account: String,
    room: String,
    thread: Option<String>,
}

impl Destination {
    fn threaded(&self) -> bool {
        self.thread.is_some()
    }
}

/// Ends one picker's background work without waiting for it.
#[derive(Default)]
struct Stop {
    stopped: AtomicBool,
    wake: Notify,
}

impl Stop {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            wake.await;
        }
    }
}

/// The single sticker decode worker. A cancelled request cannot recall a
/// codec call already running; the permit stays held until it finishes.
struct Permit(Arc<AtomicBool>);

impl Permit {
    fn acquire(gate: &Arc<AtomicBool>) -> Option<Self> {
        gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(gate.clone()))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

enum Phase {
    Loading,
    Ready(Arc<StickerCatalog>),
    Failed(String),
}

#[derive(Default)]
struct PreviewSlot {
    request: u64,
    entry: Option<usize>,
    pixels: Option<SharedPixelBuffer<Rgba8Pixel>>,
    status: String,
}

struct Picker {
    session: u64,
    destination: Destination,
    selection: AccountSelection,
    phase: Phase,
    query: String,
    pack: Option<usize>,
    rows: Vec<usize>,
    highlighted: Option<usize>,
    /// The destination needs consent: from the catalog, or because a later
    /// action found the room encrypted.
    consent_required: bool,
    consent: Option<PublicMediaConsent>,
    notice: String,
    preview: PreviewSlot,
    stop: Arc<Stop>,
}

impl Picker {
    fn catalog(&self) -> Option<&Arc<StickerCatalog>> {
        match &self.phase {
            Phase::Ready(catalog) => Some(catalog),
            _ => None,
        }
    }

    fn current_entry(&self) -> Option<usize> {
        self.highlighted.and_then(|row| self.rows.get(row).copied())
    }

    fn consented(&self) -> bool {
        !self.consent_required || self.consent.is_some()
    }

    fn refilter(&mut self) {
        let rows = match self.catalog() {
            Some(catalog) => filter(catalog.entries(), catalog.packs(), &self.query, self.pack),
            None => Vec::new(),
        };
        self.highlighted = (!rows.is_empty()).then_some(0);
        self.rows = rows;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Cancel,
    Retry,
}

struct Tracked {
    send: u64,
    destination: Destination,
    selection: AccountSelection,
    status: String,
    can_cancel: bool,
    can_retry: bool,
    finished: bool,
    commands: Option<mpsc::UnboundedSender<Command>>,
}

#[derive(Default)]
struct State {
    picker: Option<Picker>,
    tracked: Option<Tracked>,
    enqueuing: bool,
}

pub(crate) struct StickerUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    state: Mutex<State>,
    sessions: AtomicU64,
    previews: AtomicU64,
    sends: AtomicU64,
    worker: Arc<AtomicBool>,
}

fn destination(ui: &AppWindow, threaded: bool) -> Option<Destination> {
    let account = ui.get_active_account_local_id().to_string();
    let room = ui.get_active_room_id().to_string();
    if account.is_empty() || room.is_empty() {
        return None;
    }
    let thread = if threaded {
        let root = ui.get_thread_root_id().to_string();
        if root.is_empty() {
            return None;
        }
        Some(root)
    } else {
        None
    };
    Some(Destination {
        account,
        room,
        thread,
    })
}

/// The composer the picker belongs to is visible and idle. A room picker is
/// for new messages only; reply and edit modes close it.
fn allowed(ui: &AppWindow, threaded: bool) -> bool {
    ui.get_live_session()
        && ui.get_mention_ui_allowed()
        && if threaded {
            !ui.get_thread_send_pending() && ui.get_side_panel_tab() == 1
        } else {
            !ui.get_composer_busy() && ui.get_composer_mode() == 0
        }
}

fn binding_matches(
    current_selection: bool,
    allowed: bool,
    visible: Option<&Destination>,
    bound: &Destination,
) -> bool {
    current_selection && allowed && visible == Some(bound)
}

fn picker_valid(ui: &AppWindow, picker: &Picker) -> bool {
    let threaded = picker.destination.threaded();
    binding_matches(
        picker.selection.is_current(),
        allowed(ui, threaded),
        destination(ui, threaded).as_ref(),
        &picker.destination,
    )
}

fn tracked_visible(ui: &AppWindow, tracked: &Tracked) -> bool {
    let threaded = tracked.destination.threaded();
    tracked.selection.is_current()
        && ui.get_live_session()
        && destination(ui, threaded).as_ref() == Some(&tracked.destination)
        && (!threaded || ui.get_side_panel_tab() == 1)
}

/// Case-insensitive search over sanitized labels, shortcodes and pack names.
fn filter(
    entries: &[StickerEntry],
    packs: &[StickerPack],
    query: &str,
    pack: Option<usize>,
) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| pack.is_none_or(|pack| entry.pack == pack))
        .filter(|(_, entry)| {
            needle.is_empty()
                || entry.label.to_lowercase().contains(&needle)
                || entry.shortcode.to_lowercase().contains(&needle)
                || packs
                    .get(entry.pack)
                    .is_some_and(|pack| pack.name.to_lowercase().contains(&needle))
        })
        .map(|(index, _)| index)
        .take(MAX_ROWS)
        .collect()
}

fn item_key(session: u64, entry: usize) -> String {
    format!("{session}:{entry}")
}

/// An opaque key is valid only for this picker session and a shown row.
fn parse_key(key: &str, session: u64, rows: &[usize]) -> Option<usize> {
    let (owner, entry) = key.split_once(':')?;
    if owner.parse::<u64>().ok()? != session {
        return None;
    }
    let entry = entry.parse::<usize>().ok()?;
    rows.iter().position(|row| *row == entry)
}

fn bounded_query(query: &str) -> String {
    query.chars().take(MAX_QUERY_CHARS).collect()
}

fn catalog_error(error: &MatrixCoreError) -> String {
    match error {
        MatrixCoreError::SelectedAccountChanged | MatrixCoreError::NoActiveAccount => {
            "The account changed. Open the sticker picker again."
        }
        MatrixCoreError::StickerUnavailable => {
            "Stickers are unavailable here. This conversation may have been left or removed."
        }
        MatrixCoreError::InvalidMedia(_) => {
            "Room encryption could not be checked, so sticker images stay off."
        }
        _ => "Sticker packs could not be loaded. Close the picker and try again.",
    }
    .into()
}

fn preview_failure(error: PreviewError) -> &'static str {
    match error {
        PreviewError::TooLarge => {
            "Too large to preview: 2 MiB, 4096 pixels per side and 4 megapixels. You can still send it."
        }
        PreviewError::Unsupported => {
            "Previews support static PNG and baseline JPEG. You can still send this sticker."
        }
        PreviewError::Invalid => "This sticker image could not be decoded safely.",
        PreviewError::Unavailable | PreviewError::Busy => "The preview is unavailable right now.",
    }
}

fn download_failure(error: &MatrixCoreError) -> &'static str {
    match error {
        MatrixCoreError::MediaTooLarge { .. } => preview_failure(PreviewError::TooLarge),
        MatrixCoreError::StickerConsentRequired => "Allow sticker images here to preview them.",
        MatrixCoreError::StickerUnavailable => "This sticker changed. Choose it again.",
        _ => "The preview could not be loaded.",
    }
}

/// Status of the tracked send after one queue update: text, cancel, retry,
/// finished. A wedged request is retried in place, never sent again.
fn update_status(update: QueuedSendUpdate) -> (&'static str, bool, bool, bool) {
    match update {
        QueuedSendUpdate::Sent => ("Sticker sent.", false, false, true),
        QueuedSendUpdate::Retained { recoverable: true } => (
            "Sticker not sent yet. It will retry automatically.",
            true,
            false,
            false,
        ),
        QueuedSendUpdate::Retained { recoverable: false } => (
            "Sticker could not be sent. Retry it or cancel it.",
            true,
            true,
            false,
        ),
        QueuedSendUpdate::Cancelled => ("Sticker cancelled.", false, false, true),
        QueuedSendUpdate::Unknown => (
            "Sticker status is unknown. Check the conversation before sending another.",
            false,
            false,
            true,
        ),
    }
}

fn send_failure(error: &MatrixCoreError) -> String {
    match error {
        MatrixCoreError::StickerConsentRequired => {
            "Allow sticker images here before sending.".into()
        }
        MatrixCoreError::StickerUnavailable => {
            "This sticker or its pack changed. Choose a sticker again.".into()
        }
        MatrixCoreError::SelectedAccountChanged | MatrixCoreError::SelectedRoomChanged => {
            "The conversation changed before the sticker was queued. Nothing was sent.".into()
        }
        MatrixCoreError::RecoveryIncomplete
        | MatrixCoreError::RecoveryBlocked
        | MatrixCoreError::PendingLimit(_) => error.to_string(),
        _ => "The sticker could not be queued. Nothing was sent.".into(),
    }
}

impl StickerUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime,
            state: Mutex::new(State::default()),
            sessions: AtomicU64::new(0),
            previews: AtomicU64::new(0),
            sends: AtomicU64::new(0),
            worker: Arc::new(AtomicBool::new(false)),
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_open_stickers(move |threaded| {
            if let Some(ui) = weak.upgrade() {
                this.open(&ui, threaded);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_query(move |query| {
            if let Some(ui) = weak.upgrade() {
                this.search(&ui, query.as_str());
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_navigate(move |offset| {
            if let Some(ui) = weak.upgrade() {
                this.navigate(&ui, offset);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_choose_pack(move |index| {
            if let Some(ui) = weak.upgrade() {
                this.choose_pack(&ui, index);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_highlight(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.highlight(&ui, key.as_str());
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_send(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.send(&ui, key.as_str());
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_consent(move |allowed| {
            if let Some(ui) = weak.upgrade() {
                this.consent(&ui, allowed);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                this.close(&ui, true);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.publish(&ui);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_send_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                this.command(&ui, Command::Cancel);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_send_retry(move || {
            if let Some(ui) = weak.upgrade() {
                this.command(&ui, Command::Retry);
            }
        });
        let weak = ui.as_weak();
        let this = owner.clone();
        ui.on_sticker_send_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                this.dismiss_status(&ui);
            }
        });
        owner
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Republish on the UI thread; publication re-checks every binding.
    fn schedule_publish(self: &Arc<Self>, weak: &slint::Weak<AppWindow>) {
        let owner = self.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| owner.publish(&ui));
    }

    fn open(self: &Arc<Self>, ui: &AppWindow, threaded: bool) {
        if !allowed(ui, threaded) {
            return;
        }
        let Some(destination) = destination(ui, threaded) else {
            return;
        };
        let reopen = self
            .lock()
            .picker
            .as_ref()
            .is_some_and(|picker| picker.destination == destination);
        // The entry button toggles: activating it again closes the picker.
        self.close(ui, reopen);
        if reopen {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&destination.account) else {
            return;
        };
        let session = self.sessions.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let stop = Arc::new(Stop::default());
        self.lock().picker = Some(Picker {
            session,
            destination: destination.clone(),
            selection: selection.clone(),
            phase: Phase::Loading,
            query: String::new(),
            pack: None,
            rows: Vec::new(),
            highlighted: None,
            consent_required: false,
            consent: None,
            notice: String::new(),
            preview: PreviewSlot::default(),
            stop: stop.clone(),
        });
        ui.set_sticker_query_text(SharedString::new());
        ui.set_sticker_pack_index(0);
        self.publish(ui);
        let owner = self.clone();
        let manager = self.live.manager.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            let built = tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = selection.revoked() => {
                    owner.schedule_publish(&weak);
                    return;
                }
                built = tokio::time::timeout(
                    CATALOG_TIMEOUT,
                    manager.sticker_catalog_for(
                        &selection,
                        &destination.room,
                        destination.thread.as_deref(),
                    ),
                ) => built,
            };
            let phase = match built {
                Ok(Ok(catalog)) => Phase::Ready(Arc::new(catalog)),
                Ok(Err(error)) => Phase::Failed(catalog_error(&error)),
                Err(_) => Phase::Failed(
                    "Loading sticker packs timed out. Close the picker and try again.".into(),
                ),
            };
            if owner.adopt(session, phase, String::new()) {
                owner.request_preview(&weak);
                owner.schedule_publish(&weak);
                owner.watch(session, selection, stop, weak).await;
            } else {
                owner.schedule_publish(&weak);
            }
        });
    }

    /// Install a catalog (or failure) in the still-open picker of `session`.
    /// A replaced catalog loses its consent, choice and preview.
    fn adopt(&self, session: u64, phase: Phase, notice: String) -> bool {
        let mut state = self.lock();
        let Some(picker) = state
            .picker
            .as_mut()
            .filter(|picker| picker.session == session && picker.selection.is_current())
        else {
            return false;
        };
        picker.consent_required = match &phase {
            Phase::Ready(catalog) => catalog.requires_public_media_consent(),
            _ => false,
        };
        picker.phase = phase;
        picker.consent = None;
        picker.pack = None;
        picker.notice = notice;
        picker.preview = PreviewSlot::default();
        picker.refilter();
        true
    }

    /// While the picker is open, compare its catalog with the live sources:
    /// a changed, redacted, disabled, newly enabled or departed pack replaces
    /// the catalog and revokes the choice, preview and consent made on it.
    async fn watch(
        self: &Arc<Self>,
        session: u64,
        selection: AccountSelection,
        stop: Arc<Stop>,
        weak: slint::Weak<AppWindow>,
    ) {
        let manager = self.live.manager.clone();
        let mut updates = manager
            .subscribe_room_updates()
            .await
            .ok()
            .filter(|(account, _)| *account == selection.account_id())
            .map(|(_, updates)| updates);
        loop {
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = selection.revoked() => {
                    self.schedule_publish(&weak);
                    return;
                }
                _ = tokio::time::sleep(WATCH_INTERVAL) => {}
                _ = next_room_update(&mut updates) => {}
            }
            let catalog = {
                let state = self.lock();
                match state.picker.as_ref().filter(|picker| picker.session == session) {
                    Some(picker) => picker.catalog().cloned(),
                    None => return,
                }
            };
            let Some(catalog) = catalog else {
                return;
            };
            let refreshed = tokio::select! {
                biased;
                _ = stop.wait() => return,
                refreshed = tokio::time::timeout(
                    CATALOG_TIMEOUT,
                    manager.refresh_sticker_catalog(&catalog),
                ) => refreshed,
            };
            match refreshed {
                Ok(Ok(None)) | Err(_) => {}
                Ok(Ok(Some(fresh))) => {
                    if self.adopt(
                        session,
                        Phase::Ready(Arc::new(fresh)),
                        "Sticker packs changed. Choose a sticker again.".into(),
                    ) {
                        self.request_preview(&weak);
                    }
                    self.schedule_publish(&weak);
                }
                Ok(Err(error)) => {
                    self.adopt(session, Phase::Failed(catalog_error(&error)), String::new());
                    self.schedule_publish(&weak);
                    if matches!(error, MatrixCoreError::SelectedAccountChanged) {
                        return;
                    }
                }
            }
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = tokio::time::sleep(WATCH_MINIMUM) => {}
            }
        }
    }

    fn search(self: &Arc<Self>, ui: &AppWindow, query: &str) {
        {
            let mut state = self.lock();
            let Some(picker) = state.picker.as_mut() else {
                return;
            };
            picker.query = bounded_query(query);
            picker.notice.clear();
            picker.refilter();
        }
        self.request_preview(&ui.as_weak());
        self.publish(ui);
    }

    fn navigate(self: &Arc<Self>, ui: &AppWindow, offset: i32) {
        {
            let mut state = self.lock();
            let Some(picker) = state.picker.as_mut().filter(|picker| !picker.rows.is_empty())
            else {
                return;
            };
            let length = picker.rows.len() as i64;
            let current = picker.highlighted.map_or(-1, |row| row as i64);
            let next = if current < 0 {
                if offset < 0 { length - 1 } else { 0 }
            } else {
                (current + i64::from(offset.signum())).rem_euclid(length)
            };
            picker.highlighted = Some(next as usize);
            picker.notice.clear();
        }
        self.request_preview(&ui.as_weak());
        self.publish(ui);
    }

    fn choose_pack(self: &Arc<Self>, ui: &AppWindow, index: i32) {
        {
            let mut state = self.lock();
            let Some(picker) = state.picker.as_mut() else {
                return;
            };
            let packs = picker.catalog().map_or(0, |catalog| catalog.packs().len());
            picker.pack = usize::try_from(index)
                .ok()
                .and_then(|index| index.checked_sub(1))
                .filter(|pack| *pack < packs);
            picker.notice.clear();
            picker.refilter();
        }
        self.request_preview(&ui.as_weak());
        self.publish(ui);
    }

    fn highlight(self: &Arc<Self>, ui: &AppWindow, key: &str) {
        {
            let mut state = self.lock();
            let Some(picker) = state.picker.as_mut() else {
                return;
            };
            let Some(row) = parse_key(key, picker.session, &picker.rows) else {
                return;
            };
            picker.highlighted = Some(row);
            picker.notice.clear();
        }
        self.request_preview(&ui.as_weak());
        self.publish(ui);
    }

    fn consent(self: &Arc<Self>, ui: &AppWindow, allowed: bool) {
        if !allowed {
            // Cancelling the consent prompt closes the picker; nothing loads.
            self.close(ui, true);
            return;
        }
        {
            let mut state = self.lock();
            let Some(picker) = state.picker.as_mut() else {
                return;
            };
            let Some(catalog) = picker.catalog().cloned() else {
                return;
            };
            picker.consent = Some(catalog.public_media_consent());
            picker.notice.clear();
            picker.preview = PreviewSlot::default();
        }
        self.request_preview(&ui.as_weak());
        self.publish(ui);
    }

    /// Start (after a short pause) a preview of the highlighted sticker,
    /// replacing any earlier request. Without consent nothing is fetched.
    fn request_preview(self: &Arc<Self>, weak: &slint::Weak<AppWindow>) {
        let (session, request, catalog, entry, consent, stop) = {
            let mut state = self.lock();
            let Some(picker) = state.picker.as_mut() else {
                return;
            };
            let Some(catalog) = picker.catalog().cloned() else {
                return;
            };
            let Some(entry) = picker.current_entry() else {
                picker.preview = PreviewSlot {
                    status: "Choose a sticker to see it.".into(),
                    ..PreviewSlot::default()
                };
                return;
            };
            if picker.preview.entry == Some(entry) && picker.preview.request != 0 {
                return;
            }
            let request = self.previews.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let consented = picker.consented();
            picker.preview = PreviewSlot {
                request,
                entry: Some(entry),
                pixels: None,
                status: if consented {
                    "Loading preview…".into()
                } else {
                    "Images appear after you allow sticker images here.".into()
                },
            };
            if !consented {
                return;
            }
            (
                picker.session,
                request,
                catalog,
                entry,
                picker.consent,
                picker.stop.clone(),
            )
        };
        let owner = self.clone();
        let weak = weak.clone();
        self.runtime.spawn(async move {
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = tokio::time::sleep(PREVIEW_DELAY) => {}
            }
            if !owner.preview_current(session, request) {
                return;
            }
            let manager = owner.live.manager.clone();
            let downloaded = tokio::select! {
                biased;
                _ = stop.wait() => return,
                downloaded = tokio::time::timeout(
                    PREVIEW_TIMEOUT,
                    manager.download_sticker_preview_for(&catalog, entry, consent.as_ref()),
                ) => downloaded,
            };
            let result = match downloaded {
                Ok(Ok(preview)) => owner.decode(preview.into_bytes(), session, request, &stop).await,
                Ok(Err(error)) => {
                    if matches!(error, MatrixCoreError::StickerConsentRequired) {
                        owner.require_consent(session);
                    }
                    Err(download_failure(&error))
                }
                Err(_) => Err("The preview timed out."),
            };
            owner.finish_preview(session, request, result);
            owner.schedule_publish(&weak);
        });
    }

    fn preview_current(&self, session: u64, request: u64) -> bool {
        self.lock().picker.as_ref().is_some_and(|picker| {
            picker.session == session
                && picker.preview.request == request
                && picker.selection.is_current()
                && !picker.stop.is_stopped()
        })
    }

    fn require_consent(&self, session: u64) {
        let mut state = self.lock();
        if let Some(picker) = state
            .picker
            .as_mut()
            .filter(|picker| picker.session == session)
        {
            picker.consent_required = true;
            picker.consent = None;
        }
    }

    /// Decode on the dedicated sticker worker with the shared preview limits.
    async fn decode(
        &self,
        bytes: Vec<u8>,
        session: u64,
        request: u64,
        stop: &Stop,
    ) -> Result<SharedPixelBuffer<Rgba8Pixel>, &'static str> {
        let bytes = Zeroizing::new(bytes);
        let deadline = tokio::time::Instant::now() + WORKER_WAIT;
        let permit = loop {
            if let Some(permit) = Permit::acquire(&self.worker) {
                break permit;
            }
            if !self.preview_current(session, request) || tokio::time::Instant::now() >= deadline {
                return Err(preview_failure(PreviewError::Busy));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("sticker-preview".into())
            .spawn(move || {
                let _permit = permit;
                let decoded = image_previews::decode_rgba(bytes).map(|(width, height, rgba)| {
                    SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_slice(), width, height)
                });
                let _ = sender.send(decoded);
            });
        if spawned.is_err() {
            return Err(preview_failure(PreviewError::Unavailable));
        }
        tokio::select! {
            biased;
            _ = stop.wait() => Err(preview_failure(PreviewError::Unavailable)),
            decoded = receiver => match decoded {
                Ok(Ok(pixels)) => Ok(pixels),
                Ok(Err(error)) => Err(preview_failure(error)),
                Err(_) => Err(preview_failure(PreviewError::Invalid)),
            },
        }
    }

    fn finish_preview(
        &self,
        session: u64,
        request: u64,
        result: Result<SharedPixelBuffer<Rgba8Pixel>, &'static str>,
    ) {
        let mut state = self.lock();
        let Some(picker) = state.picker.as_mut().filter(|picker| {
            picker.session == session
                && picker.preview.request == request
                && picker.selection.is_current()
        }) else {
            return;
        };
        match result {
            Ok(pixels) => {
                picker.preview.status = format!("{} × {}", pixels.width(), pixels.height());
                picker.preview.pixels = Some(pixels);
            }
            Err(text) => {
                picker.preview.status = text.into();
                picker.preview.pixels = None;
            }
        }
    }

    fn send(self: &Arc<Self>, ui: &AppWindow, key: &str) {
        let prepared = {
            let mut state = self.lock();
            let busy = state.enqueuing
                || state
                    .tracked
                    .as_ref()
                    .is_some_and(|tracked| !tracked.finished && tracked.selection.is_current());
            let Some(picker) = state.picker.as_mut() else {
                return;
            };
            let (Some(row), Some(catalog)) = (
                parse_key(key, picker.session, &picker.rows),
                picker.catalog().cloned(),
            ) else {
                return;
            };
            picker.highlighted = Some(row);
            let entry = picker.rows[row];
            if busy {
                picker.notice =
                    "Wait for the previous sticker to finish sending, or cancel it.".into();
                None
            } else if !picker.consented() {
                picker.notice = "Allow sticker images here before sending.".into();
                None
            } else {
                let label = catalog
                    .entries()
                    .get(entry)
                    .map(|entry| entry.label.clone())
                    .unwrap_or_default();
                let prepared = (
                    picker.session,
                    catalog,
                    entry,
                    picker.consent,
                    picker.selection.clone(),
                    picker.destination.clone(),
                    label,
                );
                state.enqueuing = true;
                Some(prepared)
            }
        };
        self.publish(ui);
        let Some((session, catalog, entry, consent, selection, destination, label)) = prepared
        else {
            return;
        };
        let owner = self.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            // The core re-validates the selection, room, consent and pack
            // source after every await and holds the switch lock to enqueue.
            let queued = owner
                .live
                .send_sticker(&catalog, entry, consent.as_ref())
                .await;
            owner.lock().enqueuing = false;
            match queued {
                Ok(queued) => {
                    owner.track(queued, selection, destination, &label, &weak);
                    let owner = owner.clone();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        let close = owner
                            .lock()
                            .picker
                            .as_ref()
                            .is_some_and(|picker| picker.session == session);
                        if close {
                            owner.close(&ui, true);
                        } else {
                            owner.publish(&ui);
                        }
                    });
                }
                Err(MatrixCoreError::ComposedSendUncertain) => {
                    owner.finished_status(
                        selection,
                        destination,
                        "The sticker may already be queued. Check the conversation before sending another.",
                    );
                    owner.schedule_publish(&weak);
                }
                Err(error) => {
                    {
                        let mut state = owner.lock();
                        if let Some(picker) = state
                            .picker
                            .as_mut()
                            .filter(|picker| picker.session == session)
                        {
                            if matches!(error, MatrixCoreError::StickerConsentRequired) {
                                picker.consent_required = true;
                                picker.consent = None;
                            }
                            picker.notice = send_failure(&error);
                        }
                    }
                    owner.schedule_publish(&weak);
                }
            }
        });
    }

    /// Observe the one queued sticker by its transaction until it finishes,
    /// the user cancels it, or its account selection ends. The SDK request
    /// itself persists in the account's queue either way.
    fn track(
        self: &Arc<Self>,
        mut queued: QueuedMessage,
        selection: AccountSelection,
        destination: Destination,
        label: &str,
        weak: &slint::Weak<AppWindow>,
    ) {
        let send = self.sends.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let (commands, mut receiver) = mpsc::unbounded_channel();
        self.lock().tracked = Some(Tracked {
            send,
            destination,
            selection: selection.clone(),
            status: if label.is_empty() {
                "Sending sticker…".into()
            } else {
                format!("Sending “{label}”…")
            },
            can_cancel: true,
            can_retry: false,
            finished: false,
            commands: Some(commands),
        });
        let owner = self.clone();
        let weak = weak.clone();
        self.runtime.spawn(async move {
            enum Event {
                Update(QueuedSendUpdate),
                Command(Option<Command>),
            }
            loop {
                let event = tokio::select! {
                    biased;
                    _ = selection.revoked() => {
                        owner.end_tracked(send);
                        owner.schedule_publish(&weak);
                        return;
                    }
                    update = queued.next_update() => Event::Update(update),
                    command = receiver.recv() => Event::Command(command),
                };
                match event {
                    Event::Update(update) => {
                        let (text, can_cancel, can_retry, finished) = update_status(update);
                        owner.set_status(send, text, can_cancel, can_retry, finished);
                        owner.schedule_publish(&weak);
                        if finished {
                            if update == QueuedSendUpdate::Sent {
                                tokio::time::sleep(SENT_STATUS).await;
                                owner.clear_finished(send);
                                owner.schedule_publish(&weak);
                            }
                            return;
                        }
                    }
                    Event::Command(Some(Command::Cancel)) => {
                        let (text, can_retry) = match queued.abort().await {
                            Ok(true) => ("Cancelling sticker…", false),
                            Ok(false) => ("The sticker was already sent.", false),
                            Err(_) => ("The sticker could not be cancelled yet.", true),
                        };
                        let finished = text == "The sticker was already sent.";
                        owner.set_status(send, text, !finished, can_retry, finished);
                        owner.schedule_publish(&weak);
                        if finished {
                            return;
                        }
                    }
                    Event::Command(Some(Command::Retry)) => {
                        let text = match queued.retry_wedged().await {
                            Ok(()) => "Retrying sticker…",
                            Err(_) => "The sticker could not be retried. Cancel it or check the conversation.",
                        };
                        owner.set_status(send, text, true, false, false);
                        owner.schedule_publish(&weak);
                    }
                    Event::Command(None) => return,
                }
            }
        });
    }

    fn set_status(&self, send: u64, text: &str, can_cancel: bool, can_retry: bool, finished: bool) {
        let mut state = self.lock();
        if let Some(tracked) = state
            .tracked
            .as_mut()
            .filter(|tracked| tracked.send == send)
        {
            tracked.status = text.into();
            tracked.can_cancel = can_cancel && !finished;
            tracked.can_retry = can_retry && !finished;
            tracked.finished = finished;
            if finished {
                tracked.commands = None;
            }
        }
    }

    fn finished_status(&self, selection: AccountSelection, destination: Destination, text: &str) {
        let send = self.sends.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.lock().tracked = Some(Tracked {
            send,
            destination,
            selection,
            status: text.into(),
            can_cancel: false,
            can_retry: false,
            finished: true,
            commands: None,
        });
    }

    fn end_tracked(&self, send: u64) {
        let mut state = self.lock();
        if state
            .tracked
            .as_ref()
            .is_some_and(|tracked| tracked.send == send)
        {
            state.tracked = None;
        }
    }

    fn clear_finished(&self, send: u64) {
        let mut state = self.lock();
        if state
            .tracked
            .as_ref()
            .is_some_and(|tracked| tracked.send == send && tracked.finished)
        {
            state.tracked = None;
        }
    }

    fn command(&self, ui: &AppWindow, command: Command) {
        {
            let state = self.lock();
            let Some(tracked) = state
                .tracked
                .as_ref()
                .filter(|tracked| tracked_visible(ui, tracked))
            else {
                return;
            };
            let permitted = match command {
                Command::Cancel => tracked.can_cancel,
                Command::Retry => tracked.can_retry,
            };
            if let (true, Some(commands)) = (permitted, tracked.commands.as_ref()) {
                let _ = commands.send(command);
            }
        }
        self.publish(ui);
    }

    fn dismiss_status(&self, ui: &AppWindow) {
        {
            let mut state = self.lock();
            if state
                .tracked
                .as_ref()
                .is_some_and(|tracked| tracked.finished || !tracked.selection.is_current())
            {
                state.tracked = None;
            }
        }
        self.publish(ui);
    }

    /// Close the picker, end its background work and drop its catalog,
    /// consent and preview pixels. Focus returns to its composer on request.
    fn close(&self, ui: &AppWindow, focus: bool) {
        let closed = self.lock().picker.take();
        if let Some(picker) = &closed {
            picker.stop.stop();
        }
        self.publish(ui);
        if let (true, Some(picker)) = (focus, closed) {
            if picker.selection.is_current() {
                ui.invoke_focus_composer(picker.destination.threaded());
            }
        }
    }

    /// Publish the picker and send status for the visible destination. Any
    /// binding that no longer matches closes the picker before publication.
    fn publish(&self, ui: &AppWindow) {
        let stale = {
            let mut state = self.lock();
            let stale = state
                .picker
                .as_ref()
                .is_some_and(|picker| !picker_valid(ui, picker));
            let stopped = if stale { state.picker.take() } else { None };
            if state
                .tracked
                .as_ref()
                .is_some_and(|tracked| !tracked.selection.is_current())
            {
                state.tracked = None;
            }
            stopped
        };
        if let Some(picker) = stale {
            picker.stop.stop();
        }
        let state = self.lock();
        match &state.picker {
            None => {
                ui.set_sticker_open(false);
                ui.set_sticker_items(ModelRc::new(VecModel::from(Vec::<StickerItem>::new())));
                ui.set_sticker_packs(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
                ui.set_sticker_index(-1);
                ui.set_sticker_phase(0);
                ui.set_sticker_status(SharedString::new());
                ui.set_sticker_notice(SharedString::new());
                ui.set_sticker_needs_consent(false);
                ui.set_sticker_can_send(false);
                ui.set_sticker_preview_image(slint::Image::default());
                ui.set_sticker_preview_loaded(false);
                ui.set_sticker_preview_status(SharedString::new());
            }
            Some(picker) => {
                let (items, packs, status) = match &picker.phase {
                    Phase::Loading => (Vec::new(), Vec::new(), "Loading sticker packs…".into()),
                    Phase::Failed(text) => (Vec::new(), Vec::new(), text.clone()),
                    Phase::Ready(catalog) => {
                        let items = picker
                            .rows
                            .iter()
                            .filter_map(|entry| {
                                let sticker = catalog.entries().get(*entry)?;
                                Some(StickerItem {
                                    key: item_key(picker.session, *entry).into(),
                                    label: sticker.label.clone().into(),
                                    pack: catalog
                                        .packs()
                                        .get(sticker.pack)
                                        .map(|pack| pack.name.clone())
                                        .unwrap_or_default()
                                        .into(),
                                })
                            })
                            .collect::<Vec<_>>();
                        let mut packs = vec![SharedString::from("All packs")];
                        packs.extend(
                            catalog
                                .packs()
                                .iter()
                                .map(|pack| SharedString::from(pack.name.as_str())),
                        );
                        (items, packs, ready_status(catalog, picker))
                    }
                };
                ui.set_sticker_threaded(picker.destination.threaded());
                ui.set_sticker_items(ModelRc::new(VecModel::from(items)));
                ui.set_sticker_packs(ModelRc::new(VecModel::from(packs)));
                ui.set_sticker_pack_index(picker.pack.map_or(0, |pack| pack as i32 + 1));
                ui.set_sticker_index(picker.highlighted.map_or(-1, |row| row as i32));
                ui.set_sticker_phase(match picker.phase {
                    Phase::Loading => 0,
                    Phase::Ready(_) => 1,
                    Phase::Failed(_) => 2,
                });
                ui.set_sticker_status(status.into());
                ui.set_sticker_notice(picker.notice.clone().into());
                ui.set_sticker_needs_consent(
                    picker.catalog().is_some() && !picker.consented(),
                );
                let busy = state.enqueuing
                    || state
                        .tracked
                        .as_ref()
                        .is_some_and(|tracked| !tracked.finished);
                ui.set_sticker_can_send(
                    picker.catalog().is_some()
                        && picker.current_entry().is_some()
                        && picker.consented()
                        && !busy,
                );
                match &picker.preview.pixels {
                    Some(pixels) => {
                        ui.set_sticker_preview_image(slint::Image::from_rgba8(pixels.clone()));
                        ui.set_sticker_preview_loaded(true);
                    }
                    None => {
                        ui.set_sticker_preview_image(slint::Image::default());
                        ui.set_sticker_preview_loaded(false);
                    }
                }
                ui.set_sticker_preview_status(picker.preview.status.clone().into());
                ui.set_sticker_open(true);
            }
        }
        match state
            .tracked
            .as_ref()
            .filter(|tracked| tracked_visible(ui, tracked))
        {
            Some(tracked) => {
                ui.set_sticker_send_threaded(tracked.destination.threaded());
                ui.set_sticker_send_status(tracked.status.clone().into());
                ui.set_sticker_send_can_cancel(tracked.can_cancel);
                ui.set_sticker_send_can_retry(tracked.can_retry);
                ui.set_sticker_send_visible(true);
            }
            None => {
                ui.set_sticker_send_visible(false);
                ui.set_sticker_send_status(SharedString::new());
                ui.set_sticker_send_can_cancel(false);
                ui.set_sticker_send_can_retry(false);
            }
        }
    }
}

fn ready_status(catalog: &StickerCatalog, picker: &Picker) -> String {
    if catalog.entries().is_empty() {
        let mut text = String::from(
            "No sticker packs here yet. Packs come from this room and from packs you enabled in rooms you have joined.",
        );
        if catalog.unavailable_packs() > 0 {
            text.push_str(" Some enabled packs are in rooms not joined on this device.");
        }
        return text;
    }
    if picker.rows.is_empty() {
        return "No stickers match your search.".into();
    }
    let mut text = format!(
        "{} sticker{} in {} pack{}.",
        picker.rows.len(),
        if picker.rows.len() == 1 { "" } else { "s" },
        catalog.packs().len(),
        if catalog.packs().len() == 1 { "" } else { "s" },
    );
    if picker.rows.len() == MAX_ROWS {
        text.push_str(" Refine the search to see more.");
    }
    if catalog.skipped() > 0 {
        text.push_str(" Some malformed or oversized entries were left out.");
    }
    if catalog.unavailable_packs() > 0 {
        text.push_str(" Some enabled packs are in rooms not joined on this device.");
    }
    text
}

async fn next_room_update(
    updates: &mut Option<broadcast::Receiver<matrix_sdk::sync::RoomUpdates>>,
) {
    match updates {
        Some(receiver) => match receiver.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => {
                *updates = None;
                std::future::pending::<()>().await;
            }
        },
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::StickerPackSource;

    fn entry(label: &str, shortcode: &str, pack: usize) -> StickerEntry {
        StickerEntry {
            label: label.into(),
            shortcode: shortcode.into(),
            pack,
        }
    }

    fn pack(name: &str) -> StickerPack {
        StickerPack {
            name: name.into(),
            attribution: None,
            source: StickerPackSource::Room,
            stickers: 1,
        }
    }

    fn place(thread: Option<&str>) -> Destination {
        Destination {
            account: "robin-local".into(),
            room: "!room:example.org".into(),
            thread: thread.map(str::to_owned),
        }
    }

    #[test]
    fn search_matches_labels_shortcodes_and_pack_names_within_a_pack_filter() {
        let entries = [
            entry("Waving fox", "fox_wave", 0),
            entry("Sleeping owl", "owl_nap", 0),
            entry("Bright star", "star", 1),
        ];
        let packs = [pack("Forest friends"), pack("Night sky")];
        assert_eq!(filter(&entries, &packs, "", None), [0, 1, 2]);
        assert_eq!(filter(&entries, &packs, "  WAVING ", None), [0]);
        assert_eq!(filter(&entries, &packs, "owl_", None), [1]);
        assert_eq!(filter(&entries, &packs, "night", None), [2]);
        assert_eq!(filter(&entries, &packs, "", Some(0)), [0, 1]);
        assert_eq!(filter(&entries, &packs, "star", Some(0)), Vec::<usize>::new());
        let many: Vec<_> = (0..MAX_ROWS + 9).map(|_| entry("x", "x", 0)).collect();
        assert_eq!(filter(&many, &packs, "", None).len(), MAX_ROWS);
    }

    #[test]
    fn keys_belong_to_one_picker_session_and_a_shown_row() {
        let rows = [4, 9];
        assert_eq!(parse_key(&item_key(7, 9), 7, &rows), Some(1));
        assert_eq!(parse_key(&item_key(6, 9), 7, &rows), None);
        assert_eq!(parse_key(&item_key(7, 5), 7, &rows), None);
        for key in ["", "7", "7:", ":9", "7:x", "x:9", "7:9:1", "mxc://example.org/a"] {
            assert_eq!(parse_key(key, 7, &rows), None, "{key:?}");
        }
    }

    #[test]
    fn a_binding_needs_its_selection_composer_and_exact_destination() {
        let bound = place(Some("$root"));
        assert!(binding_matches(true, true, Some(&bound), &bound));
        assert!(!binding_matches(false, true, Some(&bound), &bound));
        assert!(!binding_matches(true, false, Some(&bound), &bound));
        assert!(!binding_matches(true, true, None, &bound));
        for other in [place(None), place(Some("$other")), {
            let mut account = bound.clone();
            account.account = "casey-local".into();
            account
        }] {
            assert!(!binding_matches(true, true, Some(&other), &bound));
        }
    }

    #[test]
    fn queue_updates_map_to_honest_status_without_resending() {
        assert_eq!(update_status(QueuedSendUpdate::Sent), ("Sticker sent.", false, false, true));
        let (_, cancel, retry, finished) =
            update_status(QueuedSendUpdate::Retained { recoverable: true });
        assert!(cancel && !retry && !finished);
        let (_, cancel, retry, finished) =
            update_status(QueuedSendUpdate::Retained { recoverable: false });
        assert!(cancel && retry && !finished);
        for update in [QueuedSendUpdate::Cancelled, QueuedSendUpdate::Unknown] {
            let (_, cancel, retry, finished) = update_status(update);
            assert!(!cancel && !retry && finished);
        }
    }

    #[test]
    fn failures_use_fixed_text_without_core_details() {
        let detail = MatrixCoreError::Sdk("PRIVATE mxc://example.org/secret".into());
        assert!(!send_failure(&detail).contains("PRIVATE"));
        assert!(!catalog_error(&detail).contains("PRIVATE"));
        assert!(!download_failure(&detail).contains("PRIVATE"));
        assert!(send_failure(&MatrixCoreError::StickerConsentRequired).contains("Allow"));
        assert!(send_failure(&MatrixCoreError::StickerUnavailable).contains("again"));
    }

    #[test]
    fn queries_are_bounded() {
        assert_eq!(bounded_query(&"q".repeat(MAX_QUERY_CHARS + 5)).chars().count(), MAX_QUERY_CHARS);
    }

    #[tokio::test]
    async fn a_stopped_picker_wakes_every_waiter_once_and_for_all() {
        let stop = Stop::default();
        let mut first = Box::pin(stop.wait());
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        stop.stop();
        tokio::time::timeout(Duration::from_secs(1), async {
            first.await;
            stop.wait().await;
        })
        .await
        .unwrap();
        assert!(stop.is_stopped());
    }

    #[test]
    fn the_decode_worker_is_single_and_released_on_drop() {
        let gate = Arc::new(AtomicBool::new(false));
        let permit = Permit::acquire(&gate).unwrap();
        assert!(Permit::acquire(&gate).is_none());
        drop(permit);
        assert!(Permit::acquire(&gate).is_some());
    }
}
